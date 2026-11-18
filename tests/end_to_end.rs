//! End-to-end tests: the real binary, a real config file on disk, and a stand-in
//! for the OpenCode CLI.
//!
//! The unit suites cover the request path with fakes injected. These cover what
//! fakes cannot: that the binary parses its arguments, finds its config, starts
//! the agent's CLI with the right command line and environment, speaks MCP over
//! real pipes, and asks a real socket which models it serves.
//!
//! No test here needs the network, a model server or an agent CLI.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ensemble");

/// Port 1 is reserved and nothing listens there.
const NOWHERE: &str = "http://127.0.0.1:1/v1";

/// A scratch project that cleans itself up.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ensemble-it-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The helper team: the team `team` in the scratch computer's teams
    /// directory, so its agents are `team/<agent>`.
    fn team_dir(&self) -> PathBuf {
        self.0
            .with_extension("xdg-config")
            .join("ensemble/node/teams/team")
    }

    /// Where the project's tasks and runs go: its state directory, under
    /// the scratch's own XDG state directory.
    fn state(&self) -> PathBuf {
        ensemble::state::dir_in(
            &self.0.with_extension("xdg-state").join("ensemble"),
            &self.0,
        )
        .unwrap()
    }

    /// This scratch computer's node file, `node.yaml`: the node `desk`.
    fn node_yaml(&self) -> PathBuf {
        let config = self.0.with_extension("xdg-config").join("ensemble/node");
        std::fs::create_dir_all(&config).unwrap();
        let path = config.join("node.yaml");
        if !path.exists() {
            std::fs::write(
                &path,
                "version: 1\nname: desk\naccess: private\noffers: {}\n",
            )
            .unwrap();
        }
        path
    }

    /// This scratch computer's node folder, `~/.config/ensemble/node/`.
    fn node_dir(&self) -> PathBuf {
        self.0.with_extension("xdg-config").join("ensemble/node")
    }

    /// A path in the project; `@team/<file>` is one in the helper team's
    /// folder, `@node/<file>` one in the node's, such as its `.env`, and
    /// `@state/<path>` one in the project's state directory.
    fn place(&self, name: &str) -> PathBuf {
        if let Some(rest) = name.strip_prefix("@node/") {
            self.node_dir().join(rest)
        } else if let Some(rest) = name.strip_prefix("@team/") {
            self.team_dir().join(rest)
        } else if let Some(rest) = name.strip_prefix("@state/") {
            self.state().join(rest)
        } else {
            self.0.join(name)
        }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.place(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).expect("write scratch file");
        if name == "@node/.env" {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        p
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.place(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
    }

    /// What the stand-in CLI saved: `args` or `config`.
    fn seen(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.with_extension("bin").join(name))
            .unwrap_or_else(|e| panic!("read {name}: {e}"))
    }

    /// A stand-in for `opencode`, kept outside the project so it is not one
    /// of the files a run changes. It answers `--version`; otherwise it saves
    /// its arguments (one per line) and its inline config beside itself,
    /// runs `body`, and prints `answer` as OpenCode's JSON events.
    fn opencode(&self, answer: &str, body: &str) -> String {
        let bin = self.0.with_extension("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("opencode");
        let event = serde_json::json!({"type": "text", "part": {"text": answer}}).to_string();
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s\\n' \"$@\" > '{seen}/args'\nprintf '%s' \"$OPENCODE_CONFIG_CONTENT\" > '{seen}/config'\n{body}\nprintf '%s\\n' '{{\"type\":\"step_start\",\"part\":{{}}}}'\nprintf '%s\\n' '{}'\n",
            event.replace('\'', "'\\''"),
            seen = bin.display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_dir_all(self.0.with_extension("bin"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-data"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-config"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("machines"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-state"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-cache"));
    }
}

/// The helper team: two OpenCode agents run by `command`: `reviewer` on a provider OpenCode
/// knows, and `coder` on the `openai-local` LLM backend at `backend_url`.
fn config_yaml(command: &str, backend_url: &str) -> String {
    format!(
        r#"
version: 1

llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url: "{backend_url}"
    api_key_env: ENSEMBLE_TEST_LOCAL_KEY
    timeout_seconds: 20

limits: {{}}

gates: {{}}

agents:
  reviewer:
    prompt: "Reviews code and reports findings."
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: {{ type: provider, name: opencode/reviewer-model }}
      tools: [read, glob, grep, list]
      system_prompt: replace
      max_steps: 30
      command: "{command}"
  coder:
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model:
        type: llm_backend
        try:
          - {{ backend: openai-local, name: qwen3-coder, context_window: 32768 }}
        when_none: fail
      tools: [read, edit]
      system_prompt: append
      max_steps: 30
      command: "{command}"
"#
    )
}

/// The binary, run in `scratch` with none of the test variables inherited:
/// a stale one would silently change what is being tested.
fn command(scratch: &Scratch) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(scratch.path())
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .env("XDG_CONFIG_HOME", scratch.0.with_extension("xdg-config"))
        .env("XDG_DATA_HOME", scratch.0.with_extension("xdg-data"))
        .env_remove("ENSEMBLE_TEST_LOCAL_URL")
        .env_remove("ENSEMBLE_TEST_LOCAL_KEY");
    c
}

/// The binary with only the scratch's directories set: no team, no state, as
/// an agent CLI starts `ensemble mcp` to reach a node.
fn bare_command(scratch: &Scratch) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(scratch.path())
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .env("XDG_CONFIG_HOME", scratch.0.with_extension("xdg-config"))
        .env("XDG_DATA_HOME", scratch.0.with_extension("xdg-data"));
    c
}

fn ensemble(scratch: &Scratch, args: &[&str]) -> std::process::Output {
    command(scratch).args(args).output().expect("run ensemble")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Answer every request with `body` (200, JSON) until the test ends.
fn models_stub(body: &str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let body = body.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(&stream);
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            // Read the body too: closing with it unread resets the connection,
            // and the client can see the reset before the answer.
            let _ = reader.read_exact(&mut vec![0; length]);
            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        }
    });
    url
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

#[test]
fn check_accepts_a_valid_config_and_reports_the_team() {
    let s = Scratch::new("check-ok");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("team: ok (made here)"), "{text}");
    assert!(text.contains("mcp: ready"), "{text}");
    assert!(
        text.contains("team/coder  ->  opencode on team/openai-local / qwen3-coder"),
        "{text}"
    );
    assert!(text.contains("openai-compatible-api"), "{text}");
}

#[test]
fn check_reports_several_problems_at_once() {
    let s = Scratch::new("check-many");
    s.write(
        "@team/config.yaml",
        r#"
version: 9
llm_backends:
  x: { type: openai-compatible-api, base_url: "nope", timeout_seconds: 5 }
limits: {}
gates: {}
agents:
  r:
    max_seconds: 60
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: { type: llm_backend, try: [{ backend: nowhere, name: "", context_window: 8000 }], when_none: fail }
      tools: []
      system_prompt: append
      max_steps: 3
      command: opencode
"#,
    );
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("version"), "{err}");
    assert!(err.contains("llm_backends.x"), "{err}");
    assert!(err.contains("nowhere"), "{err}");
}

#[test]
fn an_api_key_written_into_the_config_is_refused() {
    let s = Scratch::new("inline-key");
    s.write(
        "@team/config.yaml",
        r#"
version: 1
llm_backends:
  x:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:1/v1"
    timeout_seconds: 5
    api_key: "sk-should-never-be-here"
"#,
    );
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success(), "secrets in the config must not load");
    assert!(stderr_of(&out).contains("api_key"), "{}", stderr_of(&out));
}

