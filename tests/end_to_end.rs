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

    /// The helper team directory, kept beside the project so it is not one of
    /// the files a run changes.
    fn org(&self) -> PathBuf {
        self.0.with_extension("org")
    }

    /// Where tasks and runs go (`--state`), beside the project too.
    fn state(&self) -> PathBuf {
        self.0.with_extension("state")
    }

    /// A path in the project, or one of the old `.ensemble/` names, which are
    /// the helper team (`config.yaml`, `.env`) and the state (the rest).
    fn place(&self, name: &str) -> PathBuf {
        match name.strip_prefix(".ensemble/") {
            Some(rest) if rest == "config.yaml" || rest == ".env" => self.org().join(rest),
            Some(rest) => self.state().join(rest),
            None => self.0.join(name),
        }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.place(name);
        let converted;
        let contents = if name == ".ensemble/config.yaml" {
            converted = ensemble::config::migrate::to_v3(contents).unwrap_or(contents.to_string());
            converted.as_str()
        } else {
            contents
        };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).expect("write scratch file");
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
        let _ = std::fs::remove_dir_all(self.org());
        let _ = std::fs::remove_dir_all(self.state());
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-state"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-cache"));
    }
}

/// Two OpenCode agents run by `command`: `reviewer` on a provider OpenCode
/// knows, and `coder` on the `openai-local` LLM backend at `backend_url`.
fn config_v1(command: &str, backend_url: &str) -> String {
    format!(
        r#"
version: 1

llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url: "{backend_url}"
    api_key_env: ENSEMBLE_TEST_LOCAL_KEY
    timeout_seconds: 20

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
        backend: openai-local
        name: qwen3-coder
        context_window: 32768
      tools: [read, edit]
      system_prompt: append
      max_steps: 30
      command: "{command}"
"#
    )
}

/// The same, as the version 3 helper team it is loaded as.
fn config_yaml(command: &str, backend_url: &str) -> String {
    ensemble::config::migrate::to_v3(&config_v1(command, backend_url)).expect("a version 1 sample")
}

/// The binary, run in `scratch` with none of the test variables inherited:
/// a stale one would silently change what is being tested.
fn command(scratch: &Scratch) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(scratch.path())
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .arg("--team")
        .arg(scratch.org())
        .arg("--state")
        .arg(scratch.state())
        .env_remove("ENSEMBLE_TEST_LOCAL_URL")
        .env_remove("ENSEMBLE_TEST_LOCAL_KEY");
    let env_file = scratch.org().join(".env");
    if env_file.exists() {
        c.arg("--env-file").arg(env_file);
    }
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
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = ensemble(&s, &["check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("ok (version 3)"), "{text}");
    assert!(text.contains("mcp: ready"), "{text}");
    assert!(
        text.contains("coder  ->  opencode on openai-local / qwen3-coder"),
        "{text}"
    );
    assert!(text.contains("openai-compatible-api"), "{text}");
}

#[test]
fn check_reports_several_problems_at_once() {
    let s = Scratch::new("check-many");
    s.write(
        ".ensemble/config.yaml",
        r#"
version: 4
llm_backends:
  x: { type: openai-compatible-api, base_url: "nope", timeout_seconds: 5 }
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
      model: { type: llm_backend, backend: nowhere, name: "", context_window: 8000 }
      tools: []
      system_prompt: append
      max_steps: 3
      command: opencode
"#,
    );
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("version"), "{err}");
    assert!(err.contains("llm_backends.x"), "{err}");
    assert!(err.contains("nowhere"), "{err}");
}

#[test]
fn a_config_with_the_removed_ensemble_type_says_what_to_use_instead() {
    let s = Scratch::new("removed-type");
    s.write(
        ".ensemble/config.yaml",
        "version: 1\nllm_backends: {}\ngates: {}\nagents:\n  coder:\n    max_seconds: 60\n    max_concurrent: 2\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter: { type: ensemble, model: m }\n",
    );
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(
        err.contains("'ensemble' was removed") && err.contains("'llm_backend'"),
        "{err}"
    );
}

#[test]
fn an_api_key_written_into_the_config_is_refused() {
    let s = Scratch::new("inline-key");
    s.write(
        ".ensemble/config.yaml",
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
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success(), "secrets in the config must not load");
    assert!(stderr_of(&out).contains("api_key"), "{}", stderr_of(&out));
}

#[test]
fn nothing_is_found_without_an_org() {
    let s = Scratch::new("no-org");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    // The project folder holds a config of the old kind; it is not looked at.
    s.write("config.yaml", "version: 2\n");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = Command::new(BIN)
        .arg("check")
        .current_dir(s.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "no --team means no helper team");
    assert!(stderr_of(&out).contains("--team"), "{}", stderr_of(&out));
}

