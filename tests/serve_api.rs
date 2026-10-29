//! `ensemble serve`, end to end: the real binary, a real TCP port, a real
//! HTTP client, and a scripted model server on loopback.
//!
//! The unit suite drives the handler in process with fakes injected, which is
//! where the branch coverage lives. This covers what that cannot: that the
//! binary parses its new flags, binds, speaks HTTP/1.1 well enough for an
//! ordinary client, streams SSE over a socket, and refuses to start when the
//! manager cannot do the job.
//!
//! Nothing here needs the network, a model server or a subscription. The
//! upstream is forty lines of `std::net` answering from a queue.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ensemble");

// ---------------------------------------------------------------------------
// Scratch
// ---------------------------------------------------------------------------

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ensemble-serve-{name}-{}-{:?}",
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
    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.0.join(name), contents).expect("write scratch file");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// A scripted OpenAI-compatible upstream
// ---------------------------------------------------------------------------

/// Answers `/chat/completions` from a queue and records what it was asked.
///
/// When the queue runs dry it repeats `fallback`, so a test scripts only the
/// turns it cares about.
struct Upstream {
    url: String,
    /// The turn requests, in order. The startup probe is not one of them.
    seen: Arc<Mutex<Vec<String>>>,
    /// The startup probes, kept apart so `seen` still means "turns".
    probes: Arc<Mutex<Vec<String>>>,
}

fn completion(content: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "stub-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6}
    })
    .to_string()
}

/// Render a non-streaming completion body (as `completion`/`tool_call_completion`
/// produce it) as the SSE chunks a real streaming server would send for the
/// same answer, so one fixture serves a request whichever way it asks.
fn as_sse(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let choice = &v["choices"][0];
    let message = &choice["message"];
    let mut chunks = vec![serde_json::json!({
        "id": v["id"], "object": "chat.completion.chunk", "model": v["model"],
        "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
    })];

    if let Some(content) = message["content"].as_str().filter(|c| !c.is_empty()) {
        chunks.push(serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
        }));
    }
    if let Some(calls) = message["tool_calls"].as_array().filter(|c| !c.is_empty()) {
        let delta_calls: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({
                    "index": i, "id": c["id"], "type": "function",
                    "function": {"name": c["function"]["name"],
                                 "arguments": c["function"]["arguments"]}
                })
            })
            .collect();
        chunks.push(serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": delta_calls}, "finish_reason": null}]
        }));
    }
    chunks.push(serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": choice["finish_reason"]}]
    }));
    chunks.push(serde_json::json!({ "choices": [], "usage": v["usage"] }));

    let mut out = String::new();
    for c in chunks {
        out.push_str("data: ");
        out.push_str(&c.to_string());
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn tool_call_completion(id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-up", "object": "chat.completion", "model": "stub-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": id, "type": "function",
                            "function": {"name": name, "arguments": arguments}}]},
            "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6}
    })
    .to_string()
}

fn start_upstream(script: Vec<String>, fallback: String) -> Upstream {
    start_upstream_with(script, fallback, true)
}