#[test]
fn with_no_team_check_says_how_to_make_one() {
    let s = Scratch::new("no-org");
    // The project folder holds a config of the old kind; it is not looked at.
    s.write("config.yaml", "version: 2\n");
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success(), "no team means no helper team");
    assert!(
        stderr_of(&out).contains("ensemble node team new"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_broken_team_is_named_rather_than_skipped() {
    let s = Scratch::new("no-config");
    s.write("@team/config.yaml", "version: 1\n");
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("team 'team'"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn agents_prints_the_routing_table() {
    let s = Scratch::new("agents");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = ensemble(&s, &["node", "agent", "list"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("team/coder\topencode on team/openai-local\tqwen3-coder"),
        "{text}"
    );
    assert!(
        text.contains("team/reviewer\topencode\topencode/reviewer-model"),
        "{text}"
    );
}

#[test]
fn the_base_url_is_the_one_in_the_config_and_no_variable_changes_it() {
    let s = Scratch::new("url-from-config");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = command(&s)
        .args(["node", "team", "check"])
        .env("ENSEMBLE_TEST_LOCAL_URL", "http://10.0.0.9:9999/v1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains(NOWHERE), "{}", stdout_of(&out));
    assert!(!stdout_of(&out).contains("10.0.0.9"), "{}", stdout_of(&out));
}

// ---------------------------------------------------------------------------
// Starting the agent's CLI
// ---------------------------------------------------------------------------

#[test]
fn run_starts_the_agents_cli_with_the_task_and_prints_its_answer() {
    let s = Scratch::new("run");
    let cli = s.opencode("the review is clean", "");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));

    let out = ensemble(
        &s,
        &[
            "node",
            "agent",
            "run",
            "team/reviewer",
            "review",
            "this",
            "code",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("the review is clean"),
        "{}",
        stdout_of(&out)
    );
    // Provenance goes to stderr so stdout stays pipeable.
    assert!(
        stderr_of(&out).contains("team/reviewer via opencode / opencode/reviewer-model"),
        "{}",
        stderr_of(&out)
    );
    let args = s.seen("args");
    assert!(
        args.contains("-m\nopencode/reviewer-model\n--\nreview this code\n"),
        "{args}"
    );
    let config: serde_json::Value = serde_json::from_str(&s.seen("config")).unwrap();
    assert_eq!(
        config["agent"]["ensemble-team-reviewer"]["prompt"],
        "Reviews code and reports findings."
    );
    assert!(config.get("provider").is_none(), "{config}");
}

#[test]
fn an_agent_on_an_llm_backend_gets_its_endpoint_and_the_key_from_dotenv() {
    let s = Scratch::new("run-backend");
    let cli = s.opencode("done", "");
    s.write(
        "@team/config.yaml",
        &config_yaml(&cli, "http://127.0.0.1:8888/v1"),
    );
    s.write("@node/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");

    let out = ensemble(&s, &["node", "agent", "run", "team/coder", "do it"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        s.seen("args")
            .contains("-m\nensemble-team-openai-local/qwen3-coder\n"),
        "{}",
        s.seen("args")
    );
    let config: serde_json::Value = serde_json::from_str(&s.seen("config")).unwrap();
    let provider = &config["provider"]["ensemble-team-openai-local"];
    assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:8888/v1");
    assert_eq!(provider["options"]["apiKey"], "sk-from-dotenv");
    assert_eq!(provider["models"]["qwen3-coder"]["limit"]["context"], 32768);
}

#[test]
fn show_prints_how_an_agent_starts_with_the_key_hidden() {
    let s = Scratch::new("show");
    s.write(
        "@team/config.yaml",
        &config_yaml("opencode", "http://127.0.0.1:8888/v1"),
    );
    s.write("@node/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-secret-value\n");

    let out = ensemble(&s, &["node", "agent", "show", "team/coder"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.starts_with("opencode run --standalone --format json --agent ensemble-team-coder -m ensemble-team-openai-local/qwen3-coder -- '<task>'"),
        "{text}"
    );
    assert!(
        text.contains("\"baseURL\": \"http://127.0.0.1:8888/v1\""),
        "{text}"
    );
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("sk-secret-value"), "the key leaked: {text}");

    let out = ensemble(&s, &["node", "agent", "show", "nobody"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("team/coder, team/reviewer"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn files_the_agent_changed_are_reported() {
    let s = Scratch::new("changes");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");

    let out = ensemble(
        &s,
        &["node", "agent", "run", "team/coder", "rename foo to bar"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(s.read("a.txt"), "bar\n");
}

#[test]
fn health_checks_the_cli_and_that_the_backend_serves_the_model() {
    let s = Scratch::new("health");
    let cli = s.opencode("", "");
    let serving = models_stub(r#"{"object":"list","data":[{"id":"qwen3-coder"}]}"#);
    s.write("@team/config.yaml", &config_yaml(&cli, &serving));
    let out = ensemble(&s, &["node", "agent", "health"]);
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    let text = stdout_of(&out);
    assert!(text.contains(&format!("opencode ({cli}): ok")), "{text}");
    assert!(text.contains("openai-local: ok"), "{text}");

    let other = models_stub(r#"{"object":"list","data":[{"id":"something-else"}]}"#);
    s.write("@team/config.yaml", &config_yaml(&cli, &other));
    let out = ensemble(&s, &["node", "agent", "health"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(text.contains("openai-local: FAILED"), "{text}");
    assert!(
        text.contains("does not serve 'qwen3-coder'. It serves: something-else."),
        "{text}"
    );

    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let out = ensemble(&s, &["node", "agent", "health"]);
    assert!(!out.status.success());
    assert!(
        stdout_of(&out).contains("could not reach the endpoint"),
        "{}",
        stdout_of(&out)
    );
}

// ---------------------------------------------------------------------------
// The harness interface, over real pipes
// ---------------------------------------------------------------------------

/// A live `ensemble mcp` subprocess, driven the way a harness drives it,
/// with the node it is a session of.
struct McpSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
    /// The notifications that came before replies, in order.
    notes: Vec<serde_json::Value>,
    /// Stopped after the session, when this is dropped.
    _node: Served,
}

impl McpSession {
    fn start(scratch: &Scratch) -> Self {
        Self::start_with(scratch, &[])
    }

    /// A session of a node started with `extra` arguments, such as
    /// `--hub <dir>`.
    fn start_with(scratch: &Scratch, extra: &[&std::ffi::OsStr]) -> Self {
        let node = start_node(scratch, extra);
        let mut child = bare_command(scratch)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ensemble mcp");
        let reader = BufReader::new(child.stdout.take().unwrap());
        McpSession {
            child,
            reader,
            next_id: 0,
            notes: Vec::new(),
            _node: node,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();

        // Notifications (progress, log messages) may arrive before the reply.
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("read a reply");
            assert!(!line.trim().is_empty(), "server closed the stream");
            let v: serde_json::Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("reply was not JSON ({e}): {line}"));
            if v.get("id").is_none() && v.get("method").is_some() {
                self.notes.push(v);
                continue;
            }
            assert_eq!(v["id"], id, "replies must match requests: {v}");
            return v;
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = serde_json::json!({"jsonrpc": "2.0", "method": method});
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn call(&mut self, name: &str, args: serde_json::Value) -> serde_json::Value {
        self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        )
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool_text(reply: &serde_json::Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content: {reply}"))
        .to_string()
}

#[test]
fn a_harness_can_complete_the_mcp_handshake_and_list_the_tools() {
    let s = Scratch::new("mcp-handshake");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);

    let init = session.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "integration-test", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "ensemble");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    // A notification must produce no reply; if it did, the next read would
    // return it and the id assertion in `request` would fail.
    session.notify("notifications/initialized");

    let list = session.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "delegate",
            "submit",
            "status",
            "wait",
            "cancel",
            "assign",
            "answer",
            "review",
            "agents",
            "teams",
            "team_create",
            "health"
        ]
    );
}

#[test]
fn a_harness_can_delegate_in_the_background_and_collect_the_work() {
    let s = Scratch::new("mcp-background");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let started = session.call(
        "delegate",
        serde_json::json!({"agent": "team/coder", "task": "rename foo to bar", "background": true}),
    );
    assert_eq!(started["result"]["isError"], false, "{started}");
    assert!(tool_text(&started).starts_with("adhoc-1 started"));

    let reply = session.call(
        "wait",
        serde_json::json!({"task": "adhoc-1", "timeout_seconds": 30}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let text = tool_text(&reply);
    assert!(text.starts_with("renamed it"), "{text}");
    assert!(text.contains("[files changed: a.txt (modified)]"), "{text}");

    let status = tool_text(&session.call("status", serde_json::json!({})));
    assert!(status.contains("adhoc-1 · team/coder · done"), "{status}");
}

#[test]
fn a_harness_can_submit_a_task_and_collect_its_result_from_the_hub() {
    let s = Scratch::new("mcp-submit");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let _hub = with_hub(&s);
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let submitted = session.call(
        "submit",
        serde_json::json!({"agent": "team/coder", "title": "Rename foo", "task": "rename foo to bar"}),
    );
    assert_eq!(submitted["result"]["isError"], false, "{submitted}");
    assert!(tool_text(&submitted).starts_with("task 0001-rename-foo queued"));

    let reply = session.call(
        "wait",
        serde_json::json!({"task": "0001-rename-foo", "timeout_seconds": 30}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let text = tool_text(&reply);
    assert!(text.contains("status: done"), "{text}");
    assert!(text.contains("[files changed: a.txt (modified)]"), "{text}");

    drop(session);
    assert!(ended(&s, "0001-rename-foo"), "the hub has its result");
}

#[test]
fn a_harness_can_delegate_through_mcp_and_get_the_work_back() {
    let s = Scratch::new("mcp-delegate");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let agents = tool_text(&session.call("agents", serde_json::json!({})));
    assert!(agents.contains("coder [can change files]"), "{agents}");

    let reply = session.call(
        "delegate",
        serde_json::json!({"agent": "team/coder", "task": "rename foo to bar", "context": "in a.txt"}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let text = tool_text(&reply);
    assert!(text.starts_with("renamed it"), "{text}");
    assert!(
        text.ends_with(
            "[team/coder: 1 step(s), 0 tool call(s)]\n[files changed: a.txt (modified)]"
        ),
        "{text}"
    );
    // The task and its context reached the CLI.
    assert!(
        s.seen("args")
            .contains("Context:\nin a.txt\n\nrename foo to bar"),
        "{}",
        s.seen("args")
    );
}

#[test]
fn an_unknown_agent_comes_back_as_a_readable_tool_error_and_the_session_survives() {
    let s = Scratch::new("mcp-bad-agent");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let reply = session.call(
        "delegate",
        serde_json::json!({"agent": "architect", "task": "design it"}),
    );
    assert!(
        reply.get("error").is_none(),
        "must be a tool result, not a JSON-RPC error: {reply}"
    );
    assert_eq!(reply["result"]["isError"], true);
    let text = tool_text(&reply);
    assert!(text.contains("architect"), "{text}");
    assert!(text.contains("reviewer"), "lists the real agents: {text}");

    // The session continues — the point of returning this as a result.
    let after = session.call("agents", serde_json::json!({}));
    assert_eq!(after["result"]["isError"], false, "{after}");
}

#[test]
fn an_agent_failure_reaches_the_model_as_a_readable_reason() {
    let s = Scratch::new("mcp-agent-fails");
    let cli = s.opencode("", "echo 'Model unavailable: qwen3-coder' >&2\nexit 1");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let reply = session.call(
        "delegate",
        serde_json::json!({"agent": "team/coder", "task": "write a function"}),
    );
    assert_eq!(reply["result"]["isError"], true);
    let text = tool_text(&reply);
    assert!(
        text.contains("delegation failed (agent 'team/coder')"),
        "{text}"
    );
    assert!(text.contains("Model unavailable"), "{text}");
    assert!(
        text.contains("Retrying"),
        "tells the model what to do next: {text}"
    );
}

#[test]
fn the_tools_never_offer_a_way_to_pick_a_model_or_a_backend() {
    let s = Scratch::new("mcp-no-model-param");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let list = session.request("tools/list", serde_json::json!({}));
    let schema = list["result"]["tools"].to_string();
    assert!(!schema.contains("\"model\""), "{schema}");
    assert!(!schema.contains("\"backend\""), "{schema}");

    // And the agent listing does not leak them either.
    let agents = tool_text(&session.call("agents", serde_json::json!({})));
    assert!(!agents.contains("qwen3-coder"), "{agents}");
    assert!(!agents.contains("reviewer-model"), "{agents}");
}

#[test]
fn nothing_but_protocol_is_ever_written_to_stdout() {
    // One stray println corrupts the stream and the harness sees a parse error
    // instead of a tool. The stand-in CLI's own output must not leak either.
    let s = Scratch::new("mcp-clean-stdout");
    let cli = s.opencode("fine", "echo 'not json, from the CLI'");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));

    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));
    session.call(
        "delegate",
        serde_json::json!({"agent": "team/reviewer", "task": "look"}),
    );
    session.call("health", serde_json::json!({}));
    let last = session.request("ping", serde_json::json!({}));
    // Every read in `request` already asserted the line parsed as JSON with a
    // matching id; reaching here with the stream still aligned is the result.
    assert!(last["result"].is_object(), "{last}");
}

#[test]
fn a_malformed_message_does_not_end_the_session() {
    let s = Scratch::new("mcp-garbage");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);

    {
        let stdin = session.child.stdin.as_mut().unwrap();
        writeln!(stdin, "this is not json").unwrap();
        stdin.flush().unwrap();
    }
    let mut line = String::new();
    session.reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["error"]["code"], -32700, "{v}");

    // Still alive and aligned.
    let ping = session.request("ping", serde_json::json!({}));
    assert!(ping["result"].is_object());
}

#[test]
fn the_server_exits_cleanly_when_the_harness_closes_the_pipe() {
    let s = Scratch::new("mcp-eof");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    drop(session.child.stdin.take());
    let status = session.child.wait().expect("the server should exit on EOF");
    assert!(status.success(), "exited {status}");
}

#[test]
fn an_invalid_team_stops_the_node_before_it_serves_anything() {
    // Better to fail at start, where it is seen, than to accept sessions and
    // fail every delegation.
    let s = Scratch::new("node-bad-config");
    s.write("@team/config.yaml", "version: 1\nbackends: {}\n");
    s.node_yaml();
    let out = bare_command(&s).args(["node", "serve"]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("backend"), "{}", stderr_of(&out));
}

#[test]
fn team_new_writes_a_sample_helper_team_that_checks_and_never_overwrites_by_accident() {
    let s = Scratch::new("init");
    let out = ensemble(&s, &["node", "team", "new", "team"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let written = std::fs::read_to_string(s.team_dir().join("config.yaml")).unwrap();
    assert!(written.contains("type: opencode"), "{written}");
    assert!(written.contains("\nagents:\n"), "{written}");
    assert!(!written.contains("teams:"), "{written}");
    assert!(
        !s.team_dir().join(".env").exists() && !s.team_dir().join(".gitignore").exists(),
        "a helper team holds no secrets, so it needs no .env"
    );
    assert!(
        !s.path().join(".ensemble").exists(),
        "nothing is written in the project"
    );

    let check = ensemble(&s, &["node", "team", "check"]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    let text = stdout_of(&check);
    assert!(text.contains("team/coder  ->"), "{text}");

    s.write("@team/config.yaml", "mine");
    let again = ensemble(&s, &["node", "team", "new", "team"]);
    assert!(!again.status.success());
    assert!(
        stderr_of(&again).contains("already"),
        "{}",
        stderr_of(&again)
    );
    assert_eq!(
        std::fs::read_to_string(s.team_dir().join("config.yaml")).unwrap(),
        "mine"
    );

    let printed = ensemble(&s, &["node", "team", "new", "--stdout"]);
    assert_eq!(stdout_of(&printed), written);
}

#[test]
fn variables_come_from_the_env_file_unless_the_environment_sets_them() {
    let s = Scratch::new("dotenv");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    s.write("@node/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("authenticated"), "{text}");
    assert!(!text.contains("sk-from-dotenv"), "{text}");

    // --env-file is read instead of the node's .env.
    let other = s.write("keys.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-other\n");
    let out = command(&s)
        .arg("--env-file")
        .arg(&other)
        .args(["node", "agent", "show", "team/coder"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));

    s.write("@node/.env", "not a line\n");
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("line 1: expected NAME=value"),
        "{}",
        stderr_of(&out)
    );
    let out = command(&s)
        .arg("--env-file")
        .arg(&other)
        .args(["node", "team", "check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));

    // One that others can read is still read, with a note to make it private.
    let dotenv = s.write("@node/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dotenv, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("chmod 600"), "{}", stderr_of(&out));
}

#[test]
fn check_says_which_tasks_on_the_hub_this_node_cannot_run() {
    let s = Scratch::new("check-tasks");
    let _hub = with_hub(&s);
    let gated = config_yaml("opencode", NOWHERE).replace(
        "gates: {}",
        "gates:\n  test: { command: \"true\", timeout_seconds: 5 }",
    );
    s.write("@team/config.yaml", &gated);
    for args in [
        vec![
            "task",
            "new",
            "Ok",
            "--agent",
            "team/coder",
            "--text",
            "do it",
            "--gate",
            "test:1",
        ],
        vec![
            "task",
            "new",
            "Look",
            "--agent",
            "team/reviewer",
            "--text",
            "look",
        ],
    ] {
        let out = ensemble(&s, &args);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("tasks: 2 on the hub ws://")
            && stdout_of(&out).contains("2 unfinished"),
        "{}",
        stdout_of(&out)
    );

    // The gate is gone, and the reviewer is another node's now.
    s.write(
        "@team/config.yaml",
        &config_yaml("opencode", NOWHERE).replace("  reviewer:", "  critic:"),
    );
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("0001-ok: gate 'test' is not configured"),
        "{text}"
    );
    assert!(
        text.contains("0002-look: agent 'team/reviewer' is not one of this node's"),
        "{text}"
    );
}

#[test]
fn a_light_agent_is_one_request_to_its_backend() {
    let s = Scratch::new("llm-agent");
    let url = models_stub(
        r#"{"data":[{"id":"small"},{"id":"qwen3-coder"}],"choices":[{"message":{"content":"three bullets"},"finish_reason":"stop"}]}"#,
    );
    let config = format!(
        "{}  summariser:\n    prompt: You summarise.\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: direct\n      try: [{{ backend: openai-local, model: small }}]\n      when_none: fail\n      max_tokens: 300\n",
        config_yaml("opencode", &url)
    );
    s.write("@team/config.yaml", &config);

    let out = ensemble(
        &s,
        &["node", "agent", "run", "team/summariser", "sum", "it", "up"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("three bullets"),
        "{}",
        stdout_of(&out)
    );

    let out = ensemble(&s, &["node", "agent", "show", "team/summariser"]);
    let text = stdout_of(&out);
    assert!(
        text.starts_with(&format!("POST {url}/chat/completions\n")),
        "{text}"
    );
    assert!(
        text.contains("\"max_tokens\": 300") && text.contains("You summarise."),
        "{text}"
    );

    let check = stdout_of(&ensemble(&s, &["node", "team", "check"]));
    assert!(
        check.contains("team/summariser  ->  direct on team/openai-local / small"),
        "{check}"
    );
}

// ---------------------------------------------------------------------------
// Getting started
// ---------------------------------------------------------------------------

/// A directory of stand-ins for the client and agent CLIs. Each answers
/// `--version`, and otherwise saves its arguments, one per line, to
/// `<name>.args` beside itself.
fn stub_clis(scratch: &Scratch, names: &[&str]) -> PathBuf {
    let bin = scratch.0.with_extension("clis");
    std::fs::create_dir_all(&bin).unwrap();
    for name in names {
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s\\n' \"$@\" > '{}/{name}.args'\n",
                bin.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// The binary, with only `bin` on its PATH, so no real CLI is ever started.
fn with_path(scratch: &Scratch, bin: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(scratch.path())
        .env("PATH", bin)
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CONFIG_HOME", scratch.0.with_extension("xdg-config"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .env("XDG_DATA_HOME", scratch.0.with_extension("xdg-data"))
        .env("HOME", scratch.0.with_extension("home"));
    c
}

#[test]
fn connect_registers_a_session_of_the_node_with_the_client_and_says_what_it_ran() {
    for (client, program, scope) in [
        ("claude-code", "claude", "--scope\nuser\n"),
        ("opencode", "opencode", "--global\n"),
    ] {
        let s = Scratch::new(&format!("connect-{program}"));
        let bin = stub_clis(&s, &[program]);
        let out = with_path(&s, &bin)
            .args(["node", "connect", client, "--global"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr_of(&out));
        let said = stdout_of(&out);
        assert!(said.contains(&format!("$ {program} mcp add")), "{said}");
        assert!(said.contains("There is no node yet"), "{said}");
        let args = std::fs::read_to_string(bin.join(format!("{program}.args"))).unwrap();
        assert!(args.starts_with("mcp\nadd\n"), "{args}");
        assert!(args.contains(scope), "{args}");
        assert!(args.ends_with("\nmcp\n"), "{args}");
    }
}

#[test]
fn connect_without_the_clients_cli_prints_the_command_and_fails() {
    let s = Scratch::new("connect-missing");
    let bin = stub_clis(&s, &[]);
    let out = with_path(&s, &bin)
        .args(["node", "connect", "claude-code"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stdout_of(&out).contains("$ claude mcp add"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        stderr_of(&out).contains("not on PATH"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn setup_writes_the_node_registers_it_and_checks_its_teams() {
    let s = Scratch::new("setup");
    s.write(
        "@team/config.yaml",
        "version: 1\nllm_backends: {}\nlimits: {}\ngates: {}\nagents:\n  reviewer:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: { type: unlimited }\n    adapter: { type: claude-code, model: sonnet, tools: [Read], permission_mode: dontAsk, max_turns: 5, command: claude }\n",
    );
    let bin = stub_clis(&s, &["claude", "opencode"]);
    let out = with_path(&s, &bin)
        .args([
            "node",
            "setup",
            "--yes",
            "--client",
            "claude-code",
            "--name",
            "desk",
        ])
        .output()
        .unwrap();
    let said = stdout_of(&out);
    assert!(out.status.success(), "{said}\n{}", stderr_of(&out));
    assert!(said.contains("Done."), "{said}");
    let node =
        s.0.with_extension("xdg-config")
            .join("ensemble/node/node.yaml");
    let written = std::fs::read_to_string(&node).unwrap();
    assert!(
        written.contains("name: \"desk\"") && written.contains("version: 1"),
        "{written}"
    );
    let args = std::fs::read_to_string(bin.join("claude.args")).unwrap();
    assert!(args.ends_with("\nmcp\n"), "{args}");

    // A second run keeps the node that is there.
    let again = with_path(&s, &bin)
        .args(["node", "setup", "--yes", "--client", "claude-code"])
        .output()
        .unwrap();
    assert!(
        stdout_of(&again).contains("keeping it"),
        "{}",
        stdout_of(&again)
    );
    assert_eq!(std::fs::read_to_string(&node).unwrap(), written);
}

#[test]
fn setup_without_any_agent_cli_says_what_to_install() {
    let s = Scratch::new("setup-nothing");
    let bin = stub_clis(&s, &[]);
    let out = with_path(&s, &bin)
        .args(["node", "setup", "--yes", "--name", "desk"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("Install"), "{}", stderr_of(&out));
    assert!(
        !s.0.with_extension("xdg-config")
            .join("ensemble/node/node.yaml")
            .exists()
    );
}

#[test]
fn doctor_says_what_is_wrong_and_how_to_fix_it() {
    let s = Scratch::new("doctor");
    s.write(
        "@team/config.yaml",
        &config_yaml("/nonexistent/opencode", NOWHERE),
    );
    let out = ensemble(&s, &["node", "doctor"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("problem: opencode (/nonexistent/opencode)"),
        "{text}"
    );
    assert!(text.contains("fix: install `opencode`"), "{text}");
    assert!(
        text.contains("ensemble node agent remove team/coder"),
        "{text}"
    );
    assert!(text.contains("problem: team/openai-local"), "{text}");
    assert!(text.contains("fix: start the model server"), "{text}");

    let s = Scratch::new("doctor-ok");
    s.write("@team/config.yaml", &config_yaml("true", NOWHERE));
    let url = models_stub(r#"{"data":[{"id":"qwen3-coder"}]}"#);
    s.write("@team/config.yaml", &config_yaml("true", &url));
    std::fs::create_dir_all(s.path().join(".git")).unwrap();
    let out = ensemble(&s, &["node", "doctor"]);
    let text = stdout_of(&out);
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("problem"), "{text}");
}

#[test]
fn add_agent_and_remove_agent_edit_the_config_and_keep_it_loading() {
    let s = Scratch::new("add-agent");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(
        &s,
        &[
            "node",
            "agent",
            "add",
            "team/tester",
            "--adapter",
            "claude-code",
            "--prompt",
            "Tests it.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let agents = ensemble(&s, &["node", "agent", "list"]);
    assert!(
        stdout_of(&agents).contains("team/tester\tclaude-code\tsonnet"),
        "{}",
        stdout_of(&agents)
    );

    let dup = ensemble(
        &s,
        &[
            "node",
            "agent",
            "add",
            "team/tester",
            "--adapter",
            "claude-code",
        ],
    );
    assert!(!dup.status.success());
    assert!(
        stderr_of(&dup).contains("already has"),
        "{}",
        stderr_of(&dup)
    );

    let bad = ensemble(&s, &["node", "agent", "add", "team/x", "--adapter", "llm"]);
    assert!(!bad.status.success(), "`llm` is no adapter any more");

    let gone = ensemble(&s, &["node", "agent", "remove", "team/tester"]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(!stdout_of(&ensemble(&s, &["node", "agent", "list"])).contains("tester"));
    let missing = ensemble(&s, &["node", "agent", "remove", "team/tester"]);
    assert!(!missing.status.success());
}

// ---------------------------------------------------------------------------
// Remote helper teams
// ---------------------------------------------------------------------------

/// A git repository holding `config.yaml`, as a `file://` URL and its commit.
fn origin(scratch: &Scratch, config: &str) -> (String, String) {
    let dir = scratch.0.with_extension("origin");
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    if !dir.join(".git").exists() {
        git(&["init", "--quiet", "-b", "main"]);
    }
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "c"]);
    (
        format!("file://{}", dir.display()),
        git(&["rev-parse", "HEAD"]),
    )
}

/// What `pulled.json` of the team `name` pins it to.
fn pinned_commit(scratch: &Scratch, name: &str) -> String {
    let file = scratch
        .0
        .with_extension("xdg-config")
        .join("ensemble/node/teams")
        .join(name)
        .join("pulled.json");
    let pulled: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    pulled["commit"].as_str().unwrap().to_string()
}

#[test]
fn a_pulled_team_is_pinned_shown_trusted_and_then_used() {
    let s = Scratch::new("remote");
    let (url, sha) = origin(&s, &config_yaml("true", NOWHERE));

    // Without a terminal and without --yes there is nobody to ask: a refusal,
    // and nothing is kept.
    let refused = command(&s)
        .args(["node", "team", "pull", &url, "--name", "web"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("--yes"),
        "{}",
        stderr_of(&refused)
    );
    assert!(!stdout_of(&ensemble(&s, &["node", "team", "list"])).contains("web"));

    let pulled = command(&s)
        .args([
            "node",
            "team",
            "pull",
            &format!("{url}@main"),
            "--name",
            "web",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(pulled.status.success(), "{}", stderr_of(&pulled));
    let said = stdout_of(&pulled);
    assert!(said.contains("coder: runs `true`"), "{said}");
    assert!(
        said.contains(&format!("pulled {url}#{sha} as the team 'web'")),
        "{said}"
    );
    assert_eq!(pinned_commit(&s, "web"), sha);

    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains(&format!("web: ok (pulled from {url}#{sha})")),
        "{text}"
    );
    assert!(text.contains("web/coder"), "{text}");
    let again = command(&s)
        .args(["node", "team", "pull", &url, "--name", "web", "--yes"])
        .output()
        .unwrap();
    assert!(!again.status.success(), "a name is taken once");

    // A pin nobody trusted is not used, and says how to trust it.
    let (_, other) = origin(&s, &config_yaml("opencode", NOWHERE));
    let teams = s.0.with_extension("xdg-config").join("ensemble/node/teams");
    std::fs::create_dir_all(teams.join("sneaky")).unwrap();
    std::fs::write(
        teams.join("sneaky/pulled.json"),
        serde_json::json!({"url": url, "commit": other}).to_string(),
    )
    .unwrap();
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(!out.status.success(), "an untrusted commit is not used");
    assert!(
        stderr_of(&out).contains("team 'sneaky'")
            && stderr_of(&out).contains("ensemble node team pull"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn team_update_shows_what_changed_and_moves_only_once_trusted() {
    let s = Scratch::new("remote-update");
    let (url, old) = origin(&s, &config_yaml("true", NOWHERE));
    let pulled = command(&s)
        .args(["node", "team", "pull", &url, "--name", "web", "--yes"])
        .output()
        .unwrap();
    assert!(pulled.status.success(), "{}", stderr_of(&pulled));
    let (_, new) = origin(&s, &config_yaml("opencode", NOWHERE));

    let refused = command(&s)
        .args(["node", "team", "update", "web", "--ref", "main"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success(), "nobody trusted the new commit");
    assert!(
        stdout_of(&refused).contains("command"),
        "what changed is shown: {}",
        stdout_of(&refused)
    );
    assert_eq!(pinned_commit(&s, "web"), old, "the pin did not move");

    let moved = command(&s)
        .args(["node", "team", "update", "web", "--ref", "main", "--yes"])
        .output()
        .unwrap();
    assert!(moved.status.success(), "{}", stderr_of(&moved));
    assert!(
        stdout_of(&moved).contains(&format!("'web' is at {url}#{new} now")),
        "{}",
        stdout_of(&moved)
    );
    assert_eq!(pinned_commit(&s, "web"), new);
    let again = ensemble(&s, &["node", "team", "update", "web", "--ref", "main"]);
    assert!(
        stdout_of(&again).contains("already"),
        "{}",
        stdout_of(&again)
    );

    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let made = ensemble(&s, &["node", "team", "update", "team"]);
    assert!(!made.status.success());
    assert!(
        stderr_of(&made).contains("was not pulled"),
        "{}",
        stderr_of(&made)
    );
}

// ---------------------------------------------------------------------------
// Tasks from the terminal
// ---------------------------------------------------------------------------

#[test]
fn a_task_kept_on_the_hub_by_submit_is_listed_shown_and_run_by_a_session() {
    let s = Scratch::new("cli-tasks");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let _hub = with_hub(&s);

    let none = ensemble(&s, &["task", "list"]);
    assert!(
        stdout_of(&none).contains("No tasks"),
        "{}",
        stdout_of(&none)
    );

    let out = ensemble(
        &s,
        &[
            "task",
            "new",
            "Rename foo",
            "--agent",
            "team/coder",
            "--text",
            "rename foo to bar",
            "--goal",
            "no foo left",
            "--done-when",
            "a.txt says bar,tests pass",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("task 0001-rename-foo kept on the hub ws://"),
        "{}",
        stdout_of(&out)
    );

    let listed = ensemble(&s, &["task", "list"]);
    assert!(
        stdout_of(&listed).contains("0001-rename-foo · team/coder · queued · Rename foo"),
        "{}",
        stdout_of(&listed)
    );
    let shown = ensemble(&s, &["task", "show", "0001-rename-foo"]);
    let text = stdout_of(&shown);
    assert!(
        text.contains("rename foo to bar") && text.contains("Goal: no foo left"),
        "{text}"
    );
    assert!(text.contains("- tests pass"), "{text}");

    // A running session picks it up, as it does a task the manager wrote.
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));
    let reply = session.call(
        "wait",
        serde_json::json!({"task": "0001-rename-foo", "timeout_seconds": 30}),
    );
    assert!(
        tool_text(&reply).contains("status: done"),
        "{}",
        tool_text(&reply)
    );
    drop(session);

    let done = ensemble(&s, &["task", "list", "--status", "done"]);
    assert!(
        stdout_of(&done).contains("0001-rename-foo"),
        "{}",
        stdout_of(&done)
    );
    let queued = ensemble(&s, &["task", "list", "--status", "queued"]);
    assert!(
        stdout_of(&queued).contains("No tasks with status 'queued'"),
        "{}",
        stdout_of(&queued)
    );
    let shown = ensemble(&s, &["task", "show", "0001-rename-foo"]);
    assert!(
        stdout_of(&shown).contains("status: done"),
        "{}",
        stdout_of(&shown)
    );
}

#[test]
fn submit_refuses_what_a_task_cannot_be() {
    let s = Scratch::new("cli-submit-bad");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    // Tasks are kept on a hub: without one there is nowhere to keep them.
    s.node_yaml();
    let nowhere = ensemble(
        &s,
        &["task", "new", "T", "--agent", "team/coder", "--text", "x"],
    );
    assert!(!nowhere.status.success());
    assert!(
        stderr_of(&nowhere).contains("names no hub"),
        "{}",
        stderr_of(&nowhere)
    );
    let _hub = with_hub(&s);
    let no_text = ensemble(&s, &["task", "new", "T", "--agent", "team/coder"]);
    assert!(!no_text.status.success());
    for (args, needle) in [
        (
            vec!["task", "new", "T", "--agent", "nobody", "--text", "x"],
            "no agent",
        ),
        (
            vec![
                "task",
                "new",
                "T",
                "--agent",
                "team/coder",
                "--text",
                "x",
                "--gate",
                "nope:2",
            ],
            "no gate",
        ),
        (
            vec![
                "task",
                "new",
                "T",
                "--agent",
                "team/coder",
                "--text",
                "x",
                "--gate",
                "test:0",
            ],
            "attempts",
        ),
        (
            vec![
                "task",
                "new",
                "T",
                "--agent",
                "team/coder",
                "--text",
                "x",
                "--repo",
                "../out",
            ],
            "`repo`",
        ),
    ] {
        let out = ensemble(&s, &args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stderr_of(&out).contains(needle),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }
    assert!(stdout_of(&ensemble(&s, &["task", "list"])).contains("No tasks"));
}

#[test]
fn answer_gives_a_waiting_task_its_answer() {
    let s = Scratch::new("cli-answer");
    let cli = s.opencode(
        "x",
        r#"if printf '%s' "$*" | grep -q 'use bar'; then t="renamed with bar"; else t="NEEDS INPUT: Which name?"; fi
printf '%s\n' "{\"type\":\"text\",\"part\":{\"text\":\"$t\"}}"; exit 0"#,
    );
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let _hub = with_hub(&s);
    let _node = start_node(&s, &[]);
    let out = ensemble(
        &s,
        &[
            "task",
            "new",
            "Ask",
            "--agent",
            "team/coder",
            "--text",
            "rename it",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        wait_for(30, || stdout_of(&ensemble(&s, &["task", "list"]))
            .contains("needs input: Which name?")),
        "{}",
        stdout_of(&ensemble(&s, &["task", "list"]))
    );
    let none = ensemble(&s, &["task", "answer", "0001-ask"]);
    assert!(!none.status.success());
    let out = ensemble(&s, &["task", "answer", "0001-ask", "use", "bar"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let again = ensemble(&s, &["task", "answer", "0001-ask", "twice"]);
    assert!(
        !again.status.success(),
        "a second answer has no question to answer"
    );
    assert!(wait_for(30, || ended(&s, "0001-ask")), "it did not go on");
    let shown = stdout_of(&ensemble(&s, &["task", "show", "0001-ask"]));
    assert!(
        shown.contains("status: done") && shown.contains("renamed with bar"),
        "{shown}"
    );
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

/// Wait up to `seconds` for `done` to be true.
fn wait_for(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    done()
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn the_node_runs_a_submitted_task_without_a_session_and_stops_on_sigterm() {
    let s = Scratch::new("serve");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let _hub = with_hub(&s);
    let mut node = start_node(&s, &[]);
    // `submit` tells the node about the folder, which no session has opened.
    let out = ensemble(
        &s,
        &[
            "task",
            "new",
            "Rename foo",
            "--agent",
            "team/coder",
            "--text",
            "rename foo to bar",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("your node runs it"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        wait_for(30, || ended(&s, "0001-rename-foo")),
        "the node did not run the task"
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );
    let listed = ensemble(&s, &["task", "list"]);
    assert!(
        stdout_of(&listed).contains("done"),
        "{}",
        stdout_of(&listed)
    );

    // One node per computer: a second one refuses.
    let second = bare_command(&s)
        .args(["node", "serve"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        stderr_of(&second).contains("already runs the node"),
        "{}",
        stderr_of(&second)
    );

    sigterm(&node);
    let status = node.wait().unwrap();
    assert!(status.success(), "the node ends cleanly on SIGTERM");
    assert!(
        !s.place("@state/worker.lock").exists(),
        "the folder's lock is released"
    );
}

#[test]
fn a_task_stopped_in_the_middle_is_run_again_when_the_node_starts_again() {
    let s = Scratch::new("serve-resume");
    // The first run is slow and is stopped; the marker says it had started, so
    // the run after it answers at once.
    let marker = s.0.with_extension("bin").join("started");
    let body = format!(
        "if [ -e '{0}' ]; then :; else touch '{0}'; sleep 60; fi",
        marker.display()
    );
    let cli = s.opencode("done", &body);
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let _hub = with_hub(&s);
    let mut first = start_node(&s, &[]);
    let out = ensemble(
        &s,
        &[
            "task",
            "new",
            "Slow",
            "--agent",
            "team/coder",
            "--text",
            "take your time",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(wait_for(30, || marker.exists()));
    let listed = ensemble(&s, &["task", "list"]);
    assert!(
        stdout_of(&listed).contains("running on desk"),
        "{}",
        stdout_of(&listed)
    );
    sigterm(&first);
    first.wait().unwrap();
    assert!(!ended(&s, "0001-slow"), "a stopped task has no result");
    let listed = ensemble(&s, &["task", "list"]);
    assert!(
        stdout_of(&listed).contains("queued (interrupted on desk, where its work is)"),
        "{}",
        stdout_of(&listed)
    );

    // The node remembers the folder, and runs it again when it starts.
    let _second = start_node(&s, &[]);
    assert!(
        wait_for(30, || ended(&s, "0001-slow")),
        "the node did not run it again"
    );
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[test]
fn every_run_leaves_a_record_that_stats_sums_up() {
    let s = Scratch::new("stats");
    let cli = s.opencode("fine", "");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let none = ensemble(&s, &["node", "agent", "stats"]);
    assert!(
        stdout_of(&none).contains("No runs recorded"),
        "{}",
        stdout_of(&none)
    );

    for _ in 0..2 {
        let out = ensemble(&s, &["node", "agent", "run", "team/reviewer", "look"]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    // A run that cannot start is a failure on the record.
    s.write(
        "@team/config.yaml",
        &config_yaml("/nonexistent/opencode", NOWHERE),
    );
    let out = ensemble(&s, &["node", "agent", "run", "team/reviewer", "look"]);
    assert!(!out.status.success());

    let table = ensemble(&s, &["node", "agent", "stats"]);
    let text = stdout_of(&table);
    let row = text
        .lines()
        .find(|l| l.starts_with("team/reviewer"))
        .unwrap_or_else(|| panic!("{text}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!((cells[1], cells[2]), ("3", "1"), "3 runs, 1 failure: {row}");

    let json = ensemble(&s, &["node", "agent", "stats", "--json", "--since", "1h"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(&json)).unwrap();
    assert_eq!(parsed[0]["agent"], "team/reviewer");
    assert_eq!(parsed[0]["runs"], 3);

    let bad = ensemble(&s, &["node", "agent", "stats", "--since", "soon"]);
    assert!(!bad.status.success());
    assert!(stderr_of(&bad).contains("duration"), "{}", stderr_of(&bad));
}

// ---------------------------------------------------------------------------
// A helper team that works on its own
// ---------------------------------------------------------------------------

#[test]
fn a_backlog_task_waits_until_it_is_assigned_and_the_helper_team_says_when_it_is_idle() {
    let s = Scratch::new("backlog");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let _hub = with_hub(&s);

    let out = ensemble(
        &s,
        &["task", "new", "Rename foo", "--text", "rename foo to bar"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let listed = stdout_of(&ensemble(&s, &["task", "list"]));
    assert!(listed.contains("0001-rename-foo · - · backlog"), "{listed}");
    assert!(
        listed.contains("Idle: no active tasks (1 in the backlog, to be assigned)"),
        "{listed}"
    );

    // Nothing runs it while it has no assignee.
    let node = start_node(&s, &[]);
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!ended(&s, "0001-rename-foo"));

    let bad = ensemble(&s, &["task", "assign", "0001-rename-foo", "nobody"]);
    assert!(!bad.status.success());
    let ok = ensemble(&s, &["task", "assign", "0001-rename-foo", "team/coder"]);
    assert!(ok.status.success(), "{}", stderr_of(&ok));
    assert!(
        stdout_of(&ok).contains("assigned to team/coder"),
        "{}",
        stdout_of(&ok)
    );
    assert!(
        wait_for(30, || ended(&s, "0001-rename-foo")),
        "the node did not run the task once assigned"
    );
    drop(node);
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );
    assert!(stdout_of(&ensemble(&s, &["task", "list"])).contains("Idle: no active tasks"));
    let shown = stdout_of(&ensemble(&s, &["task", "show", "0001-rename-foo"]));
    assert!(shown.contains("status: done"), "{shown}");
}

#[test]
fn check_warns_about_what_agents_may_do_without_failing() {
    let s = Scratch::new("check-warn");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(&s, &["node", "team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("warnings:"), "{text}");
    assert!(
        text.contains("coder can change files directly in your working folder"),
        "{text}"
    );
    assert!(text.contains("isolation: worktree"), "{text}");
    assert!(!text.contains("reviewer can change"), "{text}");
}

#[test]
fn show_prints_how_a_codex_agent_starts() {
    let s = Scratch::new("show-codex");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(
        &s,
        &[
            "node",
            "agent",
            "add",
            "team/fixer",
            "--adapter",
            "codex",
            "--prompt",
            "Fixes it.",
            "--writes",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let shown = ensemble(&s, &["node", "agent", "show", "team/fixer"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let text = stdout_of(&shown);
    assert!(
        text.starts_with("codex exec --json --skip-git-repo-check --sandbox workspace-write --model gpt-5-codex -- "),
        "{text}"
    );
    assert!(text.contains("Fixes it."), "{text}");
    let agents = stdout_of(&ensemble(&s, &["node", "agent", "list"]));
    assert!(agents.contains("fixer\tcodex\tgpt-5-codex"), "{agents}");
}

#[test]
fn a_light_agent_on_an_anthropic_backend_shows_a_messages_request() {
    let s = Scratch::new("show-anthropic");
    let v1 = format!(
        "{}\n",
        config_yaml("opencode", NOWHERE).replacen(
            "llm_backends:\n",
            "llm_backends:\n  claude-api:\n    type: anthropic-api\n    base_url: \"https://api.anthropic.com\"\n    api_key_env: ENSEMBLE_TEST_ANTHROPIC_KEY\n    timeout_seconds: 30\n",
            1,
        )
    );
    s.write("@team/config.yaml", &v1);
    let out = ensemble(
        &s,
        &[
            "node",
            "agent",
            "add",
            "team/helper",
            "--adapter",
            "direct",
            "--backend",
            "claude-api",
            "--model",
            "claude-haiku-4-5",
            "--prompt",
            "You help.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let shown = ensemble(&s, &["node", "agent", "show", "team/helper"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let text = stdout_of(&shown);
    assert!(
        text.starts_with("POST https://api.anthropic.com/v1/messages"),
        "{text}"
    );
    assert!(text.contains("\"system\": \"You help.\""), "{text}");
    let check = stdout_of(&ensemble(&s, &["node", "team", "check"]));
    assert!(check.contains("claude-api  [anthropic-api]"), "{check}");
}

// ---------------------------------------------------------------------------
// Reviewing and merging a worktree task
// ---------------------------------------------------------------------------

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_worktree_task_is_reviewed_by_anyone_and_merged_only_by_the_person_who_types_it() {
    let s = Scratch::new("review-merge");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(
        "@team/config.yaml",
        &config_yaml(&cli, NOWHERE).replace("isolation: shared", "isolation: worktree"),
    );
    s.write("a.txt", "foo\n");
    git_in(s.path(), &["init", "--quiet", "-b", "main"]);
    // The worktree commit and the merge are made as whoever the repository says.
    git_in(s.path(), &["config", "user.name", "T"]);
    git_in(s.path(), &["config", "user.email", "t@t"]);
    git_in(s.path(), &["add", "a.txt"]);
    git_in(s.path(), &["commit", "--quiet", "-m", "start"]);

    let _hub = with_hub(&s);
    let node = start_node(&s, &[]);
    let out = ensemble(
        &s,
        &[
            "task",
            "new",
            "Rename foo",
            "--agent",
            "team/coder",
            "--text",
            "rename foo to bar",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        wait_for(30, || ended(&s, "0001-rename-foo")),
        "the node did not run the task"
    );
    drop(node);
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "foo\n",
        "the project is untouched"
    );

    let review = ensemble(&s, &["task", "review", "0001-rename-foo"]);
    let text = stdout_of(&review);
    assert!(review.status.success(), "{}", stderr_of(&review));
    assert!(
        text.contains("branch ensemble/0001-rename-foo") && text.contains("a.txt | 2"),
        "{text}"
    );
    assert!(text.contains("Result:"), "{text}");
    let diff = stdout_of(&ensemble(&s, &["task", "diff", "0001-rename-foo"]));
    assert!(diff.contains("-foo\n+bar"), "{diff}");

    // The manager can look, through the review tool, and has nothing to merge with.
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));
    let looked = session.call("review", serde_json::json!({"task": "0001-rename-foo"}));
    assert!(
        tool_text(&looked).contains("a.txt | 2"),
        "{}",
        tool_text(&looked)
    );
    let tools = session.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.iter().any(|n| n.contains("merge")), "{names:?}");
    drop(session);

    std::fs::write(s.path().join("a.txt"), "dirty\n").unwrap();
    let dirty = ensemble(&s, &["task", "merge", "0001-rename-foo"]);
    assert!(!dirty.status.success());
    assert!(
        stderr_of(&dirty).contains("uncommitted changes"),
        "{}",
        stderr_of(&dirty)
    );
    git_in(s.path(), &["checkout", "--quiet", "a.txt"]);

    let merged = ensemble(
        &s,
        &["task", "merge", "0001-rename-foo", "--squash", "--cleanup"],
    );
    assert!(merged.status.success(), "{}", stderr_of(&merged));
    assert!(
        stdout_of(&merged).contains("squashed"),
        "{}",
        stdout_of(&merged)
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );
    assert!(!s.place("@state/worktrees/0001-rename-foo").exists());
    let again = ensemble(&s, &["task", "review", "0001-rename-foo"]);
    assert!(!again.status.success(), "its branch is gone");
}

#[test]
fn show_lists_an_agents_models_in_order_and_probe_says_which_a_run_would_use() {
    let s = Scratch::new("show-probe");
    let central = models_stub(r#"{"data":[{"id":"big"}]}"#);
    s.write(
        "@team/config.yaml",
        &format!(
            "version: 1\nllm_backends:\n  home: {{ type: openai-compatible-api, base_url: \"{NOWHERE}\", timeout_seconds: 5 }}\n  central: {{ type: openai-compatible-api, base_url: \"{central}\", timeout_seconds: 5 }}\nlimits: {{}}\ngates: {{}}\nagents:\n  summariser:\n    max_seconds: 30\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try:\n        - {{ backend: home, model: small }}\n        - {{ backend: central, model: big }}\n      when_none: wait\n      max_tokens: 10\n"
        ),
    );
    let out = ensemble(&s, &["node", "agent", "show", "team/summariser"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("models, in order (the first is shown above; when none works: wait):"),
        "{text}"
    );
    assert!(
        text.contains("  1. team/home / small\n  2. team/central / big\n"),
        "{text}"
    );
    assert!(!text.contains("a run now"), "{text}");

    let probed = ensemble(&s, &["node", "agent", "show", "team/summariser", "--probe"]);
    assert!(probed.status.success(), "{}", stderr_of(&probed));
    assert!(
        stdout_of(&probed).contains("a run now would use 2."),
        "{}",
        stdout_of(&probed)
    );

    let health = ensemble(&s, &["node", "agent", "health"]);
    assert!(!health.status.success(), "home is down");
    let said = stdout_of(&health);
    assert!(
        said.contains("team/home: FAILED") && said.contains("team/central: ok"),
        "{said}"
    );
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

#[test]
fn hub_init_writes_a_hub_and_its_vault_and_check_says_which_model_distills() {
    let scratch = Scratch::new("hub-init");
    let hub = hub_dir(&scratch);
    let out = ensemble(&scratch, &["hub", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("ensemble hub init"),
        "{}",
        stderr_of(&out)
    );

    let out = ensemble(&scratch, &["hub", "init"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(hub.join("hub.yaml").exists());
    assert!(
        hub.join("vault/.git").exists(),
        "the vault is a git repository"
    );
    let again = ensemble(&scratch, &["hub", "init"]);
    assert!(
        !again.status.success(),
        "it does not overwrite without --force"
    );

    let url = models_stub(r#"{"data":[{"id":"small"}]}"#);
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let (head, tail) = text.split_once("llm_backends:").unwrap();
    let (_, rest) = tail.split_once("# ---").unwrap();
    let rest = rest.split_once("knowledge:").unwrap().1;
    let rest = rest.split_once("  recall:").unwrap().1;
    std::fs::write(
        hub.join("hub.yaml"),
        format!(
            "{head}llm_backends:\n  down: {{ type: openai-compatible-api, base_url: \"{NOWHERE}\", timeout_seconds: 5 }}\n  up: {{ type: openai-compatible-api, base_url: \"{url}\", timeout_seconds: 5 }}\n\nknowledge:\n  vault: vault\n  distill:\n    try:\n      - {{ backend: down, model: small }}\n      - {{ backend: up, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall:{rest}"
        ),
    )
    .unwrap();
    let out = ensemble(&scratch, &["hub", "check"]);
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stdout_of(&out).contains("distilled by small on up"),
        "{}",
        stdout_of(&out)
    );
}

/// The files under the vault's `shares/`.
fn share_files(hub: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![hub.join("vault/shares")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                found.push(std::fs::read_to_string(path).unwrap());
            }
        }
    }
    found
}

#[test]
fn knowledge_is_shared_by_hand_or_by_the_agent_through_the_nodes_hub() {
    let s = Scratch::new("hub-share");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(&s, &["knowledge", "share", "--title", "x", "--text", "y"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no node file"),
        "without a node there is no hub: {}",
        stderr_of(&out)
    );
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    node_with_hub(&s, &format!("ws://{address}"), &token);

    let out = ensemble(
        &s,
        &[
            "knowledge",
            "share",
            "--title",
            "Ports",
            "--text",
            "The shop listens on 8080.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("Shared as "),
        "{}",
        stdout_of(&out)
    );
    let files = share_files(&hub);
    assert_eq!(files.len(), 1);
    assert!(files[0].contains("client: ensemble-cli"), "{}", files[0]);
    assert!(
        files[0].contains("The shop listens on 8080."),
        "{}",
        files[0]
    );

    let mut session = McpSession::start(&s);
    session.request(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "claude-code", "version": "2"}}),
    );
    let list = session.request("tools/list", serde_json::json!({}));
    assert!(list.to_string().contains("\"share\""), "{list}");
    let reply = session.request(
        "tools/call",
        serde_json::json!({"name": "share", "arguments": {
            "title": "Deploys", "text": "Deploys go out on Tuesdays only."}}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let files = share_files(&hub);
    assert_eq!(files.len(), 2);
    assert!(
        files
            .iter()
            .any(|f| f.contains("client: claude-code") && f.contains("Tuesdays")),
        "{files:?}"
    );
}

/// A model that answers every chat request with these items.
fn items_stub(items: &str) -> String {
    let chat = serde_json::json!({
        "choices": [{"message": {"content": items}, "finish_reason": "stop"}]
    });
    models_stub(&chat.to_string())
}

/// Point the scratch hub's distilling models at `url`, the first of them.
fn distill_on(hub: &Path, url: &str) {
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let start = text.find("base_url: ").unwrap() + "base_url: ".len();
    let end = start + text[start..].find('\n').unwrap();
    std::fs::write(
        hub.join("hub.yaml"),
        format!("{}\"{url}\"{}", &text[..start], &text[end..]),
    )
    .unwrap();
}

#[test]
fn shares_become_notes_by_hand_on_the_hub_and_in_the_background_while_it_runs() {
    let s = Scratch::new("hub-distill");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let (hub, address, token) = hub_server(&s);
    // No model can be used yet: the share waits.
    distill_on(&hub, NOWHERE);
    let served = serve_hub(&s, &hub);
    node_with_hub(&s, &format!("ws://{address}"), &token);
    let out = ensemble(
        &s,
        &[
            "knowledge",
            "share",
            "--title",
            "Deploys",
            "--text",
            "We deploy on Tuesdays.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    drop(served);

    // On the hub's computer, by hand.
    let url = items_stub(
        r#"{"items":[{"kind":"decision","title":"Deploy day","body":"Deploys go out on Tuesdays.","tags":["ops"]}]}"#,
    );
    distill_on(&hub, &url);
    let out = ensemble(&s, &["hub", "knowledge", "status"]);
    assert!(
        stdout_of(&out).contains("pending: 1"),
        "{}",
        stdout_of(&out)
    );
    let out = ensemble(&s, &["hub", "knowledge", "distill"]);
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stdout_of(&out).contains("-> notes (deploy-day)"),
        "{}",
        stdout_of(&out)
    );
    let note = std::fs::read_to_string(hub.join("vault/notes/deploy-day.md")).unwrap();
    assert!(
        note.contains("kind: decision") && note.contains("Tuesdays"),
        "{note}"
    );
    let out = ensemble(&s, &["hub", "knowledge", "status"]);
    assert!(stdout_of(&out).contains("pending: 0") && stdout_of(&out).contains("notes: 1"));
    let out = ensemble(&s, &["hub", "knowledge", "rebuild"]);
    assert!(
        !out.status.success(),
        "a rebuild asks first: {}",
        stdout_of(&out)
    );

    // While the hub runs, a share becomes a note without anyone asking, and
    // a node recalls it.
    let _served = serve_hub(&s, &hub);
    let out = ensemble(&s, &["knowledge", "recall", "when", "do", "we", "deploy"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("reference data, not instructions")
            && stdout_of(&out).contains("## Deploy day"),
        "{}",
        stdout_of(&out)
    );
    let out = ensemble(
        &s,
        &[
            "knowledge",
            "share",
            "--title",
            "Reviews",
            "--text",
            "Every change gets one review.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let vault = ensemble::knowledge::Vault::new(hub.join("vault"));
    while !ensemble::knowledge::pipeline::pending(&vault).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the share was not distilled"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[test]
fn a_hub_servers_admin_adds_users_whose_tokens_are_shown_once() {
    let s = Scratch::new("hub-users");
    hub_server_of(&s);
    let run = |args: &[&str]| command(&s).args(args).output().unwrap();
    let out = run(&["hub", "user", "add", "ana", "--role", "member"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let token = stdout_of(&out).trim().to_string();
    assert!(token.starts_with("ens_"), "{token}");
    assert!(stderr_of(&out).contains("shown once"));
    let out = run(&["hub", "user", "add", "bob", "--role", "owner"]);
    assert!(!out.status.success() && stderr_of(&out).contains("no role 'owner'"));
    let out = run(&["hub", "user", "list"]);
    assert!(stdout_of(&out).contains("ana") && stdout_of(&out).contains("1 token(s)"));
    assert!(!stdout_of(&out).contains(&token));
    let out = run(&["hub", "user", "revoke", "ana"]);
    assert!(
        stdout_of(&out).contains("revoked 1 token(s)"),
        "{}",
        stdout_of(&out)
    );
}

/// A hub server made by `hub init`, listening on a free loopback port, with
/// `ana` a member. Returns the hub, its address and ana's token.
/// A hub server made by `hub init --server`, beside the project.
/// The scratch computer's hub folder, `~/.config/ensemble/hub/`.
fn hub_dir(scratch: &Scratch) -> PathBuf {
    scratch.0.with_extension("xdg-config").join("ensemble/hub")
}

/// A hub made by `ensemble hub init` on the scratch computer.
fn hub_server_of(scratch: &Scratch) -> PathBuf {
    let out = command(scratch).args(["hub", "init"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    // No test reaches a hosted model: the sample's Anthropic backend goes.
    let hub = hub_dir(scratch);
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let start = text.find("  claude:\n").unwrap();
    let end = start + text[start..].find("\n\n").unwrap() + 1;
    let text = format!("{}{}", &text[..start], &text[end..]).replace(
        "      - backend: claude\n        model: claude-haiku-4-5\n",
        "",
    );
    std::fs::write(hub.join("hub.yaml"), text).unwrap();
    hub
}

/// The scratch computer's node, naming the hub at `url`.
fn node_names_hub(scratch: &Scratch, url: &str) {
    let config = scratch.0.with_extension("xdg-config").join("ensemble/node");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("node.yaml"),
        format!("version: 1\nname: desk\naccess: private\nhub: \"{url}\"\noffers: {{}}\n"),
    )
    .unwrap();
}

/// The scratch computer's node, naming the hub at `url` and signed in to it
/// with `token`.
fn node_with_hub(scratch: &Scratch, url: &str, token: &str) {
    node_names_hub(scratch, url);
    let out = command(scratch)
        .args(["node", "login", url, "--token", token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
}

/// A port for a hub to listen on, free now. Taken below the range the system
/// gives out for outgoing connections, so that no test's connection takes it
/// before the hub binds it, and counted up per process, so that no two tests
/// pick the same one.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 400) as u16 * 25;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 10_000;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

fn hub_server(scratch: &Scratch) -> (PathBuf, String, String) {
    let hub = hub_server_of(scratch);
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    std::fs::write(
        hub.join("hub.yaml"),
        text.replace("127.0.0.1:7700", &address),
    )
    .unwrap();
    let out = command(scratch)
        .args(["hub", "user", "add", "ana", "--role", "member"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    (hub, address, stdout_of(&out).trim().to_string())
}

/// The scratch computer with a hub of its own, running until dropped, which
/// its node names and is signed in to as ana: where its tasks are kept.
fn with_hub(scratch: &Scratch) -> Served {
    let (hub, address, token) = hub_server(scratch);
    let served = serve_hub(scratch, &hub);
    node_with_hub(scratch, &format!("ws://{address}"), &token);
    served
}

/// Whether task `id` of the scratch project has ended, as the hub says.
fn ended(scratch: &Scratch, id: &str) -> bool {
    stdout_of(&ensemble(scratch, &["task", "show", id])).contains("\n---\nstatus: ")
}

/// A running `ensemble hub serve`, stopped when the test ends, passed or
/// failed: left running, it would keep the test runner's output open.
struct Served(Child);

impl std::ops::Deref for Served {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Served {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// This scratch computer's node, running its helper team until dropped;
/// `extra` goes to `node serve`, such as `--hub <dir>`.
fn start_node(s: &Scratch, extra: &[&std::ffi::OsStr]) -> Served {
    s.node_yaml();
    let mut c = bare_command(s);
    c.args(["node", "serve"]).args(extra);
    let mut node = c
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = BufReader::new(node.stderr.take().unwrap());
    let (mut line, mut all) = (String::new(), String::new());
    while !line.contains("ready for `ensemble mcp` sessions") {
        line.clear();
        assert!(
            said.read_line(&mut line).unwrap() > 0,
            "the node stopped: {all}"
        );
        all.push_str(&line);
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut said, &mut std::io::sink());
    });
    Served(node)
}

fn serve_hub(scratch: &Scratch, _hub: &Path) -> Served {
    let mut child = command(scratch)
        .args(["hub", "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap() > 0, "it stopped: {seen}");
        seen.push_str(&line);
        if line.contains("listening on") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = err.read_to_string(&mut rest);
    });
    Served(child)
}

#[test]
fn a_hub_server_refuses_a_node_without_a_token_and_stops_on_sigterm() {
    let s = Scratch::new("hub-serve");
    let (hub, address, _) = hub_server(&s);
    let mut child = serve_hub(&s, &hub);
    let mut stream = std::net::TcpStream::connect(&address).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(&stream).read_line(&mut status).unwrap();
    assert!(status.contains("401"), "{status}");

    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn a_node_signs_in_to_a_hub_server_and_shares_and_recalls_there() {
    let s = Scratch::new("hub-remote");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let (hub, address, token) = hub_server(&s);
    let mut child = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    node_names_hub(&s, &url);

    let out = command(&s)
        .args(["node", "login", &url, "--token", "ens_wrong"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("did not accept the token"),
        "{}",
        stderr_of(&out)
    );
    let out = command(&s)
        .args(["knowledge", "share", "--title", "x", "--text", "y"])
        .output()
        .unwrap();
    assert!(
        stderr_of(&out).contains("ensemble node login"),
        "nothing was saved: {}",
        stderr_of(&out)
    );

    let out = command(&s)
        .args(["node", "login", &url, "--token", &token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("as ana (member: share, recall, tasks, compute)"),
        "{}",
        stdout_of(&out)
    );
    // An admin adds an account on the server while the hub runs: the database
    // is shared between the two processes, and the new token works at once.
    let added = command(&s)
        .args(["hub", "user", "add", "kim", "--role", "member"])
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", stderr_of(&added));
    let kim = stdout_of(&added).trim().to_string();
    let other = Scratch::new("hub-remote-kim");
    let out = command(&other)
        .args(["node", "login", &url, "--token", &kim])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("as kim"), "{}", stdout_of(&out));
    let tokens =
        s.0.with_extension("xdg-config")
            .join("ensemble/node/hubs.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&tokens).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let out = command(&s)
        .args([
            "knowledge",
            "share",
            "--title",
            "Deploys",
            "--text",
            "Deploys go out on Tuesdays.",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("on the hub"),
        "{}",
        stdout_of(&out)
    );

    let mut session = McpSession::start(&s);
    session.request(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "claude-code", "version": "2"}}),
    );
    let list = session.request("tools/list", serde_json::json!({}));
    assert!(list.to_string().contains("\"recall\"") && list.to_string().contains("\"share\""));
    let reply = session.request(
        "tools/call",
        serde_json::json!({"name": "share", "arguments": {"title": "Reviews", "text": "Every change gets one review."}}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let shares = share_files(&hub);
    assert_eq!(shares.len(), 2);
    assert!(
        shares.iter().all(|f| f.contains("shared_by: ana")),
        "{shares:?}"
    );
    assert!(
        shares.iter().any(|f| f.contains("client: claude-code")),
        "{shares:?}"
    );

    let out = command(&s).args(["node", "logout", &url]).output().unwrap();
    assert!(stdout_of(&out).contains("signed out"));
    let _ = child.kill();
    let _ = child.wait();
}

/// POST JSON to a hub server's REST API: the status and the answer.
fn post_json(address: &str, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!("http://{address}{path}"))
        .header("content-type", "application/json")
        .send(body.to_string())
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn someone_signs_up_is_let_in_and_logs_in_from_a_node_with_a_password() {
    let s = Scratch::new("hub-register");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let (hub, address, _) = hub_server(&s);
    let child = serve_hub(&s, &hub);
    let password = "correct horse battery";
    let (status, body) = post_json(
        &address,
        "/api/v1/register",
        serde_json::json!({"name": "dana", "password": password}),
    );
    assert_eq!(status, 201, "{body}");
    let url = format!("ws://{address}");
    let login = |user: &str| {
        let mut child = command(&s)
            .args(["node", "login", &url, "--user", user])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{password}").unwrap();
        child.wait_with_output().unwrap()
    };
    let out = login("dana");
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("approval"), "{}", stderr_of(&out));

    let run = |args: &[&str]| command(&s).args(args).output().unwrap();
    let out = run(&["hub", "user", "list"]);
    assert!(stdout_of(&out).contains("dana") && stdout_of(&out).contains("pending"));
    let out = run(&["hub", "user", "approve", "dana", "--role", "member"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = login("dana");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("as dana (member"),
        "{}",
        stdout_of(&out)
    );
    node_names_hub(&s, &url);
    let out = command(&s)
        .args(["knowledge", "recall", "deploys"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    drop(child);

    // A closed hub takes no sign-ups.
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let closed = text.replace(
        "    type: approval               # anyone may sign up; an admin approves. Or: { type: closed }\n    max_pending: 20              # sign-ups waiting at once\n",
        "    type: closed\n",
    );
    assert_ne!(closed, text);
    std::fs::write(hub.join("hub.yaml"), closed).unwrap();
    let _child = serve_hub(&s, &hub);
    let (status, _) = post_json(
        &address,
        "/api/v1/register",
        serde_json::json!({"name": "eve", "password": password}),
    );
    assert_eq!(status, 403);
}

#[test]
fn a_node_file_is_written_once_and_an_account_names_itself_in_the_tracker() {
    let s = Scratch::new("node-init");
    let out = command(&s).args(["node", "init"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let node =
        s.0.with_extension("xdg-config")
            .join("ensemble/node/node.yaml");
    assert!(std::fs::read_to_string(&node).unwrap().contains("offers:"));
    let again = command(&s).args(["node", "init"]).output().unwrap();
    assert!(
        !again.status.success(),
        "it does not overwrite without --force"
    );

    hub_server_of(&s);
    let run = |args: &[&str]| command(&s).args(args).output().unwrap();
    assert!(
        run(&["hub", "user", "add", "ana", "--role", "member"])
            .status
            .success()
    );
    let out = run(&["hub", "user", "tracker", "ana", "ana.petrosyan"]);
    assert!(
        stdout_of(&out).contains("ana is ana.petrosyan in the tracker"),
        "{}",
        stdout_of(&out)
    );
    let out = run(&["hub", "user", "tracker", "ana"]);
    assert!(
        stdout_of(&out).contains("the account's own name"),
        "{}",
        stdout_of(&out)
    );
}

#[test]
fn an_issue_on_the_hub_server_is_worked_on_a_members_node_and_reported_back() {
    let s = Scratch::new("hub-tasks");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let (hub, address, token) = hub_server(&s);
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    std::fs::write(
        hub.join("hub.yaml"),
        text.replace("  poll_seconds: 30\n", "  poll_seconds: 1\n"),
    )
    .unwrap();
    let out = command(&s)
        .args(["hub", "user", "tracker", "ana", "ana.p"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    std::fs::create_dir_all(hub.join("issues")).unwrap();
    let _hub = serve_hub(&s, &hub);

    // Ana's node, with her helper team, signed in to the hub.
    let url = format!("ws://{address}");
    let out = command(&s)
        .args(["node", "login", &url, "--token", &token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let config = s.0.with_extension("xdg-config").join("ensemble/node");
    std::fs::write(
        config.join("node.yaml"),
        format!("version: 1\nname: laptop\naccess: private\nhub: \"{url}\"\noffers: {{}}\n"),
    )
    .unwrap();
    let mut node = command(&s)
        .args(["node", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = BufReader::new(node.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready for `ensemble mcp` sessions") {
        line.clear();
        assert!(said.read_line(&mut line).unwrap() > 0, "the node stopped");
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut said, &mut std::io::sink());
    });
    let _node = Served(node);

    // A session in the project's folder is how the node learns where it is.
    let mut session = bare_command(&s)
        .arg("mcp")
        .current_dir(s.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        session.stdin.as_mut().unwrap(),
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    )
    .unwrap();
    drop(session.stdin.take());
    let out = session.wait_with_output().unwrap();
    assert!(stdout_of(&out).contains("\"id\":1"), "{}", stderr_of(&out));

    let project = s.path().file_name().unwrap().to_string_lossy().into_owned();
    let issue = hub.join("issues/7.md");
    std::fs::write(
        &issue,
        format!("---\ntitle: Rename foo\nassignee: ana.p\nlabels: [ensemble, \"ensemble:team/coder\"]\nproject: {project}\nstatus: open\n---\nRename foo to bar in a.txt.\n"),
    )
    .unwrap();
    assert!(
        wait_for(60, || std::fs::read_to_string(&issue)
            .unwrap_or_default()
            .contains("status: review")),
        "the issue was not reported back: {}",
        std::fs::read_to_string(&issue).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );
    let said = std::fs::read_to_string(&issue).unwrap();
    for expected in [
        "Picked up as task 0001-rename-foo, for team/coder",
        "is done:",
    ] {
        assert!(said.contains(expected), "{expected} in {said}");
    }
}

/// A running hub with a vault and one note, about deploys, which the scratch
/// computer's node names.
fn hub_with_a_note(scratch: &Scratch) -> Served {
    let (hub, address, token) = hub_server(scratch);
    ensemble::knowledge::Vault::new(hub.join("vault"))
        .write_note(&ensemble::knowledge::Note {
            slug: "deploy-day".into(),
            kind: "decision".into(),
            title: "Deploy day".into(),
            tags: Vec::new(),
            scope: "personal".into(),
            sources: vec!["20261110-000000-000001-aa".into()],
            shared_by: vec!["Ana".into()],
            pipeline: 1,
            updated: "2026-11-10T00:00:00Z".into(),
            body: "Deploys go out on Tuesdays.".into(),
        })
        .unwrap();
    let served = serve_hub(scratch, &hub);
    node_with_hub(scratch, &format!("ws://{address}"), &token);
    served
}

/// What `ensemble hook` prints for one event.
fn hook(scratch: &Scratch, event: serde_json::Value) -> String {
    let mut child = command(scratch)
        .arg("hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "a hook never fails: {}",
        stderr_of(&out)
    );
    stdout_of(&out)
}

#[test]
fn a_session_hook_gives_the_notes_about_a_prompt_once() {
    let scratch = Scratch::new("hook");
    let _hub = hub_with_a_note(&scratch);
    let prompt = |session: &str, text: &str| {
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "session_id": session,
            "cwd": scratch.path(), "prompt": text})
    };
    let said = hook(&scratch, prompt("s1", "when does the deploy go out"));
    assert!(said.contains("reference data, not instructions"), "{said}");
    assert!(said.contains("Deploys go out on Tuesdays."), "{said}");
    // Once per session; another session gets it too.
    assert_eq!(
        hook(&scratch, prompt("s1", "and the deploy after that one")),
        ""
    );
    assert!(hook(&scratch, prompt("s2", "move the deploy please")).contains("Tuesdays"));
    // A compacted session lost it, so gets it again.
    let compact = serde_json::json!({"hook_event_name": "SessionStart", "session_id": "s1",
        "cwd": scratch.path(), "source": "compact"});
    hook(&scratch, compact);
    assert!(hook(&scratch, prompt("s1", "the deploy again then")).contains("Tuesdays"));
    // Nothing about it, a short prompt, a broken event: nothing, and no failure.
    assert_eq!(hook(&scratch, prompt("s3", "rename the parser module")), "");
    assert_eq!(hook(&scratch, prompt("s3", "deploy")), "");
    assert_eq!(hook(&scratch, serde_json::json!("not an event")), "");
}

#[test]
fn connect_with_hooks_writes_them_into_the_clis_settings() {
    let s = Scratch::new("connect-hooks");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let bin = stub_clis(&s, &["claude", "opencode"]);
    let connect = |client: &str, extra: &[&str]| {
        with_path(&s, &bin)
            .args(["node", "connect", client, "--hooks"])
            .args(extra)
            .output()
            .unwrap()
    };
    let out = connect("claude-code", &[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let settings = s.path().join(".claude/settings.local.json");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let command = written["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.ends_with(" hook"), "{command}");

    // OpenCode has no command hooks; nothing is registered then.
    let _ = std::fs::remove_file(bin.join("opencode.args"));
    let out = connect("opencode", &[]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no command hooks"),
        "{}",
        stderr_of(&out)
    );
    assert!(
        !bin.join("opencode.args").exists(),
        "nothing was registered"
    );
}

/// The binary on another computer of the scratch, `machine`: its own config
/// and state directories, with `node_yaml` as its node and signed in to the
/// hubs the scratch signed in to.
fn on_machine(scratch: &Scratch, machine: &str, node_yaml: &str) -> Command {
    let home = scratch.0.with_extension("machines").join(machine);
    let config = home.join("config").join("ensemble/node");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("node.yaml"), node_yaml).unwrap();
    let tokens = scratch
        .0
        .with_extension("xdg-config")
        .join("ensemble/node/hubs.json");
    if tokens.exists() {
        std::fs::copy(&tokens, config.join("hubs.json")).unwrap();
    }
    let mut c = Command::new(BIN);
    c.current_dir(scratch.path())
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"));
    c
}

/// The team `team` of the computer `machine`, with `config` as its
/// `config.yaml`.
fn machine_team(scratch: &Scratch, machine: &str, team: &str, config: &str) {
    let dir = scratch
        .0
        .with_extension("machines")
        .join(machine)
        .join("config/ensemble/node/teams")
        .join(team);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config).unwrap();
}

/// `ensemble node serve` on the computer `machine`, whose node is
/// `node_yaml`, started and waited for until it is connected.
fn serve_node(scratch: &Scratch, url: &str, machine: &str, node_yaml: &str) -> Served {
    // The node names the hub it connects to.
    let node_yaml = if node_yaml.contains("\nhub:") {
        node_yaml.to_string()
    } else {
        node_yaml.replacen("\noffers:", &format!("\nhub: \"{url}\"\noffers:"), 1)
    };
    let mut child = on_machine(scratch, machine, &node_yaml)
        .args(["node", "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap() > 0, "it stopped: {seen}");
        seen.push_str(&line);
        if line.contains(" serving ws") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = err.read_to_string(&mut rest);
    });
    Served(child)
}

#[test]
fn an_agent_runs_on_a_node_and_what_it_changed_comes_back() {
    let s = Scratch::new("node-run");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    let out = command(&s)
        .args(["node", "login", &url, "--token", &token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));

    // Ana's node, with a helper team of its own whose `reviewer` changes a
    // file, adds one, and lists what it was given.
    let cli = s.opencode(
        "changed it there",
        "printf 'hello, changed' > a.txt\nprintf new > b.txt\nls -a > given.txt",
    );
    machine_team(&s, "laptop", "work", &config_yaml(&cli, NOWHERE));
    let node_yaml =
        "version: 1\nname: laptop\naccess: private\noffers:\n  runs: { agents: [work/reviewer] }\n";
    let _node = serve_node(&s, &url, "laptop", node_yaml);
    node_names_hub(&s, &url);
    let out = command(&s).args(["node", "peers"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("ana/laptop  (private, 0 running)\n  agents: work/reviewer"),
        "{}",
        stdout_of(&out)
    );

    // The project: a git repository, with a secret git ignores.
    git_in(s.path(), &["init", "-q"]);
    s.write("a.txt", "hello");
    s.write(".gitignore", ".env\n");
    s.write(".env", "KEY=secret");
    s.write(
        "@team/config.yaml",
        &format!(
            "version: 1\nllm_backends: {{}}\nlimits: {{}}\ngates: {{}}\nagents:\n  heavy:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter: {{ type: node, hub: \"{url}\", agent: work/reviewer, nodes: [ana/laptop] }}\n"
        ),
    );
    let out = ensemble(
        &s,
        &["node", "agent", "run", "team/heavy", "change", "a.txt"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("changed it there"),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "hello, changed"
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("b.txt")).unwrap(),
        "new"
    );
    let given = std::fs::read_to_string(s.path().join("given.txt")).unwrap();
    assert!(
        given.contains("a.txt") && given.contains(".gitignore"),
        "{given}"
    );
    assert!(
        !given.contains(".env"),
        "what git ignores stays here: {given}"
    );

    // A node that is not there is said by name.
    s.write(
        "@team/config.yaml",
        &format!(
            "version: 1\nllm_backends: {{}}\nlimits: {{}}\ngates: {{}}\nagents:\n  heavy:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter: {{ type: node, hub: \"{url}\", agent: work/reviewer, nodes: [kim/gpu-box] }}\n"
        ),
    );
    let out = ensemble(
        &s,
        &["node", "agent", "run", "team/heavy", "change", "a.txt"],
    );
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("kim/gpu-box is not connected"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_node_serves_its_models_through_the_hub_and_a_shared_node_lends_no_login() {
    let s = Scratch::new("node-models");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    let out = command(&s)
        .args(["node", "login", &url, "--token", &token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let server = models_stub(r#"{"choices":[{"message":{"content":"hi from the node"}}]}"#);
    let node_yaml = |access: &str, offers: &str| {
        format!("version: 1\nname: {access}-box\naccess: {access}\nhub: \"{url}\"\n{offers}")
    };
    let models = format!(
        "offers:\n  models: {{ base_url: \"{server}\", timeout_seconds: 5, names: [small] }}\n"
    );
    let _node = serve_node(&s, &url, "private-box", &node_yaml("private", &models));
    let mut answer = ureq::post(format!("http://{address}/nodes/v1/chat/completions"))
        .header("authorization", format!("Bearer {token}"))
        .send(r#"{"model": "small", "messages": [{"role": "user", "content": "hi"}]}"#)
        .unwrap();
    let body = answer.body_mut().read_to_string().unwrap();
    assert!(body.contains("hi from the node"), "{body}");

    // A member may not host a shared node.
    let out = on_machine(&s, "shared-box", &node_yaml("shared", &models))
        .args(["node", "serve"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("`host` permission"),
        "{}",
        stderr_of(&out)
    );
    // And a shared node never offers an agent on its CLI's own login.
    machine_team(&s, "shared-box", "work", &config_yaml("opencode", NOWHERE));
    let out = on_machine(
        &s,
        "shared-box",
        &node_yaml("shared", "offers:\n  runs: { agents: [work/reviewer] }\n"),
    )
    .args(["node", "serve"])
    .output()
    .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("never lends"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_session_reaches_the_node_from_its_folder_and_a_task_goes_on_after_it_ends() {
    let s = Scratch::new("node-session");
    let cli = s.opencode("renamed it", "sleep 1; echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let session = |input: &[serde_json::Value]| {
        let mut child = bare_command(&s)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for message in input {
            writeln!(stdin, "{message}").unwrap();
        }
        drop(stdin);
        child.wait_with_output().unwrap()
    };

    // With no node running, `mcp` says how to start one.
    let _hub = with_hub(&s);
    let out = session(&[]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no node 'desk' is running"),
        "{}",
        stderr_of(&out)
    );

    let mut node = bare_command(&s)
        .args(["node", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = BufReader::new(node.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready for `ensemble mcp` sessions") {
        line.clear();
        assert!(said.read_line(&mut line).unwrap() > 0, "the node stopped");
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut said, &mut std::io::sink());
    });
    let _node = Served(node);

    let out = session(&[
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "submit",
            "arguments": {"agent": "team/coder", "title": "Rename foo", "task": "rename foo to bar"}
        }}),
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let replies = stdout_of(&out);
    assert!(replies.contains("0001-rename-foo"), "{replies}");

    // The session has ended; the node works the task all the same.
    assert!(
        wait_for(30, || std::fs::read_to_string(s.path().join("a.txt"))
            .is_ok_and(|t| t == "bar\n")),
        "the task did not run after the session"
    );
}

#[test]
fn an_agent_writes_a_team_that_runs_only_once_the_person_approves_it() {
    let s = Scratch::new("team-create");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let sample = tool_text(&session.call("teams", serde_json::json!({"sample": true})));
    assert!(sample.contains("agents:"), "{sample}");
    let docs = config_yaml("true", NOWHERE);
    let made = session.call(
        "team_create",
        serde_json::json!({"name": "docs", "config": docs}),
    );
    assert_eq!(made["result"]["isError"], false, "{made}");
    assert!(
        tool_text(&made).contains("ensemble node team approve docs"),
        "{made}"
    );
    let broken = session.call(
        "team_create",
        serde_json::json!({"name": "half", "config": "version: 1\n"}),
    );
    assert_eq!(
        broken["result"]["isError"], true,
        "a broken team is refused"
    );
    let taken = session.call(
        "team_create",
        serde_json::json!({"name": "team", "config": docs}),
    );
    assert_eq!(
        taken["result"]["isError"], true,
        "an existing team is not changed"
    );
    let listed = tool_text(&session.call("teams", serde_json::json!({})));
    assert!(
        listed.contains("docs (made here, waiting for approval")
            && listed.contains("team (made here)")
            && listed.contains("  team/coder:"),
        "{listed}"
    );
    drop(session);

    // It runs nothing until the person approves it.
    let check = ensemble(&s, &["node", "team", "check"]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    assert!(
        !stdout_of(&check).contains("docs/coder"),
        "{}",
        stdout_of(&check)
    );
    assert!(
        stderr_of(&check).contains("team 'docs' waits for your approval"),
        "{}",
        stderr_of(&check)
    );
    let approved = ensemble(&s, &["node", "team", "approve", "docs", "--yes"]);
    assert!(approved.status.success(), "{}", stderr_of(&approved));
    assert!(
        stdout_of(&approved).contains("coder: runs `true`"),
        "what it runs is shown first: {}",
        stdout_of(&approved)
    );
    let check = ensemble(&s, &["node", "team", "check"]);
    assert!(
        stdout_of(&check).contains("docs/coder"),
        "{}",
        stdout_of(&check)
    );

    // A folder elsewhere joins as a link, and leaves without being deleted.
    let elsewhere = s.0.with_extension("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("config.yaml"), config_yaml("true", NOWHERE)).unwrap();
    let linked = command(&s)
        .args(["node", "team", "link", "pcb"])
        .arg(&elsewhere)
        .output()
        .unwrap();
    assert!(linked.status.success(), "{}", stderr_of(&linked));
    let teams = stdout_of(&ensemble(&s, &["node", "team", "list"]));
    assert!(
        teams.contains(&format!("pcb  (linked to {})", elsewhere.display()))
            && teams.contains("  pcb/coder:"),
        "{teams}"
    );
    let removed = ensemble(&s, &["node", "team", "remove", "pcb", "--yes"]);
    assert!(removed.status.success(), "{}", stderr_of(&removed));
    assert!(elsewhere.join("config.yaml").is_file(), "the folder stays");
    assert!(!stdout_of(&ensemble(&s, &["node", "team", "list"])).contains("pcb"));
    let _ = std::fs::remove_dir_all(&elsewhere);
}

#[test]
fn a_running_node_reloads_its_teams_and_tells_its_sessions_the_tools_changed() {
    let s = Scratch::new("reload");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));
    let tools = session
        .request("tools/list", serde_json::json!({}))
        .to_string();
    assert!(!tools.contains("pcb/coder"), "{tools}");

    // A team linked while the node runs is used at once.
    let elsewhere = s.0.with_extension("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("config.yaml"), config_yaml("true", NOWHERE)).unwrap();
    let added = command(&s)
        .args(["node", "team", "link", "pcb"])
        .arg(&elsewhere)
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", stderr_of(&added));
    assert!(
        stdout_of(&added).contains("The node uses it now"),
        "{}",
        stdout_of(&added)
    );
    let tools = session
        .request("tools/list", serde_json::json!({}))
        .to_string();
    assert!(
        tools.contains("pcb/coder") && tools.contains("team/coder"),
        "{tools}"
    );
    assert!(
        session
            .notes
            .iter()
            .any(|n| n["method"] == "notifications/tools/list_changed"),
        "{:?}",
        session.notes
    );

    // A team that cannot be loaded leaves the node's teams as they were.
    std::fs::write(elsewhere.join("config.yaml"), "version: 1\n").unwrap();
    let out = ensemble(&s, &["node", "reload"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("kept its teams as they were")
            && stderr_of(&out).contains("team 'pcb'"),
        "{}",
        stderr_of(&out)
    );
    let tools = session
        .request("tools/list", serde_json::json!({}))
        .to_string();
    assert!(tools.contains("pcb/coder"), "{tools}");
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// The node of the computer `machine`, named for it, with the hub at `url`:
/// started and waited for until it is ready.
fn machine_node(scratch: &Scratch, machine: &str, url: &str) -> Served {
    let yaml =
        format!("version: 1\nname: {machine}\naccess: private\nhub: \"{url}\"\noffers: {{}}\n");
    let mut node = on_machine(scratch, machine, &yaml)
        .args(["node", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = BufReader::new(node.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready for `ensemble mcp` sessions") {
        line.clear();
        assert!(said.read_line(&mut line).unwrap() > 0, "the node stopped");
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut said, &mut std::io::sink());
    });
    Served(node)
}

/// A command on the computer `machine`, in its folder `proj`.
fn in_proj(scratch: &Scratch, machine: &str, url: &str, args: &[&str]) -> std::process::Output {
    let yaml =
        format!("version: 1\nname: {machine}\naccess: private\nhub: \"{url}\"\noffers: {{}}\n");
    let proj = scratch
        .0
        .with_extension("machines")
        .join(machine)
        .join("proj");
    on_machine(scratch, machine, &yaml)
        .current_dir(&proj)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_task_stopped_on_one_computer_is_carried_on_by_another_from_its_pushed_branch() {
    let s = Scratch::new("carry-on");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    node_with_hub(&s, &url, &token);

    // One repository on a remote, cloned as `proj` on the desk and the laptop.
    let origin = s.0.with_extension("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git_in(
        &origin,
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    );
    s.write("README", "hello\n");
    git_in(s.path(), &["init", "--quiet", "-b", "main"]);
    git_in(s.path(), &["add", "README"]);
    git_in(s.path(), &["commit", "--quiet", "-m", "start"]);
    git_in(
        s.path(),
        &["push", "--quiet", &origin.to_string_lossy(), "main"],
    );
    // The first run does part 1, then is slow; the run after it, wherever it
    // is, finds part 1 and does part 2.
    let marker = s.0.with_extension("bin").join("started");
    let cli = s.opencode(
        "both parts done",
        &format!(
            "if [ -e part1.txt ]; then echo 2 > part2.txt; else echo 1 > part1.txt; touch '{}'; sleep 60; fi",
            marker.display()
        ),
    );
    let team = config_yaml(&cli, NOWHERE).replace("isolation: shared", "isolation: worktree");
    for machine in ["desk", "laptop"] {
        let home = s.0.with_extension("machines").join(machine);
        std::fs::create_dir_all(&home).unwrap();
        git_in(
            &home,
            &["clone", "--quiet", &origin.to_string_lossy(), "proj"],
        );
        git_in(&home.join("proj"), &["config", "user.name", "T"]);
        git_in(&home.join("proj"), &["config", "user.email", "t@t"]);
        machine_team(&s, machine, "team", &team);
    }

    let mut desk = machine_node(&s, "desk", &url);
    let out = in_proj(
        &s,
        "desk",
        &url,
        &[
            "task",
            "new",
            "Two parts",
            "--agent",
            "team/coder",
            "--text",
            "do both parts",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        wait_for(30, || marker.exists()),
        "the desk did not start it"
    );
    sigterm(&desk);
    desk.wait().unwrap();
    let listed = stdout_of(&in_proj(&s, "desk", &url, &["task", "list"]));
    assert!(listed.contains("queued (interrupted on desk)"), "{listed}");
    assert!(
        git_in(
            &origin,
            &["ls-tree", "--name-only", "ensemble/0001-two-parts"]
        )
        .contains("part1.txt"),
        "the desk pushed its work so far"
    );

    // The laptop, which has served a folder named `proj`, takes it over.
    let _laptop = machine_node(&s, "laptop", &url);
    let out = in_proj(
        &s,
        "laptop",
        &url,
        &["task", "assign", "0001-two-parts", "team/coder"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let ended = || {
        stdout_of(&in_proj(
            &s,
            "laptop",
            &url,
            &["task", "show", "0001-two-parts"],
        ))
        .contains("\n---\nstatus: ")
    };
    assert!(wait_for(30, ended), "the laptop did not carry it on");
    let shown = stdout_of(&in_proj(
        &s,
        "laptop",
        &url,
        &["task", "show", "0001-two-parts"],
    ));
    assert!(
        shown.contains("status: done") && shown.contains("both parts done"),
        "{shown}"
    );
    let files = git_in(
        &origin,
        &["ls-tree", "--name-only", "ensemble/0001-two-parts"],
    );
    assert!(
        files.contains("part1.txt") && files.contains("part2.txt"),
        "{files}"
    );

    // The desk's own branch, behind, is brought up to it to be looked at.
    let review = in_proj(&s, "desk", &url, &["task", "review", "0001-two-parts"]);
    assert!(review.status.success(), "{}", stderr_of(&review));
    let text = stdout_of(&review);
    assert!(
        text.contains("part1.txt") && text.contains("part2.txt"),
        "{text}"
    );
}

#[test]
fn settings_are_changed_from_the_command_line_and_those_nix_declares_are_kept() {
    let s = Scratch::new("config");
    let node = s.node_yaml();
    let get = |key: &str| {
        stdout_of(&ensemble(&s, &["node", "config", "get", key]))
            .trim()
            .to_string()
    };
    assert_eq!(get("name"), "desk");

    let out = ensemble(&s, &["node", "config", "set", "access", "shared"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(get("access"), "shared");
    // One that would not load is refused, and nothing changes.
    let out = ensemble(&s, &["node", "config", "set", "name", "a b/c"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("nothing was changed"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(get("name"), "desk");
    let out = ensemble(&s, &["node", "config", "unset", "name"]);
    assert!(!out.status.success(), "a node needs a name");
    assert_eq!(
        stdout_of(&ensemble(&s, &["node", "config", "path"])).trim(),
        node.display().to_string()
    );

    // What Nix declares is merged in, kept, and refused here.
    let declared = s.write("declared.yaml", "access: private\noffers: {}\n");
    let out = ensemble(&s, &["node", "config", "merge", declared.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(get("access"), "private");
    let out = ensemble(&s, &["node", "config", "set", "access", "shared"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("`access` is set in your Nix configuration"),
        "{}",
        stderr_of(&out)
    );
    let out = ensemble(&s, &["node", "config", "set", "name", "rogstrix"]);
    assert!(
        out.status.success(),
        "the rest stays the CLI's: {}",
        stderr_of(&out)
    );

    // A team made here, too; the running node would reload it.
    let out = ensemble(&s, &["node", "team", "new", "web"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = ensemble(
        &s,
        &[
            "node",
            "team",
            "config",
            "web",
            "set",
            "gates.test.timeout_seconds",
            "60",
        ],
    );
    assert!(
        !out.status.success(),
        "a gate needs its command: {}",
        stdout_of(&out)
    );
    let out = ensemble(&s, &["node", "team", "config", "web", "get", "version"]);
    assert_eq!(stdout_of(&out).trim(), "1");
}

#[test]
fn signing_in_to_a_hub_names_it_in_the_node_and_signing_out_takes_it_out() {
    let s = Scratch::new("login-names-hub");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    s.node_yaml();
    let url = format!("ws://{address}");
    let out = ensemble(&s, &["node", "login", &url, "--token", &token]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("The node names this hub now"),
        "{}",
        stdout_of(&out)
    );
    let hub_now = stdout_of(&ensemble(&s, &["node", "config", "get", "hub"]));
    assert_eq!(hub_now.trim(), url);
    let status = stdout_of(&ensemble(&s, &["node", "status"]));
    assert!(
        status.contains(&format!("hub: {url}, signed in")),
        "{status}"
    );

    let out = ensemble(&s, &["node", "logout", &url]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("The node names no hub now"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        !ensemble(&s, &["node", "config", "get", "hub"])
            .status
            .success()
    );
}
