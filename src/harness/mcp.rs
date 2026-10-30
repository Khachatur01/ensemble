//! The harness interface: MCP over stdio.
//!
//! The agent that calls Ensemble (Claude Code, OpenCode, ...) is the manager.
//! It spawns `ensemble mcp` with the project as its working directory and sees
//! the team as tools: `agents`, `delegate`, `health`.
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
use crate::router::Ensemble;
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
pub fn handle(ensemble: &Ensemble, request: &Value) -> Option<Value> {
    handle_reporting(ensemble, request, &|_| {}, &AtomicBool::new(false))
}

/// [`handle`], with `notify` called for each notification to send to the client
/// before the reply, such as what a delegated agent is doing. Setting `cancel`
/// stops a delegation at its next step.
pub fn handle_reporting(
    ensemble: &Ensemble,
    request: &Value,
    notify: &dyn Fn(Value),
    cancel: &AtomicBool,
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
/// and it gets no reply: the client has stopped waiting for one. When stdin
/// closes, calls still running are finished before this returns.
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
    let (send, notify, running) = (&send, &notify, &running);

    std::thread::scope(|scope| {
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
                        let response = handle_reporting(ensemble, &request, notify, &cancel);
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
            if let Some(response) =
                handle_reporting(ensemble, &request, notify, &AtomicBool::new(false))
            {
                send(&response)?;
            }
        }
        Ok(())
    })
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
        let tmp = TempDir::new();
        let bin = tmp.0.join("bin");
        let project = tmp.0.join("project");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("a.txt"), "hello").unwrap();
        let mut yaml = String::from("version: 1\nagents:\n");
        for (name, tools, body) in scripts {
            let command = cli(&bin, name, body);
            yaml.push_str(&format!(
                "  {name}:\n    prompt: \"The {name}. Second line.\"\n    adapter:\n      type: opencode\n      model: opencode/m-{name}\n      tools: [{tools}]\n      command: \"{command}\"\n"
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
    fn tools_list_advertises_the_three_tools_with_the_real_agent_names() {
        let (e, _tmp) = working();
        let r = handle(&e, &rpc("tools/list", json!({}))).unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["delegate", "agents", "health"]);

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
            "version: 1\nagents:\n  x:\n    adapter: { type: opencode, model: a/b, command: /nonexistent/opencode }\n",
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
}
