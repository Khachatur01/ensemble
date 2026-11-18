//! Where tasks are kept: on the hub, per account and project (a working
//! folder's name), so that a task outlives the node that runs it.
//!
//! A node does not own a task; it **leases** it. It claims a task that may
//! start for a while ([`LEASE`]), renews the claim as it runs, and says how the
//! run ended: a result, a question, or a hand-off to another agent. A node
//! that stops cleanly puts its tasks back; one that dies stops renewing, its
//! lease runs out, and any node of the person with the agent runs the task
//! again. A node whose lease ran out and was taken cannot write a result, so a
//! task is never done twice.
//!
//! Where the work is decides who may go on with it. A task whose run works on
//! a branch that is pushed (a worktree agent, a repository with a remote) is
//! **movable**: when its run stops, the node says where its branch is
//! ([`Progress`]), and the next node fetches it and carries on. Any other task
//! changes one computer's folder, so once it has run there it is **pinned**
//! to that node, and waits for it.
//!
//! [`Board`] is what the queue and the tools use. The hub keeps its tasks in
//! its database ([`DbBoard`]) and answers the link's task methods with them
//! ([`Service`]); a node reaches them over the link ([`RemoteBoard`]).

use crate::tasks::{Detail, Outcome, Status, Task, Ticket, slug, utc, yaml_str};
use async_trait::async_trait;
use ensemble_core::runtime::blocking;
use ensemble_link::protocol::{
    TASK, TASK_ANSWER, TASK_ASSIGN, TASK_CANCEL, TASK_CLAIM, TASK_CREATE, TASK_FINISH,
    TASK_RELEASE, TASK_RENEW, TASKS, TaskService,
};
use ensemble_link::store::{self, Value as Sql};
use ensemble_link::{Link, Token};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// How long a claim lasts unless renewed: a node that dies gives its tasks
/// up within this.
pub const LEASE: Duration = Duration::from_secs(60);

/// Whether a node may run a task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Claim {
    /// It is this node's until its lease runs out.
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
    /// The node that pushed it.
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
    /// It runs on this node, which stops it.
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
    /// The agent stopped to ask; the task waits for the answer.
    Ask { question: String, reply: String },
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
    /// Keep `ticket` as a new task, and return its id. Its own id is not used.
    async fn create(&self, ticket: &Ticket) -> Result<String, String>;
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
    /// node runs it again.
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
    /// Answer the question it waits on.
    async fn answer(&self, id: &str, text: &str) -> Result<(), String>;
    /// Give it to `agent`, from the backlog or from whoever has it.
    async fn assign(&self, id: &str, agent: &str) -> Result<(), String>;
    async fn cancel(&self, id: &str) -> Result<Cancelled, String>;
}

// ---------------------------------------------------------------------------
// On the hub
// ---------------------------------------------------------------------------

