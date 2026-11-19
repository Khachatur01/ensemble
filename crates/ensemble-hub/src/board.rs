//! The hub's tasks, kept in its database per account and project and leased
//! to runners, as `ensemble_tasks::board` says. [`DbBoard`] is one project's
//! tasks of one account; [`Service`] answers the link's task methods with
//! them, as the server passes those on ([`TaskService`]), with the runners
//! connected now that may take an account's tasks ([`Present`]), which say
//! whether a task given back is blocked.
//!
//! A task is offered to the runners of its own account with its agent first.
//! A runner of another account that works for the hub (a [`Lender`]) may
//! take it only when none of those is connected, or every one gave it back,
//! and only when the task's account has `compute` and the runner lends the
//! task's agent. Being busy is not giving back: a task waits for its
//! account's own runners while one with its agent is connected and has not
//! given it back.

use async_trait::async_trait;
use ensemble_core::runtime::blocking;
use ensemble_link::protocol::{
    TASK, TASK_ANSWER, TASK_ASSIGN, TASK_CANCEL, TASK_CLAIM, TASK_CREATE, TASK_FINISH,
    TASK_GIVE_BACK, TASK_RELEASE, TASK_RENEW, TASKS, TASKS_WAITING,
};
use ensemble_store::{self as store, Value as Sql};
use ensemble_tasks::board::{Board, Cancelled, Claim, End, LEASE, Progress, Renewal, Waiting};
use ensemble_tasks::tasks::{Detail, GivenBack, Outcome, Status, Task, Ticket, utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// The hub's tasks, as the server passes the link's task methods to them, so
/// the server knows nothing of how they are kept.
pub trait TaskService: Sync {
    /// One of the link's task methods (`TASK_METHODS`), asked by `asker`: on
    /// its own account's tasks, or, as a lender, on another's it may take.
    fn handle(&self, asker: &Asker<'_>, method: &str, params: &Value) -> Result<Value, String>;
}

/// Who asks for a task method, as the server knows them now.
pub struct Asker<'a> {
    /// The signed-in account.
    pub user: &'a str,
    /// The runner of the account the request names (`runner`), when it is
    /// connected, works for the hub and the account's token has `host`: what
    /// it may do on other accounts' tasks.
    pub lender: Option<Lender>,
    /// The runners connected now that may take an account's tasks.
    pub runners: &'a (dyn Fn(&str) -> Vec<Present> + Sync),
    /// Whether an account's role has `compute`: whether others' runners work
    /// on its tasks.
    pub compute: &'a (dyn Fn(&str) -> bool + Sync),
}

/// A runner connected now that may take an account's tasks, and the agents
/// it takes them with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Present {
    /// As its claims name it: its name for a runner of the account's own,
    /// `<its account>/<its name>` for another's.
    pub name: String,
    /// Every agent of a runner of the account's own; those another's lends.
    pub agents: Vec<String>,
    /// Whether it is a runner of the account's own.
    pub own: bool,
}

