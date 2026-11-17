use super::*;
use ensemble_config as config;
use ensemble_core::workspace::testing::{TempDir, executable};
use ensemble_knowledge::Memory;
use std::collections::BTreeMap;

/// What a session's hub brings in these tests: its knowledge.
#[derive(Clone, Copy, Default)]
struct Hub<'a> {
    knowledge: Option<&'a dyn KnowledgeAccess>,
}

/// A session of `e` on `input` with its folder's worker beside it, as a node
/// runs one, except that the queue closes when `input` ends.
fn serve(
    e: &Ensemble,
    input: impl AsyncBufRead + Unpin + Send,
    output: impl AsyncWrite + Unpin + Send,
) -> std::io::Result<()> {
    serve_with(e, Hub::default(), input, output)
}

/// [`serve`], with what a hub adds.
fn serve_with(
    e: &Ensemble,
    hub: Hub<'_>,
    input: impl AsyncBufRead + Unpin + Send,
    output: impl AsyncWrite + Unpin + Send,
) -> std::io::Result<()> {
    let state = e.state_dir()?;
    // As on a node: only the worker that holds the folder runs its tasks.
    let lock = ensemble_tasks::lock::acquire(&state);
    let mut store = Store::new(&state);
    if let Some(usage) = e.usage_dir() {
        store = store.with_usage_dir(usage);
    }
    let queue = queue_for(e, Some(store), hub.knowledge);
    runtime::block_on(async {
        let session = serve_session(e, hub.knowledge, &queue, input, output, true, None);
        tokio::join!(session, queue.work(lock.is_ok())).0
    })
}

/// One OpenCode JSON event per line, as `printf` commands.
fn emit(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| {
            format!(
                "printf '%s\\n' '{}'\n",
                e.to_string().replace('\'', "'\\''")
            )
        })
        .collect()
}

fn step(events: Vec<Value>) -> Vec<Value> {
    let mut v = vec![json!({"type": "step_start", "part": {}})];
    v.extend(events);
    v.push(json!({"type": "step_finish", "part": {"tokens": {"input": 5, "output": 2}}}));
    v
}

fn text(t: &str) -> Value {
    json!({"type": "text", "part": {"text": t}})
}

/// A stand-in for `opencode` that answers `--version` and otherwise runs
/// `body` in the project.
fn cli(dir: &std::path::Path, name: &str, body: &str) -> String {
    executable(
        dir,
        name,
        &format!("[ \"$1\" = --version ] && exit 0\n{body}"),
    )
    .display()
    .to_string()
}

/// A team of OpenCode agents, each running its own stand-in script, in a
/// fresh project. `scripts` is (agent, tools, body).
fn team(scripts: &[(&str, &str, &str)]) -> (Ensemble, TempDir) {
    team_limited(scripts, 4)
}

/// [`team`], each agent running at most `max_concurrent` at once.
fn team_limited(scripts: &[(&str, &str, &str)], max_concurrent: u32) -> (Ensemble, TempDir) {
    team_with(scripts, max_concurrent, "shared")
}

/// [`team_limited`], with each agent's tasks run as `isolation` says.
fn team_with(
    scripts: &[(&str, &str, &str)],
    max_concurrent: u32,
    isolation: &str,
) -> (Ensemble, TempDir) {
    team_changed(scripts, max_concurrent, isolation, |_| {})
}

/// [`team_with`], with its config changed by `change` before use.
fn team_changed(
    scripts: &[(&str, &str, &str)],
    max_concurrent: u32,
    isolation: &str,
    change: impl FnOnce(&mut config::Config),
) -> (Ensemble, TempDir) {
    let tmp = TempDir::new();
    let bin = tmp.0.join("bin");
    let project = tmp.0.join("project");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.txt"), "hello").unwrap();
    let mut yaml = String::from(
        "version: 1\nllm_backends: {}\ngates:\n  done-file: { command: \"test -e done.txt\", timeout_seconds: 10 }\nagents:\n",
    );
    for (name, tools, body) in scripts {
        let command = cli(&bin, name, body);
        yaml.push_str(&format!(
                "  {name}:\n    prompt: \"The {name}. Second line.\"\n    max_seconds: 1800\n    max_concurrent: {max_concurrent}\n    isolation: {isolation}\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      model: {{ type: provider, name: opencode/m-{name} }}\n      tools: [{tools}]\n      system_prompt: append\n      max_steps: 30\n      command: \"{command}\"\n"
            ));
    }
    let yaml = config::migrate::to_v3(&yaml).unwrap();
    let mut cfg =
        config::parse(&yaml, std::path::Path::new("config.yaml"), &BTreeMap::new()).unwrap();
    change(&mut cfg);
    let state = project.join(".ensemble");
    (Ensemble::new(cfg, Some(project)).with_state(state), tmp)
}

fn working() -> (Ensemble, TempDir) {
    team(&[
        ("coder", "read, edit", &emit(&step(vec![text("work done")]))),
        ("reviewer", "read", &emit(&step(vec![text("reviewed")]))),
    ])
}

fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

fn call(e: &Ensemble, name: &str, args: Value) -> Value {
    handle(
        e,
        &rpc("tools/call", json!({"name": name, "arguments": args})),
    )
    .unwrap()
}

fn text_of(response: &Value) -> &str {
    response["result"]["content"][0]["text"].as_str().unwrap()
}

/// `scout` reads a.txt, then answers.
fn scouting() -> (Ensemble, TempDir) {
    let events = step(vec![
        json!({"type": "tool_use", "part": {"tool": "read",
                "state": {"status": "completed", "input": {"filePath": "a.txt"}}}}),
        text("it says hello"),
    ]);
    team(&[("scout", "read", &emit(&events))])
}

#[test]
fn agents_are_labelled_and_a_delegation_reports_what_it_did() {
    let (e, _tmp) = team(&[("coder", "read, edit", "true"), ("scout", "read", "true")]);
    let listing = call(&e, "agents", json!({}));
    let listing = text_of(&listing);
    assert!(
        listing.contains("coder [can change files]: The coder. Second line."),
        "{listing}"
    );
    assert!(listing.contains("scout [read-only]"), "{listing}");

    let (e, _tmp) = scouting();
    let r = call(&e, "delegate", json!({"agent": "scout", "task": "look"}));
    let text = text_of(&r);
    assert!(text.starts_with("it says hello"), "{text}");
    assert!(
        text.ends_with("[scout: 1 step(s), 1 tool call(s)]"),
        "{text}"
    );
}

#[test]
fn a_delegation_lists_the_files_the_agent_changed() {
    let body = format!("echo bye > a.txt\n{}", emit(&step(vec![text("fixed")])));
    let (e, _tmp) = team(&[("fixer", "read, edit", &body)]);
    let r = call(&e, "delegate", json!({"agent": "fixer", "task": "fix"}));
    let text = text_of(&r);
    assert!(
        text.ends_with("tool call(s)]\n[files changed: a.txt (modified)]"),
        "{text}"
    );
}

fn served(e: &Ensemble, input: &str) -> Vec<Value> {
    let mut out = Vec::new();
    serve(e, std::io::Cursor::new(input.to_string()), &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const SCOUT_CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"delegate","arguments":{"agent":"scout","task":"look"},"_meta":{"progressToken":"tok"}}}"#;

#[test]
fn a_delegation_streams_progress_and_log_messages_before_its_reply() {
    let (e, _tmp) = scouting();
    let out = served(&e, &format!("{SCOUT_CALL}\n"));

    let (reply, before) = out.split_last().unwrap();
    assert_eq!(reply["id"], 7, "the reply comes last");

    let progress: Vec<&Value> = before
        .iter()
        .filter(|m| m["method"] == "notifications/progress")
        .collect();
    assert_eq!(progress.len(), 3, "{before:?}");
    assert!(
        progress
            .iter()
            .all(|m| m["params"]["progressToken"] == "tok")
    );
    let counters: Vec<u64> = progress
        .iter()
        .map(|m| m["params"]["progress"].as_u64().unwrap())
        .collect();
    assert_eq!(counters, [1, 2, 3], "the counter rises");
    assert_eq!(
        progress[1]["params"]["message"],
        "scout · step 1 · read(a.txt)"
    );

    let logs: Vec<&Value> = before
        .iter()
        .filter(|m| m["method"] == "notifications/message")
        .collect();
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[2]["params"]["data"], "scout · done");
    assert_eq!(logs[2]["params"]["level"], "info");
}

