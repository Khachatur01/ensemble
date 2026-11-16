//! What a node and a hub server say to each other: JSON text frames over one
//! WebSocket the node opens, signed in with `Authorization: Bearer <token>`
//! when it connects.
//!
//! - The node asks: `{"id": 1, "method": "share", "params": {...}}`.
//! - The hub answers: `{"id": 1, "result": {...}}` or `{"id": 1, "error": "..."}`.
//! - The hub may also tell: `{"event": "...", "data": {...}}`, unasked: how
//!   tasks reach a node that has subscribed to them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who the node signed in as, what they may do, and the knowledge tools and
/// instructions their agent gets.
pub const HELLO: &str = "hello";
/// Keep knowledge in the organization vault: `{title, text, client, project}`.
pub const SHARE: &str = "share";
/// The notes about a topic: `{query}`.
pub const RECALL: &str = "recall";

/// Start receiving this account's tasks as events. Needs the `tasks`
/// permission.
pub const SUBSCRIBE: &str = "subscribe";
/// A task was made for a delivery: `{delivery, task}`.
pub const TASK_ACCEPT: &str = "task_accept";
/// This computer cannot take a delivery: `{delivery, why}`.
pub const TASK_REFUSE: &str = "task_refuse";
/// A task has something to say: `{delivery, question}` or
/// `{delivery, result: {status, text}}`.
pub const TASK_UPDATE: &str = "task_update";
/// An answer event was passed to its task: `{delivery}`.
pub const ANSWER_TAKEN: &str = "answer_taken";

/// Event: a task for this account, `{delivery, issue, title, body, project,
/// labels, assignee}`.
pub const TASK_EVENT: &str = "task";
/// Event: the person answered a task's question, `{delivery, text}`.
pub const ANSWER_EVENT: &str = "answer";

/// This connection is a node: `{name, access, agents: [{name, about}],
/// models: [names]}`. A shared node needs the `host` permission. Answered
/// with `{node}`, its `<owner>/<name>`.
pub const NODE: &str = "node";
/// The nodes this account may use: `{}`, answered with `{nodes: [{node,
/// access, agents, models, running}]}`.
pub const NODES: &str = "nodes";
/// Run an agent a node offers: `{agent, nodes, task, context, files}`.
/// Answered at once with `{run, node}`; then `run_progress` and `run_done`
/// events come on this connection.
pub const RUN: &str = "run";
/// Stop a run this connection started: `{run}`.
pub const RUN_CANCEL: &str = "run_cancel";
/// From the node running it: a line of what the agent is doing, `{run, line}`.
pub const RUN_PROGRESS: &str = "run_progress";
/// From the node running it: how it ended, `{run, ok, text | error, model,
/// prompt_tokens, completion_tokens, steps, tool_calls, commands, changes}`.
pub const RUN_DONE: &str = "run_done";
/// From a node answering a model call: `{call, status, content_type}` first,
/// then `{call, chunk}` (base64) as the server sends it, then `{call, done}`;
/// or `{call, error}`.
pub const MODEL_REPLY: &str = "model_reply";

/// Event: run this for someone, `{run, from, agent, task, context, files}`.
/// Also, on the connection that asked for a run: `run_progress` and
/// `run_done`, with the data the node sent.
pub const RUN_EVENT: &str = "run";
/// Event: the run was cancelled, or its caller went away, `{run}`.
pub const RUN_CANCEL_EVENT: &str = "run_cancel";
/// Event: a request for one of the node's models, `{call, from, path, body}`.
pub const MODEL_CALL_EVENT: &str = "model_call";

/// The most a WebSocket message may be: a run's files, base64, and some room.
pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;

/// What the link's WebSockets accept, on both ends.
pub fn socket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// Something the hub tells a node unasked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: Value,
}

/// What a hub server's task inbox offers the link, so the link knows nothing
/// of trackers: the events waiting for an account's computers, and what a node
/// says about its tasks.
pub trait TaskRelay: Sync {
    /// Events waiting for `user`'s computers, each with an id that names it
    /// across connections, so a connection sends each one once.
    fn pending(&self, user: &str) -> Vec<(String, Event)>;
    /// A node's request about its tasks: `task_accept`, `task_refuse`,
    /// `task_update` or `answer_taken`.
    fn handle(&self, user: &str, method: &str, params: &Value) -> Result<Value, String>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn ok(id: u64, result: Value) -> Reply {
        Reply {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Reply {
        Reply {
            id,
            result: None,
            error: Some(error.into()),
        }
    }
}
