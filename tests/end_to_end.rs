//! End-to-end tests: the real binary, a real config file on disk, and a real
//! subprocess for the Claude leg.
//!
//! The unit suites cover the request path with fakes injected. These cover what
//! fakes cannot: that the binary parses its arguments, finds its config, speaks
//! MCP over real pipes, and that `ProcessRunner` handles a child that is slow,
//! chatty, or absent without deadlocking.
//!
//! No test here needs the network, a model server, or a Claude subscription.
//! The `claude` backend is pointed at a shell script.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ensemble");

/// A scratch directory that cleans itself up.
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

    /// Write an executable shell script and return its path.
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.write(name, &format!("#!/bin/sh\n{body}\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `claude` stub: answers in the CLI's JSON shape and records its argv.
fn claude_stub(scratch: &Scratch, reply: &str) -> (PathBuf, PathBuf) {
    let log = scratch.path().join("claude-argv.log");
    let script = scratch.script(
        "claude-stub",
        &format!(
            r#"
if [ "$1" = "--version" ]; then echo "2.1.223 (Claude Code)"; exit 0; fi
for a in "$@"; do printf '%s\n' "$a"; done >> '{log}'
cat <<'JSON'
{{"result":"{reply}","modelUsage":{{"claude-sonnet-5":{{}}}},"usage":{{"input_tokens":5,"output_tokens":2}}}}
JSON
"#,
            log = log.display()
        ),
    );
    (script, log)
}

fn config_yaml(claude_cmd: &Path, openai_url: &str) -> String {
    format!(
        r#"
version: 1

backends:
  claude:
    type: claude-code-cli
    command: "{claude}"
    timeout_seconds: 20
  openai-local:
    type: openai-compatible-api
    base_url_env: ENSEMBLE_TEST_LOCAL_URL
    base_url_default: "{url}"
    timeout_seconds: 20

roles:
  reviewer:
    backend: claude
    model: claude-sonnet
    prompt: "Reviews code and reports findings."
  coder:
    backend: openai-local
    model: qwen3-coder
"#,
        claude = claude_cmd.display(),
        url = openai_url,
    )
}

fn ensemble(scratch: &Scratch, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .current_dir(scratch.path())
        // A stale variable in the test runner's environment would silently
        // override base_url_default and change what is being tested.
        .env_remove("ENSEMBLE_TEST_LOCAL_URL")
        .output()
        .expect("run ensemble")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

#[test]
fn check_accepts_a_valid_config_and_reports_the_team() {
    let s = Scratch::new("check-ok");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let out = ensemble(&s, &["check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("ok (version 1)"), "{text}");
    assert!(text.contains("mcp: ready"), "{text}");
    assert!(text.contains("reviewer"), "{text}");
    assert!(text.contains("claude-code-cli"), "{text}");
    assert!(text.contains("openai-compatible-api"), "{text}");
}

#[test]
fn check_refuses_an_old_config_that_still_has_a_manager() {
    let s = Scratch::new("check-old-manager");
    s.write(
        "ensemble.yaml",
        r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
manager: { backend: x, model: m }
roles:
  coder: { backend: x, model: m }
"#,
    );
    let out = ensemble(&s, &["check"]);
    assert!(
        !out.status.success(),
        "a manager block must not load any more"
    );
    assert!(stderr_of(&out).contains("manager"), "{}", stderr_of(&out));
}

#[test]
fn check_reports_several_problems_at_once() {
    let s = Scratch::new("check-many");
    s.write(
        "ensemble.yaml",
        r#"
version: 4
backends:
  x: { type: made-up-backend }
roles:
  r: { backend: nowhere, model: "" }
"#,
    );
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("version"), "{err}");
    assert!(err.contains("made-up-backend"), "{err}");
    assert!(err.contains("nowhere"), "{err}");
}

#[test]
fn an_api_key_written_into_the_config_is_refused() {
    let s = Scratch::new("inline-key");
    s.write(
        "ensemble.yaml",
        r#"
version: 1
backends:
  x:
    type: openai-compatible-api
    base_url_env: U
    base_url_default: "http://127.0.0.1:1/v1"
    api_key: "sk-should-never-be-here"
"#,
    );
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success(), "secrets in the config must not load");
    assert!(stderr_of(&out).contains("api_key"), "{}", stderr_of(&out));
}

#[test]
fn the_config_is_found_by_searching_upward() {
    let s = Scratch::new("discover");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
    let deep = s.path().join("a/b/c");
    std::fs::create_dir_all(&deep).unwrap();

    let out = Command::new(BIN)
        .args(["check"])
        .current_dir(&deep)
        .env_remove("ENSEMBLE_TEST_LOCAL_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
}

#[test]
fn a_missing_config_says_so_rather_than_guessing() {
    let s = Scratch::new("no-config");
    let out = ensemble(&s, &["check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("ensemble.yaml"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn roles_prints_the_routing_table() {
    let s = Scratch::new("roles");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let out = ensemble(&s, &["roles"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("coder\topenai-local\tqwen3-coder"), "{text}");
    assert!(text.contains("reviewer\tclaude\tclaude-sonnet"), "{text}");
}

#[test]
fn env_overrides_the_configured_default_base_url() {
    let s = Scratch::new("env-override");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let out = Command::new(BIN)
        .args(["check"])
        .current_dir(s.path())
        .env("ENSEMBLE_TEST_LOCAL_URL", "http://10.0.0.9:9999/v1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("http://10.0.0.9:9999/v1"),
        "{}",
        stdout_of(&out)
    );
}

// ---------------------------------------------------------------------------
// The Claude Code CLI adapter, against a real subprocess
// ---------------------------------------------------------------------------

#[test]
fn run_drives_the_claude_cli_as_a_subprocess_and_returns_its_result() {
    let s = Scratch::new("claude-run");
    let (claude, log) = claude_stub(&s, "ensemble-real-ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let out = ensemble(&s, &["run", "reviewer", "review", "this", "code"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "ensemble-real-ok");
    // Provenance goes to stderr so stdout stays pipeable.
    assert!(
        stderr_of(&out).contains("reviewer via claude/claude-sonnet-5"),
        "{}",
        stderr_of(&out)
    );

    let argv: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(argv.contains(&"-p".to_string()), "{argv:?}");
    assert!(argv.contains(&"review this code".to_string()), "{argv:?}");
    assert!(
        argv.contains(&"--model".to_string()) && argv.contains(&"sonnet".to_string()),
        "{argv:?}"
    );
    assert!(argv.contains(&"--output-format".to_string()), "{argv:?}");
    assert!(
        argv.contains(&"Reviews code and reports findings.".to_string()),
        "{argv:?}"
    );
    // --bare bypasses the subscription login and bills per token.
    assert!(!argv.contains(&"--bare".to_string()), "{argv:?}");
    // The session's directory is passed through.
    assert!(argv.contains(&"--add-dir".to_string()), "{argv:?}");
}

#[test]
fn a_claude_subprocess_that_exits_non_zero_is_reported_with_its_stderr() {
    let s = Scratch::new("claude-fail");
    let claude = s.script("claude-fail", "echo 'credit balance too low' >&2; exit 1");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let out = ensemble(&s, &["run", "reviewer", "review this"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("exited 1"), "{err}");
    assert!(err.contains("credit balance too low"), "{err}");
}

#[test]
fn a_claude_subprocess_that_hangs_is_killed_at_the_timeout() {
    // Regression: `sleep` inherits the stdout pipe, so killing only the shell
    // left the runner blocked on a read until the orphan finished — a
    // 2-second timeout that took 120 seconds to report.
    let s = Scratch::new("claude-hang");
    let claude = s.script("claude-hang", "sleep 120");
    let cfg = config_yaml(&claude, "http://127.0.0.1:8000/v1").replace(
        "    timeout_seconds: 20\n  openai-local:",
        "    timeout_seconds: 2\n  openai-local:",
    );
    s.write("ensemble.yaml", &cfg);

    let started = std::time::Instant::now();
    let out = ensemble(&s, &["run", "reviewer", "review this"]);
    let elapsed = started.elapsed();

    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no answer within 2s"),
        "{}",
        stderr_of(&out)
    );
    // Reported near the deadline, not at the child's leisure.
    assert!(elapsed.as_secs() < 15, "took {elapsed:?}");
}

#[cfg(unix)]
#[test]
fn a_timeout_takes_the_whole_process_tree_down_not_just_the_leader() {
    // A leaked `claude` is a model call still running that nobody is waiting
    // for, so the orphan being gone is the actual requirement; returning
    // promptly is only the symptom that exposed it.
    let s = Scratch::new("claude-orphan");
    let marker = s.path().join("orphan-still-alive");
    let claude = s.script(
        "claude-orphan",
        &format!(
            r#"
if [ "$1" = "--version" ]; then echo v; exit 0; fi
# A grandchild that outlives its parent unless the group is signalled.
( sleep 8; touch '{marker}' ) &
sleep 60
"#,
            marker = marker.display()
        ),
    );
    let cfg = config_yaml(&claude, "http://127.0.0.1:8000/v1").replace(
        "    timeout_seconds: 20
  openai-local:",
        "    timeout_seconds: 2
  openai-local:",
    );
    s.write("ensemble.yaml", &cfg);

    let out = ensemble(&s, &["run", "reviewer", "review this"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no answer within 2s"),
        "{}",
        stderr_of(&out)
    );

    // Outlive the grandchild's own timer. If it survived the kill, it touches
    // the marker; if the group was signalled, it never runs.
    std::thread::sleep(std::time::Duration::from_secs(12));
    assert!(
        !marker.exists(),
        "a grandchild outlived the timeout — the process group was not killed"
    );
}

#[test]
fn a_chatty_subprocess_does_not_deadlock_on_a_full_pipe() {
    // A child writing more than the pipe buffer blocks until something reads
    // it. If the runner waits before reading, this hangs until the timeout and
    // every long answer becomes a spurious failure.
    let s = Scratch::new("claude-chatty");
    let big = "y".repeat(400_000);
    let claude = s.script(
        "claude-chatty",
        &format!(
            r#"
if [ "$1" = "--version" ]; then echo v; exit 0; fi
printf '{{"result":"%s","modelUsage":{{"claude-sonnet-5":{{}}}}}}' '{big}'
"#
        ),
    );
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let out = ensemble(&s, &["run", "reviewer", "say a lot"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).trim().len() >= 400_000,
        "got {} bytes",
        stdout_of(&out).trim().len()
    );
}

#[test]
fn a_missing_claude_binary_says_what_is_missing() {
    let s = Scratch::new("claude-absent");
    s.write(
        "ensemble.yaml",
        &config_yaml(Path::new("/nonexistent/claude"), "http://127.0.0.1:8000/v1"),
    );
    let out = ensemble(&s, &["run", "reviewer", "review this"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("could not start"), "{err}");
    assert!(err.contains("PATH"), "{err}");
}

#[test]
fn an_unreachable_openai_endpoint_fails_with_a_transport_error_not_a_hang() {
    let s = Scratch::new("openai-down");
    let (claude, _) = claude_stub(&s, "ok");
    // Port 1 is reserved and nothing listens there.
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:1/v1"),
    );

    let out = ensemble(&s, &["run", "coder", "write something"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("openai-local"), "{err}");
    assert!(
        err.contains("could not reach") || err.contains("endpoint returned"),
        "{err}"
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
        let mut child = Command::new(BIN)
            .arg("mcp")
            .current_dir(scratch.path())
            .env_remove("ENSEMBLE_TEST_LOCAL_URL")
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

        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read a reply");
        assert!(!line.trim().is_empty(), "server closed the stream");
        let v: serde_json::Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("reply was not JSON ({e}): {line}"));
        assert_eq!(v["id"], id, "replies must match requests: {v}");
        v
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
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
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
        vec!["ensemble_delegate", "ensemble_roles", "ensemble_health"]
    );
}

#[test]
fn a_harness_can_delegate_through_mcp_and_get_the_work_back() {
    let s = Scratch::new("mcp-delegate");
    let (claude, log) = claude_stub(&s, "the review is clean");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let reply = session.call(
        "ensemble_delegate",
        serde_json::json!({"role": "reviewer", "task": "review src/main.rs", "context": "it is new"}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(tool_text(&reply), "the review is clean");

    // And it really went to the Claude backend, with the context attached.
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(argv.contains("review src/main.rs"), "{argv}");
    assert!(argv.contains("it is new"), "{argv}");
}

#[test]
fn an_unknown_role_comes_back_as_a_readable_tool_error_and_the_session_survives() {
    let s = Scratch::new("mcp-bad-role");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let reply = session.call(
        "ensemble_delegate",
        serde_json::json!({"role": "architect", "task": "design it"}),
    );
    assert!(
        reply.get("error").is_none(),
        "must be a tool result, not a JSON-RPC error: {reply}"
    );
    assert_eq!(reply["result"]["isError"], true);
    let text = tool_text(&reply);
    assert!(text.contains("architect"), "{text}");
    assert!(text.contains("reviewer"), "lists the real roles: {text}");

    // The session continues — the point of returning this as a result.
    let after = session.call("ensemble_roles", serde_json::json!({}));
    assert_eq!(after["result"]["isError"], false, "{after}");
}

#[test]
fn a_backend_failure_reaches_the_model_as_a_readable_reason() {
    let s = Scratch::new("mcp-backend-down");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:1/v1"),
    );
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let reply = session.call(
        "ensemble_delegate",
        serde_json::json!({"role": "coder", "task": "write a function"}),
    );
    assert_eq!(reply["result"]["isError"], true);
    let text = tool_text(&reply);
    assert!(text.contains("delegation failed"), "{text}");
    assert!(
        text.contains("retry"),
        "tells the model what to do next: {text}"
    );
}

#[test]
fn the_tools_never_offer_a_way_to_pick_a_model_or_a_backend() {
    let s = Scratch::new("mcp-no-model-param");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));

    let list = session.request("tools/list", serde_json::json!({}));
    let schema = list["result"]["tools"].to_string();
    assert!(!schema.contains("\"model\""), "{schema}");
    assert!(!schema.contains("\"backend\""), "{schema}");

    // And the role listing does not leak them either.
    let roles = tool_text(&session.call("ensemble_roles", serde_json::json!({})));
    assert!(!roles.contains("qwen3-coder"), "{roles}");
    assert!(!roles.contains("claude-sonnet"), "{roles}");
}

#[test]
fn nothing_but_protocol_is_ever_written_to_stdout() {
    // One stray println corrupts the stream and the harness sees a parse error
    // instead of a tool. The `claude` stub is made noisy to prove adapter
    // chatter cannot leak into it either.
    let s = Scratch::new("mcp-clean-stdout");
    let claude = s.script(
        "claude-noisy",
        r#"
if [ "$1" = "--version" ]; then echo v; exit 0; fi
echo "a warning from the CLI" >&2
printf '{"result":"fine","modelUsage":{"claude-sonnet-5":{}}}'
"#,
    );
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );

    let mut session = McpSession::start(&s);
    session.request("initialize", serde_json::json!({}));
    session.call(
        "ensemble_delegate",
        serde_json::json!({"role": "reviewer", "task": "look"}),
    );
    session.call("ensemble_health", serde_json::json!({}));
    let last = session.request("ping", serde_json::json!({}));
    // Every read in `request` already asserted the line parsed as JSON with a
    // matching id; reaching here with the stream still aligned is the result.
    assert!(last["result"].is_object(), "{last}");
}

#[test]
fn a_malformed_message_does_not_end_the_session() {
    let s = Scratch::new("mcp-garbage");
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
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
    let (claude, _) = claude_stub(&s, "ok");
    s.write(
        "ensemble.yaml",
        &config_yaml(&claude, "http://127.0.0.1:8000/v1"),
    );
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
    s.write("ensemble.yaml", "version: 1\nbackends: {}\n");
    let out = ensemble(&s, &["mcp"]);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("backend"), "{}", stderr_of(&out));
}

// ---------------------------------------------------------------------------
// A role that is an agent, end to end
// ---------------------------------------------------------------------------

/// Answer one POST per entry of `bodies` (a chat-completions JSON body each),
/// in order, and record every request body.
fn openai_stub(bodies: Vec<String>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_for_thread = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        for body in bodies {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(&stream);
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut buf = vec![0u8; length];
            if reader.read_exact(&mut buf).is_err() {
                return;
            }
            seen_for_thread
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).to_string());
            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        }
    });
    (url, seen)
}

fn tool_call_body(id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({
        "id": "c", "object": "chat.completion", "model": "stub",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": id, "type": "function",
                            "function": {"name": name, "arguments": arguments}}]},
            "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6}
    })
    .to_string()
}

