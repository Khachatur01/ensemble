//! The harness interface: MCP over stdio.
//!
//! The agent that calls Ensemble (Claude Code, OpenCode, ...) is the manager.
//! It spawns `ensemble mcp` with the project as its working directory and sees
//! the team as tools: `agents`, `delegate`, `submit` and the tools that follow
//! tasks, `health`. While it runs, agents work through the project's tasks.
//!
//! Ensemble is a subprocess the harness spawns, speaking JSON-RPC 2.0 as
//! newline-delimited JSON on stdin/stdout. **Nothing but protocol goes to
//! stdout** — one stray `println!` corrupts the stream and the harness sees a
//! parse error instead of a tool. Diagnostics go to stderr, which the harness
//! logs.
//!
//! WHAT THE TOOLS DELIBERATELY DO NOT EXPOSE
//! -----------------------------------------
//! No tool takes a backend or a model. The caller names an *agent* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.
//!
//! The tools themselves live in `super::delegation`, kept apart from the
//! transport so what an agent is and what a failed delegation says do not depend
//! on how it was asked. This module owns only the JSON-RPC envelope.

use super::delegation;
use super::queue::{Queue, Task};
use crate::router::Ensemble;
use crate::tasks::Store;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "ensemble";

/// The shared tool catalogue, wrapped in MCP's envelope.
///
/// `agents` is the whole team.
fn tool_definitions(agents: &[&str]) -> Value {
    Value::Array(
        delegation::tools(agents)
            .into_iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.parameters,
                })
            })
            .collect(),
    )
}

/// MCP log levels, lowest first. A client that sets a level asks to hear only
/// that level and above.
const LEVELS: [&str; 8] = [
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];

/// Progress goes out as `info` log messages.
const PROGRESS_LEVEL: &str = "info";

fn rank(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

/// `notifications/progress`: a counter that rises with each line, and the line.
fn progress_message(token: &Value, progress: u64, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": progress, "message": text }
    })
}

fn log_message(text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": { "level": PROGRESS_LEVEL, "logger": SERVER_NAME, "data": text }
    })
}

/// Handle one JSON-RPC message.
///
/// `None` means "send nothing" — which is required, not merely allowed, for
/// notifications: a notification carries no id, and answering one is a protocol
/// violation that some clients treat as fatal.
///
/// With no session around it, a background delegation runs to its end before
/// this returns and there are no tasks; [`serve`] is what runs them alongside
/// the rest.
pub fn handle(ensemble: &Ensemble, request: &Value) -> Option<Value> {
    handle_reporting(
        ensemble,
        request,
        &|_| {},
        &AtomicBool::new(false),
        &Queue::new(ensemble, None),
        &|task| task(),
    )
}

/// [`handle`], with `notify` called for each notification to send to the client
/// before the reply, such as what a delegated agent is doing. Setting `cancel`
/// stops a delegation at its next step. Tasks and background delegations are
/// kept in `queue`, and background work is started with `spawn`.
pub fn handle_reporting<'s>(
    ensemble: &'s Ensemble,
    request: &Value,
    notify: &dyn Fn(Value),
    cancel: &AtomicBool,
    queue: &'s Queue<'s>,
    spawn: &dyn Fn(Task<'s>),
) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");

    let reply = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));

    match method {
        "initialize" => {
            // Echo the client's protocol version when it sends one. Insisting
            // on ours would refuse clients that are otherwise compatible.
            let version = request
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            reply(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false }, "logging": {} },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
            }))
        }

        "tools/list" => reply(json!({ "tools": tool_definitions(&ensemble.agents()) })),

        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // The client's own token when it sent one; otherwise one made from
            // the request id, so the notification still names its call.
            let token = params
                .get("_meta")
                .and_then(|m| m.get("progressToken"))
                .cloned()
                .or_else(|| id.clone())
                .unwrap_or_else(|| json!("ensemble"));
            let count = Cell::new(0u64);
            let result = delegation::call(
                ensemble,
                name,
                &args,
                &|line| {
                    count.set(count.get() + 1);
                    notify(progress_message(&token, count.get(), line));
                    notify(log_message(line));
                },
                cancel,
                queue,
                spawn,
            );
            reply(json!({
                "content": [{ "type": "text", "text": result.text }],
                "isError": result.is_error
            }))
        }

        "ping" => reply(json!({})),

        // The level itself is applied by `serve`, which owns the output.
        "logging/setLevel" => reply(json!({})),

        // Notifications: no id, no answer.
        m if m.starts_with("notifications/") => None,

        other => {
            if id.is_none() || id == Some(Value::Null) {
                return None;
            }
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }))
        }
    }
}

