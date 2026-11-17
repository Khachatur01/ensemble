//! The harness interface: MCP over stdio.
//!
//! The agent that calls Ensemble (Claude Code, OpenCode, ...) spawns
//! `ensemble mcp` with the project as its working directory and sees the helper
//! team as tools: `agents`, `delegate`, `submit` and the tools that follow
//! tasks, `health`. With `--team` the session runs the team itself, and agents
//! work through the folder's tasks while it runs; without, it is a bridge to a
//! node, which serves each session with [`serve_session`].
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
//! on how it was asked. This crate owns only the JSON-RPC envelope: the node
//! (`src/daemon.rs`) runs a session with [`serve_session`] beside the
//! folder's queue.

use ensemble_agents::router::Ensemble;
use ensemble_knowledge::{KnowledgeAccess, RECALL, RECALL_HEADER, recalled_within};
use ensemble_tasks::delegation;
use ensemble_tasks::queue::Queue;
use ensemble_tasks::tasks::Store;
use serde_json::{Value, json};

use ensemble_agents::runtime;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

mod notify;
mod protocol;

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
/// this returns and there are no tasks; a node runs them alongside the rest
/// ([`serve_session`]). This waits for the answer, so it is for code that is not async.
pub fn handle(ensemble: &Ensemble, request: &Value) -> Option<Value> {
    handle_with(ensemble, None, request)
}

/// [`handle`], with the knowledge of a hub when there is one.
pub fn handle_with(
    ensemble: &Ensemble,
    memory: Option<&dyn KnowledgeAccess>,
    request: &Value,
) -> Option<Value> {
    let queue = queue_for(ensemble, None, memory);
    runtime::block_on(async {
        let reply = handle_reporting(
            ensemble,
            memory,
            request,
            &|_| {},
            &AtomicBool::new(false),
            &queue,
        )
        .await;
        queue.finish().await;
        reply
    })
}

/// [`handle`], with `notify` called for each notification to send to the client
/// before the reply, such as what a delegated agent is doing. Setting `cancel`
/// stops a delegation at its next step. Tasks and background delegations are
/// kept in `queue`, whose [`Queue::work`] runs them. With `memory`, the
/// knowledge tools are offered too.
pub async fn handle_reporting(
    ensemble: &Ensemble,
    memory: Option<&dyn KnowledgeAccess>,
    request: &Value,
    notify: &(dyn Fn(Value) + Sync),
    cancel: &AtomicBool,
    queue: &Queue<'_>,
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
                instructions.push_str(&memory.instructions().await);
            }
            reply(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false }, "logging": {} },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                "instructions": instructions
            }))
        }

        "tools/list" => {
            let tools = tool_definitions(&ensemble.agents(), memory).await;
            reply(json!({ "tools": tools }))
        }

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
            let known = match memory {
                Some(m) => m.call(name, &args).await,
                None => None,
            };
            if let Some(result) = known {
                let (text, is_error) = match result {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                return reply(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error
                }));
            }
            let count = AtomicU64::new(0);
            let result = delegation::call(
                ensemble,
                name,
                &args,
                &|line| {
                    let n = count.fetch_add(1, Ordering::Relaxed) + 1;
                    notify(progress_message(&token, n, line));
                    notify(log_message(line));
                },
                cancel,
                queue,
            )
            .await;
            reply(json!({
                "content": [{ "type": "text", "text": result.text }],
                "isError": result.is_error
            }))
        }

        "ping" => reply(json!({})),

        // The level itself is applied by `serve_session`, which owns the output.
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