/// A task as the hub keeps it.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Record {
    ticket: Ticket,
    created: String,
    questions: Vec<Question>,
    /// The reply that last asked.
    reply: String,
    /// What agents handed on, as the next agent reads it.
    handoffs: String,
    lease: Option<Lease>,
    interrupted_on: Option<String>,
    /// The node whose folder its work is in, when that work cannot move.
    pinned_to: Option<String>,
    /// Where its work was pushed when a run last stopped.
    progress: Option<Progress>,
    /// Asked to stop while it ran.
    cancel: bool,
    result: Option<Outcome>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Question {
    asked: String,
    text: String,
    answer: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Lease {
    node: String,
    /// Until when, in seconds since the epoch, by the hub's clock.
    until: u64,
}

impl Record {
    /// The node whose lease has not run out.
    fn holder(&self, now: u64) -> Option<&str> {
        self.lease
            .as_ref()
            .filter(|l| l.until > now)
            .map(|l| l.node.as_str())
    }

    fn open_question(&self) -> Option<&Question> {
        self.questions.last().filter(|q| q.answer.is_none())
    }

    fn task(&self, now: u64) -> Task {
        Task {
            ticket: self.ticket.clone(),
            status: self.result.as_ref().map(|r| r.status),
            question: self.open_question().map(|q| q.text.clone()),
            running_on: self
                .holder(now)
                .filter(|_| self.result.is_none())
                .map(str::to_string),
            interrupted_on: match (&self.lease, &self.interrupted_on) {
                // Its node stopped renewing.
                (Some(l), _) if l.until <= now && self.result.is_none() => Some(l.node.clone()),
                (None, Some(node)) if self.result.is_none() => Some(node.clone()),
                _ => None,
            },
            pinned_to: self.pinned_to.clone(),
        }
    }

    fn detail(&self, now: u64) -> Detail {
        let conversation = (!self.questions.is_empty()).then(|| {
            let mut text = String::new();
            for (n, q) in self.questions.iter().enumerate() {
                text.push_str(&format!(
                    "## Question {} ({})\n{}\n\n",
                    n + 1,
                    q.asked,
                    q.text
                ));
                if let Some(a) = &q.answer {
                    text.push_str(&format!("### Answer\n{a}\n\n"));
                }
            }
            (text, self.reply.clone())
        });
        Detail {
            task: self.task(now),
            outcome: self.result.clone(),
            conversation,
            handoffs: (!self.handoffs.is_empty()).then(|| self.handoffs.clone()),
            progress: self.progress.clone(),
        }
    }
}

/// Held while a task is read, changed and written, so two requests at once
/// cannot lose a change. Only the hub's process writes its tasks.
static CHANGING: Mutex<()> = Mutex::new(());

/// One project's tasks of one account, in the hub's database in `dir`.
#[derive(Debug, Clone)]
pub struct DbBoard {
    dir: PathBuf,
    owner: String,
    project: String,
}

impl DbBoard {
    pub fn new(dir: &Path, owner: &str, project: &str) -> Self {
        DbBoard {
            dir: dir.to_path_buf(),
            owner: owner.to_string(),
            project: project.to_string(),
        }
    }

    fn keys(&self) -> Vec<Sql> {
        vec![
            Sql::Text(self.owner.clone()),
            Sql::Text(self.project.clone()),
        ]
    }

    /// Every task's record, oldest first.
    fn records(&self) -> Result<Vec<Record>, String> {
        let rows = store::with(&self.dir, async |c| {
            store::rows(
                c,
                "SELECT data FROM tasks WHERE owner = ?1 AND project = ?2 ORDER BY number",
                self.keys(),
            )
            .await
        })?;
        rows.iter()
            .map(|row| {
                let data = store::text(&row[0]).unwrap_or_default();
                serde_json::from_str(&data).map_err(|e| format!("a task is unreadable: {e}"))
            })
            .collect()
    }

    fn record(&self, id: &str) -> Result<Option<Record>, String> {
        let mut keys = self.keys();
        keys.push(Sql::Text(id.to_string()));
        let rows = store::with(&self.dir, async |c| {
            store::rows(
                c,
                "SELECT data FROM tasks WHERE owner = ?1 AND project = ?2 AND id = ?3",
                keys,
            )
            .await
        })?;
        rows.first()
            .map(|row| {
                let data = store::text(&row[0]).unwrap_or_default();
                serde_json::from_str(&data).map_err(|e| format!("task {id} is unreadable: {e}"))
            })
            .transpose()
    }

    /// Change task `id` with `change`, which says what to answer; nothing is
    /// written when it fails.
    fn change<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut Record, u64) -> Result<T, String>,
    ) -> Result<T, String> {
        let _held = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = self.record(id)?.ok_or_else(|| format!("no task '{id}'"))?;
        let out = change(&mut record, unix_now())?;
        let data = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        let mut params = vec![Sql::Text(data)];
        params.extend(self.keys());
        params.push(Sql::Text(id.to_string()));
        store::with(&self.dir, async |c| {
            store::execute(
                c,
                "UPDATE tasks SET data = ?1 WHERE owner = ?2 AND project = ?3 AND id = ?4",
                params,
            )
            .await
        })?;
        Ok(out)
    }

    /// Every task, oldest first.
    pub fn list(&self) -> Result<Vec<Task>, String> {
        let now = unix_now();
        Ok(self.records()?.iter().map(|r| r.task(now)).collect())
    }

    pub fn detail(&self, id: &str) -> Result<Option<Detail>, String> {
        Ok(self.record(id)?.map(|r| r.detail(unix_now())))
    }

    /// Keep `ticket` as a new task: the next number, then a slug of its title.
    pub fn add(&self, ticket: &Ticket) -> Result<String, String> {
        ticket.check()?;
        let _held = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
        let records = self.records()?;
        if let Some(missing) = ticket
            .after
            .iter()
            .find(|a| !records.iter().any(|r| &r.ticket.id == *a))
        {
            return Err(format!(
                "`after` names '{missing}', which is not a task. Tasks are named by the id `submit` returned."
            ));
        }
        let number = records
            .iter()
            .filter_map(|r| {
                let digits: String = r
                    .ticket
                    .id
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse::<i64>().ok()
            })
            .max()
            .map_or(1, |n| n + 1);
        let slug = slug(&ticket.title);
        let id = if slug.is_empty() {
            format!("{number:04}")
        } else {
            format!("{number:04}-{slug}")
        };
        let record = Record {
            ticket: Ticket {
                id: id.clone(),
                title: ticket.title.trim().to_string(),
                agent: ticket.agent.trim().to_string(),
                text: ticket.text.trim().to_string(),
                ..ticket.clone()
            },
            created: utc(SystemTime::now()),
            questions: Vec::new(),
            reply: String::new(),
            handoffs: String::new(),
            lease: None,
            interrupted_on: None,
            pinned_to: None,
            progress: None,
            cancel: false,
            result: None,
        };
        let data = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        let mut params = self.keys();
        params.extend([Sql::Text(id.clone()), Sql::Integer(number), Sql::Text(data)]);
        store::with(&self.dir, async |c| {
            store::execute(
                c,
                "INSERT INTO tasks (owner, project, id, number, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params,
            )
            .await
        })?;
        Ok(id)
    }

    /// Lease task `id` to `node` for `seconds`, if it may start: assigned,
    /// not ended, not waiting on an answer or another task, not running on
    /// another node, and not pinned to one. A node that claims a task it held
    /// before, such as after it started again, gets it back. Unless the run
    /// is `movable`, the task is pinned to `node`.
    pub fn take(&self, id: &str, node: &str, seconds: u64, movable: bool) -> Result<Claim, String> {
        let done: Vec<String> = self
            .records()?
            .into_iter()
            .filter(|r| r.result.as_ref().is_some_and(|o| o.status == Status::Done))
            .map(|r| r.ticket.id)
            .collect();
        self.change(id, |r, now| {
            let refused = |why: &str| Ok(Claim::Refused(why.to_string()));
            if r.result.is_some() {
                return refused("it has ended");
            }
            if r.ticket.agent.is_empty() {
                return refused("it is in the backlog");
            }
            if r.open_question().is_some() {
                return refused("it waits for an answer");
            }
            if let Some(dep) = r.ticket.after.iter().find(|a| !done.contains(a)) {
                return refused(&format!("it waits for {dep}"));
            }
            if let Some(holder) = r.holder(now)
                && holder != node
            {
                return refused(&format!("it runs on {holder}"));
            }
            if let Some(pinned) = r.pinned_to.as_ref().filter(|p| *p != node) {
                return refused(&format!(
                    "its work is in {pinned}'s folder, so it waits for {pinned}"
                ));
            }
            r.lease = Some(Lease {
                node: node.to_string(),
                until: now + seconds,
            });
            r.interrupted_on = None;
            if !movable {
                r.pinned_to = Some(node.to_string());
            }
            Ok(Claim::Taken)
        })
    }

    /// Keep `node`'s lease of task `id` for another `seconds`. A lease that
    /// ran out is kept too while nobody else took the task.
    pub fn keep(&self, id: &str, node: &str, seconds: u64) -> Result<Renewal, String> {
        self.change(id, |r, now| {
            let ours = r.lease.as_ref().is_some_and(|l| l.node == node);
            if !ours || r.result.is_some() {
                return Ok(Renewal::Lost);
            }
            r.lease = Some(Lease {
                node: node.to_string(),
                until: now + seconds,
            });
            Ok(if r.cancel {
                Renewal::Cancel
            } else {
                Renewal::Kept
            })
        })
    }

    /// `node` stops running task `id`, unfinished, its work pushed as
    /// `progress`. One that was cancelled meanwhile ends as cancelled.
    pub fn put_back(&self, id: &str, node: &str, progress: Option<Progress>) -> Result<(), String> {
        self.change(id, |r, _| {
            if r.lease.as_ref().is_none_or(|l| l.node != node) {
                return Ok(());
            }
            r.lease = None;
            if progress.is_some() {
                r.progress = progress;
            }
            if r.cancel {
                r.result = Some(outcome(
                    Status::Cancelled,
                    &r.ticket.agent,
                    &utc(SystemTime::now()),
                    &[],
                    "Cancelled while it ran.",
                ));
            } else {
                r.interrupted_on = Some(node.to_string());
            }
            Ok(())
        })
    }

    /// How `node`'s run of task `id` ended, its work pushed as `progress`.
    /// Refused when the task is not `node`'s any more.
    pub fn end(
        &self,
        id: &str,
        node: &str,
        end: End,
        progress: Option<Progress>,
    ) -> Result<(), String> {
        self.change(id, |r, _| {
            match &r.lease {
                Some(l) if l.node == node && r.result.is_none() => {}
                Some(l) if r.result.is_none() => {
                    return Err(format!("{id} runs on {} now", l.node));
                }
                _ => return Err(format!("{id} is not running on {node}")),
            }
            r.lease = None;
            if progress.is_some() {
                r.progress = progress;
            }
            match end {
                End::Result {
                    status,
                    agent,
                    started,
                    extra,
                    text,
                } => {
                    r.result = Some(outcome(status, &agent, &started, &extra, &text));
                    r.cancel = false;
                }
                End::Ask { question, reply } => {
                    r.questions.push(Question {
                        asked: utc(SystemTime::now()),
                        text: question.trim().to_string(),
                        answer: None,
                    });
                    r.reply = reply;
                }
                End::Handoff {
                    from,
                    to,
                    note,
                    reply,
                } => {
                    r.handoffs.push_str(&format!(
                        "## {from} handed this to {to}\n{}\n\nWhat {from} had said:\n{}\n\n",
                        if note.trim().is_empty() {
                            "(no note)"
                        } else {
                            note.trim()
                        },
                        reply.trim()
                    ));
                    r.ticket.agent = to;
                }
            }
            Ok(())
        })
    }

    /// Answer the question task `id` waits on.
    pub fn reply(&self, id: &str, text: &str) -> Result<(), String> {
        self.change(id, |r, _| {
            let Some(q) = r.questions.last_mut().filter(|q| q.answer.is_none()) else {
                return Err(format!("{id} has no question waiting for an answer."));
            };
            q.answer = Some(text.trim().to_string());
            Ok(())
        })
    }

    /// Give task `id` to `agent`; not one that runs or has ended.
    pub fn give(&self, id: &str, agent: &str) -> Result<(), String> {
        self.change(id, |r, now| {
            if r.result.is_some() {
                return Err(format!(
                    "{id} has ended; it cannot be assigned. Submit it again."
                ));
            }
            if r.holder(now).is_some() {
                return Err(format!("{id} is running; assign it when it stops."));
            }
            r.ticket.agent = agent.trim().to_string();
            Ok(())
        })
    }

    /// Cancel task `id`: at once when it has not started, by its node when it
    /// runs.
    pub fn stop(&self, id: &str) -> Result<Cancelled, String> {
        self.change(id, |r, now| {
            if r.result.is_some() {
                return Ok(Cancelled::Ended);
            }
            if let Some(holder) = r.holder(now).map(str::to_string) {
                r.cancel = true;
                return Ok(Cancelled::Running(holder));
            }
            r.lease = None;
            r.result = Some(outcome(
                Status::Cancelled,
                &r.ticket.agent,
                &utc(SystemTime::now()),
                &[],
                "Cancelled before it started.",
            ));
            Ok(Cancelled::Before)
        })
    }
}