/// A runner of another account that works for the hub, acting on an
/// account's tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lender {
    /// Its name on others' tasks: `<its account>/<its name>`.
    pub name: String,
    /// The agents it lends: those of its teams on `llm_backends`.
    pub lends: Vec<String>,
}

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
    /// The runner whose folder its work is in, when that work cannot move.
    pinned_to: Option<String>,
    /// Where its work was pushed when a run last stopped.
    progress: Option<Progress>,
    /// Asked to stop while it ran.
    cancel: bool,
    result: Option<Outcome>,
    /// The runners that gave it back, and why.
    given_back: Vec<GivenBack>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Question {
    asked: String,
    text: String,
    answer: Option<String>,
    /// Asked by a runner with no copy of the project: its answer is where the
    /// project's repository is.
    repository: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Lease {
    node: String,
    /// Until when, in seconds since the epoch, by the hub's clock.
    until: u64,
}

impl Record {
    /// The runner whose lease has not run out.
    fn holder(&self, now: u64) -> Option<&str> {
        self.lease
            .as_ref()
            .filter(|l| l.until > now)
            .map(|l| l.node.as_str())
    }

    fn open_question(&self) -> Option<&Question> {
        self.questions.last().filter(|q| q.answer.is_none())
    }

    /// Whether every runner of `present` that may take it gave it back, when
    /// it has been given back at all and has not ended. Not known without
    /// them.
    fn blocked(&self, present: Option<&[Present]>) -> bool {
        let Some(present) = present else {
            return false;
        };
        self.result.is_none()
            && !self.given_back.is_empty()
            && present
                .iter()
                .filter(|r| r.agents.contains(&self.ticket.agent))
                .all(|r| self.given_back.iter().any(|g| g.runner == r.name))
    }

    /// Whether the account's own runners of `present` leave it to others: none
    /// of them has its agent, or every one that has gave it back.
    fn left_to_others(&self, present: &[Present]) -> bool {
        present
            .iter()
            .filter(|r| r.own && r.agents.contains(&self.ticket.agent))
            .all(|r| self.given_back.iter().any(|g| g.runner == r.name))
    }

    /// Whether `lender` may take it, as `present` are connected: not ended,
    /// for an agent it lends, not given back by it; and its already (it runs
    /// there or its work is there), or left to others and neither running on
    /// nor pinned to another runner.
    fn open_to(&self, lender: &Lender, present: &[Present], now: u64) -> bool {
        let it = |n: &str| n == lender.name;
        if self.result.is_some()
            || !lender.lends.contains(&self.ticket.agent)
            || self.given_back.iter().any(|g| it(&g.runner))
        {
            return false;
        }
        if self.holder(now).is_some_and(it) || self.pinned_to.as_deref().is_some_and(it) {
            return true;
        }
        self.holder(now).is_none() && self.pinned_to.is_none() && self.left_to_others(present)
    }

    /// The task as runners and clients see it; whether it is blocked as
    /// `present`, the account's connected runners, say.
    fn task(&self, now: u64, present: Option<&[Present]>) -> Task {
        Task {
            ticket: self.ticket.clone(),
            status: self.result.as_ref().map(|r| r.status),
            question: self.open_question().map(|q| q.text.clone()),
            running_on: self
                .holder(now)
                .filter(|_| self.result.is_none())
                .map(str::to_string),
            interrupted_on: match (&self.lease, &self.interrupted_on) {
                // Its runner stopped renewing.
                (Some(l), _) if l.until <= now && self.result.is_none() => Some(l.node.clone()),
                (None, Some(node)) if self.result.is_none() => Some(node.clone()),
                _ => None,
            },
            pinned_to: self.pinned_to.clone(),
            given_back: self.given_back.clone(),
            blocked: self.blocked(present),
        }
    }

    /// The task with what it said. A runner's question where the project is
    /// is not the agent's, so it is not in what the agent is told it asked.
    fn detail(&self, now: u64, present: Option<&[Present]>) -> Detail {
        let asked: Vec<&Question> = self.questions.iter().filter(|q| !q.repository).collect();
        let conversation = (!asked.is_empty()).then(|| {
            let mut text = String::new();
            for (n, q) in asked.iter().enumerate() {
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
            task: self.task(now, present),
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
    /// The runners connected now that may take the account's tasks, when
    /// known: whether a task given back is blocked depends on them.
    present: Option<Vec<Present>>,
    /// Another account's runner that acts on these tasks, when it is not
    /// the account itself: it sees and takes only those it may.
    lender: Option<Lender>,
}

impl DbBoard {
    pub fn new(dir: &Path, owner: &str, project: &str) -> Self {
        DbBoard {
            dir: dir.to_path_buf(),
            owner: owner.to_string(),
            project: project.to_string(),
            present: None,
            lender: None,
        }
    }

    /// The same, knowing that `runners` that may take the account's tasks are
    /// connected now.
    pub fn with_runners(mut self, runners: &[Present]) -> Self {
        self.present = Some(runners.to_vec());
        self
    }

    /// The same, as `lender`, a runner of another account, sees them: only
    /// the tasks it may take ([`Record::open_to`]), and those they come after.
    pub fn for_lender(mut self, lender: Lender) -> Self {
        self.lender = Some(lender);
        self
    }

    /// Whether the lender acting on these tasks, if any, may take `r`.
    fn open(&self, r: &Record, now: u64) -> bool {
        self.lender
            .as_ref()
            .is_none_or(|l| r.open_to(l, self.present.as_deref().unwrap_or(&[]), now))
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

    /// Every task, oldest first; for a lender, those it may take and those
    /// they come after.
    pub fn list(&self) -> Result<Vec<Task>, String> {
        let now = unix_now();
        let present = self.present.as_deref();
        let records = self.records()?;
        // What a lender may take, and what that comes after.
        let shown: Option<HashSet<&str>> = self.lender.as_ref().map(|_| {
            records
                .iter()
                .filter(|r| self.open(r, now))
                .flat_map(|r| std::iter::once(&r.ticket.id).chain(&r.ticket.after))
                .map(String::as_str)
                .collect()
        });
        Ok(records
            .iter()
            .filter(|r| {
                shown
                    .as_ref()
                    .is_none_or(|s| s.contains(r.ticket.id.as_str()))
            })
            .map(|r| r.task(now, present))
            .collect())
    }

    /// Task `id` with what it said; for a lender, only one it may take.
    pub fn detail(&self, id: &str) -> Result<Option<Detail>, String> {
        let now = unix_now();
        Ok(self
            .record(id)?
            .filter(|r| self.open(r, now))
            .map(|r| r.detail(now, self.present.as_deref())))
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
            given_back: Vec::new(),
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
    /// another runner, and not pinned to one; for a lender, one it may take.
    /// A runner that claims a task it held before, such as after it started
    /// again, gets it back. Unless the run is `movable`, the task is pinned
    /// to `node`.
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
            if !self.open(r, now) {
                return refused(&format!(
                    "it is not offered to {node}: it waits for a runner of {}'s own, or for an agent {node} does not lend",
                    self.owner
                ));
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
            if let Some(given) = r.given_back.iter().find(|g| g.runner == node) {
                return refused(&format!("{node} gave it back: {}", given.why));
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
                End::Ask {
                    question,
                    reply,
                    repository,
                } => {
                    r.questions.push(Question {
                        asked: utc(SystemTime::now()),
                        text: question.trim().to_string(),
                        answer: None,
                        repository,
                    });
                    // The runner asked, not the agent: what the agent last
                    // said stays.
                    if !repository {
                        r.reply = reply;
                    }
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

    /// Answer the question task `id` waits on. The answer to where its project
    /// is becomes its `repository`. Answered, it is offered to every runner
    /// again, those that gave it back too.
    pub fn reply(&self, id: &str, text: &str) -> Result<(), String> {
        self.change(id, |r, _| {
            let Some(q) = r.questions.last_mut().filter(|q| q.answer.is_none()) else {
                return Err(format!("{id} has no question waiting for an answer."));
            };
            let text = text.trim();
            if q.repository {
                ensemble_tasks::tasks::check_repository(text)
                    .map_err(|e| format!("{id} asked where its project's repository is: {e}"))?;
                r.ticket.repository = Some(text.to_string());
            }
            q.answer = Some(text.to_string());
            r.given_back.clear();
            Ok(())
        })
    }

    /// `node` gives task `id` back, saying `why`, and does not take it again;
    /// a claim of its own on it ends. A lender gives back only a task of an
    /// agent it lends. Whether the task is blocked now: every runner
    /// connected that may take it gave it back, as far as the board knows who
    /// is connected.
    pub fn give_back(&self, id: &str, node: &str, why: &str) -> Result<bool, String> {
        let present = self.present.clone();
        self.change(id, |r, _| {
            if r.result.is_some() {
                return Err(format!("{id} has ended"));
            }
            if let Some(lender) = &self.lender
                && !lender.lends.contains(&r.ticket.agent)
            {
                return Err(format!(
                    "{id} is for {}, which {node} does not lend",
                    r.ticket.agent
                ));
            }
            if r.lease.as_ref().is_some_and(|l| l.node == node) {
                r.lease = None;
            }
            r.given_back.retain(|g| g.runner != node);
            r.given_back.push(GivenBack {
                runner: node.to_string(),
                why: why.trim().to_string(),
            });
            Ok(r.blocked(present.as_deref()))
        })
    }

    /// Give task `id` to `agent`; not one that runs or has ended. Assigned,
    /// it is offered to every runner again, those that gave it back too.
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
            r.given_back.clear();
            Ok(())
        })
    }

    /// Cancel task `id`: at once when it has not started, by its runner when it
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

/// A YAML scalar that reads back as exactly `s`.
fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

/// Lowercase words of the title, joined by dashes, at most 40 characters.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
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
    async fn give_back(&self, id: &str, node: &str, why: &str) -> Result<bool, String> {
        blocking(|| self.give_back(id, node, why))
    }
}

/// Every task of `owner`'s, in every project, that waits for one of `agents`
/// and that `runner`, a runner of `owner`'s, may take or ask about: not
/// ended, not given back by it, and neither running on nor pinned to another
/// runner. Those that wait for an answer or another task are there too: the
/// runner looks for the projects it has no copy of.
pub fn waiting(
    dir: &Path,
    owner: &str,
    runner: &str,
    agents: &[String],
    present: &[Present],
) -> Result<Vec<Waiting>, String> {
    let rows = store::with(dir, async |c| {
        store::rows(
            c,
            "SELECT project, data FROM tasks WHERE owner = ?1 ORDER BY project, number",
            vec![Sql::Text(owner.to_string())],
        )
        .await
    })?;
    let now = unix_now();
    let mut out = Vec::new();
    for row in &rows {
        let (Some(project), Some(data)) = (store::text(&row[0]), store::text(&row[1])) else {
            continue;
        };
        let r: Record =
            serde_json::from_str(&data).map_err(|e| format!("a task is unreadable: {e}"))?;
        let mine = |n: &str| n == runner;
        if r.result.is_some()
            || !agents.contains(&r.ticket.agent)
            || r.given_back.iter().any(|g| mine(&g.runner))
            || r.holder(now).is_some_and(|h| !mine(h))
            || r.pinned_to.as_deref().is_some_and(|p| !mine(p))
        {
            continue;
        }
        out.push(Waiting {
            account: owner.to_string(),
            project,
            task: r.task(now, Some(present)),
        });
    }
    Ok(out)
}

/// Every task of other accounts than `user`'s, in every project, that
/// `lender`, a runner of `user`'s, may take ([`Record::open_to`]): of the
/// accounts whose role has `compute`, as `runners` of each are connected.
/// Those that wait for an answer or another task are there too, as in
/// [`waiting`].
pub fn lent(
    dir: &Path,
    user: &str,
    lender: &Lender,
    runners: &(dyn Fn(&str) -> Vec<Present> + Sync),
    compute: &(dyn Fn(&str) -> bool + Sync),
) -> Result<Vec<Waiting>, String> {
    let rows = store::with(dir, async |c| {
        store::rows(
            c,
            "SELECT owner, project, data FROM tasks WHERE owner != ?1 ORDER BY owner, project, number",
            vec![Sql::Text(user.to_string())],
        )
        .await
    })?;
    let now = unix_now();
    // Each account's runners, once, and none for an account without `compute`.
    let mut accounts: HashMap<String, Option<Vec<Present>>> = HashMap::new();
    let mut out = Vec::new();
    for row in &rows {
        let (Some(owner), Some(project), Some(data)) = (
            store::text(&row[0]),
            store::text(&row[1]),
            store::text(&row[2]),
        ) else {
            continue;
        };
        let present = accounts
            .entry(owner.clone())
            .or_insert_with(|| compute(&owner).then(|| runners(&owner)));
        let Some(present) = present else {
            continue;
        };
        let r: Record =
            serde_json::from_str(&data).map_err(|e| format!("a task is unreadable: {e}"))?;
        if !r.open_to(lender, present, now) {
            continue;
        }
        out.push(Waiting {
            account: owner,
            project,
            task: r.task(now, Some(present)),
        });
    }
    Ok(out)
}

/// What the notes related to a task are recalled by: its title and text.
pub fn recall_query(ticket: &Ticket) -> String {
    format!("{}\n{}", ticket.title.trim(), ticket.text.trim())
}

/// What is done with a task that ended done: given its account, its project
/// and the task.
pub type OnDone = dyn Fn(&str, &str, &Detail) + Send + Sync;

/// The notes related to a new task: given its account and what they are
/// recalled by ([`recall_query`]), the notes that account may see, marked as
/// reference data, or none when nothing is related.
pub type Recall = dyn Fn(&str, &str) -> Option<String> + Send + Sync;

/// The methods a lender may ask on another account's tasks: reading those it
/// may take, and running them. Making, answering, assigning and cancelling
/// are the account's own.
const LENDER_METHODS: [&str; 7] = [
    TASKS,
    TASK,
    TASK_CLAIM,
    TASK_RENEW,
    TASK_RELEASE,
    TASK_FINISH,
    TASK_GIVE_BACK,
];

/// The hub's side of the link's task methods: each for the signed-in
/// account's own tasks, or another account's for a lender, in the hub's
/// database in `dir`.
pub struct Service {
    dir: PathBuf,
    done: Option<Box<OnDone>>,
    notes: Option<Box<Recall>>,
}

impl Service {
    pub fn new(dir: &Path) -> Self {
        Service {
            dir: dir.to_path_buf(),
            done: None,
            notes: None,
        }
    }

    /// Give each task that ends done to `done`, as the hub gives it to its
    /// knowledge.
    pub fn on_done(mut self, done: impl Fn(&str, &str, &Detail) + Send + Sync + 'static) -> Self {
        self.done = Some(Box::new(done));
        self
    }

    /// Keep in each new task the notes `notes` recalls for it, in place of any
    /// its maker sent.
    pub fn with_notes(
        mut self,
        notes: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.notes = Some(Box::new(notes));
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

impl Service {
    /// The board of `account`'s `project` that `asker`'s runner may act on
    /// with `method`, as a lender: refused unless the runner is a lender, it
    /// names itself as one, the method is one a lender may ask, and the
    /// account's role has `compute`. Which of its tasks it may take, the
    /// board says.
    fn lent_board(
        &self,
        asker: &Asker<'_>,
        account: &str,
        project: &str,
        method: &str,
        params: &Value,
    ) -> Result<DbBoard, String> {
        let Some(lender) = &asker.lender else {
            return Err(format!(
                "{method}: only a runner of yours connected now that works for the hub, with the `host` permission, acts on {account}'s tasks; name it as `runner`"
            ));
        };
        if !LENDER_METHODS.contains(&method) {
            return Err(format!(
                "{method}: only {account} does this to its tasks; a runner that works for the hub only takes and runs them"
            ));
        }
        for key in ["runner", "node"] {
            if let Some(named) = params.get(key).and_then(Value::as_str)
                && named.trim() != lender.name
            {
                return Err(format!(
                    "{method}: on {account}'s tasks the runner is {}, not '{named}'",
                    lender.name
                ));
            }
        }
        if !(asker.compute)(account) {
            return Err(format!(
                "{method}: {account}'s role has no `compute`, so runners of other accounts do not work on its tasks"
            ));
        }
        Ok(DbBoard::new(&self.dir, account, project)
            .with_runners(&(asker.runners)(account))
            .for_lender(lender.clone()))
    }
}

impl TaskService for Service {
    fn handle(&self, asker: &Asker<'_>, method: &str, params: &Value) -> Result<Value, String> {
        let user = asker.user;
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
        if method == TASKS_WAITING {
            let agents: Vec<String> = serde_json::from_value(parsed("agents")?)
                .map_err(|e| format!("{method}: `agents` is a list of agents: {e}"))?;
            let runner = text("runner")?;
            let own = (asker.runners)(user);
            let mut tasks = waiting(&self.dir, user, runner, &agents, &own)?;
            if let Some(lender) = &asker.lender {
                tasks.extend(lent(&self.dir, user, lender, asker.runners, asker.compute)?);
            }
            return Ok(json!({ "runner": format!("{user}/{runner}"), "tasks": tasks }));
        }
        let project = text("project")?;
        if project.contains(['/', '\\']) || project.starts_with('.') {
            return Err(format!("{method}: '{project}' is not a project name"));
        }
        // The task's account: the signed-in one's, or another a lender names.
        let account = params
            .get("account")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty() && *a != user);
        let (owner, board) = match account {
            None => (
                user,
                DbBoard::new(&self.dir, user, project).with_runners(&(asker.runners)(user)),
            ),
            Some(account) => (
                account,
                self.lent_board(asker, account, project, method, params)?,
            ),
        };
        match method {
            TASKS => Ok(json!({ "tasks": board.list()? })),
            TASK => Ok(json!({ "task": board.detail(text("id")?)? })),
            TASK_CREATE => {
                let mut ticket: Ticket = serde_json::from_value(parsed("ticket")?)
                    .map_err(|e| format!("{method}: `ticket` is not a ticket: {e}"))?;
                ticket.notes = self
                    .notes
                    .as_ref()
                    .and_then(|recall| recall(user, &recall_query(&ticket)));
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
                // Read as its account has it: ended, it is no lender's to see.
                if over
                    && let Some(done) = &self.done
                    && let Some(task) = DbBoard::new(&self.dir, owner, project).detail(id)?
                    && task
                        .outcome
                        .as_ref()
                        .is_some_and(|o| o.status == Status::Done)
                {
                    done(owner, project, &task);
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
            TASK_GIVE_BACK => {
                let blocked = board.give_back(text("id")?, text("runner")?, text("why")?)?;
                Ok(json!({ "blocked": blocked }))
            }
            other => Err(format!("no method `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;
    use ensemble_tasks::tasks::GateRef;

    /// A ticket with only what every ticket has.
    fn draft(title: &str, agent: &str, text: &str) -> Ticket {
        Ticket {
            title: title.into(),
            agent: agent.into(),
            text: text.into(),
            ..Ticket::default()
        }
    }

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
        assert_eq!(
            slug("Add retry: to the fetcher!"),
            "add-retry-to-the-fetcher"
        );
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
    fn a_task_put_back_is_free_for_any_runner() {
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
    fn work_that_cannot_move_waits_for_its_runner_and_work_that_can_is_carried_on() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        // In the desk's folder: it stays with the desk.
        let here = b.add(&draft("Here", "web/coder", "t")).unwrap();
        assert_eq!(b.take(&here, "desk", 60, false).unwrap(), Claim::Taken);
        b.put_back(&here, "desk", None).unwrap();
        let t = &b.list().unwrap()[0];
        assert_eq!(t.pinned_to.as_deref(), Some("desk"));
        assert_eq!(t.interrupted_on.as_deref(), Some("desk"));
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
                repository: false,
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

    /// A runner of ana's connected now, with `agents`.
    fn present(name: &str, agents: &[&str]) -> Present {
        Present {
            name: name.into(),
            agents: agents.iter().map(|a| a.to_string()).collect(),
            own: true,
        }
    }

    /// No runner connected.
    fn nobody(_: &str) -> Vec<Present> {
        Vec::new()
    }

    /// No account's role has `compute`.
    fn without(_: &str) -> bool {
        false
    }

    /// `user` asking, with no runner connected.
    fn alone(user: &str) -> Asker<'_> {
        Asker {
            user,
            lender: None,
            runners: &nobody,
            compute: &without,
        }
    }

    #[test]
    fn a_task_given_back_by_every_connected_runner_with_its_agent_is_blocked() {
        let tmp = TempDir::new();
        let connected = [
            present("desk", &["web/coder"]),
            present("laptop", &["web/coder", "web/reviewer"]),
            present("lab", &["pcb/router"]),
        ];
        let b = board(&tmp).with_runners(&connected);
        let id = b.add(&draft("A", "web/coder", "t")).unwrap();
        assert_eq!(b.take(&id, "desk", 60, true).unwrap(), Claim::Taken);
        // The laptop has not tried it yet; the lab has not the agent.
        assert!(
            !b.give_back(&id, "desk", "cannot reach the repository")
                .unwrap()
        );
        assert_eq!(
            b.take(&id, "desk", 60, true).unwrap(),
            Claim::Refused("desk gave it back: cannot reach the repository".into())
        );
        let t = &b.list().unwrap()[0];
        assert_eq!(t.running_on, None, "its claim ended");
        assert_eq!(t.interrupted_on, None);
        assert_eq!(
            t.given_back,
            [GivenBack {
                runner: "desk".into(),
                why: "cannot reach the repository".into()
            }]
        );
        assert!(!t.blocked);
        assert!(b.give_back(&id, "laptop", "no key").unwrap());
        assert!(b.list().unwrap()[0].blocked);
        assert!(b.detail(&id).unwrap().unwrap().task.blocked);
        assert_eq!(b.list().unwrap()[0].given_back.len(), 2);
        // Without knowing who is connected, nothing is said to be blocked.
        assert!(!DbBoard::new(&tmp.0, "ana", "shop").list().unwrap()[0].blocked);
        // A runner with the agent that has not given it back connects.
        let more = [
            present("desk", &["web/coder"]),
            present("gpu", &["web/coder"]),
        ];
        let with_gpu = board(&tmp).with_runners(&more);
        assert!(!with_gpu.list().unwrap()[0].blocked);
        assert_eq!(with_gpu.take(&id, "gpu", 60, true).unwrap(), Claim::Taken);
        with_gpu.put_back(&id, "gpu", None).unwrap();
        // Assigned again, it is every runner's again.
        b.give(&id, "web/coder").unwrap();
        let t = &b.list().unwrap()[0];
        assert!(t.given_back.is_empty() && !t.blocked);
        assert_eq!(b.take(&id, "desk", 60, true).unwrap(), Claim::Taken);
        b.end(&id, "desk", result("done"), None).unwrap();
        let err = b.give_back(&id, "desk", "late").unwrap_err();
        assert!(err.contains("has ended"), "{err}");
    }

    #[test]
    fn the_answer_to_where_a_project_is_becomes_its_repository() {
        let tmp = TempDir::new();
        let b = board(&tmp);
        let id = b.add(&draft("A", "web/coder", "t")).unwrap();
        b.take(&id, "desk", 60, true).unwrap();
        b.give_back(&id, "laptop", "no key").unwrap();
        b.end(
            &id,
            "desk",
            End::Ask {
                question: "Which repository should desk clone?".into(),
                reply: String::new(),
                repository: true,
            },
            None,
        )
        .unwrap();
        let d = b.detail(&id).unwrap().unwrap();
        assert_eq!(
            d.task.question.as_deref(),
            Some("Which repository should desk clone?")
        );
        assert_eq!(d.task.pinned_to, None, "asking pins it nowhere");
        assert_eq!(d.conversation, None, "the agent asked nothing");
        let err = b.reply(&id, "--upload-pack=x").unwrap_err();
        assert!(err.contains("not a git URL"), "{err}");
        b.reply(&id, " git@example.com:acme/shop.git ").unwrap();
        let d = b.detail(&id).unwrap().unwrap();
        assert_eq!(
            d.task.ticket.repository.as_deref(),
            Some("git@example.com:acme/shop.git")
        );
        assert_eq!(d.task.question, None);
        assert_eq!(d.conversation, None);
        assert!(
            d.task.given_back.is_empty(),
            "answered: everyone tries again"
        );
        assert_eq!(b.take(&id, "laptop", 60, true).unwrap(), Claim::Taken);
    }

    #[test]
    fn a_runner_hears_of_the_tasks_for_its_agents_in_every_project() {
        let tmp = TempDir::new();
        let shop = DbBoard::new(&tmp.0, "ana", "shop");
        let blog = DbBoard::new(&tmp.0, "ana", "blog");
        let waits = shop
            .add(&Ticket {
                repository: Some("/srv/shop.git".into()),
                ..draft("Waits", "web/coder", "t")
            })
            .unwrap();
        let elsewhere = shop.add(&draft("Elsewhere", "pcb/router", "t")).unwrap();
        let backlog = shop.add(&draft("Backlog", "", "t")).unwrap();
        let ended = blog.add(&draft("Ended", "web/coder", "t")).unwrap();
        blog.stop(&ended).unwrap();
        let given = blog.add(&draft("Given", "web/coder", "t")).unwrap();
        blog.give_back(&given, "desk", "no key").unwrap();
        let running = blog.add(&draft("Running", "web/coder", "t")).unwrap();
        blog.take(&running, "laptop", 60, true).unwrap();
        let asks = blog.add(&draft("Asks", "web/coder", "t")).unwrap();
        blog.take(&asks, "desk", 60, true).unwrap();
        blog.end(
            &asks,
            "desk",
            End::Ask {
                question: "Where?".into(),
                reply: String::new(),
                repository: true,
            },
            None,
        )
        .unwrap();
        DbBoard::new(&tmp.0, "rex", "blog")
            .add(&draft("Not ana's", "web/coder", "t"))
            .unwrap();
        let found = waiting(
            &tmp.0,
            "ana",
            "desk",
            &["web/coder".into(), "web/reviewer".into()],
            &[],
        )
        .unwrap();
        let listed: Vec<(&str, &str)> = found
            .iter()
            .map(|w| (w.project.as_str(), w.task.id()))
            .collect();
        assert_eq!(listed, [("blog", asks.as_str()), ("shop", waits.as_str())]);
        assert_eq!(found[0].task.question.as_deref(), Some("Where?"));
        assert_eq!(
            found[1].task.ticket.repository.as_deref(),
            Some("/srv/shop.git")
        );
        for left in [elsewhere, backlog] {
            assert!(!listed.iter().any(|(_, id)| *id == left), "{left}");
        }
    }

    #[test]
    fn a_new_task_keeps_the_notes_the_hub_recalls_for_it_and_none_its_maker_sent() {
        let tmp = TempDir::new();
        let service = Service::new(&tmp.0).with_notes(|user, query| {
            (!query.contains("nothing")).then(|| format!("notes for {user} about {query}"))
        });
        let mut ticket = draft("Deploy", "web/coder", "Ship the cart.");
        ticket.notes = Some("mine, not the hub's".into());
        let made = service
            .handle(
                &alone("ana"),
                TASK_CREATE,
                &json!({"project": "shop", "ticket": ticket}),
            )
            .unwrap();
        let id = made["id"].as_str().unwrap();
        let kept = board(&tmp).detail(id).unwrap().unwrap().task.ticket;
        assert_eq!(
            kept.notes.as_deref(),
            Some("notes for ana about Deploy\nShip the cart.")
        );
        let made = service
            .handle(
                &alone("ana"),
                TASK_CREATE,
                &json!({"project": "shop", "ticket": draft("Other", "web/coder", "nothing")}),
            )
            .unwrap();
        let kept = board(&tmp)
            .detail(made["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(kept.task.ticket.notes, None);
        for project in ["..", ".git", "a/b"] {
            let err = service
                .handle(&alone("ana"), TASKS, &json!({ "project": project }))
                .unwrap_err();
            assert!(err.contains("not a project name"), "{project}: {err}");
        }
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

    /// root's runner gpu, which works for the hub and lends `web/coder`.
    fn gpu() -> Lender {
        Lender {
            name: "root/gpu".into(),
            lends: vec!["web/coder".into()],
        }
    }

    /// gpu as one of the runners that may take ana's tasks.
    fn lending() -> Present {
        Present {
            name: "root/gpu".into(),
            agents: vec!["web/coder".into()],
            own: false,
        }
    }

    #[test]
    fn another_accounts_runner_is_offered_a_task_only_when_no_runner_of_its_own_takes_it() {
        let tmp = TempDir::new();
        let connected = [present("desk", &["web/coder"]), lending()];
        let own = board(&tmp).with_runners(&connected);
        let lent = board(&tmp).with_runners(&connected).for_lender(gpu());
        let id = own.add(&draft("A", "web/coder", "t")).unwrap();
        let other = own.add(&draft("R", "web/reviewer", "t")).unwrap();
        // The desk has the agent and has not given it back: it is the desk's.
        assert!(lent.list().unwrap().is_empty());
        assert_eq!(lent.detail(&id).unwrap(), None);
        let refused = lent.take(&id, "root/gpu", 60, true).unwrap();
        assert!(
            matches!(&refused, Claim::Refused(why) if why.contains("waits for a runner of ana's own")),
            "{refused:?}"
        );
        // Busy is not giving back: running on the desk, it stays the desk's.
        own.take(&id, "desk", 60, true).unwrap();
        assert!(lent.list().unwrap().is_empty());
        // The desk gives it back: now gpu may take it, and is the last to try.
        assert!(!own.give_back(&id, "desk", "no key").unwrap());
        let listed: Vec<String> = lent.list().unwrap().iter().map(|t| t.id().into()).collect();
        assert_eq!(
            listed,
            std::slice::from_ref(&id),
            "never the reviewer's: not lent"
        );
        assert_eq!(lent.take(&id, "root/gpu", 60, true).unwrap(), Claim::Taken);
        assert_eq!(
            own.detail(&id).unwrap().unwrap().task.running_on.as_deref(),
            Some("root/gpu")
        );
        assert_eq!(lent.keep(&id, "root/gpu", 60).unwrap(), Renewal::Kept);
        assert!(lent.give_back(&id, "root/gpu", "cannot clone").unwrap());
        assert!(
            own.list().unwrap()[0].blocked,
            "every one that may take it gave it back"
        );
        // An agent it does not lend: never offered, never given back by it.
        assert!(matches!(
            lent.take(&other, "root/gpu", 60, true).unwrap(),
            Claim::Refused(_)
        ));
        let err = lent.give_back(&other, "root/gpu", "x").unwrap_err();
        assert!(err.contains("does not lend"), "{err}");

        // With no runner of ana's with the agent connected, it is offered at
        // once, and so is what it comes after, to read.
        let alone = [lending()];
        let lent = board(&tmp).with_runners(&alone).for_lender(gpu());
        let first = own.add(&draft("First", "web/reviewer", "t")).unwrap();
        let then = own
            .add(&Ticket {
                after: vec![first.clone()],
                ..draft("Then", "web/coder", "t")
            })
            .unwrap();
        let listed: Vec<String> = lent.list().unwrap().iter().map(|t| t.id().into()).collect();
        assert_eq!(listed, [first.clone(), then.clone()]);
        assert_eq!(lent.detail(&first).unwrap(), None, "only what it may take");
        assert_eq!(
            lent.take(&then, "root/gpu", 60, true).unwrap(),
            Claim::Refused(format!("it waits for {first}"))
        );
    }

    /// A service asked by root's runner gpu, a lender, with `compute` for
    /// ana alone, and no runner of ana's connected.
    fn as_gpu(lender: Option<Lender>) -> Asker<'static> {
        fn runners(account: &str) -> Vec<Present> {
            if account == "ana" {
                vec![lending()]
            } else {
                Vec::new()
            }
        }
        fn compute(account: &str) -> bool {
            account == "ana"
        }
        Asker {
            user: "root",
            lender,
            runners: &runners,
            compute: &compute,
        }
    }

    #[test]
    fn a_runner_acts_on_another_accounts_task_only_as_a_lender_the_hub_lets() {
        let tmp = TempDir::new();
        let shared: &'static Mutex<Vec<String>> = Box::leak(Box::default());
        let service = Service::new(&tmp.0).on_done(|user, project, _| {
            shared.lock().unwrap().push(format!("{user}/{project}"));
        });
        let id = board(&tmp).add(&draft("A", "web/coder", "t")).unwrap();
        let rexs = DbBoard::new(&tmp.0, "rex", "shop")
            .add(&draft("Rex's", "web/coder", "t"))
            .unwrap();
        let ask =
            |asker: &Asker<'_>, method: &str, params: Value| service.handle(asker, method, &params);
        let on = |params: Value| {
            let mut p = json!({"account": "ana", "project": "shop", "id": id, "runner": "root/gpu", "node": "root/gpu"});
            for (k, v) in params.as_object().unwrap() {
                p[k] = v.clone();
            }
            p
        };

        // What it finds waiting: root's own, then ana's it may take; never
        // rex's, whose role has no `compute`.
        let found = ask(
            &as_gpu(Some(gpu())),
            TASKS_WAITING,
            json!({"runner": "gpu", "agents": ["web/coder"]}),
        )
        .unwrap();
        assert_eq!(found["runner"], "root/gpu");
        let tasks = found["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{found}");
        assert_eq!(
            (
                tasks[0]["account"].as_str(),
                tasks[0]["task"]["ticket"]["id"].as_str()
            ),
            (Some("ana"), Some(id.as_str()))
        );
        // A runner that is no lender finds only its own account's.
        let found = ask(
            &as_gpu(None),
            TASKS_WAITING,
            json!({"runner": "gpu", "agents": ["web/coder"]}),
        )
        .unwrap();
        assert_eq!(found["tasks"], json!([]));

        // Each refused: no lender (not connected, works for its owner, or no
        // `host`), a method only the account asks, another name, an account
        // without `compute`.
        let err = ask(&as_gpu(None), TASK_CLAIM, on(json!({}))).unwrap_err();
        assert!(
            err.contains("works for the hub, with the `host` permission"),
            "{err}"
        );
        for method in [TASK_CREATE, TASK_ANSWER, TASK_ASSIGN, TASK_CANCEL] {
            let err = ask(&as_gpu(Some(gpu())), method, on(json!({}))).unwrap_err();
            assert!(err.contains("only ana does this"), "{method}: {err}");
        }
        for (key, name) in [("node", "gpu"), ("runner", "root/lab")] {
            let err = ask(&as_gpu(Some(gpu())), TASK_CLAIM, on(json!({ key: name }))).unwrap_err();
            assert!(err.contains("the runner is root/gpu"), "{key}: {err}");
        }
        let err = ask(
            &as_gpu(Some(gpu())),
            TASK_CLAIM,
            on(json!({"account": "rex", "id": rexs})),
        )
        .unwrap_err();
        assert!(err.contains("rex's role has no `compute`"), "{err}");
        // An agent it does not lend.
        let lab = Lender {
            lends: vec!["web/reviewer".into()],
            ..gpu()
        };
        let claim = ask(&as_gpu(Some(lab)), TASK_CLAIM, on(json!({}))).unwrap();
        assert_eq!(
            claim["claim"]["refused"]
                .as_str()
                .map(|w| w.contains("does not lend")),
            Some(true)
        );

        // As a lender: it runs ana's task, and what it reports is ana's.
        let claim = ask(&as_gpu(Some(gpu())), TASK_CLAIM, on(json!({}))).unwrap();
        assert_eq!(claim["claim"], "taken");
        let listed = ask(&as_gpu(Some(gpu())), TASKS, on(json!({}))).unwrap();
        assert_eq!(listed["tasks"][0]["running_on"], "root/gpu");
        let end = serde_json::to_value(result("done")).unwrap();
        ask(&as_gpu(Some(gpu())), TASK_FINISH, on(json!({ "end": end }))).unwrap();
        assert_eq!(*shared.lock().unwrap(), ["ana/shop"]);
        assert_eq!(board(&tmp).list().unwrap()[0].status, Some(Status::Done));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_reaches_its_tasks_through_the_hub() {
        let tmp = TempDir::new();
        let hub =
            crate::server::testing::start_with_tasks(Box::leak(Box::new(Service::new(&tmp.0))));
        let laptop = ensemble_tasks::board::RemoteBoard::new(&hub.url, &hub.ana, "shop");
        let kept = DbBoard::new(&tmp.0, "ana", "shop");
        let id = kept.add(&draft("A", "web/coder", "t")).unwrap();
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
        let err = ensemble_tasks::board::RemoteBoard::new(&hub.url, &hub.ana, "a/b")
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
        let hub = crate::server::testing::start_with_tasks(Box::leak(Box::new(service)));
        let laptop = ensemble_tasks::board::RemoteBoard::new(&hub.url, &hub.ana, "shop");
        let mut ticket = draft("Retry fetches", "web/coder", "t");
        ticket.goal = Some("Requests survive a flaky network.".into());
        let kept = DbBoard::new(&tmp.0, "ana", "shop");
        let id = kept.add(&ticket).unwrap();
        // Asking is no end: nothing is given.
        laptop.claim(&id, "laptop", LEASE, true).await.unwrap();
        let ask = End::Ask {
            question: "Which status codes?".into(),
            reply: "r".into(),
            repository: false,
        };
        laptop.finish(&id, "laptop", ask, None).await.unwrap();
        assert!(given.lock().unwrap().is_empty());
        kept.reply(&id, "502 and 503").unwrap();
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
            ..kept.detail(&id).unwrap().unwrap()
        };
        assert_eq!(learned("shop", &failed), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_takes_another_accounts_task_only_while_it_is_connected_and_works_for_the_hub()
    {
        use ensemble_link::client::Connection;
        use ensemble_link::protocol::RUNNER;
        use ensemble_tasks::board::RemoteBoard;
        let tmp = TempDir::new();
        let hub =
            crate::server::testing::start_with_tasks(Box::leak(Box::new(Service::new(&tmp.0))));
        let id = board(&tmp).add(&draft("A", "web/coder", "t")).unwrap();
        let gpu = RemoteBoard::new(&hub.url, &hub.root, "shop").of_account("ana", "root/gpu");
        let as_gpu = |works_for: &str| {
            json!({"name": "gpu", "works_for": works_for,
                   "agents": [{"name": "web/coder", "about": ""}], "lends": ["web/coder"]})
        };
        // Not connected.
        let err = gpu.claim(&id, "root/gpu", LEASE, true).await.unwrap_err();
        assert!(
            err.contains("connected now that works for the hub"),
            "{err}"
        );
        // Connected, working for its owner alone.
        let mut link = Connection::open(&hub.url, &hub.root).await.unwrap();
        link.call(RUNNER, as_gpu("owner")).await.unwrap();
        let err = gpu.claim(&id, "root/gpu", LEASE, true).await.unwrap_err();
        assert!(
            err.contains("connected now that works for the hub"),
            "{err}"
        );
        // Working for the hub, but asked with a token of root's without
        // `host`.
        link.call(RUNNER, as_gpu("hub")).await.unwrap();
        let scoped = {
            let mut users = crate::users::Users::load(&hub.tmp.0.join("state")).unwrap();
            let (_, token, _) = users
                .create_token(
                    "root",
                    "tasks-only",
                    None,
                    Some(vec![ensemble_config::Permission::Tasks]),
                    &hub.server.roles(),
                )
                .unwrap();
            users.save().unwrap();
            token
        };
        let err = RemoteBoard::new(&hub.url, &scoped, "shop")
            .of_account("ana", "root/gpu")
            .claim(&id, "root/gpu", LEASE, true)
            .await
            .unwrap_err();
        assert!(err.contains("`host`"), "{err}");
        // ana has no runner with the agent, and `compute`: gpu takes it.
        assert_eq!(
            gpu.claim(&id, "root/gpu", LEASE, true).await.unwrap(),
            Claim::Taken
        );
        assert_eq!(
            gpu.scan().await.unwrap()[0].running_on.as_deref(),
            Some("root/gpu")
        );
        // Gone, it acts on ana's tasks no more.
        link.close().await;
        drop(link);
        let gone = async {
            while hub.server.routes.lender("root", "gpu").is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), gone)
            .await
            .expect("a runner that went away is no lender");
        let err = gpu.renew(&id, "root/gpu", LEASE).await.unwrap_err();
        assert!(err.contains("connected now"), "{err}");
    }
}