#[test]
fn without_a_client_token_progress_is_keyed_by_the_request_id() {
    let (e, _tmp) = scouting();
    let call = SCOUT_CALL.replace(r#","_meta":{"progressToken":"tok"}"#, "");
    let out = served(&e, &format!("{call}\n"));
    let first = out
        .iter()
        .find(|m| m["method"] == "notifications/progress")
        .unwrap();
    assert_eq!(first["params"]["progressToken"], 7);
}

#[test]
fn set_level_above_info_silences_log_messages_but_not_progress() {
    let (e, _tmp) = scouting();
    let input = format!(
        "{}\n{SCOUT_CALL}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"logging/setLevel","params":{"level":"warning"}}"#
    );
    let out = served(&e, &input);
    assert_eq!(out[0]["id"], 1, "setLevel is answered");
    assert!(out.iter().all(|m| m["method"] != "notifications/message"));
    assert!(out.iter().any(|m| m["method"] == "notifications/progress"));
}

#[test]
fn delegations_run_at_the_same_time() {
    // Each run leaves a mark and waits up to five seconds for a second
    // one. Runs handled one at a time can never meet.
    let meet = r#"touch ".mark.$$"
i=0
while [ "$(ls -a | grep -c '^\.mark\.')" -lt 2 ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
if [ $i -lt 50 ]; then t=met; else t=alone; fi
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;
    let (e, _tmp) = team(&[("coder", "read", meet)]);
    let call = |id: u32| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"delegate","arguments":{{"agent":"coder","task":"t"}}}}}}"#
        )
    };
    let out = served(&e, &format!("{}\n{}\n", call(1), call(2)));
    let mut replies: Vec<(u64, String)> = out
        .iter()
        .filter(|m| m.get("id").is_some())
        .map(|m| {
            let text = m["result"]["content"][0]["text"].as_str().unwrap();
            (
                m["id"].as_u64().unwrap(),
                text.lines().next().unwrap().to_string(),
            )
        })
        .collect();
    replies.sort();
    assert_eq!(replies, vec![(1, "met".into()), (2, "met".into())]);
}

#[test]
fn a_cancelled_delegation_stops_and_gets_no_reply() {
    let body = format!(
        "{}sleep 30\n",
        emit(&[json!({"type": "step_start", "part": {}})])
    );
    let (e, _tmp) = team(&[("scout", "read", &body)]);
    let input = format!(
        "{SCOUT_CALL}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
        r#"{"jsonrpc":"2.0","id":8,"method":"ping"}"#
    );
    let started = std::time::Instant::now();
    let out = served(&e, &input);
    assert!(out.iter().all(|m| m["id"] != 7), "no reply: {out:?}");
    assert!(out.iter().any(|m| m["id"] == 8), "ping answered: {out:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the agent was stopped, not waited for"
    );
}

#[test]
fn initialize_advertises_logging() {
    let (e, _tmp) = working();
    let r = handle(&e, &rpc("initialize", json!({}))).unwrap();
    assert!(r["result"]["capabilities"]["logging"].is_object());
}

#[test]
fn initialize_reports_the_server_and_echoes_the_clients_protocol_version() {
    let (e, _tmp) = working();
    let r = handle(
        &e,
        &rpc("initialize", json!({"protocolVersion": "2024-11-05"})),
    )
    .unwrap();
    assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(r["result"]["serverInfo"]["name"], "ensemble");
    assert!(r["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn initialize_falls_back_to_our_version_when_the_client_sends_none() {
    let (e, _tmp) = working();
    let r = handle(&e, &rpc("initialize", json!({}))).unwrap();
    assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
}

#[test]
fn tools_list_advertises_the_tools_with_the_real_agent_names() {
    let (e, _tmp) = working();
    let r = handle(&e, &rpc("tools/list", json!({}))).unwrap();
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
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

    let enum_ = &tools[0]["inputSchema"]["properties"]["agent"]["enum"];
    let agents: Vec<&str> = enum_
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(agents, vec!["coder", "reviewer"]);
}

#[test]
fn no_tool_accepts_a_model_or_a_backend_parameter() {
    // The indirection is the product. A `model` parameter would undo it at
    // the first opportunity, so its absence is asserted rather than assumed.
    let (e, _tmp) = working();
    let r = handle(&e, &rpc("tools/list", json!({}))).unwrap();
    let schema = r["result"]["tools"].to_string();
    assert!(!schema.contains("\"model\""), "{schema}");
    assert!(!schema.contains("\"backend\""), "{schema}");
}

#[test]
fn delegate_returns_the_work_of_the_agent_it_names() {
    let (e, _tmp) = working();
    let r = call(
        &e,
        "delegate",
        json!({"agent": "coder", "task": "write a parser"}),
    );
    assert!(text_of(&r).starts_with("work done"), "{}", text_of(&r));
    assert_eq!(r["result"]["isError"], false);

    let r = call(
        &e,
        "delegate",
        json!({"agent": "reviewer", "task": "review it"}),
    );
    assert!(text_of(&r).starts_with("reviewed"), "{}", text_of(&r));
}

#[test]
fn delegate_passes_the_task_and_its_context_to_the_agent() {
    // The prompt is the CLI's last argument.
    let body = format!(
        "for last; do :; done\nprintf '%s' \"$last\" > prompt.txt\n{}",
        emit(&step(vec![text("ok")]))
    );
    let (e, tmp) = team(&[("coder", "read", &body)]);
    call(
        &e,
        "delegate",
        json!({"agent": "coder", "task": "fix it", "context": "the test fails on line 9"}),
    );
    let prompt = std::fs::read_to_string(tmp.0.join("project/prompt.txt")).unwrap();
    assert!(
        prompt.starts_with("Context:\nthe test fails on line 9\n\nfix it\n\n"),
        "{prompt}"
    );
    assert!(
        prompt.contains("NEEDS INPUT: <your question>"),
        "a delegation may ask: {prompt}"
    );
}

#[test]
fn a_failed_delegation_is_an_is_error_result_not_a_jsonrpc_error() {
    // The model must be able to read the reason. A JSON-RPC error is
    // handled by the harness and never reaches it.
    let (e, _tmp) = team(&[("coder", "read", "echo 'provider exploded' >&2\nexit 3\n")]);
    let r = call(&e, "delegate", json!({"agent": "coder", "task": "t"}));
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["result"]["isError"], true);
    let text = text_of(&r);
    assert!(
        text.contains("(agent 'coder')") && text.contains("provider exploded"),
        "{text}"
    );
    assert!(
        text.contains("fail the same way"),
        "says whether to retry: {text}"
    );
}

#[test]
fn an_unknown_agent_tells_the_model_not_to_loop() {
    let (e, _tmp) = working();
    let r = call(&e, "delegate", json!({"agent": "architect", "task": "t"}));
    assert_eq!(r["result"]["isError"], true);
    let text = text_of(&r);
    assert!(text.contains("architect"), "{text}");
    assert!(text.contains("coder"), "lists the real agents: {text}");
    assert!(text.contains("fail the same way"), "{text}");
}

#[test]
fn missing_or_empty_arguments_are_refused_with_a_usable_message() {
    let (e, _tmp) = working();
    for args in [
        json!({"task": "t"}),
        json!({"agent": "coder"}),
        json!({"agent": "coder", "task": "   "}),
    ] {
        let r = call(&e, "delegate", args.clone());
        assert_eq!(r["result"]["isError"], true, "{args}");
    }
}

#[test]
fn agents_lists_the_team_without_naming_models() {
    let (e, _tmp) = working();
    let r = call(&e, "agents", json!({}));
    let text = text_of(&r);
    assert!(
        text.contains("coder") && text.contains("reviewer"),
        "{text}"
    );
    assert!(text.contains("The coder. Second line."), "{text}");
    assert!(!text.contains("m-coder"), "must not leak the model: {text}");
}

#[test]
fn health_reports_each_cli_and_flags_a_broken_one() {
    let (e, _tmp) = working();
    let good = call(&e, "health", json!({}));
    assert_eq!(good["result"]["isError"], false, "{}", text_of(&good));
    assert!(text_of(&good).contains(": ok"), "{}", text_of(&good));

    let cfg = config::parse(
            &config::migrate::to_v3("version: 1\nllm_backends: {}\ngates: {}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 4\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter: { type: opencode, model: { type: provider, name: a/b }, tools: [read], system_prompt: append, max_steps: 3, command: /nonexistent/opencode }\n").unwrap(),
            std::path::Path::new("config.yaml"),
            &BTreeMap::new(),
        )
        .unwrap();
    let bad = call(&Ensemble::new(cfg, None), "health", json!({}));
    assert_eq!(bad["result"]["isError"], true);
    assert!(
        text_of(&bad).contains("opencode (/nonexistent/opencode): FAILED"),
        "{}",
        text_of(&bad)
    );
}

#[test]
fn an_unknown_tool_names_the_ones_that_exist() {
    let (e, _tmp) = working();
    let r = call(&e, "ensemble_do_everything", json!({}));
    assert_eq!(r["result"]["isError"], true);
    assert!(text_of(&r).contains("delegate"), "{}", text_of(&r));
}

#[test]
fn notifications_are_never_answered() {
    // Answering one is a protocol violation some clients treat as fatal.
    let (e, _tmp) = working();
    assert!(
        handle(
            &e,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .is_none()
    );
    assert!(
        handle(
            &e,
            &json!({"jsonrpc":"2.0","method":"notifications/cancelled"})
        )
        .is_none()
    );
}

#[test]
fn an_unknown_method_with_an_id_gets_method_not_found() {
    let (e, _tmp) = working();
    let r = handle(&e, &rpc("resources/list", json!({}))).unwrap();
    assert_eq!(r["error"]["code"], -32601);
}

#[test]
fn ping_is_answered_so_liveness_checks_pass() {
    let (e, _tmp) = working();
    let r = handle(&e, &rpc("ping", json!({}))).unwrap();
    assert!(r["result"].is_object());
}

#[test]
fn the_serve_loop_writes_one_json_object_per_line_and_skips_notifications() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
    );
    let (e, _tmp) = working();
    let mut out = Vec::new();
    serve(&e, std::io::Cursor::new(input), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "one reply each for initialize and tools/list: {text}"
    );
    for line in lines {
        let v: Value = serde_json::from_str(line).expect("each line is one JSON object");
        assert_eq!(v["jsonrpc"], "2.0");
    }
}

