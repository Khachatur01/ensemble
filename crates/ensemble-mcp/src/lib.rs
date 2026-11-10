//! The harness interface: MCP over stdio.
//!
//! The agent that calls Ensemble (Claude Code, OpenCode, ...) spawns
//! `ensemble mcp` with the project as its working directory and sees the helper
//! team as tools: `agents`, `delegate`, `submit` and the tools that follow
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
//! The tools themselves live in `ensemble_tasks::delegation`, kept apart from the
//! transport so what an agent is and what a failed delegation says do not depend
//! on how it was asked. This crate owns only the JSON-RPC envelope, and `serve`,
//! the worker that runs a folder's tasks with no session around it.

use ensemble_agents::router::Ensemble;
use ensemble_knowledge::Memory;
use ensemble_tasks::delegation;
use ensemble_tasks::queue::{Queue, Task};
use ensemble_tasks::tasks::Store;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod notify;
mod protocol;
pub mod serve;

use notify::*;
use protocol::tool_definitions;
pub use protocol::{PROTOCOL_VERSION, SERVER_NAME};

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
    handle_with(ensemble, None, request)
}

/// [`handle`], with the knowledge of a hub when there is one.
pub fn handle_with(ensemble: &Ensemble, memory: Option<&Memory>, request: &Value) -> Option<Value> {
    handle_reporting(
        ensemble,
        memory,
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
/// kept in `queue`, and background work is started with `spawn`. With
/// `memory`, the knowledge tools are offered too.
pub fn handle_reporting<'s>(
    ensemble: &'s Ensemble,
    memory: Option<&Memory>,
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
            // Who is on the other end, named in what it shares.
            if let (Some(memory), Some(client)) = (
                memory,
                request
                    .pointer("/params/clientInfo/name")
                    .and_then(Value::as_str),
            ) {
                memory.set_client(client);
            }
            let mut instructions = protocol::instructions(ensemble.config());
            if let Some(memory) = memory {
                instructions.push_str(memory.instructions());
            }
            reply(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false }, "logging": {} },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                "instructions": instructions
            }))
        }

        "tools/list" => reply(json!({ "tools": tool_definitions(&ensemble.agents(), memory) })),

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
            if let Some(result) = memory.and_then(|m| m.call(name, &args)) {
                let (text, is_error) = match result {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                return reply(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error
                }));
            }
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
/// Alongside, a worker runs the tasks in the state directory's `tasks/`, and,
/// with a hub's knowledge, another turns shares into notes.
/// When stdin closes, running tasks and background delegations are stopped
/// (tasks that had not started stay queued for the next session), and blocking
/// calls still running are finished before this returns.
pub fn serve<W: Write + Send>(
    ensemble: &Ensemble,
    input: impl BufRead,
    output: W,
) -> std::io::Result<()> {
    serve_with(ensemble, None, input, output)
}

/// [`serve`], with the knowledge of a hub when there is one: its tools are
/// offered.
pub fn serve_with<W: Write + Send>(
    ensemble: &Ensemble,
    memory: Option<&Memory>,
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
    let state = ensemble.state_dir()?;
    // One worker per folder: when another process (`ensemble serve`, or another
    // session) has it, this one reads and writes the tasks and runs none.
    let lock = ensemble_tasks::lock::acquire(&state);
    if let Err(pid) = &lock {
        eprintln!(
            "ensemble: process {pid} is running this folder's tasks; this session only reads and writes them"
        );
    }
    let worker = lock.is_ok();
    let mut store = Store::new(&state);
    if let Some(usage) = ensemble.usage_dir() {
        store = store.with_usage_dir(usage);
    }
    let queue = Queue::new(ensemble, Some(store));
    let (send, notify, running, queue) = (&send, &notify, &running, &queue);

    let knowledge_stop = &AtomicBool::new(false);
    std::thread::scope(|scope| {
        if worker {
            scope.spawn(move || queue.work(&spawner(scope)));
        }
        // Shares become notes alongside, unless another process does that.
        if let Some(memory) = memory {
            scope.spawn(move || memory.work(knowledge_stop, &|line| eprintln!("ensemble: {line}")));
        }
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
                                memory,
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
                    memory,
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
        knowledge_stop.store(true, Ordering::Relaxed);
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
mod tests;