#[test]
fn a_missing_config_says_so_rather_than_guessing() {
    let s = Scratch::new("no-config");
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("config.yaml"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn agents_prints_the_routing_table() {
    let s = Scratch::new("agents");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = ensemble(&s, &["agents"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("coder\topencode on openai-local\tqwen3-coder"),
        "{text}"
    );
    assert!(
        text.contains("reviewer\topencode\topencode/reviewer-model"),
        "{text}"
    );
}

#[test]
fn the_base_url_is_the_one_in_the_config_and_no_variable_changes_it() {
    let s = Scratch::new("url-from-config");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = command(&s)
        .arg("check")
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
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));

    let out = ensemble(&s, &["run", "reviewer", "review", "this", "code"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("the review is clean"),
        "{}",
        stdout_of(&out)
    );
    // Provenance goes to stderr so stdout stays pipeable.
    assert!(
        stderr_of(&out).contains("reviewer via opencode / opencode/reviewer-model"),
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
        config["agent"]["ensemble-reviewer"]["prompt"],
        "Reviews code and reports findings."
    );
    assert!(config.get("provider").is_none(), "{config}");
}

#[test]
fn an_agent_on_an_llm_backend_gets_its_endpoint_and_the_key_from_dotenv() {
    let s = Scratch::new("run-backend");
    let cli = s.opencode("done", "");
    s.write(
        ".ensemble/config.yaml",
        &config_yaml(&cli, "http://127.0.0.1:8888/v1"),
    );
    s.write(".ensemble/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");

    let out = ensemble(&s, &["run", "coder", "do it"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        s.seen("args")
            .contains("-m\nensemble-openai-local/qwen3-coder\n"),
        "{}",
        s.seen("args")
    );
    let config: serde_json::Value = serde_json::from_str(&s.seen("config")).unwrap();
    let provider = &config["provider"]["ensemble-openai-local"];
    assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:8888/v1");
    assert_eq!(provider["options"]["apiKey"], "sk-from-dotenv");
    assert_eq!(provider["models"]["qwen3-coder"]["limit"]["context"], 32768);
}

#[test]
fn show_prints_how_an_agent_starts_with_the_key_hidden() {
    let s = Scratch::new("show");
    s.write(
        ".ensemble/config.yaml",
        &config_yaml("opencode", "http://127.0.0.1:8888/v1"),
    );
    s.write(
        ".ensemble/.env",
        "ENSEMBLE_TEST_LOCAL_KEY=sk-secret-value\n",
    );

    let out = ensemble(&s, &["show", "coder"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.starts_with("opencode run --standalone --format json --agent ensemble-coder -m ensemble-openai-local/qwen3-coder -- '<task>'"),
        "{text}"
    );
    assert!(
        text.contains("\"baseURL\": \"http://127.0.0.1:8888/v1\""),
        "{text}"
    );
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("sk-secret-value"), "the key leaked: {text}");

    let out = ensemble(&s, &["show", "nobody"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("coder, reviewer"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn files_the_agent_changed_are_reported() {
    let s = Scratch::new("changes");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");

    let out = ensemble(&s, &["run", "coder", "rename foo to bar"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(s.read("a.txt"), "bar\n");
}

#[test]
fn health_checks_the_cli_and_that_the_backend_serves_the_model() {
    let s = Scratch::new("health");
    let cli = s.opencode("", "");
    let serving = models_stub(r#"{"object":"list","data":[{"id":"qwen3-coder"}]}"#);
    s.write(".ensemble/config.yaml", &config_yaml(&cli, &serving));
    let out = ensemble(&s, &["health"]);
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
    s.write(".ensemble/config.yaml", &config_yaml(&cli, &other));
    let out = ensemble(&s, &["health"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(text.contains("openai-local: FAILED"), "{text}");
    assert!(
        text.contains("does not serve 'qwen3-coder'. It serves: something-else."),
        "{text}"
    );

    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    let out = ensemble(&s, &["health"]);
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

/// A live `ensemble mcp` subprocess, driven the way a harness drives it.
struct McpSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl McpSession {
    fn start(scratch: &Scratch) -> Self {
        Self::start_with(scratch, &[])
    }

    /// `ensemble mcp` with `extra` arguments, such as `--hub <dir>`.
    fn start_with(scratch: &Scratch, extra: &[&std::ffi::OsStr]) -> Self {
        let mut child = command(scratch)
            .arg("mcp")
            .args(extra)
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
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
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
            "delegate", "submit", "status", "wait", "cancel", "assign", "answer", "review",
            "agents", "health"
        ]
    );
}

#[test]
fn a_harness_can_delegate_in_the_background_and_collect_the_work() {
    let s = Scratch::new("mcp-background");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let started = session.call(
        "delegate",
        serde_json::json!({"agent": "coder", "task": "rename foo to bar", "background": true}),
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
    assert!(status.contains("adhoc-1 · coder · done"), "{status}");
    assert!(
        !s.path().join(".ensemble/tasks").exists(),
        "a background delegation writes no ticket"
    );
}

#[test]
fn a_harness_can_submit_a_task_and_collect_its_result_from_the_ticket() {
    let s = Scratch::new("mcp-submit");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let submitted = session.call(
        "submit",
        serde_json::json!({"agent": "coder", "title": "Rename foo", "task": "rename foo to bar"}),
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

    let ticket = s.place(".ensemble/tasks/0001-rename-foo");
    assert!(ticket.join("task.md").is_file() && ticket.join("result.md").is_file());
}

#[test]
fn a_harness_can_delegate_through_mcp_and_get_the_work_back() {
    let s = Scratch::new("mcp-delegate");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let agents = tool_text(&session.call("agents", serde_json::json!({})));
    assert!(agents.contains("coder [can change files]"), "{agents}");

    let reply = session.call(
        "delegate",
        serde_json::json!({"agent": "coder", "task": "rename foo to bar", "context": "in a.txt"}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let text = tool_text(&reply);
    assert!(text.starts_with("renamed it"), "{text}");
    assert!(
        text.ends_with("[coder: 1 step(s), 0 tool call(s)]\n[files changed: a.txt (modified)]"),
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
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
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
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let reply = session.call(
        "delegate",
        serde_json::json!({"agent": "coder", "task": "write a function"}),
    );
    assert_eq!(reply["result"]["isError"], true);
    let text = tool_text(&reply);
    assert!(text.contains("delegation failed (agent 'coder')"), "{text}");
    assert!(text.contains("Model unavailable"), "{text}");
    assert!(
        text.contains("Retrying"),
        "tells the model what to do next: {text}"
    );
}

#[test]
fn the_tools_never_offer_a_way_to_pick_a_model_or_a_backend() {
    let s = Scratch::new("mcp-no-model-param");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
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
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));

    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));
    session.call(
        "delegate",
        serde_json::json!({"agent": "reviewer", "task": "look"}),
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
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
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
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    drop(session.child.stdin.take());
    let status = session.child.wait().expect("the server should exit on EOF");
    assert!(status.success(), "exited {status}");
}

#[test]
fn an_invalid_config_stops_the_mcp_server_before_it_serves_anything() {
    // Better to fail at spawn, where the harness reports a server that would
    // not start, than to accept a handshake and fail every delegation.
    let s = Scratch::new("mcp-bad-config");
    s.write(".ensemble/config.yaml", "version: 1\nbackends: {}\n");
    let out = ensemble(&s, &["mcp"]);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("backend"), "{}", stderr_of(&out));
}

#[test]
fn init_writes_a_sample_helper_team_that_checks_and_never_overwrites_by_accident() {
    let s = Scratch::new("init");
    let out = ensemble(&s, &["init"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let written = std::fs::read_to_string(s.org().join("config.yaml")).unwrap();
    assert!(written.contains("type: opencode"), "{written}");
    assert!(written.contains("\nagents:\n"), "{written}");
    assert!(!written.contains("teams:"), "{written}");
    assert!(
        !s.org().join(".env").exists() && !s.org().join(".gitignore").exists(),
        "a helper team holds no secrets, so it needs no .env"
    );
    assert!(
        !s.path().join(".ensemble").exists(),
        "nothing is written in the project"
    );

    let check = ensemble(&s, &["check"]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    let text = stdout_of(&check);
    assert!(text.contains("coder  ->"), "{text}");

    s.write(".ensemble/config.yaml", "mine");
    let again = ensemble(&s, &["init"]);
    assert!(!again.status.success());
    assert!(
        stderr_of(&again).contains("--force"),
        "{}",
        stderr_of(&again)
    );
    assert_eq!(
        std::fs::read_to_string(s.org().join("config.yaml")).unwrap(),
        "mine"
    );

    let printed = ensemble(&s, &["init", "--stdout"]);
    assert_eq!(stdout_of(&printed), written);
    assert!(ensemble(&s, &["init", "--force"]).status.success());
    assert_eq!(
        std::fs::read_to_string(s.org().join("config.yaml")).unwrap(),
        written
    );
}

#[test]
fn migrate_turns_a_version_1_config_into_a_helper_team() {
    let s = Scratch::new("migrate");
    let old = s.write("old.yaml", &{
        let v1 = r#"
version: 1
llm_backends: {}
gates: {}
agents:
  solo:
    prompt: "Does it."
    max_seconds: 60
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: { type: provider, name: opencode/m }
      tools: [read]
      system_prompt: append
      max_steps: 3
      command: opencode
"#;
        v1.to_string()
    });
    let out = ensemble(&s, &["migrate", old.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let check = ensemble(&s, &["check"]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    assert!(stdout_of(&check).contains("solo"), "{}", stdout_of(&check));
    let again = ensemble(&s, &["migrate", old.to_str().unwrap()]);
    assert!(!again.status.success(), "migrate does not overwrite");
}

#[test]
fn migrate_turns_the_version_2_sample_into_a_flat_helper_team() {
    let s = Scratch::new("migrate-v2");
    let old =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/config/v2.yaml");
    let out = ensemble(&s, &["migrate", old.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let written = std::fs::read_to_string(s.org().join("config.yaml")).unwrap();
    assert!(written.contains("version: 3"), "{written}");
    assert!(!written.contains("teams"), "{written}");
    assert!(written.contains("type: direct"), "{written}");
    let agents = ensemble(&s, &["agents"]);
    assert!(agents.status.success(), "{}", stderr_of(&agents));
    for name in ["coder", "explorer", "summariser", "fixer", "reviewer"] {
        assert!(
            stdout_of(&agents)
                .lines()
                .any(|l| l.starts_with(&format!("{name}\t"))),
            "{name}: {}",
            stdout_of(&agents)
        );
    }
}

#[test]
fn variables_come_from_the_env_file_unless_the_environment_sets_them() {
    let s = Scratch::new("dotenv");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    s.write(".ensemble/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");
    let out = ensemble(&s, &["check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("authenticated"), "{text}");
    assert!(!text.contains("sk-from-dotenv"), "{text}");

    s.write(".ensemble/.env", "not a line\n");
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("line 1: expected NAME=value"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn check_reports_a_task_that_could_not_run() {
    let s = Scratch::new("check-tasks");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    s.write(
        ".ensemble/tasks/0001-ok/task.md",
        "---\ntitle: Ok\nagent: coder\n---\nDo it.\n",
    );
    let out = ensemble(&s, &["check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("tasks: 1 in"),
        "{}",
        stdout_of(&out)
    );

    s.write(".ensemble/tasks/0002-bad/task.md", "Do it.\n");
    s.write(
        ".ensemble/tasks/0003-who/task.md",
        "---\ntitle: Who\nagent: architect\n---\nDo it.\n",
    );
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("0002-bad:") && text.contains("front matter"),
        "{text}"
    );
    assert!(
        text.contains("0003-who: agent 'architect' is not configured"),
        "{text}"
    );
}

#[test]
fn check_reports_a_task_that_can_never_start() {
    let s = Scratch::new("check-after");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    s.write(
        ".ensemble/tasks/0001-a/task.md",
        "---\ntitle: a\nagent: coder\nafter: [0002-b]\n---\na\n",
    );
    s.write(
        ".ensemble/tasks/0002-b/task.md",
        "---\ntitle: b\nagent: coder\nafter: [0001-a]\n---\nb\n",
    );
    s.write(
        ".ensemble/tasks/0003-c/task.md",
        "---\ntitle: c\nagent: coder\nafter: [0009]\n---\nc\n",
    );
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("0001-a: its `after` leads back to itself"),
        "{text}"
    );
    assert!(
        text.contains("0003-c: `after` 0009 (no such task)"),
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
        "{}  summariser:\n    prompt: You summarise.\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: direct\n      backend: openai-local\n      model: small\n      max_tokens: 300\n",
        config_v1("opencode", &url)
    );
    s.write(".ensemble/config.yaml", &config);

    let out = ensemble(&s, &["run", "summariser", "sum", "it", "up"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("three bullets"),
        "{}",
        stdout_of(&out)
    );

    let out = ensemble(&s, &["show", "summariser"]);
    let text = stdout_of(&out);
    assert!(
        text.starts_with(&format!("POST {url}/chat/completions\n")),
        "{text}"
    );
    assert!(
        text.contains("\"max_tokens\": 300") && text.contains("You summarise."),
        "{text}"
    );

    let check = stdout_of(&ensemble(&s, &["check"]));
    assert!(
        check.contains("summariser  ->  direct on openai-local / small"),
        "{check}"
    );
}

// ---------------------------------------------------------------------------
// Getting started
// ---------------------------------------------------------------------------

/// A directory of stand-ins for the manager and agent CLIs. Each answers
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
        .env("XDG_STATE_HOME", scratch.state())
        .env("HOME", scratch.0.with_extension("home"));
    c
}

#[test]
fn connect_registers_the_team_with_the_agent_you_talk_to_and_says_what_it_ran() {
    for (manager, program, scope) in [
        ("claude-code", "claude", "--scope\nuser\n"),
        ("opencode", "opencode", "--global\n"),
    ] {
        let s = Scratch::new(&format!("connect-{program}"));
        s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
        let bin = stub_clis(&s, &[program]);
        let out = with_path(&s, &bin)
            .args(["connect", "--manager", manager, "--global"])
            .arg("--team")
            .arg(s.org())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr_of(&out));
        let said = stdout_of(&out);
        assert!(said.contains(&format!("$ {program} mcp add")), "{said}");
        let args = std::fs::read_to_string(bin.join(format!("{program}.args"))).unwrap();
        assert!(args.starts_with("mcp\nadd\n"), "{args}");
        assert!(args.contains(scope), "{args}");
        assert!(args.contains("\nmcp\n--team\n"), "{args}");
        assert!(args.contains(&s.org().display().to_string()), "{args}");
    }
}

#[test]
fn connect_without_the_manager_cli_prints_the_command_and_fails() {
    let s = Scratch::new("connect-missing");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let bin = stub_clis(&s, &[]);
    let out = with_path(&s, &bin)
        .args(["connect", "--manager", "claude-code"])
        .arg("--team")
        .arg(s.org())
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
fn setup_writes_a_starter_registers_it_and_checks_it() {
    let s = Scratch::new("setup");
    let bin = stub_clis(&s, &["claude", "opencode"]);
    let out = with_path(&s, &bin)
        .args([
            "setup",
            "--yes",
            "--manager",
            "claude-code",
            "--starter",
            "hardware",
        ])
        .arg("--team")
        .arg(s.org())
        .output()
        .unwrap();
    let said = stdout_of(&out);
    assert!(out.status.success(), "{said}\n{}", stderr_of(&out));
    assert!(said.contains("Done."), "{said}");
    let written = std::fs::read_to_string(s.org().join("config.yaml")).unwrap();
    assert!(
        written.contains("designer:") && written.contains("checker:"),
        "{written}"
    );
    let args = std::fs::read_to_string(bin.join("claude.args")).unwrap();
    assert!(args.contains("--team\n"), "{args}");

    // A second run keeps what is there.
    let again = with_path(&s, &bin)
        .args(["setup", "--yes", "--manager", "claude-code"])
        .arg("--team")
        .arg(s.org())
        .output()
        .unwrap();
    assert!(
        stdout_of(&again).contains("keeping it"),
        "{}",
        stdout_of(&again)
    );
    assert_eq!(
        std::fs::read_to_string(s.org().join("config.yaml")).unwrap(),
        written
    );
}

#[test]
fn setup_without_any_agent_cli_says_what_to_install() {
    let s = Scratch::new("setup-nothing");
    let bin = stub_clis(&s, &[]);
    let out = with_path(&s, &bin)
        .args(["setup", "--yes"])
        .arg("--team")
        .arg(s.org())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("Install"), "{}", stderr_of(&out));
    assert!(!s.org().join("config.yaml").exists());
}

#[test]
fn doctor_says_what_is_wrong_and_how_to_fix_it() {
    let s = Scratch::new("doctor");
    s.write(
        ".ensemble/config.yaml",
        &config_yaml("/nonexistent/opencode", NOWHERE),
    );
    let out = ensemble(&s, &["doctor"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("problem: opencode (/nonexistent/opencode)"),
        "{text}"
    );
    assert!(text.contains("fix: install `opencode`"), "{text}");
    assert!(text.contains("ensemble remove-agent coder"), "{text}");
    assert!(text.contains("problem: openai-local"), "{text}");
    assert!(text.contains("fix: start the model server"), "{text}");

    let s = Scratch::new("doctor-ok");
    s.write(".ensemble/config.yaml", &config_yaml("true", NOWHERE));
    let url = models_stub(r#"{"data":[{"id":"qwen3-coder"}]}"#);
    s.write(".ensemble/config.yaml", &config_yaml("true", &url));
    std::fs::create_dir_all(s.path().join(".git")).unwrap();
    let out = ensemble(&s, &["doctor"]);
    let text = stdout_of(&out);
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("problem"), "{text}");
}

#[test]
fn add_agent_and_remove_agent_edit_the_config_and_keep_it_loading() {
    let s = Scratch::new("add-agent");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(
        &s,
        &[
            "add-agent",
            "tester",
            "--adapter",
            "claude-code",
            "--prompt",
            "Tests it.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let agents = ensemble(&s, &["agents"]);
    assert!(
        stdout_of(&agents).contains("tester\tclaude-code\tsonnet"),
        "{}",
        stdout_of(&agents)
    );

    let dup = ensemble(&s, &["add-agent", "tester", "--adapter", "claude-code"]);
    assert!(!dup.status.success());
    assert!(
        stderr_of(&dup).contains("already has"),
        "{}",
        stderr_of(&dup)
    );

    let bad = ensemble(&s, &["add-agent", "x", "--adapter", "llm"]);
    assert!(!bad.status.success(), "`llm` is no adapter any more");

    let gone = ensemble(&s, &["remove-agent", "tester"]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(!stdout_of(&ensemble(&s, &["agents"])).contains("tester"));
    let missing = ensemble(&s, &["remove-agent", "tester"]);
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

/// The binary with its state and cache kept in the scratch place.
fn remote_cmd(scratch: &Scratch, bin: Option<&Path>) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(scratch.path())
        .env("XDG_STATE_HOME", scratch.state())
        .env("XDG_CACHE_HOME", scratch.0.with_extension("cache"))
        .env("HOME", scratch.0.with_extension("home"));
    if let Some(bin) = bin {
        c.env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }
    c
}

#[test]
fn a_remote_helper_team_is_pinned_trusted_and_then_used() {
    let s = Scratch::new("remote");
    let (url, sha) = origin(&s, &config_yaml("true", NOWHERE));
    let pinned = format!("{url}#{sha}");

    let out = remote_cmd(&s, None)
        .args(["check", "--team", &pinned])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "an untrusted helper team is not used"
    );
    assert!(
        stderr_of(&out).contains("ensemble org trust"),
        "{}",
        stderr_of(&out)
    );

    let unpinned = remote_cmd(&s, None)
        .args(["check", "--team", &format!("{url}@main")])
        .output()
        .unwrap();
    assert!(!unpinned.status.success());
    assert!(
        stderr_of(&unpinned).contains("never follows a branch"),
        "{}",
        stderr_of(&unpinned)
    );

    let no_pin = remote_cmd(&s, None)
        .args(["team", "trust", &url, "--yes"])
        .output()
        .unwrap();
    assert!(!no_pin.status.success(), "trust needs the commit");

    let trusted = remote_cmd(&s, None)
        .args(["team", "trust", &pinned, "--yes"])
        .output()
        .unwrap();
    assert!(trusted.status.success(), "{}", stderr_of(&trusted));
    let said = stdout_of(&trusted);
    assert!(said.contains("coder: runs `true`"), "{said}");
    assert!(said.contains("Trusted"), "{said}");

    let out = remote_cmd(&s, None)
        .args(["check", "--team", &pinned])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("coder"), "{}", stdout_of(&out));

    // Without a terminal and without --yes there is nobody to ask: a refusal.
    let (url2, sha2) = origin(&s, &config_yaml("opencode", NOWHERE));
    assert_eq!(url, url2);
    let refused = remote_cmd(&s, None)
        .args(["team", "trust", &format!("{url}#{sha2}")])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("--yes"),
        "{}",
        stderr_of(&refused)
    );
}

#[test]
fn connect_pins_a_remote_helper_team_shows_it_and_registers_the_pin() {
    let s = Scratch::new("remote-connect");
    let (url, sha) = origin(&s, &config_yaml("true", NOWHERE));
    let bin = stub_clis(&s, &["claude"]);
    let out = remote_cmd(&s, Some(&bin))
        .args([
            "connect",
            "--manager",
            "claude-code",
            "--yes",
            "--team",
            &format!("{url}@main"),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let said = stdout_of(&out);
    assert!(said.contains("Trusted"), "{said}");
    let args = std::fs::read_to_string(bin.join("claude.args")).unwrap();
    assert!(args.contains(&format!("{url}#{sha}")), "{args}");

    // What it registered is usable now: it was trusted on the way.
    let used = remote_cmd(&s, None)
        .args(["agents", "--team", &format!("{url}#{sha}")])
        .output()
        .unwrap();
    assert!(used.status.success(), "{}", stderr_of(&used));
}

#[test]
fn team_update_shows_what_changed_and_pins_nothing_by_itself() {
    let s = Scratch::new("remote-update");
    let (url, old) = origin(&s, &config_yaml("true", NOWHERE));
    let (_, new) = origin(&s, &config_yaml("opencode", NOWHERE));
    let out = remote_cmd(&s, None)
        .args(["team", "update", &format!("{url}#{old}"), "--ref", "main"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let said = stdout_of(&out);
    assert!(
        said.contains(&format!("The new pin is {url}#{new}")),
        "{said}"
    );
    assert!(
        said.contains("-      command: \"true\"") || said.contains("command"),
        "{said}"
    );
    assert!(said.contains("ensemble team trust"), "{said}");
    let again = remote_cmd(&s, None)
        .args(["team", "update", &format!("{url}#{new}"), "--ref", "main"])
        .output()
        .unwrap();
    assert!(
        stdout_of(&again).contains("Already at"),
        "{}",
        stdout_of(&again)
    );
    // Updating trusts nothing.
    let used = remote_cmd(&s, None)
        .args(["agents", "--team", &format!("{url}#{new}")])
        .output()
        .unwrap();
    assert!(!used.status.success());
}

// ---------------------------------------------------------------------------
// Tasks from the terminal
// ---------------------------------------------------------------------------

#[test]
fn a_task_written_by_submit_is_listed_shown_and_run_by_a_session() {
    let s = Scratch::new("cli-tasks");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");

    let none = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&none).contains("No tasks"),
        "{}",
        stdout_of(&none)
    );

    let out = ensemble(
        &s,
        &[
            "submit",
            "coder",
            "Rename foo",
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
        stdout_of(&out).contains("task 0001-rename-foo written"),
        "{}",
        stdout_of(&out)
    );

    let listed = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&listed).contains("0001-rename-foo · coder · queued · Rename foo"),
        "{}",
        stdout_of(&listed)
    );
    let shown = ensemble(&s, &["task", "0001-rename-foo"]);
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

    let done = ensemble(&s, &["tasks", "--status", "done"]);
    assert!(
        stdout_of(&done).contains("0001-rename-foo"),
        "{}",
        stdout_of(&done)
    );
    let queued = ensemble(&s, &["tasks", "--status", "queued"]);
    assert!(
        stdout_of(&queued).contains("No tasks with status 'queued'"),
        "{}",
        stdout_of(&queued)
    );
    let shown = ensemble(&s, &["task", "0001-rename-foo"]);
    assert!(
        stdout_of(&shown).contains("status: done"),
        "{}",
        stdout_of(&shown)
    );
}

#[test]
fn submit_refuses_what_a_task_cannot_be() {
    let s = Scratch::new("cli-submit-bad");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let no_text = ensemble(&s, &["submit", "coder", "T"]);
    assert!(!no_text.status.success());
    for (args, needle) in [
        (vec!["submit", "nobody", "T", "--text", "x"], "no agent"),
        (
            vec!["submit", "coder", "T", "--text", "x", "--gate", "nope:2"],
            "no gate",
        ),
        (
            vec!["submit", "coder", "T", "--text", "x", "--gate", "test:0"],
            "attempts",
        ),
        (
            vec!["submit", "coder", "T", "--text", "x", "--repo", "../out"],
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
    assert!(stdout_of(&ensemble(&s, &["tasks"])).contains("No tasks"));
}

#[test]
fn answer_gives_a_waiting_task_its_answer() {
    let s = Scratch::new("cli-answer");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    s.write(
        ".ensemble/tasks/0001-ask/task.md",
        "---\ntitle: ask\nagent: coder\n---\nask\n",
    );
    s.write(
        ".ensemble/tasks/0001-ask/questions.md",
        "## Question 1\nWhich name?\n\n",
    );
    let listed = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&listed).contains("needs input: Which name?"),
        "{}",
        stdout_of(&listed)
    );
    let none = ensemble(&s, &["answer", "0001-ask"]);
    assert!(!none.status.success());
    let out = ensemble(&s, &["answer", "0001-ask", "use", "bar"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        s.read(".ensemble/tasks/0001-ask/questions.md")
            .contains("### Answer\nuse bar")
    );
    let again = ensemble(&s, &["answer", "0001-ask", "twice"]);
    assert!(
        !again.status.success(),
        "a second answer has no question to answer"
    );
    let listed = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&listed).contains("queued"),
        "{}",
        stdout_of(&listed)
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
fn serve_runs_the_tasks_without_a_session_and_stops_on_sigterm() {
    let s = Scratch::new("serve");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");
    let out = ensemble(
        &s,
        &[
            "submit",
            "coder",
            "Rename foo",
            "--text",
            "rename foo to bar",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));

    let mut serve = command(&s)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        wait_for(30, || s
            .place(".ensemble/tasks/0001-rename-foo/result.md")
            .is_file()),
        "serve did not run the task"
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );
    let listed = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&listed).contains("done"),
        "{}",
        stdout_of(&listed)
    );

    // One worker per folder: a second serve refuses, and a session does not
    // start another worker.
    let second = command(&s)
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        stderr_of(&second).contains("already running"),
        "{}",
        stderr_of(&second)
    );

    sigterm(&serve);
    let status = serve.wait().unwrap();
    assert!(status.success(), "serve ends cleanly on SIGTERM");
    assert!(
        !s.place(".ensemble/worker.lock").exists(),
        "the lock is released"
    );
}

#[test]
fn a_task_stopped_in_the_middle_is_run_again_by_the_next_serve() {
    let s = Scratch::new("serve-resume");
    // The first run is slow and is stopped; the marker says it had started, so
    // the run after it answers at once.
    let marker = s.0.with_extension("bin").join("started");
    let body = format!(
        "if [ -e '{0}' ]; then :; else touch '{0}'; sleep 60; fi",
        marker.display()
    );
    let cli = s.opencode("done", &body);
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    let out = ensemble(&s, &["submit", "coder", "Slow", "--text", "take your time"]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    let mut first = command(&s)
        .arg("serve")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for(30, || s
        .0
        .parent()
        .unwrap()
        .join("started")
        .exists()
        || {
            let t = ensemble(&s, &["tasks"]);
            stdout_of(&t).contains("running")
        }));
    let listed = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&listed).contains("running"),
        "{}",
        stdout_of(&listed)
    );
    sigterm(&first);
    first.wait().unwrap();
    assert!(
        !s.place(".ensemble/tasks/0001-slow/result.md").exists(),
        "a stopped task has no result"
    );
    let listed = ensemble(&s, &["tasks"]);
    assert!(
        stdout_of(&listed).contains("interrupted"),
        "{}",
        stdout_of(&listed)
    );

    let mut second = command(&s)
        .arg("serve")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_for(30, || s
            .place(".ensemble/tasks/0001-slow/result.md")
            .is_file()),
        "the next serve did not run it again"
    );
    sigterm(&second);
    second.wait().unwrap();
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[test]
fn every_run_leaves_a_record_that_stats_sums_up() {
    let s = Scratch::new("stats");
    let cli = s.opencode("fine", "");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    let none = ensemble(&s, &["stats"]);
    assert!(
        stdout_of(&none).contains("No runs recorded"),
        "{}",
        stdout_of(&none)
    );

    for _ in 0..2 {
        let out = ensemble(&s, &["run", "reviewer", "look"]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    // A run that cannot start is a failure on the record.
    s.write(
        ".ensemble/config.yaml",
        &config_yaml("/nonexistent/opencode", NOWHERE),
    );
    let out = ensemble(&s, &["run", "reviewer", "look"]);
    assert!(!out.status.success());

    let table = ensemble(&s, &["stats"]);
    let text = stdout_of(&table);
    let row = text
        .lines()
        .find(|l| l.starts_with("reviewer"))
        .unwrap_or_else(|| panic!("{text}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!((cells[1], cells[2]), ("3", "1"), "3 runs, 1 failure: {row}");

    let json = ensemble(&s, &["stats", "--json", "--since", "1h"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(&json)).unwrap();
    assert_eq!(parsed[0]["agent"], "reviewer");
    assert_eq!(parsed[0]["runs"], 3);

    let bad = ensemble(&s, &["stats", "--since", "soon"]);
    assert!(!bad.status.success());
    assert!(stderr_of(&bad).contains("duration"), "{}", stderr_of(&bad));
}

// ---------------------------------------------------------------------------
// A helper team that works on its own
// ---------------------------------------------------------------------------

#[test]
fn a_backlog_task_is_assigned_then_served_and_the_helper_team_says_when_it_is_idle() {
    let s = Scratch::new("backlog");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write(".ensemble/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");

    let out = ensemble(
        &s,
        &[
            "submit",
            "Rename foo",
            "--backlog",
            "--text",
            "rename foo to bar",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let listed = stdout_of(&ensemble(&s, &["tasks"]));
    assert!(listed.contains("0001-rename-foo · - · backlog"), "{listed}");
    assert!(
        listed.contains("Idle: no active tasks (1 in the backlog, to be assigned)"),
        "{listed}"
    );
    assert!(s.place(".ensemble/tasks/0001-rename-foo/task.md").is_file());

    // Nothing runs it while it has no assignee.
    let idle = command(&s)
        .args(["serve", "--exit-when-idle"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(idle.status.success(), "{}", stderr_of(&idle));
    assert!(
        stderr_of(&idle).contains("Idle: no active tasks"),
        "{}",
        stderr_of(&idle)
    );
    assert!(
        !s.place(".ensemble/tasks/0001-rename-foo/result.md")
            .exists()
    );

    let bad = ensemble(&s, &["assign", "0001-rename-foo", "nobody"]);
    assert!(!bad.status.success());
    let ok = ensemble(&s, &["assign", "0001-rename-foo", "coder"]);
    assert!(ok.status.success(), "{}", stderr_of(&ok));
    assert!(
        stdout_of(&ok).contains("assigned to coder"),
        "{}",
        stdout_of(&ok)
    );
    let listed = stdout_of(&ensemble(&s, &["tasks"]));
    assert!(listed.contains("coder · queued"), "{listed}");
    assert!(!listed.contains("Idle"), "{listed}");

    // Served, then it stops by itself because nothing is left.
    let served = command(&s)
        .args(["serve", "--exit-when-idle"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(served.status.success(), "{}", stderr_of(&served));
    assert!(
        stderr_of(&served).contains("Idle: no active tasks"),
        "{}",
        stderr_of(&served)
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );
    assert!(stdout_of(&ensemble(&s, &["tasks"])).contains("Idle: no active tasks"));
    let shown = stdout_of(&ensemble(&s, &["task", "0001-rename-foo"]));
    assert!(shown.contains("status: done"), "{shown}");
}

#[test]
fn an_empty_helper_team_is_idle_and_serve_can_end_there() {
    let s = Scratch::new("idle-empty");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = command(&s)
        .args(["serve", "--exit-when-idle"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("Idle: no active tasks. The helper team waits for planning"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn check_warns_about_what_agents_may_do_without_failing() {
    let s = Scratch::new("check-warn");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(&s, &["check"]);
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
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble(
        &s,
        &[
            "add-agent",
            "fixer",
            "--adapter",
            "codex",
            "--prompt",
            "Fixes it.",
            "--writes",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let shown = ensemble(&s, &["show", "fixer"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let text = stdout_of(&shown);
    assert!(
        text.starts_with("codex exec --json --skip-git-repo-check --sandbox workspace-write --model gpt-5-codex -- "),
        "{text}"
    );
    assert!(text.contains("Fixes it."), "{text}");
    let agents = stdout_of(&ensemble(&s, &["agents"]));
    assert!(agents.contains("fixer\tcodex\tgpt-5-codex"), "{agents}");
}

#[test]
fn a_light_agent_on_an_anthropic_backend_shows_a_messages_request() {
    let s = Scratch::new("show-anthropic");
    let v1 = format!(
        "{}\n",
        config_v1("opencode", NOWHERE).replacen(
            "llm_backends:\n",
            "llm_backends:\n  claude-api:\n    type: anthropic-api\n    base_url: \"https://api.anthropic.com\"\n    api_key_env: ENSEMBLE_TEST_ANTHROPIC_KEY\n    timeout_seconds: 30\n",
            1,
        )
    );
    s.write(".ensemble/config.yaml", &v1);
    let out = ensemble(
        &s,
        &[
            "add-agent",
            "helper",
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
    let shown = ensemble(&s, &["show", "helper"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let text = stdout_of(&shown);
    assert!(
        text.starts_with("POST https://api.anthropic.com/v1/messages"),
        "{text}"
    );
    assert!(text.contains("\"system\": \"You help.\""), "{text}");
    let check = stdout_of(&ensemble(&s, &["check"]));
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
        ".ensemble/config.yaml",
        &config_v1(&cli, NOWHERE).replace("isolation: shared", "isolation: worktree"),
    );
    s.write("a.txt", "foo\n");
    git_in(s.path(), &["init", "--quiet", "-b", "main"]);
    // The worktree commit and the merge are made as whoever the repository says.
    git_in(s.path(), &["config", "user.name", "T"]);
    git_in(s.path(), &["config", "user.email", "t@t"]);
    git_in(s.path(), &["add", "a.txt"]);
    git_in(s.path(), &["commit", "--quiet", "-m", "start"]);

    let out = ensemble(
        &s,
        &[
            "submit",
            "coder",
            "Rename foo",
            "--text",
            "rename foo to bar",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let served = command(&s)
        .args(["serve", "--exit-when-idle"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(served.status.success(), "{}", stderr_of(&served));
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "foo\n",
        "the project is untouched"
    );

    let review = ensemble(&s, &["review", "0001-rename-foo"]);
    let text = stdout_of(&review);
    assert!(review.status.success(), "{}", stderr_of(&review));
    assert!(
        text.contains("branch ensemble/0001-rename-foo") && text.contains("a.txt | 2"),
        "{text}"
    );
    assert!(text.contains("Result:"), "{text}");
    let diff = stdout_of(&ensemble(&s, &["diff", "0001-rename-foo"]));
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
    let dirty = ensemble(&s, &["merge", "0001-rename-foo"]);
    assert!(!dirty.status.success());
    assert!(
        stderr_of(&dirty).contains("uncommitted changes"),
        "{}",
        stderr_of(&dirty)
    );
    git_in(s.path(), &["checkout", "--quiet", "a.txt"]);

    let merged = ensemble(&s, &["merge", "0001-rename-foo", "--squash", "--cleanup"]);
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
    assert!(!s.place(".ensemble/worktrees/0001-rename-foo").exists());
    let again = ensemble(&s, &["review", "0001-rename-foo"]);
    assert!(!again.status.success(), "its branch is gone");
}

#[test]
fn show_lists_an_agents_models_in_order_and_probe_says_which_a_run_would_use() {
    let s = Scratch::new("show-probe");
    let central = models_stub(r#"{"data":[{"id":"big"}]}"#);
    s.write(
        ".ensemble/config.yaml",
        &format!(
            "version: 3\nllm_backends:\n  home: {{ type: openai-compatible-api, base_url: \"{NOWHERE}\", timeout_seconds: 5 }}\n  central: {{ type: openai-compatible-api, base_url: \"{central}\", timeout_seconds: 5 }}\nlimits: {{}}\ntracker: {{ type: local_markdown }}\ngates: {{}}\nagents:\n  summariser:\n    max_seconds: 30\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try:\n        - {{ backend: home, model: small }}\n        - {{ backend: central, model: big }}\n      when_none: wait\n      max_tokens: 10\n"
        ),
    );
    let out = ensemble(&s, &["show", "summariser"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("models, in order (the first is shown above; when none works: wait):"),
        "{text}"
    );
    assert!(
        text.contains("  1. home / small\n  2. central / big\n"),
        "{text}"
    );
    assert!(!text.contains("a run now"), "{text}");

    let probed = ensemble(&s, &["show", "summariser", "--probe"]);
    assert!(probed.status.success(), "{}", stderr_of(&probed));
    assert!(
        stdout_of(&probed).contains("a run now would use 2."),
        "{}",
        stdout_of(&probed)
    );

    let health = ensemble(&s, &["health"]);
    assert!(!health.status.success(), "home is down");
    let said = stdout_of(&health);
    assert!(
        said.contains("home: FAILED") && said.contains("central: ok"),
        "{said}"
    );
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

#[test]
fn hub_init_writes_a_hub_and_its_vault_and_check_says_which_model_distills() {
    let scratch = Scratch::new("hub-init");
    let hub = scratch.0.with_extension("hub");
    let out = ensemble(&scratch, &["hub", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no hub given"),
        "{}",
        stderr_of(&out)
    );

    let out = command(&scratch)
        .args(["hub", "init", "--hub"])
        .arg(&hub)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(hub.join("hub.yaml").exists());
    assert!(
        hub.join("vault/.git").exists(),
        "the vault is a git repository"
    );
    let again = command(&scratch)
        .args(["hub", "init", "--hub"])
        .arg(&hub)
        .output()
        .unwrap();
    assert!(
        !again.status.success(),
        "it does not overwrite without --force"
    );

    let url = models_stub(r#"{"data":[{"id":"small"}]}"#);
    std::fs::write(
        hub.join("hub.yaml"),
        format!(
            "version: 1\nowner: Ana\nllm_backends:\n  down: {{ type: openai-compatible-api, base_url: \"{NOWHERE}\", timeout_seconds: 5 }}\n  up: {{ type: openai-compatible-api, base_url: \"{url}\", timeout_seconds: 5 }}\nknowledge:\n  vault: vault\n  distill:\n    try:\n      - {{ backend: down, model: small }}\n      - {{ backend: up, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall: {{ max_notes: 3, max_chars: 1000 }}\n"
        ),
    )
    .unwrap();
    let out = command(&scratch)
        .args(["hub", "check", "--hub"])
        .arg(&hub)
        .output()
        .unwrap();
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

/// A hub made by `ensemble hub init`, beside the project.
fn hub_of(scratch: &Scratch) -> PathBuf {
    let hub = scratch.0.with_extension("hub");
    let out = command(scratch)
        .args(["hub", "init", "--hub"])
        .arg(&hub)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    hub
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
fn knowledge_is_shared_by_hand_or_by_the_agent_through_mcp() {
    let s = Scratch::new("hub-share");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let hub = hub_of(&s);

    let out = command(&s)
        .args([
            "share",
            "--title",
            "Ports",
            "--text",
            "The shop listens on 8080.",
            "--hub",
        ])
        .arg(&hub)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("shared as "),
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

    let mut session = McpSession::start_with(&s, &["--hub".as_ref(), hub.as_os_str()]);
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

/// A hub whose knowledge is distilled on `url`, made beside the project.
fn hub_on(scratch: &Scratch, url: &str) -> PathBuf {
    let hub = hub_of(scratch);
    std::fs::write(
        hub.join("hub.yaml"),
        format!(
            "version: 1\nowner: Ana\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"{url}\", timeout_seconds: 5 }}\nknowledge:\n  vault: vault\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 500\n  recall: {{ max_notes: 3, max_chars: 2000 }}\n"
        ),
    )
    .unwrap();
    hub
}

/// A model that answers every chat request with these items.
fn items_stub(items: &str) -> String {
    let chat = serde_json::json!({
        "choices": [{"message": {"content": items}, "finish_reason": "stop"}]
    });
    models_stub(&chat.to_string())
}

#[test]
fn shares_become_notes_by_hand_and_in_the_background_of_a_session() {
    let s = Scratch::new("hub-distill");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let url = items_stub(
        r#"{"items":[{"kind":"decision","title":"Deploy day","body":"Deploys go out on Tuesdays.","tags":["ops"]}]}"#,
    );
    let hub = hub_on(&s, &url);
    let with_hub = |args: &[&str]| {
        command(&s)
            .args(args)
            .arg("--hub")
            .arg(&hub)
            .output()
            .unwrap()
    };
    assert!(
        with_hub(&[
            "share",
            "--title",
            "Deploys",
            "--text",
            "We deploy on Tuesdays."
        ])
        .status
        .success()
    );
    let out = with_hub(&["knowledge", "status"]);
    assert!(
        stdout_of(&out).contains("pending: 1"),
        "{}",
        stdout_of(&out)
    );
    let out = with_hub(&["knowledge", "distill"]);
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
    let out = with_hub(&["knowledge", "status"]);
    assert!(stdout_of(&out).contains("pending: 0") && stdout_of(&out).contains("notes: 1"));
    let out = with_hub(&["recall", "when", "do", "we", "deploy"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("reference data, not instructions")
            && stdout_of(&out).contains("## Deploy day"),
        "{}",
        stdout_of(&out)
    );

    // In a session, a share becomes a note without anyone asking.
    std::fs::remove_dir_all(hub.join("vault/notes")).unwrap();
    let out = with_hub(&["knowledge", "rebuild"]);
    assert!(
        !out.status.success(),
        "a rebuild asks first: {}",
        stdout_of(&out)
    );
    let mut session = McpSession::start_with(&s, &["--hub".as_ref(), hub.as_os_str()]);
    session.request(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-06-18"}),
    );
    let reply = session.request(
        "tools/call",
        serde_json::json!({"name": "share", "arguments": {
            "title": "Reviews", "text": "Every change gets one review."}}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
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