#[test]
fn a_malformed_line_is_a_parse_error_and_does_not_end_the_session() {
    let input = "{not json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
    let (e, _tmp) = working();
    let mut out = Vec::new();
    serve(&e, std::io::Cursor::new(input), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["error"]["code"], -32700);
    // And the session carried on.
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["id"], 2);
}

/// Input that arrives line by line while the server runs, ending when the
/// sender is dropped.
struct Feed {
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    pending: Vec<u8>,
}

impl tokio::io::AsyncRead for Feed {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        if self.pending.is_empty() {
            match self.lines.poll_recv(cx) {
                Poll::Ready(Some(line)) => self.pending = line.into_bytes(),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = out.remaining().min(self.pending.len());
        out.put_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Poll::Ready(Ok(()))
    }
}

/// Output the test can read while the server is still writing it.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for Shared {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl Shared {
    /// The text of the reply to request `id`, once it is written.
    fn reply_text(&self, id: u64) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let out = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
            // Only finished lines: the server may be halfway through writing
            // the last one.
            for line in out.split_inclusive('\n').filter(|l| l.ends_with('\n')) {
                let m: Value = serde_json::from_str(line).unwrap();
                if m["id"] == id {
                    return m["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .to_string();
                }
            }
            assert!(std::time::Instant::now() < deadline, "no reply to {id}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

fn tool_call(id: u64, name: &str, args: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                   "params":{"name":name,"arguments":args}})
    )
}

#[test]
fn a_background_delegation_answers_at_once_and_is_followed_with_status_and_wait() {
    let body = format!(
        "{}sleep 1\n{}",
        emit(&[json!({"type": "step_start", "part": {}})]),
        emit(&step(vec![text("late answer")]))
    );
    let (e, _tmp) = team(&[("scout", "read", &body)]);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let out = Shared::default();
    std::thread::scope(|s| {
        let server = s.spawn(|| {
            let input = tokio::io::BufReader::new(Feed {
                lines: rx,
                pending: Vec::new(),
            });
            serve(&e, input, out.clone()).unwrap();
        });

        let started = std::time::Instant::now();
        tx.send(tool_call(
            1,
            "delegate",
            json!({"agent": "scout", "task": "look", "background": true}),
        ))
        .unwrap();
        let first = out.reply_text(1);
        assert!(first.starts_with("adhoc-1 started"), "{first}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(900),
            "the reply did not wait for the agent"
        );

        tx.send(tool_call(2, "status", json!({}))).unwrap();
        let status = out.reply_text(2);
        assert!(status.contains("adhoc-1 · scout · running"), "{status}");

        tx.send(tool_call(
            3,
            "wait",
            json!({"task": "adhoc-1", "timeout_seconds": 20}),
        ))
        .unwrap();
        let reply = out.reply_text(3);
        assert!(reply.starts_with("late answer"), "{reply}");
        assert!(reply.contains("[scout: 2 step(s)"), "{reply}");

        tx.send(tool_call(4, "status", json!({"task": "adhoc-1"})))
            .unwrap();
        assert!(out.reply_text(4).contains("· done ·"));
        drop(tx);
        server.join().unwrap();
    });
}