/// `answer_probe: false` leaves the startup probe to the script, which is how
/// a test gets a server that claims to call tools and does not.
fn start_upstream_with(script: Vec<String>, fallback: String, answer_probe: bool) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let queue = Arc::new(Mutex::new(VecDeque::from(script)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probes = Arc::new(Mutex::new(Vec::new()));

    let seen_for_thread = Arc::clone(&seen);
    let probes_for_thread = Arc::clone(&probes);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut length = 0usize;
            let mut path = String::new();
            let mut first = true;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if first {
                    path = line.split(' ').nth(1).unwrap_or("").to_string();
                    first = false;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);

            let mut wants_stream = false;
            let reply = if path.ends_with("/models") {
                r#"{"object":"list","data":[]}"#.to_string()
            } else {
                let text = String::from_utf8_lossy(&body).to_string();
                wants_stream = text.contains("\"stream\":true");
                // `ensemble serve` proves the manager can return a tool call
                // before it binds. That is a real request on the wire and it
                // has to be answered, but it is startup rather than a turn, so
                // it is recorded apart and never consumes a scripted reply.
                if answer_probe && text.contains("ensemble_probe") {
                    probes_for_thread.lock().unwrap().push(text);
                    tool_call_completion("p1", "ensemble_probe", r#"{"ok":"yes"}"#)
                } else {
                    seen_for_thread.lock().unwrap().push(text);
                    queue
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| fallback.clone())
                }
            };

            let (content_type, body_out) = if wants_stream {
                ("text/event-stream", as_sse(&reply))
            } else {
                ("application/json", reply)
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\n\r\n{}",
                content_type,
                body_out.len(),
                body_out
            );
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
    });

    Upstream { url, seen, probes }
}

// ---------------------------------------------------------------------------
// The server under test
// ---------------------------------------------------------------------------

fn config_yaml(url: &str) -> String {
    format!(
        r#"
version: 1
backends:
  local:
    type: openai-compatible-api
    base_url_env: ENSEMBLE_TEST_SERVE_URL
    base_url_default: "{url}"
    timeout_seconds: 20
manager:
  backend: local
  model: manager-model
  prompt: "Coordinates the team."
roles:
  planner:
    backend: local
    model: planner-model
    prompt: "Turns a goal into an ordered plan."
"#
    )
}

/// A running `ensemble serve`, stopped when it goes out of scope.
///
/// Its stderr is drained by a thread for the whole life of the process.
/// Reading only the banner and then dropping the pipe closes it, and the next
/// line the server logs kills it with EPIPE — which looks exactly like a
/// server that crashed on the first request.
struct Server {
    child: Child,
    base: String,
    log: Arc<Mutex<String>>,
}

fn spawn_serve(
    scratch: &Scratch,
    args: &[&str],
    env: &[(&str, &str)],
) -> (Child, Arc<Mutex<String>>) {
    let mut command = Command::new(BIN);
    command
        .arg("serve")
        .args(args)
        .current_dir(scratch.path())
        .env_remove("ENSEMBLE_TEST_SERVE_URL")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = command.spawn().expect("spawn ensemble serve");

    let stderr = child.stderr.take().expect("stderr");
    let log = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&log);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut g = sink.lock().unwrap();
            g.push_str(&line);
            g.push('\n');
        }
    });
    (child, log)
}

/// Wait for a line to appear in the log, and return the log so far.
fn wait_for(log: &Arc<Mutex<String>>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        {
            let g = log.lock().unwrap();
            if g.contains(needle) {
                return g.clone();
            }
            if Instant::now() > deadline {
                panic!("never saw {needle:?} in:\n{g}");
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn address_in(log: &str) -> String {
    log.lines()
        .find_map(|l| l.strip_prefix("ensemble: serving ensemble/manager on "))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("no address in:\n{log}"))
}

impl Server {
    fn start(scratch: &Scratch, extra: &[&str]) -> Server {
        let mut args = vec!["--port", "0", "--dir"];
        let dir = scratch.path().display().to_string();
        args.push(&dir);
        args.extend_from_slice(extra);
        let (child, log) = spawn_serve(scratch, &args, &[]);
        // The banner names the port it actually got, which is how --port 0
        // becomes usable from a test.
        let base = address_in(&wait_for(&log, "ensemble: serving ensemble/manager on "));
        Server { child, base, log }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// A minimal HTTP client, so the test does not test ureq
// ---------------------------------------------------------------------------

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.body))
    }
}