/// Run the server until stdin closes.
///
/// Each `tools/call` runs on its own thread, so the caller can delegate to
/// several agents at once and still get `ping` and `tools/list` answered while
/// they work. `notifications/cancelled` stops the named call at its next step,
/// and it gets no reply: the client has stopped waiting for one.
///
/// Alongside, a worker runs the tasks in the project's `.ensemble/tasks/`.
/// When stdin closes, running tasks and background delegations are stopped
/// (tasks that had not started stay queued for the next session), and blocking
/// calls still running are finished before this returns.
pub fn serve<W: Write + Send>(
    ensemble: &Ensemble,
    input: impl BufRead,
    output: W,
) -> std::io::Result<()> {
    let output = Mutex::new(output);
    let min_level = AtomicUsize::new(rank(PROGRESS_LEVEL).unwrap_or(0));
    // Running calls by request id, so a cancellation can find its call.
    let running: Mutex<HashMap<String, Arc<AtomicBool>>> = Mutex::new(HashMap::new());
    // Flush per message. A buffered reply is a reply the harness is still
    // waiting for, and it looks exactly like a hung server.
    let send = |message: &Value| -> std::io::Result<()> {
        let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(out, "{message}")?;
        out.flush()
    };
    let notify = |n: Value| {
        // The level only filters log messages; progress is not one.
        let wanted = n["params"]["level"].as_str().and_then(rank);
        if wanted.is_none_or(|w| w >= min_level.load(Ordering::Relaxed)) {
            // A write that fails here fails again on the next reply, which
            // ends the session with the error.
            let _ = send(&n);
        }
    };
    let project = ensemble.workdir()?;
    let queue = Queue::new(
        ensemble,
        Some(Store::new(&project.join(crate::config::CONFIG_DIR))),
    );
    let (send, notify, running, queue) = (&send, &notify, &running, &queue);

    std::thread::scope(|scope| {
        scope.spawn(move || queue.work(&spawner(scope)));
        let result = (|| -> std::io::Result<()> {
            for line in input.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let request = match serde_json::from_str::<Value>(&line) {
                    Ok(request) => request,
                    Err(e) => {
                        send(&json!({
                            "jsonrpc": "2.0",
                            "id": Value::Null,
                            "error": { "code": -32700, "message": format!("parse error: {e}") }
                        }))?;
                        continue;
                    }
                };
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                let params = request.get("params");
                match method {
                    "logging/setLevel" => {
                        if let Some(level) = params
                            .and_then(|p| p.get("level"))
                            .and_then(Value::as_str)
                            .and_then(rank)
                        {
                            min_level.store(level, Ordering::Relaxed);
                        }
                    }
                    "notifications/cancelled" => {
                        if let Some(id) = params.and_then(|p| p.get("requestId"))
                            && let Some(flag) = running.lock().unwrap().get(&id.to_string())
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                        continue;
                    }
                    "tools/call" if request.get("id").is_some_and(|id| !id.is_null()) => {
                        let key = request["id"].to_string();
                        let cancel = Arc::new(AtomicBool::new(false));
                        running.lock().unwrap().insert(key.clone(), cancel.clone());
                        scope.spawn(move || {
                            let response = handle_reporting(
                                ensemble,
                                &request,
                                notify,
                                &cancel,
                                queue,
                                &spawner(scope),
                            );
                            running.lock().unwrap().remove(&key);
                            if !cancel.load(Ordering::Relaxed)
                                && let Some(response) = response
                            {
                                // A failed write here means the client is gone;
                                // the reading side finds out on its own.
                                let _ = send(&response);
                            }
                        });
                        continue;
                    }
                    _ => {}
                }
                if let Some(response) = handle_reporting(
                    ensemble,
                    &request,
                    notify,
                    &AtomicBool::new(false),
                    queue,
                    &spawner(scope),
                ) {
                    send(&response)?;
                }
            }
            Ok(())
        })();
        // The caller is gone, so nobody will ask for what background work
        // produces: stop it, or this would wait out its time limits.
        queue.close();
        result
    })
}

/// Runs background work on a thread of `scope`.
fn spawner<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
) -> impl Fn(Task<'scope>) + 'scope {
    move |task| {
        scope.spawn(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::workspace::testing::{TempDir, executable};
    use std::collections::BTreeMap;

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
                "  {name}:\n    prompt: \"The {name}. Second line.\"\n    max_seconds: 1800\n    max_concurrent: {max_concurrent}\n    isolation: {isolation}\n    adapter:\n      type: opencode\n      model: {{ type: provider, name: opencode/m-{name} }}\n      tools: [{tools}]\n      max_steps: 30\n      command: \"{command}\"\n"
            ));
        }
        let cfg =
            config::parse(&yaml, std::path::Path::new("config.yaml"), &BTreeMap::new()).unwrap();
        (Ensemble::new(cfg, Some(project)), tmp)
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
            listing.starts_with("coder [can change files]: The coder. Second line."),
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
                "delegate", "submit", "status", "wait", "cancel", "agents", "health"
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
        assert_eq!(prompt, "Context:\nthe test fails on line 9\n\nfix it");
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
            "version: 1\nllm_backends: {}\ngates: {}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 4\n    isolation: shared\n    adapter: { type: opencode, model: { type: provider, name: a/b }, tools: [read], max_steps: 3, command: /nonexistent/opencode }\n",
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
        lines: std::sync::mpsc::Receiver<String>,
        pending: Vec<u8>,
    }

    impl std::io::Read for Feed {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                match self.lines.recv() {
                    Ok(line) => self.pending = line.into_bytes(),
                    Err(_) => return Ok(0),
                }
            }
            let n = out.len().min(self.pending.len());
            out[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }

    /// Output the test can read while the server is still writing it.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        /// The text of the reply to request `id`, once it is written.
        fn reply_text(&self, id: u64) -> String {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let out = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
                for line in out.lines() {
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
        let (tx, rx) = std::sync::mpsc::channel();
        let out = Shared::default();
        std::thread::scope(|s| {
            let server = s.spawn(|| {
                let input = std::io::BufReader::new(Feed {
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
        let (tx, rx) = std::sync::mpsc::channel();
        let out = Shared::default();
        std::thread::scope(|s| {
            let server = s.spawn(|| {
                let input = std::io::BufReader::new(Feed {
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
        let (tx, rx) = std::sync::mpsc::channel();
        let out = Shared::default();
        std::thread::scope(|s| {
            let output = out.clone();
            let server = s.spawn(move || {
                let input = std::io::BufReader::new(Feed {
                    lines: rx,
                    pending: Vec::new(),
                });
                serve(e, input, output).unwrap();
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
        let ticket =
            std::fs::read_to_string(tasks(&tmp).join("0001-find-the-parser/task.md")).unwrap();
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
            crate::tasks::Status::Cancelled
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
        crate::worktree::testing::repo(&project(&tmp));
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
}
