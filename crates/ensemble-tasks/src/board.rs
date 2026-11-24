//! Where tasks are kept: on the hub, per account and project (a
//! repository's name), so that a task outlives the runner that runs it.
//!
//! A runner does not own a task; it **leases** it. It claims a task that may
//! start for a while ([`LEASE`]), renews the claim as it runs, and says how the
//! run ended: a result, a question, or a hand-off to another agent. A runner
//! that stops cleanly puts its tasks back; one that dies stops renewing, its
//! lease runs out, and any runner of the person with the agent runs the task
//! again. A runner whose lease ran out and was taken cannot write a result, so a
//! task is never done twice.
//!
//! Where the work is decides who may go on with it. A task whose run works on
//! a branch that is pushed (a worktree agent, a repository with a remote) is
//! **movable**: when its run stops, the runner says where its branch is
//! ([`Progress`]), and the next runner fetches it and carries on. Any other task
//! changes one computer's folder, so once it has run there it is **pinned**
//! to that runner, and waits for it.
//!
//! A runner that cannot work on a task where it is, such as one whose
//! repository its computer cannot reach, **gives it back** with why, and does
//! not take it again; the hub offers it to the account's other runners, and
//! when every one connected that may take it gave it back, the task is
//! **blocked** until one that has not connects, or it is assigned or answered
//! again.
//!
//! [`Board`] is what the queue uses; a runner reaches the hub's tasks over the
//! link ([`RemoteBoard`]). The hub keeps them in its database
//! (`ensemble_hub::board`).

use crate::tasks::{Detail, Status, Task};
use async_trait::async_trait;
use ensemble_link::protocol::{
    TASK, TASK_CLAIM, TASK_FINISH, TASK_GIVE_BACK, TASK_RELEASE, TASK_RENEW, TASKS, TASKS_WAITING,
};
use ensemble_link::{Link, Token};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// How long a claim lasts unless renewed: a runner that dies gives its tasks
/// up within this.
pub const LEASE: Duration = Duration::from_secs(60);

/// Whether a runner may run a task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Claim {
    /// It is this runner's until its lease runs out.
    Taken,
    /// It is not, and why: it runs elsewhere, waits, or has ended.
    Refused(String),
}

/// What renewing a lease found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Renewal {
    /// Go on.
    Kept,
    /// Someone cancelled the task: stop, and finish it as cancelled.
    Cancel,
    /// The lease ran out and the task is another's now: stop, and say nothing.
    Lost,
}

/// Where a task's work was pushed when a run of it last stopped.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Progress {
    /// The runner that pushed it.
    pub node: String,
    pub branch: String,
    pub commit: String,
}

/// What cancelling a task did.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cancelled {
    /// It had not started, and is cancelled.
    Before,
    /// It runs on this runner, which stops it.
    Running(String),
    /// It had ended already.
    Ended,
}

/// How a run of a task ended.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum End {
    /// The task is over: `extra` goes in the result's front matter after the
    /// usual (`status`, `agent`, `started`, `ended`), such as its `branch`.
    Result {
        status: Status,
        agent: String,
        /// When the run started, as UTC.
        started: String,
        extra: Vec<(String, String)>,
        text: String,
    },
    /// The agent stopped to ask; the task waits for the answer. Asked by the
    /// runner, not the agent, when it has no copy of the project and nowhere to
    /// clone it from: then the answer is where the project's `repository` is.
    Ask {
        question: String,
        reply: String,
        #[serde(default)]
        repository: bool,
    },
    /// The agent handed the task to another agent, with a note.
    Handoff {
        from: String,
        to: String,
        note: String,
        reply: String,
    },
}