fn request(url: &str, method: &str, body: Option<&str>) -> Reply {
    let rest = url.strip_prefix("http://").expect("an http url");
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(authority).expect("connect to the server");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    let mut head = format!("{method} /{path} HTTP/1.1\r\nhost: {authority}\r\n");
    if let Some(b) = body {
        head.push_str("content-type: application/json\r\n");
        head.push_str(&format!("content-length: {}\r\n", b.len()));
    }
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).unwrap();
    }
    stream.flush().unwrap();

    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read the reply");
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in: {raw}"));
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");

    // De-chunk, if it is chunked. Short enough to do by hand and it keeps the
    // test honest about the framing the server actually wrote.
    let body = if raw
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };

    Reply { status, body }
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        s = &rest[n + 2..];
    }
    out
}

fn post(server: &Server, body: serde_json::Value) -> Reply {
    request(
        &server.url("/chat/completions"),
        "POST",
        Some(&body.to_string()),
    )
}

/// The `data:` payloads of an SSE body, and whether it terminated.
fn sse(body: &str) -> (Vec<serde_json::Value>, bool) {
    let mut events = Vec::new();
    let mut done = false;
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            done = true;
        } else {
            events.push(serde_json::from_str(payload).expect("each event is JSON"));
        }
    }
    (events, done)
}

fn read_tools() -> serde_json::Value {
    serde_json::json!([{"type": "function", "function": {
        "name": "read", "description": "Read a file.",
        "parameters": {"type": "object", "properties": {"filePath": {"type": "string"}}}}}])
}

/// A scratch directory with a config pointing at `upstream`.
fn project(name: &str, upstream: &Upstream) -> Scratch {
    let scratch = Scratch::new(name);
    scratch.write("ensemble.yaml", &config_yaml(&upstream.url));
    scratch
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn models_lists_the_manager_and_nothing_else() {
    let up = start_upstream(vec![], completion("unused"));
    let scratch = project("models", &up);
    let server = Server::start(&scratch, &[]);

    let reply = request(&server.url("/models"), "GET", None);
    assert_eq!(reply.status, 200);
    let v = reply.json();
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], "ensemble/manager");
    assert!(!reply.body.contains("planner"), "{}", reply.body);
}

#[test]
fn a_chat_completion_against_the_manager_returns_an_openai_shaped_body() {
    let up = start_upstream(vec![completion("Here is the answer.")], completion("x"));
    let scratch = project("plain", &up);
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "hello"}]}),
    );
    assert_eq!(reply.status, 200);
    let v = reply.json();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "Here is the answer.");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["total_tokens"], 10);

    // The manager was reached with its configured model, not with the id the
    // client typed.
    let sent: serde_json::Value = serde_json::from_str(&up.seen.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["model"], "manager-model");
}

#[test]
fn a_delegation_runs_inside_ensemble_and_the_client_sees_only_the_answer() {
    let up = start_upstream(
        vec![
            tool_call_completion(
                "d1",
                "ensemble_delegate",
                r#"{"role":"planner","task":"outline"}"#,
            ),
            completion("THE PLAN ITSELF"),
            completion("The planner proposed two steps."),
        ],
        completion("x"),
    );
    let scratch = project("delegate", &up);
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "plan it"}]}),
    );
    assert_eq!(reply.status, 200);
    let v = reply.json();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "The planner proposed two steps."
    );
    assert!(
        !reply.body.contains("ensemble_delegate"),
        "the delegation never reaches the client: {}",
        reply.body
    );

    let seen = up.seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "manager, planner, manager");
    // The middle call is the delegation, on the planner's own model.
    let planner: serde_json::Value = serde_json::from_str(&seen[1]).unwrap();
    assert_eq!(planner["model"], "planner-model");
    assert!(
        planner.get("tools").is_none(),
        "a delegate is offered no tools"
    );
    // And usage covers all three.
    assert_eq!(v["usage"]["total_tokens"], 30);
}

