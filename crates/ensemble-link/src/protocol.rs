//! What a node and a hub server say to each other: JSON text frames over one
//! WebSocket the node opens, signed in with `Authorization: Bearer <token>`
//! when it connects.
//!
//! - The node asks: `{"id": 1, "method": "share", "params": {...}}`.
//! - The hub answers: `{"id": 1, "result": {...}}` or `{"id": 1, "error": "..."}`.
//! - The hub may also tell: `{"event": "...", "data": {...}}`, unasked: a run
//!   or a model call for a node that offers them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who the node signed in as, what they may do, and the knowledge tools and
/// instructions their agent gets.
pub const HELLO: &str = "hello";
/// Keep knowledge in the organization vault: `{title, text, client, project}`.
pub const SHARE: &str = "share";
/// The notes about a topic: `{query}`.
pub const RECALL: &str = "recall";

/// The tasks the hub keeps for the signed-in account, by project (a working
/// folder's name). Each of these takes `{project, ...}` and needs the `tasks`
/// permission; `ensemble_tasks::board` says what each does.
///
/// Every task of the project: `{project}`, answered with `{tasks}`.
pub const TASKS: &str = "tasks";
/// One task, with its result, questions and hand-offs: `{project, id}`,
/// answered with `{task}`, null when there is none.
pub const TASK: &str = "task";
/// A new task: `{project, ticket}`, answered with `{id}`.
pub const TASK_CREATE: &str = "task_create";
/// Run a task on this node, for a while: `{project, id, node, seconds}`,
/// answered with `{claimed}` and, when not, `{why}`.
pub const TASK_CLAIM: &str = "task_claim";
/// Keep running it: `{project, id, node, seconds}`, answered with `{renewal}`:
/// `kept`, `cancel` (someone cancelled it) or `lost` (another node has it).
pub const TASK_RENEW: &str = "task_renew";
/// Stop running it, unfinished, for any node to run again: `{project, id,
/// node}`.
pub const TASK_RELEASE: &str = "task_release";
/// How a run ended: `{project, id, node, end}`.
pub const TASK_FINISH: &str = "task_finish";
/// Answer the question a task asked: `{project, id, text}`.
pub const TASK_ANSWER: &str = "task_answer";
/// Give a task to an agent: `{project, id, agent}`.
pub const TASK_ASSIGN: &str = "task_assign";
/// Cancel a task: `{project, id}`, answered with `{cancel}`: `before` it
/// started, `running` (its node stops it) or `ended` already.
pub const TASK_CANCEL: &str = "task_cancel";
/// Every plan of the project: `{project}`, answered with `{plans}`.
pub const PLANS: &str = "plans";
/// One plan: `{project, id}`, answered with `{plan}`, null when there is none.
pub const PLAN: &str = "plan";
/// A new plan, waiting for the person's approval: `{project, draft}`,
/// answered with `{id}`.
pub const PLAN_PROPOSE: &str = "plan_propose";
/// A plan changed: `{project, id, draft}`, answered with `{revision}`.
pub const PLAN_REVISE: &str = "plan_revise";
/// The person approves a plan, whose steps become tasks: `{project, id}`,
/// answered with `{plan}`.
pub const PLAN_APPROVE: &str = "plan_approve";
/// The person cancels a plan and its unfinished tasks: `{project, id}`,
/// answered with `{plan}`.
pub const PLAN_CANCEL: &str = "plan_cancel";
/// Every task and plan method, for the server to pass on.
pub const TASK_METHODS: [&str; 16] = [
    TASKS,
    TASK,
    TASK_CREATE,
    TASK_CLAIM,
    TASK_RENEW,
    TASK_RELEASE,
    TASK_FINISH,
    TASK_ANSWER,
    TASK_ASSIGN,
    TASK_CANCEL,
    PLANS,
    PLAN,
    PLAN_PROPOSE,
    PLAN_REVISE,
    PLAN_APPROVE,
    PLAN_CANCEL,
];

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

/// The hub's tasks, as the link passes requests to them, so the link knows
/// nothing of how they are kept.
pub trait TaskService: Sync {
    /// One of [`TASK_METHODS`], for `user`'s own tasks.
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