/// One session: JSON-RPC from `input`, answered on `output`, until `input`
/// ends, with tasks and background delegations kept in `queue`, which
/// [`Queue::work`] runs alongside. With `close_at_end` the queue is closed when
/// the caller goes, which stops what runs in it; a node's queue is not, and
/// its work goes on. Blocking calls still running are finished before this
/// returns.
///
/// Each `tools/call` runs alongside the others, so the caller can delegate to
/// several agents at once and still get `ping` and `tools/list` answered while
/// they work. `notifications/cancelled` stops the named call at its next step,
/// and it gets no reply: the client has stopped waiting for one.
pub async fn serve_session(
    ensemble: &Ensemble,
    memory: Option<&dyn KnowledgeAccess>,
    queue: &Queue<'_>,
    input: impl AsyncBufRead + Unpin + Send,
    mut output: impl AsyncWrite + Unpin + Send,
    close_at_end: bool,
) -> std::io::Result<()> {
    // Everything for the client goes through one writer, so messages never
    // interleave, and a notification can be sent from code that is not async.
    let (out, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let read = async move {
        let min_level = AtomicUsize::new(rank(PROGRESS_LEVEL).unwrap_or(0));
        // Running calls by request id, so a cancellation can find its call.
        let running: Mutex<HashMap<String, Arc<AtomicBool>>> = Mutex::new(HashMap::new());
        let send = |message: Value| {
            // Only fails once the writer has stopped, and then nobody reads.
            let _ = out.send(message);
        };
        let notify = |n: Value| {
            // The level only filters log messages; progress is not one.
            let wanted = n["params"]["level"].as_str().and_then(rank);
            if wanted.is_none_or(|w| w >= min_level.load(Ordering::Relaxed)) {
                send(n);
            }
        };
        let (send, notify, running) = (&send, &notify, &running);
        let mut lines = input.lines();
        let mut calls = FuturesUnordered::new();
        let result = loop {
            let line = tokio::select! {
                line = lines.next_line() => line,
                Some(()) = calls.next(), if !calls.is_empty() => continue,
            };
            let line = match line {
                Ok(Some(line)) => line,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            if line.trim().is_empty() {
                continue;
            }
            let request = match serde_json::from_str::<Value>(&line) {
                Ok(request) => request,
                Err(e) => {
                    send(json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("parse error: {e}") }
                    }));
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
                    calls.push(async move {
                        let response =
                            handle_reporting(ensemble, memory, &request, notify, &cancel, queue)
                                .await;
                        running.lock().unwrap().remove(&key);
                        if !cancel.load(Ordering::Relaxed)
                            && let Some(response) = response
                        {
                            send(response);
                        }
                    });
                    continue;
                }
                _ => {}
            }
            let response = handle_reporting(
                ensemble,
                memory,
                &request,
                notify,
                &AtomicBool::new(false),
                queue,
            )
            .await;
            if let Some(response) = response {
                send(response);
            }
        };
        // The caller is gone, so nobody will ask for what background work
        // produces: stop it, or this would wait out its time limits.
        if close_at_end {
            queue.close();
        }
        while calls.next().await.is_some() {}
        result
    };
    // Flush per message. A buffered reply is a reply the harness is still
    // waiting for, and it looks exactly like a hung server. Once a write
    // fails, the rest is dropped: the client is gone.
    let write = async {
        let mut failed = None;
        while let Some(message) = outgoing.recv().await {
            if failed.is_none() {
                let line = format!("{message}\n");
                let written = match output.write_all(line.as_bytes()).await {
                    Ok(()) => output.flush().await,
                    Err(e) => Err(e),
                };
                failed = written.err();
            }
        }
        failed.map_or(Ok(()), Err)
    };
    let (read, written) = tokio::join!(read, write);
    read.and(written)
}

/// A queue of `ensemble` whose agents with `recall` are given notes from
/// `memory`, when the session has a hub that keeps knowledge.
pub fn queue_for<'s>(
    ensemble: &'s Ensemble,
    store: Option<Store>,
    memory: Option<&'s dyn KnowledgeAccess>,
) -> Queue<'s> {
    let queue = Queue::new(ensemble, store);
    let Some(memory) = memory else {
        return queue;
    };
    queue.with_recall(move |topic, max_chars| {
        Box::pin(async move {
            match memory.call(RECALL, &json!({ "query": topic })).await {
                Some(Ok(text)) => Ok(text
                    .starts_with(RECALL_HEADER)
                    .then(|| recalled_within(&text, max_chars))),
                Some(Err(e)) => Err(e),
                None => Err("this session's hub offers no recall".into()),
            }
        })
    })
}

/// Say once, on stderr, that agents with `recall` get no notes in a session
/// without a hub that keeps knowledge.
pub fn note_recall_without_knowledge(ensemble: &Ensemble, memory: Option<&dyn KnowledgeAccess>) {
    let wanting: Vec<&str> = ensemble
        .config()
        .agents
        .values()
        .filter(|a| a.recall.is_some())
        .map(|a| a.name.as_str())
        .collect();
    if memory.is_none() && !wanting.is_empty() {
        eprintln!(
            "ensemble: note: {} {} `recall`, but this node names no hub (`hub:` in node.yaml), so no notes are given",
            wanting.join(", "),
            if wanting.len() == 1 { "has" } else { "have" }
        );
    }
}

#[cfg(test)]
mod tests;