#[test]
fn the_manager_is_offered_the_harness_tools_and_the_delegation_tool() {
    let up = start_upstream(vec![completion("ok")], completion("x"));
    let scratch = project("offered", &up);
    let server = Server::start(&scratch, &[]);

    post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "go"}],
                           "tools": read_tools()}),
    );

    let sent: serde_json::Value = serde_json::from_str(&up.seen.lock().unwrap()[0]).unwrap();
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"ensemble_delegate"), "{names:?}");
    assert!(names.contains(&"read"), "{names:?}");
    assert_eq!(sent["tool_choice"], "auto");
}

#[test]
fn a_harness_tool_call_round_trips_through_two_requests_and_keeps_its_id() {
    let up = start_upstream(
        vec![
            tool_call_completion("call_r", "read", r#"{"filePath":"src/main.rs"}"#),
            completion("The file begins with a doc comment."),
        ],
        completion("x"),
    );
    let scratch = project("roundtrip", &up);
    let server = Server::start(&scratch, &[]);

    let first = post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "read it"}],
                           "tools": read_tools()}),
    );
    let v = first.json();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    let call = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_r");
    assert_eq!(
        call["function"]["arguments"],
        r#"{"filePath":"src/main.rs"}"#
    );

    // The harness runs the tool and posts the result, as a harness does.
    let second = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(), "messages": [
            {"role": "user", "content": "read it"},
            {"role": "assistant", "content": null, "tool_calls": [call]},
            {"role": "tool", "tool_call_id": "call_r", "content": "//! The ensemble binary."}]}),
    );
    assert_eq!(second.status, 200);
    assert_eq!(
        second.json()["choices"][0]["message"]["content"],
        "The file begins with a doc comment."
    );

    // The manager saw the harness's result, paired to the id it gave.
    let seen = up.seen.lock().unwrap();
    let last: serde_json::Value = serde_json::from_str(seen.last().unwrap()).unwrap();
    let tool_turn = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the result is in the transcript");
    assert_eq!(tool_turn["tool_call_id"], "call_r");
    assert_eq!(tool_turn["content"], "//! The ensemble binary.");
}

#[test]
fn an_internal_delegation_before_a_harness_tool_call_is_not_repeated_on_the_second_request() {
    let up = start_upstream(
        vec![
            // One reply, two calls: one ours, one the harness's.
            serde_json::json!({
                "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
                    "tool_calls": [
                        {"id": "d1", "type": "function", "function": {
                            "name": "ensemble_delegate",
                            "arguments": "{\"role\":\"planner\",\"task\":\"outline\"}"}},
                        {"id": "call_r", "type": "function", "function": {
                            "name": "read", "arguments": "{\"filePath\":\"a\"}"}}]},
                    "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 6}
            })
            .to_string(),
            completion("THE HIDDEN PLAN"),
            completion("Both pieces are in."),
        ],
        completion("x"),
    );
    let scratch = project("resume", &up);
    let server = Server::start(&scratch, &[]);

    let first = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(),
                           "messages": [{"role": "user", "content": "go"}]}),
    );
    let v = first.json();
    let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "only the harness's call is forwarded: {}",
        first.body
    );
    assert_eq!(calls[0]["id"], "call_r");

    let second = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(), "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": null, "tool_calls": calls},
            {"role": "tool", "tool_call_id": "call_r", "content": "file contents"}]}),
    );
    assert_eq!(second.status, 200);
    assert_eq!(
        second.json()["choices"][0]["message"]["content"],
        "Both pieces are in."
    );

    let seen = up.seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        3,
        "manager, planner, manager — the planner ran once"
    );
    let last: serde_json::Value = serde_json::from_str(seen.last().unwrap()).unwrap();
    let rendered = last.to_string();
    assert!(
        rendered.contains("THE HIDDEN PLAN"),
        "the hidden turn came back"
    );
    assert!(
        rendered.contains("file contents"),
        "and so did the harness's"
    );
}