#[test]
fn a_background_job_is_stopped_when_the_caller_goes_away() {
    let body = format!(
        "{}sleep 30\n",
        emit(&[json!({"type": "step_start", "part": {}})])
    );
    let (e, _tmp) = team(&[("scout", "read", &body)]);
    let input = tool_call(
        1,
        "delegate",
        json!({"agent": "scout", "task": "look", "background": true}),
    );
    let started = std::time::Instant::now();
    let out = served(&e, &input);
    assert!(
        out[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("adhoc-1 started")
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the agent was stopped, not waited for"
    );
}

#[test]
fn a_cancelled_background_job_is_reported_as_cancelled() {
    let body = format!(
        "{}sleep 30\n",
        emit(&[json!({"type": "step_start", "part": {}})])
    );
    let (e, _tmp) = team(&[("scout", "read", &body)]);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let out = Shared::default();
    std::thread::scope(|s| {
        let server = s.spawn(|| {
            let input = tokio::io::BufReader::new(Feed {
                lines: rx,
                pending: Vec::new(),
            });
            serve(&e, input, out.clone()).unwrap();
        });
        tx.send(tool_call(
            1,
            "delegate",
            json!({"agent": "scout", "task": "look", "background": true}),
        ))
        .unwrap();
        out.reply_text(1);
        tx.send(tool_call(2, "cancel", json!({"task": "adhoc-1"})))
            .unwrap();
        assert!(out.reply_text(2).contains("being cancelled"));
        tx.send(tool_call(
            3,
            "wait",
            json!({"task": "adhoc-1", "timeout_seconds": 20}),
        ))
        .unwrap();
        assert!(out.reply_text(3).contains("was cancelled"));
        drop(tx);
        server.join().unwrap();
    });
}

#[test]
fn a_background_call_for_an_unknown_agent_fails_at_once_without_a_job() {
    let (e, _tmp) = working();
    let r = call(
        &e,
        "delegate",
        json!({"agent": "architect", "task": "t", "background": true}),
    );
    assert_eq!(r["result"]["isError"], true);
    assert!(text_of(&r).contains("architect"), "{}", text_of(&r));
    let r = call(&e, "status", json!({}));
    assert_eq!(text_of(&r), "No tasks.");
}

#[test]
fn waiting_for_a_job_that_does_not_exist_is_a_readable_error() {
    let (e, _tmp) = working();
    let r = call(&e, "wait", json!({"task": "9"}));
    assert_eq!(r["result"]["isError"], true);
    assert!(text_of(&r).contains("no task '9'"), "{}", text_of(&r));
    let r = call(&e, "wait", json!({}));
    assert_eq!(r["result"]["isError"], true);
}
// -----------------------------------------------------------------------
// Tasks
// -----------------------------------------------------------------------

/// Run a session on `e` while `script` talks to it; the session ends, as
/// when the caller goes away, when `script` returns.
fn session(e: &Ensemble, script: impl FnOnce(&mut dyn FnMut(&str, Value) -> String)) {
    session_with(e, Hub::default(), script)
}

/// [`session`], with what a hub adds.
fn session_with(
    e: &Ensemble,
    hub: Hub<'_>,
    script: impl FnOnce(&mut dyn FnMut(&str, Value) -> String),
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let out = Shared::default();
    std::thread::scope(|s| {
        let output = out.clone();
        let server = s.spawn(move || {
            let input = tokio::io::BufReader::new(Feed {
                lines: rx,
                pending: Vec::new(),
            });
            serve_with(e, hub, input, output).unwrap();
        });
        let mut id = 0;
        script(&mut |name, args| {
            id += 1;
            tx.send(tool_call(id, name, args)).unwrap();
            out.reply_text(id)
        });
        drop(tx);
        server.join().unwrap();
    });
}

/// Ask `status` until its reply contains `wanted`.
fn until(ask: &mut dyn FnMut(&str, Value) -> String, args: Value, wanted: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = ask("status", args.clone());
        if status.contains(wanted) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never '{wanted}': {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn project(tmp: &TempDir) -> std::path::PathBuf {
    tmp.0.join("project")
}

fn tasks(tmp: &TempDir) -> std::path::PathBuf {
    project(tmp).join(".ensemble/tasks")
}

fn write_task(tmp: &TempDir, id: &str, agent: &str, text: &str) {
    let dir = tasks(tmp).join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("task.md"),
        format!("---\ntitle: Task {id}\nagent: {agent}\n---\n{text}\n"),
    )
    .unwrap();
}

fn store(tmp: &TempDir) -> Store {
    Store::new(&project(tmp).join(".ensemble"))
}

/// Answers `alone`, or `overlap` when another run of it is going on in the
/// project at the same time.
const BUSY: &str = r#"if mkdir .busy 2>/dev/null; then t=alone; sleep 0.6; rmdir .busy; else t=overlap; sleep 0.6; fi
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;

fn sleeper() -> String {
    format!(
        "{}sleep 30\n",
        emit(&[json!({"type": "step_start", "part": {}})])
    )
}

#[test]
fn a_task_written_by_hand_is_run_and_its_result_written_once() {
    let (e, tmp) = team(&[("scout", "read", &emit(&step(vec![text("found it")])))]);
    write_task(&tmp, "0001-look", "scout", "Find the parser.");
    std::fs::create_dir_all(tasks(&tmp).join("0002-bad")).unwrap();
    std::fs::write(tasks(&tmp).join("0002-bad/task.md"), "no front matter").unwrap();
    session(&e, |ask| {
        let result = ask("wait", json!({"task": "0001-look", "timeout_seconds": 20}));
        assert!(
            result.starts_with("---\nstatus: done\nagent: \"scout\"\n"),
            "{result}"
        );
        assert!(
            result.contains("found it") && result.contains("[scout: 1 step(s)"),
            "{result}"
        );
        let status = ask("status", json!({}));
        assert!(
            status.contains("0001-look · scout · done · Task 0001-look"),
            "{status}"
        );
        assert!(status.contains("0002-bad · unreadable"), "{status}");
    });
    let written = std::fs::read_to_string(tasks(&tmp).join("0001-look/result.md")).unwrap();
    assert!(written.contains("found it"), "{written}");
    let ignore = project(&tmp).join(".ensemble/runs/.gitignore");
    assert_eq!(std::fs::read_to_string(ignore).unwrap(), "*\n");
    assert!(
        std::fs::read_to_string(project(&tmp).join(".ensemble/runs/0001-look/log.txt"))
            .unwrap()
            .contains("scout"),
    );
}

#[test]
fn a_submitted_task_is_a_ticket_on_disk_whose_result_wait_returns() {
    let (e, tmp) = team(&[("scout", "read", &emit(&step(vec![text("found it")])))]);
    session(&e, |ask| {
        let r = ask(
            "submit",
            json!({"agent": "scout", "title": "Find the parser", "task": "Look.", "context": "src/"}),
        );
        assert!(r.starts_with("task 0001-find-the-parser queued"), "{r}");
        let result = ask(
            "wait",
            json!({"task": "0001-find-the-parser", "timeout_seconds": 20}),
        );
        assert!(
            result.contains("status: done") && result.contains("found it"),
            "{result}"
        );

        let r = ask(
            "submit",
            json!({"agent": "architect", "title": "x", "task": "y"}),
        );
        assert!(
            r.contains("no agent 'architect'") && r.contains("scout"),
            "{r}"
        );
        let r = ask("submit", json!({"agent": "scout", "task": "y"}));
        assert!(r.contains("`title`"), "{r}");
    });
    let ticket = std::fs::read_to_string(tasks(&tmp).join("0001-find-the-parser/task.md")).unwrap();
    assert_eq!(
        ticket,
        "---\ntitle: \"Find the parser\"\nagent: \"scout\"\ncontext: \"src/\"\n---\nLook.\n"
    );
}

#[test]
fn an_agent_runs_no_more_tasks_at_once_than_its_max_concurrent() {
    for (limit, expected) in [(1, ["alone", "alone"]), (2, ["alone", "overlap"])] {
        let (e, tmp) = team_limited(&[("coder", "read", BUSY)], limit);
        write_task(&tmp, "0001-a", "coder", "a");
        write_task(&tmp, "0002-b", "coder", "b");
        let mut got = Vec::new();
        session(&e, |ask| {
            for id in ["0001-a", "0002-b"] {
                let r = ask("wait", json!({"task": id, "timeout_seconds": 20}));
                got.push(r.lines().nth(6).unwrap_or("").to_string());
            }
        });
        got.sort();
        assert_eq!(got, expected, "max_concurrent {limit}");
    }
}

#[test]
fn a_blocking_delegation_waits_for_a_slot_taken_by_a_background_one() {
    let (e, _tmp) = team_limited(&[("coder", "read", BUSY)], 1);
    session(&e, |ask| {
        let r = ask(
            "delegate",
            json!({"agent": "coder", "task": "a", "background": true}),
        );
        assert!(r.starts_with("adhoc-1 started"), "{r}");
        let blocking = ask("delegate", json!({"agent": "coder", "task": "b"}));
        assert!(blocking.starts_with("alone"), "{blocking}");
        let background = ask("wait", json!({"task": "adhoc-1", "timeout_seconds": 20}));
        assert!(background.starts_with("alone"), "{background}");
    });
}

#[test]
fn the_end_of_a_session_interrupts_running_tasks_and_leaves_queued_ones() {
    let (e, tmp) = team_limited(&[("scout", "read", &sleeper())], 1);
    write_task(&tmp, "0001-a", "scout", "a");
    write_task(&tmp, "0002-b", "scout", "b");
    let started = std::time::Instant::now();
    session(&e, |ask| {
        let status = until(ask, json!({}), "0001-a · scout · running");
        assert!(status.contains("0002-b · scout · queued"), "{status}");
    });
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    let s = store(&tmp);
    assert!(s.outcome("0001-a").is_none() && s.outcome("0002-b").is_none());
    assert_eq!(s.state("0001-a").as_deref(), Some("interrupted"));
    assert_eq!(s.state("0002-b"), None);
}

#[test]
fn a_queued_task_can_be_cancelled_and_an_ended_one_does_not_run_again() {
    let (e, tmp) = team_limited(&[("scout", "read", &sleeper())], 1);
    write_task(&tmp, "0001-a", "scout", "a");
    write_task(&tmp, "0002-b", "scout", "b");
    write_task(&tmp, "0003-c", "scout", "c");
    std::fs::write(
        tasks(&tmp).join("0003-c/result.md"),
        "---\nstatus: done\n---\nalready\n",
    )
    .unwrap();
    session(&e, |ask| {
        until(ask, json!({"task": "0001-a"}), "running");
        let r = ask("cancel", json!({"task": "0002-b"}));
        assert!(r.contains("cancelled; it had not started"), "{r}");
        let r = ask("cancel", json!({"task": "0001-a"}));
        assert!(r.contains("being cancelled"), "{r}");
        let r = ask("wait", json!({"task": "0001-a", "timeout_seconds": 20}));
        assert!(r.contains("status: cancelled"), "{r}");
        let r = ask("cancel", json!({"task": "0003-c"}));
        assert!(r.contains("already ended"), "{r}");
        let r = ask("wait", json!({"task": "0003-c"}));
        assert!(r.contains("already"), "{r}");
    });
    let s = store(&tmp);
    assert_eq!(
        s.outcome("0002-b").unwrap().status,
        ensemble_tasks::tasks::Status::Cancelled
    );
    assert_eq!(s.state("0002-b"), None, "it never started");
    assert_eq!(s.state("0003-c"), None, "it never ran again");
}
#[test]
fn a_task_starts_only_once_every_task_it_is_after_is_done() {
    let (e, tmp) = team(&[("coder", "read", BUSY)]);
    write_task(&tmp, "0001-a", "coder", "a");
    let dir = tasks(&tmp).join("0002-b");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("task.md"),
        "---\ntitle: b\nagent: coder\nafter: [0001-a]\n---\nb\n",
    )
    .unwrap();
    let dir = tasks(&tmp).join("0003-c");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("task.md"),
        "---\ntitle: c\nagent: coder\nafter: [0009-gone]\n---\nc\n",
    )
    .unwrap();
    session(&e, |ask| {
        let status = until(ask, json!({"task": "0002-b"}), "blocked by 0001-a");
        assert!(status.contains("not done yet"), "{status}");
        for id in ["0001-a", "0002-b"] {
            let r = ask("wait", json!({"task": id, "timeout_seconds": 20}));
            assert_eq!(r.lines().nth(6), Some("alone"), "{id}: {r}");
        }
        let c = ask("status", json!({"task": "0003-c"}));
        assert!(c.contains("blocked by 0009-gone (no such task)"), "{c}");
        let r = ask(
            "submit",
            json!({"agent": "coder", "title": "d", "task": "d", "after": ["0042"]}),
        );
        assert!(r.contains("'0042', which is not a task"), "{r}");
        let r = ask(
            "submit",
            json!({"agent": "coder", "title": "e", "task": "e", "after": ["0001-a"],
                       "goal": "g", "done_when": ["x"]}),
        );
        assert!(r.starts_with("task 0004-e queued"), "{r}");
    });
    let e = store(&tmp).read("0004-e").unwrap();
    assert_eq!(
        (e.after, e.goal, e.done_when),
        (
            vec!["0001-a".to_string()],
            Some("g".to_string()),
            vec!["x".to_string()]
        )
    );
}
#[test]
fn a_gate_that_fails_sends_the_agent_back_with_its_output() {
    // The first run leaves no done.txt; the run after it does.
    let fixer = r#"printf '%s' "$*" > .last-prompt
if [ -e .tried ]; then touch done.txt; t=fixed; else touch .tried; t=first; fi
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;
    let (e, tmp) = team(&[("coder", "read, edit", fixer)]);
    let write = |id: &str, attempts: u32| {
        let dir = tasks(&tmp).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
                dir.join("task.md"),
                format!("---\ntitle: {id}\nagent: coder\ngate: {{name: done-file, attempts: {attempts}}}\n---\nMake done.txt.\n"),
            )
            .unwrap();
    };
    write("0001-once", 1);
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0001-once", "timeout_seconds": 20}));
        assert!(r.starts_with("---\nstatus: failed\n"), "{r}");
        assert!(
            r.contains("gate: \"done-file\"\ngate_result: failed\nattempts: 1\n"),
            "{r}"
        );
        assert!(
            r.contains("[gate done-file: failed (exit 1) on attempt 1 of 1]"),
            "{r}"
        );
    });
    std::fs::remove_file(project(&tmp).join(".tried")).unwrap();
    write("0002-twice", 2);
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0002-twice", "timeout_seconds": 20}));
        assert!(r.starts_with("---\nstatus: done\n"), "{r}");
        assert!(r.contains("gate_result: passed\nattempts: 2\n"), "{r}");
        assert!(
            r.contains("fixed") && r.contains("[gate done-file: passed on attempt 2 of 2]"),
            "{r}"
        );
        let r = ask(
            "submit",
            json!({"agent": "coder", "title": "x", "task": "x", "gate": {"name": "lint", "attempts": 1}}),
        );
        assert!(
            r.contains("no gate 'lint'") && r.contains("done-file"),
            "{r}"
        );
    });
    let prompt = std::fs::read_to_string(project(&tmp).join(".last-prompt")).unwrap();
    assert!(
        prompt.contains("did not pass the check `test -e done.txt` (exit 1)"),
        "{prompt}"
    );
    assert!(
        prompt.contains("Your previous reply was:\nfirst"),
        "{prompt}"
    );
}
#[test]
fn a_worktree_agent_works_on_a_branch_of_its_own_and_leaves_the_project_alone() {
    let writer = r#"printf '%s' "$(pwd)" > ../where
echo changed > a.txt
touch done.txt
printf '{"type":"text","part":{"text":"edited"}}\n'
"#;
    let (e, tmp) = team_with(&[("coder", "read, edit", writer)], 2, "worktree");
    ensemble_agents::worktree::testing::repo(&project(&tmp));
    let dir = tasks(&tmp).join("0001-edit");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("task.md"),
        "---\ntitle: Edit a.txt\nagent: coder\ngate: {name: done-file, attempts: 1}\n---\nEdit.\n",
    )
    .unwrap();
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0001-edit", "timeout_seconds": 20}));
        assert!(r.starts_with("---\nstatus: done\n"), "{r}");
        assert!(r.contains("branch: ensemble/0001-edit\ncommit: "), "{r}");
        assert!(
            r.contains("[files changed: a.txt (added), done.txt (added)]"),
            "{r}"
        );
        assert!(
            r.contains("[work committed on branch ensemble/0001-edit as "),
            "{r}"
        );
        assert!(!r.contains("another delegation ran"), "{r}");
    });
    let wt = project(&tmp).join(".ensemble/worktrees/0001-edit");
    let worked_in =
        std::fs::read_to_string(project(&tmp).join(".ensemble/worktrees/where")).unwrap();
    assert!(
        worked_in.ends_with(".ensemble/worktrees/0001-edit"),
        "{worked_in}"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        std::fs::read_to_string(project(&tmp).join("a.txt")).unwrap(),
        "hello"
    );
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(project(&tmp))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(
        git(&["log", "--format=%s", "-1", "ensemble/0001-edit"]).trim(),
        "0001-edit: Edit a.txt"
    );
    let status = git(&["status", "--porcelain"]);
    assert!(!status.contains("worktrees"), "{status}");
}

