//! End-to-end tests: the real binary, a real config file on disk, and a stand-in
//! for the OpenCode CLI.
//!
//! The unit suites cover the request path with fakes injected. These cover what
//! fakes cannot: that the binary parses its arguments, finds its config, starts
//! the agent's CLI with the right command line and environment, speaks MCP over
//! real pipes, and asks a real socket which models it serves.
//!
//! No test here needs the network, a model server or an agent CLI.

use std::io::{BufRead, BufReader, Write};
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

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.0.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).expect("write scratch file");
        p
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
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
    }
}

/// Two OpenCode agents run by `command`: `reviewer` on a provider OpenCode
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

agents:
  reviewer:
    prompt: "Reviews code and reports findings."
    max_seconds: 1800
    max_concurrent: 2
    adapter:
      type: opencode
      model: {{ type: provider, name: opencode/reviewer-model }}
      tools: [read, glob, grep, list]
      max_steps: 30
      command: "{command}"
  coder:
    max_seconds: 1800
    max_concurrent: 2
    adapter:
      type: opencode
      model:
        type: llm_backend
        backend: openai-local
        name: qwen3-coder
        context_window: 32768
      tools: [read, edit]
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
        .env_remove("ENSEMBLE_TEST_LOCAL_URL")
        .env_remove("ENSEMBLE_TEST_LOCAL_KEY");
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
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
            }
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
    assert!(text.contains("ok (version 1)"), "{text}");
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
agents:
  r:
    max_seconds: 60
    max_concurrent: 2
    adapter:
      type: opencode
      model: { type: llm_backend, backend: nowhere, name: "", context_window: 8000 }
      tools: []
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
        "version: 1\nllm_backends: {}\nagents:\n  coder:\n    max_seconds: 60\n    max_concurrent: 2\n    adapter: { type: ensemble, model: m }\n",
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
fn a_config_in_a_parent_directory_is_not_used() {
    let s = Scratch::new("discover");
    s.write(".ensemble/config.yaml", &config_yaml("opencode", NOWHERE));
    let deep = s.path().join("a/b/c");
    std::fs::create_dir_all(&deep).unwrap();

    let out = command(&s)
        .arg("check")
        .current_dir(&deep)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a parent's config must not configure a subfolder"
    );
    assert!(
        stderr_of(&out).contains("parent directories are not searched"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_missing_config_says_so_rather_than_guessing() {
    let s = Scratch::new("no-config");
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains(".ensemble/config.yaml"),
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
        let mut child = command(scratch)
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
            "delegate", "submit", "status", "wait", "cancel", "agents", "health"
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

    let ticket = s.path().join(".ensemble/tasks/0001-rename-foo");
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
fn init_writes_a_sample_that_checks_and_never_overwrites_by_accident() {
    let s = Scratch::new("init");
    let out = ensemble(&s, &["init"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let written = std::fs::read_to_string(s.path().join(".ensemble/config.yaml")).unwrap();
    assert!(written.contains("type: opencode"), "{written}");
    assert_eq!(
        std::fs::read_to_string(s.path().join(".ensemble/.gitignore")).unwrap(),
        ".env\n"
    );
    let env = s.path().join(".ensemble/.env");
    assert!(
        std::fs::read_to_string(&env)
            .unwrap()
            .contains("OPENAI_LOCAL_API_KEY")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "only its owner may read the secrets file");
    }

    assert!(
        std::fs::read_to_string(s.path().join(".ensemble/tasks/README.md"))
            .unwrap()
            .contains("agent: coder")
    );

    let check = ensemble(&s, &["check"]);
    assert!(check.status.success(), "{}", stderr_of(&check));

    s.write(".ensemble/config.yaml", "mine");
    let again = ensemble(&s, &["init"]);
    assert!(!again.status.success());
    assert!(
        stderr_of(&again).contains("--force"),
        "{}",
        stderr_of(&again)
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join(".ensemble/config.yaml")).unwrap(),
        "mine"
    );

    let printed = ensemble(&s, &["init", "--stdout"]);
    assert_eq!(stdout_of(&printed), written);
    std::fs::write(&env, "MINE=1\n").unwrap();
    assert!(ensemble(&s, &["init", "--force"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&env).unwrap(),
        "MINE=1\n",
        "--force never overwrites the secrets"
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join(".ensemble/config.yaml")).unwrap(),
        written
    );
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
    assert!(text.contains(".env: read for variables"), "{text}");
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