#[test]
fn a_role_named_directly_is_delegated_instead_of_forwarded_to_the_harness() {
    let up = start_upstream(
        vec![
            tool_call_completion("d1", "planner", r#"{"task":"outline"}"#),
            completion("THE PLAN"),
            completion("The planner proposed two steps."),
        ],
        completion("x"),
    );
    let scratch = project("direct-role", &up);
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(),
                           "messages": [{"role": "user", "content": "plan it"}]}),
    );
    assert_eq!(reply.status, 200);
    let v = reply.json();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "The planner proposed two steps."
    );
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert!(
        v["choices"][0]["message"].get("tool_calls").is_none(),
        "the role call never reaches the harness: {}",
        reply.body
    );

    let seen = up.seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "manager, planner, manager");
    let planner: serde_json::Value = serde_json::from_str(&seen[1]).unwrap();
    assert_eq!(planner["model"], "planner-model");
    assert_eq!(
        planner["messages"].as_array().unwrap().last().unwrap()["content"],
        "outline"
    );
}

#[test]
fn an_internal_delegation_survives_into_the_next_prompt() {
    let up = start_upstream(
        vec![
            tool_call_completion(
                "d1",
                "ensemble_delegate",
                r#"{"role":"planner","task":"outline"}"#,
            ),
            completion("THE HIDDEN PLAN"),
            completion("Two steps."),
            completion("The planner's outline is still here."),
        ],
        completion("x"),
    );
    let scratch = project("next-prompt", &up);
    let server = Server::start(&scratch, &[]);

    let first = post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "plan it"}]}),
    );
    assert_eq!(
        first.json()["choices"][0]["message"]["content"],
        "Two steps."
    );

    // The harness posts the transcript it holds — which has the answer but
    // never the delegation — plus the next prompt.
    let second = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "messages": [
            {"role": "user", "content": "plan it"},
            {"role": "assistant", "content": "Two steps."},
            {"role": "user", "content": "what did the planner say?"}]}),
    );
    assert_eq!(second.status, 200);
    assert_eq!(
        second.json()["choices"][0]["message"]["content"],
        "The planner's outline is still here."
    );

    let seen = up.seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "manager, planner, manager, manager");
    let last: serde_json::Value = serde_json::from_str(seen.last().unwrap()).unwrap();
    assert!(
        last.to_string().contains("THE HIDDEN PLAN"),
        "the delegation from the previous prompt came back"
    );
}

#[test]
fn two_interleaved_conversations_do_not_leak_turns_into_each_other() {
    let mixed = |delegate_id: &str, read_id: &str, file: &str| {
        serde_json::json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
                "tool_calls": [
                    {"id": delegate_id, "type": "function", "function": {
                        "name": "ensemble_delegate",
                        "arguments": "{\"role\":\"planner\",\"task\":\"x\"}"}},
                    {"id": read_id, "type": "function", "function": {
                        "name": "read", "arguments": format!("{{\"filePath\":\"{file}\"}}")}}]},
                "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string()
    };
    let up = start_upstream(
        vec![
            mixed("dA", "call_a", "a"),
            completion("PLAN FOR A"),
            mixed("dB", "call_b", "b"),
            completion("PLAN FOR B"),
            completion("A continues"),
        ],
        completion("x"),
    );
    let scratch = project("isolation", &up);
    let server = Server::start(&scratch, &[]);

    let a = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(),
                           "messages": [{"role": "user", "content": "task A"}]}),
    );
    post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(),
                           "messages": [{"role": "user", "content": "task B"}]}),
    );

    let a_calls = a.json()["choices"][0]["message"]["tool_calls"].clone();
    let follow = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "tools": read_tools(), "messages": [
            {"role": "user", "content": "task A"},
            {"role": "assistant", "content": null, "tool_calls": a_calls},
            {"role": "tool", "tool_call_id": "call_a", "content": "a contents"}]}),
    );
    assert_eq!(follow.status, 200);
    assert_eq!(
        follow.json()["choices"][0]["message"]["content"],
        "A continues"
    );

    let seen = up.seen.lock().unwrap();
    let last = seen.last().unwrap();
    assert!(last.contains("PLAN FOR A"), "A's own hidden turn: {last}");
    assert!(!last.contains("PLAN FOR B"), "and nothing of B's: {last}");
    assert!(!last.contains("call_b"), "{last}");
}