#[test]
fn a_worktree_agent_outside_a_git_repository_fails_its_task_saying_why() {
    let (e, tmp) = team_with(
        &[("coder", "read", &emit(&step(vec![text("x")])))],
        1,
        "worktree",
    );
    write_task(&tmp, "0001-a", "coder", "a");
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0001-a", "timeout_seconds": 20}));
        assert!(r.starts_with("---\nstatus: failed\n"), "{r}");
        assert!(r.contains("git repository with at least one commit"), "{r}");
    });
}
#[test]
fn a_task_that_asks_waits_for_the_answer_and_goes_on_with_it() {
    let asker = r#"if printf '%s' "$*" | grep -q '### Answer'; then t="used the answer"; else t="NEEDS INPUT: Which parser?"; fi
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;
    let (e, tmp) = team(&[("scout", "read", asker)]);
    write_task(&tmp, "0001-look", "scout", "Find the parser.");
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0001-look", "timeout_seconds": 20}));
        assert!(r.starts_with("0001-look needs input: Which parser?"), "{r}");
        let s = ask("status", json!({"task": "0001-look"}));
        assert!(s.contains("· needs input: Which parser?"), "{s}");
        let r = ask(
            "answer",
            json!({"task": "0001-look", "answer": "The new one."}),
        );
        assert!(r.contains("answered"), "{r}");
        let r = ask("wait", json!({"task": "0001-look", "timeout_seconds": 20}));
        assert!(
            r.starts_with("---\nstatus: done\n") && r.contains("used the answer"),
            "{r}"
        );
        let r = ask("answer", json!({"task": "0001-look", "answer": "again"}));
        assert!(r.contains("no question waiting"), "{r}");
        let r = ask("answer", json!({"task": "0009", "answer": "x"}));
        assert!(r.contains("no task '0009'"), "{r}");
    });
    let questions = std::fs::read_to_string(tasks(&tmp).join("0001-look/questions.md")).unwrap();
    assert!(
        questions.contains("Which parser?\n\n### Answer\nThe new one.\n"),
        "{questions}"
    );
}
#[test]
fn an_agent_over_its_budget_is_refused_and_its_tasks_wait() {
    let (e, tmp) = team(&[("scout", "read", &emit(&step(vec![text("found it")])))]);
    // One run an hour; the record is kept across sessions.
    let mut config = (*e.config()).clone();
    config.agents.get_mut("scout").unwrap().budget = config::Budget::PerHour { delegations: 1 };
    let e = Ensemble::new(config, Some(project(&tmp))).with_state(project(&tmp).join(".ensemble"));
    session(&e, |ask| {
        let r = ask("delegate", json!({"agent": "scout", "task": "a"}));
        assert!(r.starts_with("found it"), "{r}");
        let r = ask(
            "delegate",
            json!({"agent": "scout", "task": "b", "background": true}),
        );
        assert!(
            r.contains("used its budget") && r.contains("60 more minutes"),
            "{r}"
        );
    });
    write_task(&tmp, "0001-a", "scout", "a");
    session(&e, |ask| {
        let s = until(ask, json!({"task": "0001-a"}), "over its agent's budget");
        assert!(s.contains("0001-a · scout · queued"), "{s}");
    });
    assert!(store(&tmp).outcome("0001-a").is_none());
    let usage = store(&tmp).usage();
    assert_eq!(usage["scout"].len(), 1, "{usage:?}");
}

