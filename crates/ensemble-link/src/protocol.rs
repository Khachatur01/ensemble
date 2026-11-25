//! What a runner and a hub server say to each other: JSON text frames over one
//! WebSocket the runner opens at [`PATH`] on the hub's port, saying the
//! [`VERSION`] it speaks and, once it has one, signed in with
//! `Authorization: Bearer <token>` when it connects.
//! A connection without a token may only sign in ([`LOGIN`], [`JOIN`]).
//!
//! - The runner asks: `{"id": 1, "method": "share", "params": {...}}`.
//! - The hub answers: `{"id": 1, "result": {...}}` or `{"id": 1, "error": "..."}`.
//! - The hub may also tell: `{"event": "...", "data": {...}}`, unasked.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The version of what is said here. The runner sends it as the
/// [`VERSION_HEADER`] of every link it opens, and a hub refuses one of
/// another version, or of none, before anything else.
pub const VERSION: u32 = 11;
/// The path of a runner's link on the hub's port.
pub const PATH: &str = "/link";
/// The header of the upgrade that carries the runner's [`VERSION`].
pub const VERSION_HEADER: &str = "ensemble-protocol";

/// Why a hub that speaks `hub` refuses a runner that speaks `runner`, or
/// named none, and which of the two to update.
pub fn mismatch(hub: u32, runner: Option<u32>) -> String {
    match runner {
        Some(runner) if runner > hub => {
            format!("this hub speaks link protocol {hub}, the runner {runner}: update ensemble-hub")
        }
        Some(runner) => format!(
            "this hub speaks link protocol {hub}, the runner {runner}: update ensemble-runner"
        ),
        None => format!(
            "this hub speaks link protocol {hub}, and the runner named none: update ensemble-runner"
        ),
    }
}

/// On a connection without a token: sign in with an account's password,
/// `{name, password, client}`, answered with `{name, token}`, a new access
/// token of the client's own, named after it, such as `runner laptop`.
/// Failed logins lock the name as they do anywhere.
pub const LOGIN: &str = "login";
/// On a connection without a token: join with a code made for an account,
/// `{code, client}`, answered as [`LOGIN`] is. The code is spent.
pub const JOIN: &str = "join";

/// Who the runner signed in as, what they may do, and the knowledge tools and
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
/// One task, with its result and questions: `{project, id}`,
/// answered with `{task}`, null when there is none.
pub const TASK: &str = "task";
/// A new task: `{project, ticket}`, answered with `{id}`.
pub const TASK_CREATE: &str = "task_create";
/// Run a task on this runner, for a while: `{project, id, node, seconds}`,
/// answered with `{claimed}` and, when not, `{why}`.
pub const TASK_CLAIM: &str = "task_claim";
/// Keep running it: `{project, id, node, seconds}`, answered with `{renewal}`:
/// `kept`, `cancel` (someone cancelled it) or `lost` (another runner has it).
pub const TASK_RENEW: &str = "task_renew";
/// Stop running it, unfinished, for any runner to run again: `{project, id,
/// node}`.
pub const TASK_RELEASE: &str = "task_release";
/// How a run ended: `{project, id, node, end}`.
pub const TASK_FINISH: &str = "task_finish";
/// Answer the question a task asked: `{project, id, text}`.
pub const TASK_ANSWER: &str = "task_answer";
/// Give a task the gate its work must pass, or none: `{project, id, gate}`,
/// the gate `{command, timeout_seconds, attempts}` or null; not one that
/// runs or has ended.
pub const TASK_GATE: &str = "task_gate";
/// Cancel a task: `{project, id}`, answered with `{cancel}`: `before` it
/// started, `running` (its runner stops it) or `ended` already.
pub const TASK_CANCEL: &str = "task_cancel";
/// The account's tasks, in every project, that wait for a runner and that
/// this one has not given back: `{runner}`, answered with
/// `{tasks: [{project, task}]}`. A runner looks for the projects it has no
/// folder for.
pub const TASKS_WAITING: &str = "tasks_waiting";
/// Give a task back, for another runner: `{project, id, runner, why}`, such
/// as when this computer cannot reach its repository. Answered with
/// `{blocked}`: whether no connected runner that may take it is left to try.
pub const TASK_GIVE_BACK: &str = "task_give_back";
/// The projects the account has tasks in: `{}`, answered with `{projects:
/// [{project, tasks}]}`, each with how many tasks it has.
pub const TASK_PROJECTS: &str = "task_projects";
/// Every task method, for the server to pass on.
pub const TASK_METHODS: [&str; 13] = [
    TASKS,
    TASK,
    TASK_CREATE,
    TASK_CLAIM,
    TASK_RENEW,
    TASK_RELEASE,
    TASK_FINISH,
    TASK_ANSWER,
    TASK_GATE,
    TASK_CANCEL,
    TASKS_WAITING,
    TASK_GIVE_BACK,
    TASK_PROJECTS,
];

/// This connection is a runner, said once it connects: `{name, harnesses:
/// [{harness, models: [{model, tier}]}]}`, each of its harnesses that may run
/// with the models it can drive; none when it runs no task. Answered with
/// `{runner}`, its `<owner>/<name>`.
pub const RUNNER: &str = "runner";
/// The account's runners connected now: `{}`, answered with `{runners:
/// [{runner, harnesses, models}]}`, each with what it runs on, as [`RUNNER`]
/// says, and the models it shares, `[{provider, id, kind}]`, as its relay socket offered them.
pub const RUNNERS: &str = "runners";

/// The most a WebSocket message may be: a model call's request, and some
/// room.
pub const MAX_MESSAGE: usize = 24 * 1024 * 1024;

/// What the link's WebSockets accept, on both ends.
pub fn socket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// Something the hub tells a runner unasked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: Value,
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