#[test]
fn a_streaming_request_emits_sse_chunks_and_a_final_done() {
    let up = start_upstream(vec![completion("Streamed answer.")], completion("x"));
    let scratch = project("stream", &up);
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "stream": true,
                           "stream_options": {"include_usage": true},
                           "messages": [{"role": "user", "content": "hi"}]}),
    );
    assert_eq!(reply.status, 200);

    let (events, done) = sse(&reply.body);
    assert!(done, "terminated: {}", reply.body);
    assert!(reply.body.contains(": ensemble: working"), "{}", reply.body);
    assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
    let text: String = events
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Streamed answer.");
    assert!(
        events
            .iter()
            .any(|e| e["choices"][0]["finish_reason"] == "stop")
    );
    assert_eq!(events.last().unwrap()["usage"]["total_tokens"], 10);
}

#[test]
fn a_streaming_turn_with_a_slow_delegation_keeps_the_connection_alive() {
    // The upstream here is deliberately not slow — a ten-second heartbeat
    // would make this test ten seconds long. What is asserted is that the
    // first bytes reach the client before the turn finishes, which is the
    // property a client's idle timer actually depends on.
    let up = start_upstream(vec![completion("done")], completion("x"));
    let scratch = project("heartbeat", &up);
    let server = Server::start(&scratch, &[]);

    let started = Instant::now();
    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "stream": true,
                           "messages": [{"role": "user", "content": "hi"}]}),
    );
    assert!(
        reply.body.starts_with(": ensemble: working"),
        "{}",
        reply.body
    );
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn an_unknown_model_is_a_404_and_an_empty_messages_array_is_a_400() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("invalid", &up);
    let server = Server::start(&scratch, &[]);

    let unknown = post(
        &server,
        serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "x"}]}),
    );
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["error"]["code"], "model_not_found");

    let empty = post(
        &server,
        serde_json::json!({"model": "ensemble/manager", "messages": []}),
    );
    assert_eq!(empty.status, 400);
    assert_eq!(empty.json()["error"]["type"], "invalid_request_error");

    // Nothing was spent finding out.
    assert!(up.seen.lock().unwrap().is_empty());
}

#[test]
fn a_role_is_not_addressable_as_a_model() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("roleid", &up);
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/planner",
                           "messages": [{"role": "user", "content": "x"}]}),
    );
    assert_eq!(reply.status, 404);
    assert!(up.seen.lock().unwrap().is_empty());
}

#[test]
fn an_unknown_route_and_a_wrong_method_are_both_answered_in_the_openai_shape() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("routes", &up);
    let server = Server::start(&scratch, &[]);

    let missing = request(&server.url("/nope"), "GET", None);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["error"]["code"], "unknown_url");

    let wrong = request(&server.url("/chat/completions"), "GET", None);
    assert_eq!(wrong.status, 405);
}

#[test]
fn an_upstream_that_is_down_becomes_a_502_carrying_the_backends_reason() {
    let scratch = Scratch::new("down");
    // A port nothing is listening on.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", dead.local_addr().unwrap());
    drop(dead);
    scratch.write("ensemble.yaml", &config_yaml(&url));
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "hi"}]}),
    );
    assert_eq!(reply.status, 502);
    let v = reply.json();
    assert_eq!(v["error"]["code"], "upstream_error");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("local"),
        "names the backend: {}",
        reply.body
    );
}