#[test]
fn a_delegation_with_repo_works_in_that_folder_of_the_project() {
    let body = format!("echo here > marker\n{}", emit(&step(vec![text("ok")])));
    let (e, tmp) = team(&[("fixer", "read, edit", &body)]);
    std::fs::create_dir_all(project(&tmp).join("backend")).unwrap();
    let r = call(
        &e,
        "delegate",
        json!({"agent": "fixer", "task": "mark", "repo": "backend"}),
    );
    assert_eq!(r["result"]["isError"], false, "{}", text_of(&r));
    assert!(project(&tmp).join("backend/marker").is_file());
    assert!(!project(&tmp).join("marker").exists());
}

#[test]
fn a_repo_outside_the_project_is_refused_by_delegate_and_submit() {
    let (e, tmp) = team(&[("fixer", "read, edit", "true")]);
    std::fs::create_dir_all(tmp.0.join("elsewhere")).unwrap();
    for repo in ["../elsewhere", "/tmp", "nope"] {
        let r = call(
            &e,
            "delegate",
            json!({"agent": "fixer", "task": "x", "repo": repo}),
        );
        assert_eq!(r["result"]["isError"], true, "{repo}: {}", text_of(&r));
        assert!(text_of(&r).contains("`repo`"), "{}", text_of(&r));
        let r = call(
            &e,
            "submit",
            json!({"agent": "fixer", "title": "t", "task": "x", "repo": repo}),
        );
        assert_eq!(r["result"]["isError"], true, "{repo}: {}", text_of(&r));
    }
    assert!(!tasks(&tmp).exists(), "a refused task writes no ticket");
}

#[test]
fn agents_lists_every_agent_and_takes_no_team() {
    let (e, _tmp) = team(&[("coder", "read, edit", "true"), ("scout", "read", "true")]);
    let all = call(&e, "agents", json!({}));
    assert!(
        text_of(&all).contains("scout [read-only]") && text_of(&all).contains("coder"),
        "{}",
        text_of(&all)
    );
    assert!(!text_of(&all).contains("team"), "{}", text_of(&all));
}

#[test]
fn initialize_tells_the_calling_agent_the_team_and_how_to_hand_work_over() {
    let (e, _tmp) = team(&[("coder", "read, edit", "true"), ("scout", "read", "true")]);
    let r = handle(&e, &rpc("initialize", json!({}))).unwrap();
    let text = r["result"]["instructions"].as_str().unwrap();
    assert!(
        text.contains("- coder: The coder. Second line.")
            && text.contains("- scout: The scout. Second line."),
        "{text}"
    );
    assert!(
        text.contains("`agents`") && text.contains("`delegate`") && text.contains("`repo`"),
        "{text}"
    );
}

#[test]
fn a_session_runs_no_tasks_while_another_worker_holds_the_folder() {
    let (e, tmp) = team(&[("scout", "read", &emit(&step(vec![text("found it")])))]);
    // This very process stands in for the other worker.
    let state = project(&tmp).join(".ensemble");
    let _other = ensemble_tasks::lock::acquire(&state).unwrap();
    session(&e, |ask| {
        let r = ask(
            "submit",
            json!({"agent": "scout", "title": "Look", "task": "look"}),
        );
        assert!(r.contains("0001-look"), "{r}");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let status = ask("status", json!({"task": "0001-look"}));
        assert!(status.contains("queued"), "no worker ran it: {status}");
    });
    assert!(store(&tmp).outcome("0001-look").is_none());
}

#[test]
fn a_delegation_leaves_a_record_of_the_run() {
    let (e, tmp) = scouting();
    let r = call(&e, "delegate", json!({"agent": "scout", "task": "look"}));
    assert_eq!(r["result"]["isError"], false, "{}", text_of(&r));
    let records = ensemble_core::records::read(&project(&tmp).join(".ensemble"));
    assert_eq!(records.len(), 1, "{records:?}");
    let run = &records[0];
    assert_eq!(run.agent, "scout");
    assert!(run.ok && run.error.is_none());
    assert_eq!(
        (run.prompt_tokens, run.completion_tokens),
        (Some(5), Some(2))
    );
    assert_eq!(run.steps, 1);
}

const ASKER: &str = r#"if printf '%s' "$*" | grep -q 'The answer: use bar'; then t="renamed with bar"; else t="NEEDS INPUT: Which name?"; fi
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;

#[test]
fn a_delegation_that_asks_returns_a_session_and_an_answer_resumes_it() {
    let (e, _tmp) = team(&[("scout", "read", ASKER)]);
    session(&e, |ask| {
        let said = ask("delegate", json!({"agent": "scout", "task": "rename it"}));
        assert!(
            said.starts_with("needs_input: true\nsession: ask-1\nquestion: Which name?\n"),
            "{said}"
        );
        assert!(said.contains("`continue`"), "{said}");

        // The answer needs no agent: the same one goes on, with what it had.
        let second = ask("delegate", json!({"continue": "ask-1", "task": "use bar"}));
        assert!(second.starts_with("renamed with bar"), "{second}");

        // A session is for one answer.
        let again = ask("delegate", json!({"continue": "ask-1", "task": "use bar"}));
        assert!(again.contains("no question is waiting"), "{again}");
    });
}

#[test]
fn an_answer_that_does_not_satisfy_asks_again_under_a_new_session() {
    let (e, _tmp) = team(&[("scout", "read", ASKER)]);
    session(&e, |ask| {
        let first = ask("delegate", json!({"agent": "scout", "task": "rename it"}));
        assert!(first.contains("session: ask-1"), "{first}");
        let second = ask("delegate", json!({"continue": "ask-1", "task": "dunno"}));
        assert!(second.contains("session: ask-2"), "{second}");
    });
}