/// A result: front matter, then the reply.
fn outcome(
    status: Status,
    agent: &str,
    started: &str,
    extra: &[(String, String)],
    reply: &str,
) -> Outcome {
    let extra: String = extra
        .iter()
        .map(|(key, value)| format!("{key}: {value}\n"))
        .collect();
    Outcome {
        status,
        text: format!(
            "---\nstatus: {}\nagent: {}\nstarted: {started}\nended: {}\n{extra}---\n{}\n",
            status.as_str(),
            yaml_str(agent),
            utc(SystemTime::now()),
            reply.trim_end()
        ),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[async_trait]
impl Board for DbBoard {
    fn place(&self) -> String {
        format!("the hub's database in {}", self.dir.display())
    }
    async fn scan(&self) -> Result<Vec<Task>, String> {
        blocking(|| self.list())
    }
    async fn get(&self, id: &str) -> Result<Option<Detail>, String> {
        blocking(|| self.detail(id))
    }
    async fn create(&self, ticket: &Ticket) -> Result<String, String> {
        blocking(|| self.add(ticket))
    }
    async fn claim(
        &self,
        id: &str,
        node: &str,
        lease: Duration,
        movable: bool,
    ) -> Result<Claim, String> {
        blocking(|| self.take(id, node, lease.as_secs().max(1), movable))
    }
    async fn renew(&self, id: &str, node: &str, lease: Duration) -> Result<Renewal, String> {
        blocking(|| self.keep(id, node, lease.as_secs().max(1)))
    }
    async fn release(
        &self,
        id: &str,
        node: &str,
        progress: Option<Progress>,
    ) -> Result<(), String> {
        blocking(|| self.put_back(id, node, progress))
    }
    async fn finish(
        &self,
        id: &str,
        node: &str,
        end: End,
        progress: Option<Progress>,
    ) -> Result<(), String> {
        blocking(|| self.end(id, node, end, progress))
    }
    async fn answer(&self, id: &str, text: &str) -> Result<(), String> {
        blocking(|| self.reply(id, text))
    }
    async fn assign(&self, id: &str, agent: &str) -> Result<(), String> {
        blocking(|| self.give(id, agent))
    }
    async fn cancel(&self, id: &str) -> Result<Cancelled, String> {
        blocking(|| self.stop(id))
    }
}

/// What is done with a task that ended done: given its account, its project
/// and the task.
pub type OnDone = dyn Fn(&str, &str, &Detail) + Send + Sync;

/// The hub's side of the link's task methods: each for the signed-in
/// account's own tasks, in the hub's database in `dir`.
pub struct Service {
    dir: PathBuf,
    done: Option<Box<OnDone>>,
}

impl Service {
    pub fn new(dir: &Path) -> Self {
        Service {
            dir: dir.to_path_buf(),
            done: None,
        }
    }

    /// Give each task that ends done to `done`, as the hub gives it to its
    /// knowledge.
    pub fn on_done(mut self, done: impl Fn(&str, &str, &Detail) + Send + Sync + 'static) -> Self {
        self.done = Some(Box::new(done));
        self
    }
}

/// The most of a task's report shared, in characters.
const MAX_LEARNED: usize = 20_000;

/// A task that ended done, as knowledge to share: a title and a text saying
/// what was asked, what was asked and answered on the way, and what its agent
/// reported. None for a task not done.
pub fn learned(project: &str, task: &Detail) -> Option<(String, String)> {
    let outcome = task.outcome.as_ref()?;
    if outcome.status != Status::Done {
        return None;
    }
    let t = &task.task.ticket;
    let mut text = format!(
        "What the helper agent {} reported when it had done task {} in the project {project}.\n\nAsked: {}\n",
        t.agent, t.id, t.title
    );
    if let Some(goal) = &t.goal {
        text.push_str(&format!("Goal: {goal}\n"));
    }
    if let Some(context) = &t.context {
        text.push_str(&format!("Context: {context}\n"));
    }
    for check in &t.done_when {
        text.push_str(&format!("Done when: {check}\n"));
    }
    if let Some((asked, _)) = &task.conversation {
        text.push_str(&format!(
            "\nAsked and answered on the way:\n{}\n",
            asked.trim()
        ));
    }
    let report = outcome.reply().trim();
    let cut: String = report.chars().take(MAX_LEARNED).collect();
    let more = if cut.len() < report.len() {
        "\n[...]"
    } else {
        ""
    };
    text.push_str(&format!("\nIts report:\n{cut}{more}\n"));
    Some((t.title.clone(), text))
}

impl TaskService for Service {
    fn handle(&self, user: &str, method: &str, params: &Value) -> Result<Value, String> {
        let text = |key: &str| -> Result<&str, String> {
            params
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("{method}: `{key}` is required"))
        };
        let parsed = |key: &str| -> Result<Value, String> {
            params
                .get(key)
                .cloned()
                .ok_or_else(|| format!("{method}: `{key}` is required"))
        };
        let seconds = || {
            params
                .get("seconds")
                .and_then(Value::as_u64)
                .unwrap_or(LEASE.as_secs())
        };
        let progress = || -> Result<Option<Progress>, String> {
            match params.get("progress") {
                None | Some(Value::Null) => Ok(None),
                Some(p) => serde_json::from_value(p.clone())
                    .map(Some)
                    .map_err(|e| format!("{method}: `progress` is not where work is: {e}")),
            }
        };
        let project = text("project")?;
        if project.contains(['/', '\\']) {
            return Err(format!("{method}: '{project}' is not a project name"));
        }
        let board = DbBoard::new(&self.dir, user, project);
        match method {
            TASKS => Ok(json!({ "tasks": board.list()? })),
            TASK => Ok(json!({ "task": board.detail(text("id")?)? })),
            TASK_CREATE => {
                let ticket: Ticket = serde_json::from_value(parsed("ticket")?)
                    .map_err(|e| format!("{method}: `ticket` is not a ticket: {e}"))?;
                Ok(json!({ "id": board.add(&ticket)? }))
            }
            TASK_CLAIM => {
                let movable = params
                    .get("movable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let claim = board.take(text("id")?, text("node")?, seconds(), movable)?;
                Ok(json!({ "claim": claim }))
            }
            TASK_RENEW => {
                let renewal = board.keep(text("id")?, text("node")?, seconds())?;
                Ok(json!({ "renewal": renewal }))
            }
            TASK_RELEASE => {
                board.put_back(text("id")?, text("node")?, progress()?)?;
                Ok(json!({ "released": true }))
            }
            TASK_FINISH => {
                let end: End = serde_json::from_value(parsed("end")?)
                    .map_err(|e| format!("{method}: `end` is not how a run ends: {e}"))?;
                let over = matches!(end, End::Result { .. });
                let id = text("id")?;
                board.end(id, text("node")?, end, progress()?)?;
                if over
                    && let Some(done) = &self.done
                    && let Some(task) = board.detail(id)?
                    && task
                        .outcome
                        .as_ref()
                        .is_some_and(|o| o.status == Status::Done)
                {
                    done(user, project, &task);
                }
                Ok(json!({ "finished": true }))
            }
            TASK_ANSWER => {
                board.reply(text("id")?, text("text")?)?;
                Ok(json!({ "answered": true }))
            }
            TASK_ASSIGN => {
                board.give(text("id")?, text("agent")?)?;
                Ok(json!({ "assigned": true }))
            }
            TASK_CANCEL => Ok(json!({ "cancel": board.stop(text("id")?)? })),
            other => Err(format!("no method `{other}`")),
        }
    }
}

// ---------------------------------------------------------------------------
// On a node
// ---------------------------------------------------------------------------

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
    async fn create(&self, ticket: &Ticket) -> Result<String, String> {
        read(
            &self.ask(TASK_CREATE, json!({ "ticket": ticket })).await?,
            "id",
        )
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
    async fn answer(&self, id: &str, text: &str) -> Result<(), String> {
        self.ask(TASK_ANSWER, json!({ "id": id, "text": text }))
            .await
            .map(|_| ())
    }
    async fn assign(&self, id: &str, agent: &str) -> Result<(), String> {
        self.ask(TASK_ASSIGN, json!({ "id": id, "agent": agent }))
            .await
            .map(|_| ())
    }
    async fn cancel(&self, id: &str) -> Result<Cancelled, String> {
        read(&self.ask(TASK_CANCEL, json!({ "id": id })).await?, "cancel")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::tests::draft;
    use crate::tasks::{GateRef, entry_line};
    use ensemble_core::workspace::testing::TempDir;

    fn board(tmp: &TempDir) -> DbBoard {
        DbBoard::new(&tmp.0, "ana", "shop")
    }

    fn result(text: &str) -> End {
        End::Result {
            status: Status::Done,
            agent: "web/coder".into(),
            started: "2026-10-31T12:00:00Z".into(),
            extra: vec![("branch".into(), "ensemble/0001-a".into())],
            text: text.into(),
        }
    }

    #[test]
    fn a_created_ticket_reads_back_as_written_and_ids_count_up() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let written = Ticket {
            context: Some("see \"src/fetch.rs\"\nline two".into()),
            goal: Some("Requests survive a flaky network.".into()),
            constraints: vec!["No new dependency".into()],
            files: vec!["src/fetch.rs".into()],
            done_when: vec!["cargo test passes".into()],
            gate: Some(GateRef {
                name: "test".into(),
                attempts: 3,
            }),
            ..draft(
                "Add retry: to the fetcher!",
                "web/coder",
                "Retry three times.",
            )
        };
        let id = b.add(&written).unwrap();
        assert_eq!(id, "0001-add-retry-to-the-fetcher");
        let read = b.detail(&id).unwrap().unwrap().task.ticket;
        assert_eq!(
            read,
            Ticket {
                id: id.clone(),
                ..written
            }
        );
        let next = b
            .add(&Ticket {
                after: vec![id.clone()],
                ..draft("Next", "web/coder", "t")
            })
            .unwrap();
        assert_eq!(next, "0002-next");
        assert_eq!(b.add(&draft("!!", "web/coder", "t")).unwrap(), "0003");
        let err = b
            .add(&Ticket {
                after: vec!["0042".into()],
                ..draft("Later", "web/coder", "t")
            })
            .unwrap_err();
        assert!(err.contains("`after` names '0042'"), "{err}");
        assert!(b.add(&draft("", "web/coder", "t")).is_err());
    }

    #[test]
    fn each_account_and_project_has_its_own_tasks() {
        let tmp = TempDir::new();
        board(&tmp).add(&draft("Mine", "web/coder", "t")).unwrap();
        assert!(
            DbBoard::new(&tmp.0, "rex", "shop")
                .list()
                .unwrap()
                .is_empty()
        );
        assert!(
            DbBoard::new(&tmp.0, "ana", "blog")
                .list()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            DbBoard::new(&tmp.0, "rex", "shop")
                .add(&draft("Theirs", "web/coder", "t"))
                .unwrap(),
            "0001-theirs"
        );
        assert!(
            DbBoard::new(&tmp.0, "rex", "shop")
                .detail("0001-mine")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn one_node_at_a_time_runs_a_task_until_its_lease_runs_out() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let id = b.add(&draft("A", "web/coder", "t")).unwrap();
        assert_eq!(b.take(&id, "laptop", 60, true).unwrap(), Claim::Taken);
        assert_eq!(
            b.take(&id, "gpu-box", 60, true).unwrap(),
            Claim::Refused("it runs on laptop".into())
        );
        assert_eq!(
            b.take(&id, "laptop", 60, true).unwrap(),
            Claim::Taken,
            "its own again"
        );
        assert_eq!(b.list().unwrap()[0].running_on.as_deref(), Some("laptop"));
        assert_eq!(b.keep(&id, "laptop", 60).unwrap(), Renewal::Kept);
        assert_eq!(b.keep(&id, "gpu-box", 60).unwrap(), Renewal::Lost);

        // The laptop stops renewing: its lease runs out, and the task is free.
        b.change(&id, |r, _| {
            r.lease.as_mut().unwrap().until = 1;
            Ok(())
        })
        .unwrap();
        let t = &b.list().unwrap()[0];
        assert_eq!(
            (t.running_on.as_deref(), t.interrupted_on.as_deref()),
            (None, Some("laptop"))
        );
        assert!(entry_line(t, &[], None).contains("queued (interrupted on laptop)"));
        assert_eq!(b.take(&id, "gpu-box", 60, true).unwrap(), Claim::Taken);
        // The laptop comes back: the task is not its any more.
        assert_eq!(b.keep(&id, "laptop", 60).unwrap(), Renewal::Lost);
        let err = b.end(&id, "laptop", result("late"), None).unwrap_err();
        assert!(err.contains("runs on gpu-box"), "{err}");
        b.end(&id, "gpu-box", result("All done."), None).unwrap();
        let d = b.detail(&id).unwrap().unwrap();
        assert_eq!(d.task.status, Some(Status::Done));
        let outcome = d.outcome.unwrap();
        assert_eq!(outcome.reply(), "All done.\n");
        assert_eq!(outcome.field("branch"), Some("ensemble/0001-a"));
        assert_eq!(outcome.field("started"), Some("2026-10-31T12:00:00Z"));
        assert_eq!(
            b.take(&id, "gpu-box", 60, true).unwrap(),
            Claim::Refused("it has ended".into())
        );
    }

    #[test]
    fn a_task_put_back_is_free_for_any_node() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let id = b.add(&draft("A", "web/coder", "t")).unwrap();
        b.take(&id, "laptop", 60, true).unwrap();
        b.put_back(&id, "gpu-box", None).unwrap();
        assert_eq!(
            b.list().unwrap()[0].running_on.as_deref(),
            Some("laptop"),
            "not gpu-box's to put back"
        );
        b.put_back(&id, "laptop", None).unwrap();
        let t = &b.list().unwrap()[0];
        assert_eq!(
            (t.running_on.as_deref(), t.interrupted_on.as_deref()),
            (None, Some("laptop"))
        );
        assert_eq!(b.take(&id, "gpu-box", 60, true).unwrap(), Claim::Taken);
        assert_eq!(b.list().unwrap()[0].interrupted_on, None);
    }

    #[test]
    fn work_that_cannot_move_waits_for_its_node_and_work_that_can_is_carried_on() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        // In the desk's folder: it stays with the desk.
        let here = b.add(&draft("Here", "web/coder", "t")).unwrap();
        assert_eq!(b.take(&here, "desk", 60, false).unwrap(), Claim::Taken);
        b.put_back(&here, "desk", None).unwrap();
        let t = &b.list().unwrap()[0];
        assert_eq!(t.pinned_to.as_deref(), Some("desk"));
        assert!(
            entry_line(t, &[], None).contains("interrupted on desk, where its work is"),
            "{}",
            entry_line(t, &[], None)
        );
        assert_eq!(
            b.take(&here, "laptop", 60, true).unwrap(),
            Claim::Refused("its work is in desk's folder, so it waits for desk".into())
        );
        assert_eq!(b.take(&here, "desk", 60, false).unwrap(), Claim::Taken);

        // On a pushed branch: the laptop carries on where the desk stopped.
        let moving = b.add(&draft("Moving", "web/coder", "t")).unwrap();
        b.take(&moving, "desk", 60, true).unwrap();
        let pushed = Progress {
            node: "desk".into(),
            branch: "ensemble/0002-moving".into(),
            commit: "abc1234".into(),
        };
        b.put_back(&moving, "desk", Some(pushed.clone())).unwrap();
        let d = b.detail(&moving).unwrap().unwrap();
        assert_eq!((d.progress, d.task.pinned_to), (Some(pushed), None));
        assert_eq!(b.take(&moving, "laptop", 60, true).unwrap(), Claim::Taken);
        let done = Progress {
            node: "laptop".into(),
            commit: "def5678".into(),
            ..b.detail(&moving).unwrap().unwrap().progress.unwrap()
        };
        b.end(&moving, "laptop", result("done"), Some(done.clone()))
            .unwrap();
        assert_eq!(b.detail(&moving).unwrap().unwrap().progress, Some(done));
    }

    #[test]
    fn a_task_starts_only_when_assigned_answered_and_after_what_it_waits_for() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let first = b.add(&draft("First", "web/coder", "t")).unwrap();
        let second = b
            .add(&Ticket {
                after: vec![first.clone()],
                ..draft("Second", "web/coder", "t")
            })
            .unwrap();
        let backlog = b.add(&draft("Later", "", "t")).unwrap();
        assert_eq!(
            b.take(&second, "laptop", 60, true).unwrap(),
            Claim::Refused(format!("it waits for {first}"))
        );
        assert_eq!(
            b.take(&backlog, "laptop", 60, true).unwrap(),
            Claim::Refused("it is in the backlog".into())
        );
        b.give(&backlog, "web/coder").unwrap();
        assert_eq!(b.take(&backlog, "laptop", 60, true).unwrap(), Claim::Taken);
        let err = b.give(&backlog, "web/reviewer").unwrap_err();
        assert!(err.contains("is running"), "{err}");

        b.take(&first, "laptop", 60, true).unwrap();
        b.end(
            &first,
            "laptop",
            End::Ask {
                question: "Which parser?".into(),
                reply: "I found two.\nNEEDS INPUT: Which parser?".into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            b.list().unwrap()[0].question.as_deref(),
            Some("Which parser?")
        );
        assert_eq!(
            b.take(&first, "laptop", 60, true).unwrap(),
            Claim::Refused("it waits for an answer".into())
        );
        b.reply(&first, "The new one.").unwrap();
        assert!(b.reply(&first, "twice").is_err());
        let (questions, reply) = b.detail(&first).unwrap().unwrap().conversation.unwrap();
        assert!(questions.starts_with("## Question 1 ("), "{questions}");
        assert!(
            questions.contains("Which parser?\n\n### Answer\nThe new one.\n\n"),
            "{questions}"
        );
        assert!(reply.ends_with("NEEDS INPUT: Which parser?"), "{reply}");
        assert_eq!(b.take(&first, "laptop", 60, true).unwrap(), Claim::Taken);
        b.end(&first, "laptop", result("done"), None).unwrap();
        assert_eq!(b.take(&second, "laptop", 60, true).unwrap(), Claim::Taken);
        let err = b.give(&first, "web/reviewer").unwrap_err();
        assert!(err.contains("has ended"), "{err}");
    }

    #[test]
    fn a_hand_off_gives_the_task_to_the_next_agent_with_a_note() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let id = b.add(&draft("A", "web/coder", "t")).unwrap();
        b.take(&id, "laptop", 60, true).unwrap();
        b.end(
            &id,
            "laptop",
            End::Handoff {
                from: "web/coder".into(),
                to: "web/reviewer".into(),
                note: "check the tests".into(),
                reply: "coded it".into(),
            },
            None,
        )
        .unwrap();
        let d = b.detail(&id).unwrap().unwrap();
        assert_eq!(d.task.ticket.agent, "web/reviewer");
        assert_eq!(d.task.status, None);
        let handoffs = d.handoffs.unwrap();
        assert!(
            handoffs.starts_with("## web/coder handed this to web/reviewer\ncheck the tests"),
            "{handoffs}"
        );
        assert!(
            handoffs.contains("What web/coder had said:\ncoded it"),
            "{handoffs}"
        );
        assert_eq!(b.take(&id, "gpu-box", 60, true).unwrap(), Claim::Taken);
    }

    #[test]
    fn a_cancelled_task_ends_at_once_or_when_its_node_hears_of_it() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let waiting = b.add(&draft("A", "web/coder", "t")).unwrap();
        assert_eq!(b.stop(&waiting).unwrap(), Cancelled::Before);
        assert_eq!(b.list().unwrap()[0].status, Some(Status::Cancelled));
        assert_eq!(b.stop(&waiting).unwrap(), Cancelled::Ended);

        let running = b.add(&draft("B", "web/coder", "t")).unwrap();
        b.take(&running, "laptop", 60, true).unwrap();
        assert_eq!(
            b.stop(&running).unwrap(),
            Cancelled::Running("laptop".into())
        );
        assert_eq!(b.keep(&running, "laptop", 60).unwrap(), Renewal::Cancel);
        b.put_back(&running, "laptop", None).unwrap();
        let d = b.detail(&running).unwrap().unwrap();
        assert_eq!(d.task.status, Some(Status::Cancelled));
        assert_eq!(d.outcome.unwrap().reply(), "Cancelled while it ran.\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_reaches_its_tasks_through_the_hub() {
        let tmp = TempDir::new();
        let hub = ensemble_link::server::testing::start_with_tasks(Box::leak(Box::new(
            Service::new(&tmp.0),
        )));
        let laptop = RemoteBoard::new(&hub.url, &hub.ana, "shop");
        let id = laptop.create(&draft("A", "web/coder", "t")).await.unwrap();
        assert_eq!(id, "0001-a");
        assert_eq!(
            laptop.claim(&id, "laptop", LEASE, true).await.unwrap(),
            Claim::Taken
        );
        assert_eq!(
            laptop.claim(&id, "gpu-box", LEASE, true).await.unwrap(),
            Claim::Refused("it runs on laptop".into())
        );
        assert_eq!(
            laptop.renew(&id, "laptop", LEASE).await.unwrap(),
            Renewal::Kept
        );
        laptop
            .finish(&id, "laptop", result("All done."), None)
            .await
            .unwrap();
        let tasks = laptop.scan().await.unwrap();
        assert_eq!(tasks[0].status, Some(Status::Done));
        let d = laptop.get(&id).await.unwrap().unwrap();
        assert_eq!(d.outcome.unwrap().reply(), "All done.\n");
        assert_eq!(laptop.get("0009").await.unwrap(), None);
        assert_eq!(laptop.cancel(&id).await.unwrap(), Cancelled::Ended);
        // Kept for ana, never for the name a node sends.
        assert_eq!(DbBoard::new(&tmp.0, "ana", "shop").list().unwrap().len(), 1);
        let err = RemoteBoard::new(&hub.url, &hub.ana, "a/b")
            .scan()
            .await
            .unwrap_err();
        assert!(err.contains("not a project name"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_done_is_given_to_the_hub_once_with_what_it_asked_and_reported() {
        let tmp = TempDir::new();
        let given: &'static std::sync::Mutex<Vec<(String, String, String)>> =
            Box::leak(Box::default());
        let service = Service::new(&tmp.0).on_done(|user, project, task| {
            let (title, text) = learned(project, task).unwrap();
            given
                .lock()
                .unwrap()
                .push((format!("{user}/{project}"), title, text));
        });
        let hub = ensemble_link::server::testing::start_with_tasks(Box::leak(Box::new(service)));
        let laptop = RemoteBoard::new(&hub.url, &hub.ana, "shop");
        let mut ticket = draft("Retry fetches", "web/coder", "t");
        ticket.goal = Some("Requests survive a flaky network.".into());
        let id = laptop.create(&ticket).await.unwrap();
        // Asking is no end: nothing is given.
        laptop.claim(&id, "laptop", LEASE, true).await.unwrap();
        let ask = End::Ask {
            question: "Which status codes?".into(),
            reply: "r".into(),
        };
        laptop.finish(&id, "laptop", ask, None).await.unwrap();
        assert!(given.lock().unwrap().is_empty());
        laptop.answer(&id, "502 and 503").await.unwrap();
        laptop.claim(&id, "laptop", LEASE, true).await.unwrap();
        laptop
            .finish(
                &id,
                "laptop",
                result("Retries 502 and 503 three times."),
                None,
            )
            .await
            .unwrap();
        let given = given.lock().unwrap();
        assert_eq!(given.len(), 1);
        let (whose, title, text) = &given[0];
        assert_eq!(
            (whose.as_str(), title.as_str()),
            ("ana/shop", "Retry fetches")
        );
        for part in [
            "helper agent web/coder",
            "task 0001-retry-fetches in the project shop",
            "Goal: Requests survive a flaky network.",
            "Which status codes?",
            "502 and 503",
            "Its report:\nRetries 502 and 503 three times.",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }

        // A task that failed teaches nothing here.
        let failed = Detail {
            outcome: Some(Outcome {
                status: Status::Failed,
                text: "x".into(),
            }),
            ..DbBoard::new(&tmp.0, "ana", "shop")
                .detail(&id)
                .unwrap()
                .unwrap()
        };
        assert_eq!(learned("shop", &failed), None);
    }
}