/// One project's tasks, wherever they are kept.
#[async_trait]
pub trait Board: Send + Sync {
    /// Where they are, for messages: "the hub wss://…".
    fn place(&self) -> String;
    /// Every task, oldest first.
    async fn scan(&self) -> Result<Vec<Task>, String>;
    /// One task with what it said, if there is one.
    async fn get(&self, id: &str) -> Result<Option<Detail>, String>;
    /// Run task `id` on `node` for `lease`, if it may start. Unless the run is
    /// `movable`, the task stays with `node` from now on.
    async fn claim(
        &self,
        id: &str,
        node: &str,
        lease: Duration,
        movable: bool,
    ) -> Result<Claim, String>;
    /// Keep running it for another `lease`.
    async fn renew(&self, id: &str, node: &str, lease: Duration) -> Result<Renewal, String>;
    /// Stop running it, unfinished, with its work pushed as `progress`: a
    /// runner runs it again.
    async fn release(&self, id: &str, node: &str, progress: Option<Progress>)
    -> Result<(), String>;
    /// Say how a run of it ended, with its work pushed as `progress`.
    async fn finish(
        &self,
        id: &str,
        node: &str,
        end: End,
        progress: Option<Progress>,
    ) -> Result<(), String>;
    /// Give it back, for another runner, saying `why`: `node` does not take it
    /// again. Whether it is blocked now: no connected runner with its agent is
    /// left to try.
    async fn give_back(&self, id: &str, node: &str, why: &str) -> Result<bool, String>;
}

/// One project's tasks on the hub, over the link.
pub struct RemoteBoard {
    link: Link,
    project: String,
}

impl RemoteBoard {
    /// The tasks of `project`, a working folder's name, on the hub at `url`.
    pub fn new(url: &str, token: &str, project: &str) -> Self {
        Self::with_token(url, Token::new(token), project)
    }

    /// The same, following `token` when it is replaced.
    pub fn with_token(url: &str, token: Token, project: &str) -> Self {
        RemoteBoard {
            link: Link::with_token(url, token),
            project: project.to_string(),
        }
    }

    async fn ask(&self, method: &str, mut params: Value) -> Result<Value, String> {
        params["project"] = json!(self.project);
        self.link.ask(method, params).await
    }
}

/// `value[key]`, read as a `T`.
fn read<T: serde::de::DeserializeOwned>(value: &Value, key: &str) -> Result<T, String> {
    serde_json::from_value(value.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("the hub answered something else for `{key}`: {e}"))
}

#[async_trait]
impl Board for RemoteBoard {
    fn place(&self) -> String {
        format!("the hub {}", self.link.url())
    }
    async fn scan(&self) -> Result<Vec<Task>, String> {
        read(&self.ask(TASKS, json!({})).await?, "tasks")
    }
    async fn get(&self, id: &str) -> Result<Option<Detail>, String> {
        read(&self.ask(TASK, json!({ "id": id })).await?, "task")
    }
    async fn claim(
        &self,
        id: &str,
        node: &str,
        lease: Duration,
        movable: bool,
    ) -> Result<Claim, String> {
        let params = json!({
            "id": id,
            "node": node,
            "seconds": lease.as_secs().max(1),
            "movable": movable,
        });
        read(&self.ask(TASK_CLAIM, params).await?, "claim")
    }
    async fn renew(&self, id: &str, node: &str, lease: Duration) -> Result<Renewal, String> {
        let params = json!({ "id": id, "node": node, "seconds": lease.as_secs().max(1) });
        read(&self.ask(TASK_RENEW, params).await?, "renewal")
    }
    async fn release(
        &self,
        id: &str,
        node: &str,
        progress: Option<Progress>,
    ) -> Result<(), String> {
        let params = json!({ "id": id, "node": node, "progress": progress });
        self.ask(TASK_RELEASE, params).await.map(|_| ())
    }
    async fn finish(
        &self,
        id: &str,
        node: &str,
        end: End,
        progress: Option<Progress>,
    ) -> Result<(), String> {
        let params = json!({ "id": id, "node": node, "end": end, "progress": progress });
        self.ask(TASK_FINISH, params).await.map(|_| ())
    }
    async fn give_back(&self, id: &str, node: &str, why: &str) -> Result<bool, String> {
        let params = json!({ "id": id, "runner": node, "why": why });
        read(&self.ask(TASK_GIVE_BACK, params).await?, "blocked")
    }
}

/// A task that waits for one of a runner's agents: its project, and the task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Waiting {
    pub project: String,
    pub task: Task,
}

/// The tasks, in every project, that wait for one of `agents` and that
/// `runner` has not given back, over `link`: what a runner looks at for the
/// projects it has no folder for.
pub async fn waiting(link: &Link, runner: &str, agents: &[String]) -> Result<Vec<Waiting>, String> {
    let params = json!({ "runner": runner, "agents": agents });
    read(&link.ask(TASKS_WAITING, params).await?, "tasks")
}