#[test]
fn continue_needs_an_answer_and_a_plain_delegation_needs_an_agent() {
    let (e, _tmp) = team(&[("scout", "read", ASKER)]);
    session(&e, |ask| {
        let no_agent = ask("delegate", json!({"task": "x"}));
        assert!(no_agent.contains("`agent` is required"), "{no_agent}");
        let first = ask("delegate", json!({"agent": "scout", "task": "rename it"}));
        assert!(first.contains("ask-1"), "{first}");
        let blank = ask("delegate", json!({"continue": "ask-1", "task": "  "}));
        assert!(blank.contains("your answer"), "{blank}");
    });
}

#[test]
fn a_plain_answer_is_not_mistaken_for_a_question() {
    let (e, _tmp) = team(&[("scout", "read", &emit(&step(vec![text("all done")])))]);
    let r = call(&e, "delegate", json!({"agent": "scout", "task": "x"}));
    assert!(text_of(&r).starts_with("all done"), "{}", text_of(&r));
    assert!(!text_of(&r).contains("needs_input"));
}

#[test]
fn a_backlog_task_waits_for_an_assignee_and_then_runs_for_that_agent() {
    let (e, tmp) = team(&[("scout", "read", &emit(&step(vec![text("found it")])))]);
    session(&e, |ask| {
        let r = ask("submit", json!({"title": "Look around", "task": "look"}));
        assert!(r.contains("0001-look-around"), "{r}");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let status = ask("status", json!({}));
        assert!(
            status.contains("0001-look-around · - · backlog"),
            "{status}"
        );
        assert!(
            status.contains("Idle: no active tasks (1 in the backlog, to be assigned)"),
            "{status}"
        );

        let r = ask(
            "assign",
            json!({"task": "0001-look-around", "agent": "scout"}),
        );
        assert!(r.contains("assigned to scout"), "{r}");
        let r = ask(
            "wait",
            json!({"task": "0001-look-around", "timeout_seconds": 20}),
        );
        assert!(r.contains("status: done") && r.contains("found it"), "{r}");
        let status = ask("status", json!({}));
        assert!(status.contains("Idle: no active tasks"), "{status}");
        let again = ask(
            "assign",
            json!({"task": "0001-look-around", "agent": "scout"}),
        );
        assert!(again.contains("has ended"), "{again}");
        let none = ask(
            "assign",
            json!({"task": "0001-look-around", "agent": "nobody"}),
        );
        assert!(none.contains("no agent"), "{none}");
    });
    let ticket = std::fs::read_to_string(tasks(&tmp).join("0001-look-around/task.md")).unwrap();
    assert!(
        !ticket.contains("agent:"),
        "task.md is never rewritten: {ticket}"
    );
}

#[test]
fn an_agent_hands_a_task_on_and_the_next_agent_is_told_what_was_done() {
    let first = r#"t="wrote the plan\nHAND OFF: scout — now check it against the code"
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;
    let second = r#"if printf '%s' "$*" | grep -q 'now check it against the code'; then t="checked the plan"; else t="no note"; fi
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;
    let (e, tmp) = team(&[("coder", "read", first), ("scout", "read", second)]);
    write_task(&tmp, "0001-plan", "coder", "Plan and check.");
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0001-plan", "timeout_seconds": 20}));
        assert!(
            r.contains("status: done") && r.contains("checked the plan"),
            "{r}"
        );
        assert!(r.contains("scout"), "the result names who finished it: {r}");
    });
    let notes = std::fs::read_to_string(tasks(&tmp).join("0001-plan/handoffs.md")).unwrap();
    assert!(
        notes.contains("coder handed this to scout")
            && notes.contains("now check it against the code")
            && notes.contains("wrote the plan"),
        "{notes}"
    );
}

#[test]
fn a_hand_off_to_nobody_fails_the_task_and_says_so() {
    let body = r#"t="done mostly\nHAND OFF: ghost the rest"
printf '{"type":"text","part":{"text":"%s"}}\n' "$t"
"#;
    let (e, tmp) = team(&[("coder", "read", body)]);
    write_task(&tmp, "0001-x", "coder", "Do it.");
    session(&e, |ask| {
        let r = ask("wait", json!({"task": "0001-x", "timeout_seconds": 20}));
        assert!(
            r.contains("status: failed") && r.contains("which is not an agent"),
            "{r}"
        );
    });
}

#[test]
fn agents_on_one_shared_limit_do_not_overlap_whatever_each_would_allow() {
    // Each agent alone would allow two at once; the limit they share allows one.
    let (e, tmp) = team_limited(&[("coder", "read", BUSY), ("scout", "read", BUSY)], 2);
    let mut config = (*e.config()).clone();
    config.limits.insert(
        "server".into(),
        config::Limit {
            max_concurrent: 1,
            budget: config::Budget::Unlimited,
        },
    );
    for a in config.agents.values_mut() {
        a.limit = Some("server".into());
        a.max_concurrent = 1;
    }
    let e = Ensemble::new(config, Some(project(&tmp))).with_state(project(&tmp).join(".ensemble"));
    write_task(&tmp, "0001-a", "coder", "a");
    write_task(&tmp, "0002-b", "scout", "b");
    let mut got = Vec::new();
    session(&e, |ask| {
        for id in ["0001-a", "0002-b"] {
            let r = ask("wait", json!({"task": id, "timeout_seconds": 20}));
            got.push(r.lines().nth(6).unwrap_or("").to_string());
        }
    });
    got.sort();
    assert_eq!(got, ["alone", "alone"], "one limit, one run at a time");
}

#[test]
fn a_token_budget_counts_what_runs_reported_and_refuses_when_it_is_used() {
    let (e, tmp) = team(&[("scout", "read", &emit(&step(vec![text("found it")])))]);
    let mut config = (*e.config()).clone();
    config.agents.get_mut("scout").unwrap().budget = config::Budget::PerHourTokens { tokens: 100 };
    let e = Ensemble::new(config, Some(project(&tmp))).with_state(project(&tmp).join(".ensemble"));
    let dir = project(&tmp).join(".ensemble");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let run = |started: u64, tokens: Option<(u64, u64)>| ensemble_core::records::Record {
        agent: "scout".into(),
        model: None,
        started,
        seconds: 1.0,
        prompt_tokens: tokens.map(|t| t.0),
        completion_tokens: tokens.map(|t| t.1),
        steps: 1,
        tool_calls: 0,
        ok: true,
        error: None,
    };
    // 60 tokens an hour ago-ish, and a run that reported none: under budget.
    ensemble_core::records::append(&dir, &run(now - 600, Some((40, 20))));
    ensemble_core::records::append(&dir, &run(now - 300, None));
    let r = call(&e, "delegate", json!({"agent": "scout", "task": "look"}));
    assert_eq!(r["result"]["isError"], false, "{}", text_of(&r));
    // That run reported 5 + 2; now push it over.
    ensemble_core::records::append(&dir, &run(now - 100, Some((30, 20))));
    let r = call(&e, "delegate", json!({"agent": "scout", "task": "look"}));
    assert_eq!(r["result"]["isError"], true);
    assert!(
        text_of(&r).contains("used its budget for this hour"),
        "{}",
        text_of(&r)
    );
    // Runs older than an hour are not counted.
    let (e2, tmp2) = team(&[("scout", "read", &emit(&step(vec![text("ok")])))]);
    let mut config = (*e2.config()).clone();
    config.agents.get_mut("scout").unwrap().budget = config::Budget::PerHourTokens { tokens: 100 };
    let e2 =
        Ensemble::new(config, Some(project(&tmp2))).with_state(project(&tmp2).join(".ensemble"));
    ensemble_core::records::append(
        &project(&tmp2).join(".ensemble"),
        &run(now - 7200, Some((900, 900))),
    );
    let r = call(&e2, "delegate", json!({"agent": "scout", "task": "look"}));
    assert_eq!(r["result"]["isError"], false, "{}", text_of(&r));
}