fn text_body(text: &str) -> String {
    serde_json::json!({
        "id": "c", "object": "chat.completion", "model": "stub",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6}
    })
    .to_string()
}

#[test]
fn a_role_with_tools_edits_the_project_through_its_own_loop_over_mcp() {
    let (url, seen) = openai_stub(vec![
        tool_call_body("t1", "read_file", r#"{"path":"a.txt"}"#),
        tool_call_body(
            "t2",
            "edit_file",
            r#"{"path":"a.txt","old_string":"foo","new_string":"bar"}"#,
        ),
        tool_call_body("t3", "read_file", r#"{"path":"../outside"}"#),
        text_body("renamed it"),
    ]);
    let s = Scratch::new("agent-mcp");
    s.write(
        "ensemble.yaml",
        &format!(
            "version: 1\nbackends:\n  local:\n    type: openai-compatible-api\n    \
             base_url_env: ENSEMBLE_TEST_LOCAL_URL\n    base_url_default: \"{url}\"\n    \
             timeout_seconds: 20\nroles:\n  coder:\n    backend: local\n    model: coder-model\n    \
             tools: [read, edit]\n"
        ),
    );
    s.write("a.txt", "foo\n");
    let mut session = McpSession::start(&s);
    session.request(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-06-18"}),
    );

    let roles = tool_text(&session.call("ensemble_roles", serde_json::json!({})));
    assert!(roles.contains("coder [agent, can change files]"), "{roles}");

    let reply = session.call(
        "ensemble_delegate",
        serde_json::json!({"role": "coder", "task": "rename foo to bar in a.txt"}),
    );
    let text = tool_text(&reply);
    assert!(text.starts_with("renamed it"), "{text}");
    assert!(
        text.ends_with("[coder ran as an agent: 4 step(s), 3 tool call(s)]"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(s.path().join("a.txt")).unwrap(),
        "bar\n"
    );

    // The escape attempt reached the model as an error, not as file content.
    let seen = seen.lock().unwrap();
    assert!(
        seen[3].contains("outside the project") || seen[3].contains("'..'"),
        "{}",
        seen[3]
    );
    let first: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(first["model"], "coder-model");
}