#[test]
fn a_bearer_token_is_required_when_one_is_configured() {
    let up = start_upstream(vec![completion("ok")], completion("x"));
    let scratch = project("auth", &up);
    let dir = scratch.path().display().to_string();
    let (child, log) = spawn_serve(
        &scratch,
        &[
            "--port",
            "0",
            "--api-key-env",
            "ENSEMBLE_TEST_TOKEN",
            "--dir",
            &dir,
        ],
        &[("ENSEMBLE_TEST_TOKEN", "sk-test-token")],
    );
    let banner = wait_for(&log, "authentication:");
    let server = Server {
        child,
        base: address_in(&banner),
        log,
    };

    // The secret is never printed, only the fact that one is required.
    assert!(banner.contains("bearer token required"), "{banner}");
    assert!(!banner.contains("sk-test-token"), "{banner}");

    let refused = request(&server.url("/models"), "GET", None);
    assert_eq!(refused.status, 401);
    assert!(!refused.body.contains("sk-test-token"));

    // With the token it works. The tiny client above sends no headers, so do
    // this one by hand.
    let rest = server.base.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap();
    let mut stream = TcpStream::connect(authority).unwrap();
    write!(
        stream,
        "GET /{path}/models HTTP/1.1\r\nhost: {authority}\r\n\
         authorization: Bearer sk-test-token\r\nconnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200 OK"), "{raw}");
}

#[test]
fn serve_refuses_to_start_when_the_manager_backend_cannot_call_tools() {
    let scratch = Scratch::new("claude-manager");
    scratch.write(
        "ensemble.yaml",
        r#"
version: 1
backends:
  claude:
    type: claude-code-cli
manager:
  backend: claude
  model: claude-sonnet
"#,
    );
    let out = Command::new(BIN)
        .args(["serve", "--port", "0"])
        .current_dir(scratch.path())
        .output()
        .expect("run");
    assert!(!out.status.success(), "it must not bind");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("claude-code-cli"), "{err}");
    assert!(err.contains("openai-compatible-api"), "{err}");
    assert!(err.contains("ensemble mcp"), "{err}");
}

#[test]
fn serve_proves_the_manager_can_call_tools_before_it_binds() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("probe", &up);
    let _server = Server::start(&scratch, &[]);

    // One probe, on the wire, before the banner — and it forces a call rather
    // than leaving the choice to the model, because a model that chose to
    // answer and a server that dropped the call look the same otherwise.
    let probes = up.probes.lock().unwrap();
    assert_eq!(probes.len(), 1, "{probes:?}");
    let sent: serde_json::Value = serde_json::from_str(&probes[0]).unwrap();
    assert_eq!(sent["tool_choice"], "required");
    assert_eq!(sent["tools"][0]["function"]["name"], "ensemble_probe");
    assert_eq!(sent["model"], "manager-model");
    // Startup is not a turn: nothing the probe did counts as one.
    assert!(up.seen.lock().unwrap().is_empty());
}

#[test]
fn serve_refuses_to_start_when_the_server_hands_tool_calls_back_as_text() {
    // A server with no tool-call parser for its model: 200, and the call in
    // `content`. Declared capable by its type, incapable in fact.
    let up = start_upstream_with(
        vec![],
        completion("ensemble_probe(ok='yes')"),
        false, // let the fallback answer the probe, in prose
    );
    let scratch = project("unparsed", &up);
    let out = Command::new(BIN)
        .args(["serve", "--port", "0"])
        .current_dir(scratch.path())
        .env_remove("ENSEMBLE_TEST_SERVE_URL")
        .output()
        .expect("run");

    assert!(!out.status.success(), "it must not bind");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("manager-model"), "{err}");
    assert!(err.contains("--jinja"), "names the llama-server fix: {err}");
    assert!(
        err.contains("--tool-call-parser"),
        "names the vLLM fix: {err}"
    );
    assert!(err.contains(".Tools"), "names the Ollama fix: {err}");
    assert!(
        err.contains("ensemble_probe(ok="),
        "quotes what came back: {err}"
    );
}