#[test]
fn a_task_whose_agent_has_no_usable_model_waits_and_says_why() {
    let tmp = TempDir::new();
    let project = project(&tmp);
    std::fs::create_dir_all(&project).unwrap();
    let down = |n: u16| format!("http://127.0.0.1:{n}/v1");
    let yaml = format!(
        "version: 3\nllm_backends:\n  home: {{ type: openai-compatible-api, base_url: \"{}\", timeout_seconds: 5 }}\n  central: {{ type: openai-compatible-api, base_url: \"{}\", timeout_seconds: 5 }}\nlimits: {{}}\ntracker: {{ type: local_markdown }}\ngates: {{}}\nagents:\n  summariser:\n    max_seconds: 30\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try:\n        - {{ backend: home, model: small }}\n        - {{ backend: central, model: big }}\n      when_none: wait\n      max_tokens: 10\n",
        down(1),
        down(2)
    );
    let cfg = config::parse(&yaml, std::path::Path::new("config.yaml"), &BTreeMap::new()).unwrap();
    let e = Ensemble::new(cfg, Some(project.clone())).with_state(project.join(".ensemble"));
    session(&e, |ask| {
        let r = ask(
            "submit",
            json!({"agent": "summariser", "title": "Sum it", "task": "sum"}),
        );
        assert!(r.contains("0001-sum-it"), "{r}");
        let log = project.join(".ensemble/runs/0001-sum-it/log.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !log.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let said = std::fs::read_to_string(&log).unwrap();
        assert!(
            said.contains("waiting for a backend") && said.contains("home (small)"),
            "{said}"
        );
        let status = ask("status", json!({}));
        assert!(
            status.contains("0001-sum-it · summariser · queued"),
            "{status}"
        );
        assert!(
            !project
                .join(".ensemble/tasks/0001-sum-it/result.md")
                .exists()
        );
    });
}

/// The knowledge of a hub whose vault is in `tmp`.
fn memory(tmp: &TempDir) -> Memory {
    let yaml = format!(
        "version: 1\nowner: Ana\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall: {{ max_notes: 3, max_chars: 1000 }}\nserver: {{ listen: \"127.0.0.1:0\", registration: {{ type: closed }}, sessions: {{ hours: 1 }}, lockout: {{ failures: 3, minutes: 1 }} }}\nroles: {{ member: [share, recall] }}\n",
        tmp.0.join("vault").display()
    );
    let hub = config::parse_hub(&yaml, std::path::Path::new("hub.yaml"), &BTreeMap::new()).unwrap();
    let memory = Memory::new(&hub, Some("shop".into()));
    memory.vault().init().unwrap();
    memory
}

#[test]
fn with_a_hub_the_knowledge_tools_are_offered_and_a_share_is_kept() {
    let (e, tmp) = working();
    let names = |r: Value| -> Vec<String> {
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let without = names(handle(&e, &rpc("tools/list", json!({}))).unwrap());
    assert!(!without.contains(&"share".to_string()), "no hub, no share");

    let m = memory(&tmp);
    let with = names(handle_with(&e, Some(&m), &rpc("tools/list", json!({}))).unwrap());
    assert_eq!(with[with.len() - 2..], ["share", "recall"]);
    let r = handle_with(
        &e,
        Some(&m),
        &rpc(
            "initialize",
            json!({"clientInfo": {"name": "claude-code", "version": "2"}}),
        ),
    )
    .unwrap();
    let instructions = r["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("call `share`"), "{instructions}");

    let r = handle_with(
        &e,
        Some(&m),
        &rpc(
            "tools/call",
            json!({"name": "share", "arguments": {"title": "Ports", "text": "The shop listens on 8080."}}),
        ),
    )
    .unwrap();
    assert_eq!(r["result"]["isError"], false, "{r}");
    let shares = m.vault().shares();
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].client.as_deref(), Some("claude-code"));

    let r = handle_with(
        &e,
        Some(&m),
        &rpc(
            "tools/call",
            json!({"name": "share", "arguments": {"title": "x"}}),
        ),
    )
    .unwrap();
    assert_eq!(r["result"]["isError"], true, "{r}");
}

/// A hub's knowledge with one note, about deploys, and a long tail.
struct Deploys;

#[async_trait::async_trait]
impl KnowledgeAccess for Deploys {
    async fn tools(&self) -> Vec<ensemble_knowledge::Tool> {
        ensemble_knowledge::tool_definitions()
    }

    async fn instructions(&self) -> String {
        String::new()
    }

    async fn call(&self, name: &str, args: &Value) -> Option<Result<String, String>> {
        let query = args["query"].as_str().unwrap_or("");
        (name == RECALL).then(|| {
            Ok(if query.contains("deploy") {
                format!(
                    "{RECALL_HEADER}\n\n## Deploys\nWe deploy on Fridays.{}",
                    " tail".repeat(100)
                )
            } else {
                format!("No note in the user's knowledge vault is about '{query}'.")
            })
        })
    }

    fn set_client(&self, _: &str) {}
}

/// Writes its prompt to `<agent>.txt`, then answers.
fn prompt_writer(agent: &str) -> String {
    format!(
        "for last; do :; done\nprintf '%s' \"$last\" > {agent}.txt\n{}",
        emit(&step(vec![text("ok")]))
    )
}

/// A coder given at most `max_chars` of notes, and a scout given none.
fn recalling(max_chars: u32) -> (Ensemble, TempDir) {
    let (coder, scout) = (prompt_writer("coder"), prompt_writer("scout"));
    team_changed(
        &[("coder", "read", &coder), ("scout", "read", &scout)],
        4,
        "shared",
        |cfg| cfg.agents.get_mut("coder").unwrap().recall = Some(config::AgentRecall { max_chars }),
    )
}

fn prompt_of(tmp: &TempDir, agent: &str) -> String {
    std::fs::read_to_string(project(tmp).join(format!("{agent}.txt"))).unwrap()
}

#[test]
fn an_agent_with_recall_is_given_the_notes_about_its_task_within_its_size() {
    let (e, tmp) = recalling(60);
    let delegate = |agent: &str, task: &str| {
        handle_with(
            &e,
            Some(&Deploys),
            &rpc(
                "tools/call",
                json!({"name": "delegate", "arguments":
                    {"agent": agent, "task": task, "context": "on main"}}),
            ),
        )
        .unwrap()
    };
    delegate("coder", "fix the deploy script");
    let prompt = prompt_of(&tmp, "coder");
    assert!(
        prompt.starts_with(&format!("Context:\non main\n\n{RECALL_HEADER}")),
        "{prompt}"
    );
    // The header that marks them as data is kept whole; the notes after it
    // are cut to max_chars.
    let notes = prompt
        .split_once(RECALL_HEADER)
        .unwrap()
        .1
        .split_once(" [...]")
        .unwrap()
        .0;
    assert!(notes.contains("We deploy on Fridays."), "{prompt}");
    assert!(notes.chars().count() <= 60, "cut to max_chars: {prompt}");
    assert!(prompt.contains("fix the deploy script"), "{prompt}");

    // An agent without `recall` is given nothing, and nor is a task the
    // vault has no notes about.
    delegate("scout", "fix the deploy script");
    assert!(!prompt_of(&tmp, "scout").contains(RECALL_HEADER));
    delegate("coder", "rename the parser");
    let prompt = prompt_of(&tmp, "coder");
    assert!(
        prompt.starts_with("Context:\non main\n\nrename the parser"),
        "{prompt}"
    );

    // Without a hub, `recall` gives nothing either.
    call(
        &e,
        "delegate",
        json!({"agent": "coder", "task": "fix the deploy script"}),
    );
    assert!(!prompt_of(&tmp, "coder").contains(RECALL_HEADER));
}

#[test]
fn a_task_of_an_agent_with_recall_is_given_the_notes_about_it() {
    let (e, tmp) = recalling(2000);
    write_task(
        &tmp,
        "0001-deploy",
        "coder",
        "Make the deploy script retry.",
    );
    let hub = Hub {
        knowledge: Some(&Deploys),
    };
    session_with(&e, hub, |ask| {
        let r = ask(
            "wait",
            json!({"task": "0001-deploy", "timeout_seconds": 20}),
        );
        assert!(r.starts_with("---\nstatus: done\n"), "{r}");
    });
    let prompt = prompt_of(&tmp, "coder");
    assert!(
        prompt.starts_with(&format!("Context:\n{RECALL_HEADER}")),
        "{prompt}"
    );
    assert!(prompt.contains("We deploy on Fridays."), "{prompt}");
    assert!(!prompt.contains("[...]"), "within its size: {prompt}");
}