#[test]
fn a_delegation_the_server_returned_as_text_is_diagnosed_on_stderr() {
    // The probe passed, so this server can parse *some* tool call — but the
    // manager's reply arrives as prose, which is what a harness sees as "it
    // answered in one step and never delegated". Ensemble cannot tell that
    // from a finished answer, so it does not change the turn; it says why.
    let up = start_upstream(
        vec![completion(
            "Let me look into that.\nensemble_delegate(role='planner', task='outline it')",
        )],
        completion("x"),
    );
    let scratch = project("written-out", &up);
    let server = Server::start(&scratch, &[]);

    let reply = post(
        &server,
        serde_json::json!({"model": "ensemble/manager",
                           "messages": [{"role": "user", "content": "plan it"}]}),
    );
    assert_eq!(
        reply.status, 200,
        "the turn still completes: {}",
        reply.body
    );

    let log = wait_for(&server.log, "nothing was delegated");
    assert!(log.contains("ensemble_delegate"), "{log}");
    assert!(log.contains("--jinja"), "{log}");
    assert!(log.contains("--tool-call-parser"), "{log}");
    // Stderr only. The harness must not learn any of this.
    assert!(!reply.body.contains("--jinja"), "{}", reply.body);
}

#[test]
fn serve_refuses_a_public_bind_without_a_token() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("public", &up);
    let out = Command::new(BIN)
        .args(["serve", "--host", "0.0.0.0", "--port", "0"])
        .current_dir(scratch.path())
        .env_remove("ENSEMBLE_TEST_SERVE_URL")
        .output()
        .expect("run");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--api-key-env"), "{err}");
}

#[test]
fn check_reports_whether_serve_is_available() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("check", &up);
    let out = Command::new(BIN)
        .arg("check")
        .current_dir(scratch.path())
        .env_remove("ENSEMBLE_TEST_SERVE_URL")
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("serve: available"), "{text}");
    assert!(text.contains("ensemble/manager"), "{text}");

    let claude = Scratch::new("check-claude");
    claude.write(
        "ensemble.yaml",
        "version: 1\nbackends:\n  claude:\n    type: claude-code-cli\nmanager:\n  backend: claude\n  model: claude-sonnet\n",
    );
    let out = Command::new(BIN)
        .arg("check")
        .current_dir(claude.path())
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("serve: unavailable"), "{text}");
    assert!(text.contains("ensemble mcp"), "{text}");
}

#[test]
fn the_banner_prints_a_ready_to_paste_opencode_block() {
    let up = start_upstream(vec![], completion("x"));
    let scratch = project("banner", &up);
    let server = Server::start(&scratch, &[]);
    let banner = wait_for(&server.log, "baseURL");

    assert!(banner.contains("@ai-sdk/openai-compatible"), "{banner}");
    assert!(banner.contains("\"manager\""), "{banner}");
    assert!(
        banner.contains("manager -> local / manager-model"),
        "{banner}"
    );
    assert!(banner.contains("delegates to: planner"), "{banner}");
}

#[test]
fn two_requests_can_share_one_connection() {
    let up = start_upstream(vec![completion("one"), completion("two")], completion("x"));
    let scratch = project("keepalive", &up);
    let server = Server::start(&scratch, &[]);

    let rest = server.base.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap();
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    for _ in 0..2 {
        write!(
            stream,
            "GET /{path}/models HTTP/1.1\r\nhost: {authority}\r\n\r\n"
        )
        .unwrap();
    }
    stream.flush().unwrap();

    // Two complete responses arrive on the one socket. Counted over the whole
    // byte stream rather than line by line: a JSON body carries no trailing
    // newline, so the next status line shares a line with it.
    let mut raw = String::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(10);
    while raw.matches("HTTP/1.1 200 OK").count() < 2 && Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => break,
        }
    }
    assert_eq!(
        raw.matches("HTTP/1.1 200 OK").count(),
        2,
        "the connection was reused: {raw}"
    );
    assert!(raw.contains("connection: keep-alive"), "{raw}");
}
