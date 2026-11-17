//! Work the caller does not wait for: tasks, and background delegations.
//!
//! A blocking `delegate` holds the caller's tool call open for as long as the
//! agent runs, and the caller's MCP client may give up on it long before that.
//! Two things here avoid that:
//!
//! - A **task** is a ticket kept on the hub ([`crate::board`]). The manager
//!   writes one with `submit`, or a person or the hub's inbox does. While the
//!   folder's worker runs ([`Queue::work`], in the node), it claims every task
//!   of the folder's project that may start, oldest id first, as its agent's
//!   slots allow, renews its claim while the agent works, and tells the hub
//!   how the run ended. A task the worker's end interrupts is put back, and a
//!   node that dies stops renewing, so either way another run takes it up.
//! - A **background delegation** (`delegate` with `background`) is a one-off:
//!   it answers at once with an id (`adhoc-N`), and what it did is kept in
//!   memory and in the git-ignored `runs/` only.
//!
//! The caller follows both with `status`, collects them with `wait` and stops
//! them with `cancel`. Progress notifications go to the client program and
//! never reach the model, so the activity lines kept here are the only way the
//! model can see what an agent is doing.
//!
//! Every delegation of the session, blocking ones included, takes a slot of
//! its agent first, so `max_concurrent` means what it says.

use super::delegation::{ToolResult, run_delegation};
use crate::board::{Board, Cancelled, Claim, End, LEASE, Progress, Renewal};
use crate::runs::Runs;
use crate::tasks::{
    ASK_MARKER, Detail, HANDOFF_MARKER, Status, Task, Ticket, entry_line, handoff_in, idle_line,
    minutes, question_in, utc, waiting_for,
};
use ensemble_agents::external::gate;
use ensemble_agents::router::Ensemble;
use ensemble_agents::runtime::blocking;
use ensemble_agents::worktree;
use ensemble_config::{Budget, Isolation};
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

/// Activity lines kept per run.
const KEPT_LINES: usize = 8;
/// Activity lines shown per run when listing every one.
const LISTED_LINES: usize = 3;
/// Ended tasks shown when listing every task; older ones are counted.
const LISTED_ENDED: usize = 10;
/// How often the worker looks at the hub for tasks that may start.
const SCAN_EVERY: Duration = Duration::from_secs(2);
/// How many times a run's end is told to the hub before it is given up.
const FINISH_ATTEMPTS: u32 = 3;
/// How often a delegation waiting for a slot checks whether it was cancelled.
const SLOT_POLL: Duration = Duration::from_millis(200);

/// A run in progress, or waiting for a slot.
struct Live {
    agent: String,
    title: String,
    started: Instant,
    waiting: bool,
    recent: VecDeque<String>,
    cancel: Arc<AtomicBool>,
    /// Set when the caller cancelled it, as opposed to the session ending.
    asked: bool,
}

/// How a background delegation ended.
struct Ended {
    agent: String,
    title: String,
    seconds: u64,
    text: String,
    status: Status,
}

/// A background delegation, as `delegate` started it.
struct Adhoc {
    id: String,
    agent: String,
    task: String,
    context: Option<String>,
    repo: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
}

/// One run of [`Queue::work`]: a task or a background delegation.
type Run<'q> = Pin<Box<dyn Future<Output = ()> + Send + 'q>>;

/// What one look found to start.
struct Looked<'q> {
    adhoc: Vec<Adhoc>,
    /// Tasks claimed on the board, each with a slot of its agent and the flag
    /// that stops it.
    claimed: Vec<(Ticket, Slot<'q>, Arc<AtomicBool>)>,
    /// The caller is gone: nothing new starts.
    closed: bool,
}

#[derive(Default)]
struct Inner {
    /// Set by `close`: the caller is gone, and nothing new starts.
    closed: bool,
    last_adhoc: u64,
    live: BTreeMap<String, Live>,
    /// Background delegations started and not yet taken up by [`Queue::work`].
    started: Vec<Adhoc>,
    /// Background delegations that ended in this session.
    ended: BTreeMap<String, Ended>,
    /// Slots in use, per agent.
    busy: HashMap<String, u32>,
    /// When each agent with a budget started its runs in the last hour.
    usage: BTreeMap<String, Vec<u64>>,
    /// Questions that delegations stopped to ask, by session, waiting for the
    /// caller's answer.
    asks: HashMap<String, Ask>,
    asked: u64,
    /// Tasks already noted as waiting for a backend, so the note is written
    /// once per wait, not once per scan.
    backend_waits: HashSet<String>,
    /// Whether the hub could not be reached at the last look, so that is said
    /// once, not every few seconds.
    board_down: bool,
}

/// A delegation that stopped to ask, kept so that an answer can resume it. It
/// lives as long as the MCP session.
#[derive(Debug, Clone)]
pub struct Ask {
    pub agent: String,
    /// The task as it was given.
    pub task: String,
    pub context: Option<String>,
    pub repo: Option<std::path::PathBuf>,
    pub question: String,
    /// What the agent said when it stopped.
    pub reply: String,
}

impl Ask {
    /// What the agent is told when it goes on: the task, its question, the
    /// answer and what it had said.
    pub fn resume_prompt(&self, answer: &str) -> String {
        format!(
            "{}\n\n{}\n\nYou stopped earlier to ask: {}\nThe answer: {}\n\nYour reply when you last stopped:\n{}\n\nGo on from there.",
            self.task,
            crate::delegation::ASK_INSTRUCTION,
            self.question,
            answer.trim(),
            self.reply.trim()
        )
    }
}

/// What the hub's knowledge vault holds about a topic, in at most a number of
/// characters of notes: the notes as text, `None` when it holds none, or why
/// it could not be asked.
pub type Recall<'e> =
    dyn Fn(String, usize) -> BoxFuture<'e, Result<Option<String>, String>> + Sync + 'e;

/// The most of a task's text used as the topic of a recall.
const MAX_TOPIC_CHARS: usize = 1000;

/// Whether a folder's worker runs its project's tasks now: when a computer
/// has two folders of one name, only the one used last does.
pub type Turn<'e> = dyn Fn() -> bool + Send + Sync + 'e;

/// The tasks and background delegations of one folder.
pub struct Queue<'e> {
    ensemble: &'e Ensemble,
    /// Where the folder's project's tasks are kept: on the node's hub.
    /// Without one there are no tasks, and only background delegations run.
    board: Option<Box<dyn Board + 'e>>,
    /// This node's name, which its claims on tasks carry.
    node: String,
    /// How long a claim lasts unless renewed.
    lease: Duration,
    turn: Option<Box<Turn<'e>>>,
    /// What this node keeps of its runs here: logs, worktrees, usage. Without
    /// it (a request handled outside a session) runs are kept in memory.
    runs: Option<Runs>,
    /// The session's hub knowledge, for agents given notes with their tasks.
    recall: Option<Box<Recall<'e>>>,
    inner: Mutex<Inner>,
    /// Signalled whenever something starts, ends, frees a slot or is
    /// submitted.
    changed: Notify,
}

/// One slot of an agent, given back when dropped.
pub struct Slot<'q> {
    inner: &'q Mutex<Inner>,
    changed: &'q Notify,
    agent: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = inner.busy.get_mut(&self.agent) {
            *n = n.saturating_sub(1);
        }
        drop(inner);
        self.changed.notify_waiters();
    }
}

impl<'e> Queue<'e> {
    pub fn new(ensemble: &'e Ensemble, runs: Option<Runs>) -> Self {
        let last_adhoc = runs.as_ref().map_or(0, Runs::last_adhoc);
        let usage = runs.as_ref().map(Runs::usage).unwrap_or_default();
        Queue {
            ensemble,
            board: None,
            node: String::new(),
            lease: LEASE,
            turn: None,
            runs,
            recall: None,
            inner: Mutex::new(Inner {
                last_adhoc,
                usage,
                ..Inner::default()
            }),
            changed: Notify::new(),
        }
    }

    /// This queue, with the tasks of `board`, run as the node `node`.
    pub fn with_board(mut self, board: impl Board + 'e, node: &str) -> Self {
        self.board = Some(Box::new(board));
        self.node = node.to_string();
        self
    }

    /// Claims that last `lease` unless renewed, in place of [`LEASE`].
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// Run the board's tasks only while `turn` says so.
    pub fn with_turn(mut self, turn: impl Fn() -> bool + Send + Sync + 'e) -> Self {
        self.turn = Some(Box::new(turn));
        self
    }

    /// Where the tasks are kept, when they are.
    pub fn board(&self) -> Option<&dyn Board> {
        self.board.as_deref()
    }

    /// What this node keeps of its runs here, when it keeps them.
    pub fn runs(&self) -> Option<&Runs> {
        self.runs.as_ref()
    }

    /// The board, or why there are no tasks.
    fn tasks(&self) -> Result<&dyn Board, ToolResult> {
        self.board.as_deref().ok_or_else(|| {
            ToolResult::err(
                "tasks are kept on a hub, and this node names none: add `hub:` to \
                 ~/.config/ensemble/node/node.yaml and sign in with `ensemble login`. Without \
                 one, `delegate` with `background: true` runs work you do not wait for.",
            )
        })
    }

    /// Every task of the board, or why they cannot be read.
    async fn all(&self, board: &dyn Board) -> Result<Vec<Task>, ToolResult> {
        board.scan().await.map_err(|e| {
            ToolResult::err(format!("cannot read the tasks on {}: {e}", board.place()))
        })
    }

    /// This queue, with the agents that have `recall` given what `recall`
    /// finds about each of their tasks.
    pub fn with_recall(
        mut self,
        recall: impl Fn(String, usize) -> BoxFuture<'e, Result<Option<String>, String>> + Sync + 'e,
    ) -> Self {
        self.recall = Some(Box::new(recall));
        self
    }

    /// The context `agent` is given with a task about `topic`: `context`, and
    /// after it the notes of the knowledge vault about the task when the agent
    /// has `recall`, within its `max_chars`. A vault that cannot be asked is
    /// told to `progress`, and the task goes on without notes.
    pub async fn with_notes(
        &self,
        agent: &str,
        topic: &str,
        context: Option<&str>,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Option<String> {
        let context = context.map(str::to_string);
        let Some(limits) = self.ensemble.config().agent(agent).and_then(|a| a.recall) else {
            return context;
        };
        let Some(recall) = &self.recall else {
            return context;
        };
        let topic: String = topic.chars().take(MAX_TOPIC_CHARS).collect();
        let notes = match recall(topic, limits.max_chars as usize).await {
            Ok(Some(notes)) => notes,
            Ok(None) => {
                progress(&format!(
                    "{agent}: the knowledge vault has no notes about this"
                ));
                return context;
            }
            Err(e) => {
                progress(&format!("{agent}: no notes from the knowledge vault: {e}"));
                return context;
            }
        };
        progress(&format!(
            "{agent}: given {} characters of notes from the knowledge vault",
            notes.chars().count()
        ));
        Some(match context.filter(|c| !c.trim().is_empty()) {
            Some(c) => format!("{c}\n\n{notes}"),
            None => notes,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolves at the next change. Made before what it waits for is looked
    /// at, so a change in between is not missed.
    fn watch(&self) -> Pin<Box<Notified<'_>>> {
        let mut changed = Box::pin(self.changed.notified());
        changed.as_mut().enable();
        changed
    }

    /// Keep a question that a delegation stopped to ask, and return the session
    /// the answer is sent to.
    pub fn remember(&self, ask: Ask) -> String {
        let mut inner = self.lock();
        inner.asked += 1;
        let session = format!("ask-{}", inner.asked);
        inner.asks.insert(session.clone(), ask);
        session
    }

    /// The question under `session`, once: answering it ends the session.
    pub fn take_ask(&self, session: &str) -> Option<Ask> {
        self.lock().asks.remove(session)
    }

    /// What `agent`'s slots and budget are counted under: its own name, or the
    /// shared limit it names, so that every agent on one limit uses one count.
    fn key(&self, agent: &str) -> String {
        match self
            .ensemble
            .config()
            .agent(agent)
            .and_then(|a| a.limit.as_ref())
        {
            Some(limit) => format!("limit:{limit}"),
            None => agent.to_string(),
        }
    }

    /// Seconds until the runs of `agent`, and of every agent on its limit,
    /// have used fewer than `tokens` in the last hour, or `None` if they have.
    /// Counted from the record of runs, from what each reported.
    fn over_tokens(&self, agent: &str, tokens: u64) -> Option<u64> {
        let config = self.ensemble.config();
        let members: Vec<&str> = match config.agent(agent).and_then(|a| a.limit.as_ref()) {
            Some(limit) => config
                .agents
                .values()
                .filter(|a| a.limit.as_ref() == Some(limit))
                .map(|a| a.name.as_str())
                .collect(),
            None => vec![agent],
        };
        let now = unix_now();
        let mut runs: Vec<(u64, u64)> = ensemble_core::records::read(&self.ensemble.records_dir()?)
            .into_iter()
            .filter(|r| members.contains(&r.agent.as_str()) && now < r.started + HOUR)
            .map(|r| {
                (
                    r.started,
                    r.prompt_tokens.unwrap_or(0) + r.completion_tokens.unwrap_or(0),
                )
            })
            .collect();
        let mut left: u64 = runs.iter().map(|r| r.1).sum();
        if left < tokens {
            return None;
        }
        runs.sort_unstable();
        for (started, used) in runs {
            left -= used;
            if left < tokens {
                return Some((started + HOUR).saturating_sub(now).max(1));
            }
        }
        None
    }

    /// How many delegations to `agent` may run at once. An unknown agent
    /// fails as soon as it starts, so it needs no limit.
    fn limit(&self, agent: &str) -> u32 {
        self.ensemble
            .config()
            .agent(agent)
            .map_or(u32::MAX, |a| a.max_concurrent)
    }

    /// Seconds until `agent` may start another run, or `None` if it may now.
    fn over_budget(&self, inner: &Inner, agent: &str) -> Option<u64> {
        let key = self.key(agent);
        let delegations = match self.ensemble.config().agent(agent).map(|a| a.budget) {
            Some(Budget::PerHour { delegations }) => delegations,
            Some(Budget::PerHourTokens { tokens }) => return self.over_tokens(agent, tokens),
            _ => return None,
        };
        let now = unix_now();
        let mut recent: Vec<u64> = inner
            .usage
            .get(&key)
            .map(|runs| runs.iter().copied().filter(|t| now < t + HOUR).collect())
            .unwrap_or_default();
        if recent.len() < delegations as usize {
            return None;
        }
        recent.sort_unstable();
        // The run that frees a place is the one that leaves the hour first.
        let frees = recent[recent.len() - delegations as usize];
        Some((frees + HOUR).saturating_sub(now).max(1))
    }

    /// Count a run of `agent` against its budget, or say how long until it
    /// may start one.
    fn spend_locked(&self, inner: &mut Inner, agent: &str) -> Result<(), u64> {
        if let Some(wait) = self.over_budget(inner, agent) {
            return Err(wait);
        }
        if !matches!(
            self.ensemble.config().agent(agent).map(|a| a.budget),
            Some(Budget::PerHour { .. })
        ) {
            return Ok(());
        }
        let now = unix_now();
        let started = inner.usage.entry(self.key(agent)).or_default();
        started.retain(|t| now < t + HOUR);
        started.push(now);
        if let Some(runs) = &self.runs {
            runs.save_usage(&inner.usage);
        }
        Ok(())
    }

    /// [`Self::spend_locked`], as a refusal the caller can read.
    pub fn spend(&self, agent: &str) -> Result<(), String> {
        self.spend_locked(&mut self.lock(), agent)
            .map_err(|wait| over_budget_message(agent, wait))
    }

    /// Take a slot of `agent` if one is free.
    fn take(&self, inner: &mut Inner, agent: &str) -> Option<Slot<'_>> {
        let key = self.key(agent);
        let used = inner.busy.entry(key.clone()).or_default();
        if *used >= self.limit(agent) {
            return None;
        }
        *used += 1;
        Some(Slot {
            inner: &self.inner,
            changed: &self.changed,
            agent: key,
        })
    }

    /// Wait for a slot of `agent`, calling `waiting` once if there is none
    /// free. `None` when `cancel` is set first. The session's end frees slots
    /// by stopping background work, so a blocking call still gets one.
    pub async fn slot(
        &self,
        agent: &str,
        cancel: &AtomicBool,
        waiting: &(dyn Fn() + Sync),
    ) -> Option<Slot<'_>> {
        let mut told = false;
        loop {
            let changed = self.watch();
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(slot) = self.take(&mut self.lock(), agent) {
                return Some(slot);
            }
            if !told {
                told = true;
                waiting();
                continue;
            }
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep(SLOT_POLL) => {}
            }
        }
    }

    /// Record what the agent of run `id` is doing now.
    fn note(&self, id: &str, line: &str) {
        if let Some(live) = self.lock().live.get_mut(id) {
            if live.recent.len() == KEPT_LINES {
                live.recent.pop_front();
            }
            live.recent.push_back(line.to_string());
        }
        if let Some(runs) = &self.runs {
            runs.log(id, line);
        }
    }

    fn live(agent: &str, title: &str, cancel: Arc<AtomicBool>, waiting: bool) -> Live {
        Live {
            agent: agent.to_string(),
            title: super::delegation::first_line(title),
            started: Instant::now(),
            waiting,
            recent: VecDeque::new(),
            cancel,
            asked: false,
        }
    }

    /// Start a background delegation, and return its id at once. It runs in
    /// [`Self::work`].
    pub fn start_adhoc(
        &self,
        agent: &str,
        task: &str,
        context: Option<&str>,
        repo: Option<PathBuf>,
    ) -> String {
        let mut inner = self.lock();
        inner.last_adhoc += 1;
        let id = format!("adhoc-{}", inner.last_adhoc);
        // One registered after the caller left starts out cancelled.
        let cancel = Arc::new(AtomicBool::new(inner.closed));
        inner
            .live
            .insert(id.clone(), Self::live(agent, task, cancel.clone(), true));
        inner.started.push(Adhoc {
            id: id.clone(),
            agent: agent.to_string(),
            task: task.to_string(),
            context: context.map(str::to_string),
            repo,
            cancel,
        });
        drop(inner);
        self.changed.notify_waiters();
        id
    }

    async fn run_adhoc(&self, adhoc: Adhoc) {
        let Adhoc {
            id,
            agent,
            task,
            context,
            repo,
            cancel,
        } = adhoc;
        let waiting = || self.note(&id, &format!("{agent}: waiting for a free slot"));
        let result = match self.slot(&agent, &cancel, &waiting).await {
            Some(_slot) => {
                if let Some(live) = self.lock().live.get_mut(&id) {
                    live.waiting = false;
                }
                run_delegation(
                    self.ensemble,
                    &agent,
                    &task,
                    context.as_deref(),
                    repo.as_deref(),
                    &|line| self.note(&id, line),
                    &cancel,
                )
                .await
            }
            None => ToolResult::err("it was cancelled before it started."),
        };
        let mut inner = self.lock();
        if let Some(live) = inner.live.remove(&id) {
            let status = if live.cancel.load(Ordering::Relaxed) {
                Status::Cancelled
            } else if result.is_error {
                Status::Failed
            } else {
                Status::Done
            };
            inner.ended.insert(
                id,
                Ended {
                    agent: live.agent,
                    title: live.title,
                    seconds: live.started.elapsed().as_secs(),
                    text: result.text,
                    status,
                },
            );
        }
        drop(inner);
        self.changed.notify_waiters();
    }

    /// Run what this queue is given until it is closed and nothing it runs
    /// is left: background delegations, and with `tasks`, every task of the
    /// board that may start, oldest id first, as its agent's slots allow.
    /// The board is looked at again every few seconds, and at once when a
    /// task is submitted or a run ends.
    pub async fn work(&self, tasks: bool) {
        self.drive(tasks, false).await;
    }

    /// Run the background delegations started so far to their end, for a
    /// request handled with no session around it.
    pub async fn finish(&self) {
        self.drive(false, true).await;
    }

    /// Whether `task` may start here: assigned, not ended, not asking, not
    /// waiting for another task, and not running on another node.
    fn may_start(&self, task: &Task, all: &[Task]) -> bool {
        task.status.is_none()
            && !task.ticket.agent.is_empty()
            && task.question.is_none()
            && task.running_on.as_ref().is_none_or(|n| *n == self.node)
            && waiting_for(&task.ticket, all).is_empty()
    }

    /// The board's tasks, when this worker runs them now. A board that
    /// cannot be reached is said once, until it can again.
    async fn startable(&self, tasks: bool) -> (Option<&dyn Board>, Vec<Task>) {
        let board = self
            .board
            .as_deref()
            .filter(|_| tasks && self.turn.as_ref().is_none_or(|turn| turn()));
        let Some(board) = board else {
            return (None, Vec::new());
        };
        match board.scan().await {
            Ok(all) => {
                self.lock().board_down = false;
                (Some(board), all)
            }
            Err(e) => {
                let mut inner = self.lock();
                if !inner.board_down {
                    inner.board_down = true;
                    eprintln!(
                        "ensemble: cannot read the tasks on {}: {e}; trying again",
                        board.place()
                    );
                }
                (Some(board), Vec::new())
            }
        }
    }

    async fn drive(&self, tasks: bool, until_idle: bool) {
        let mut runs: FuturesUnordered<Run<'_>> = FuturesUnordered::new();
        loop {
            let changed = self.watch();
            // The runs go on while the board is looked at: a run renewing its
            // claim and the look share one link to the hub.
            let look = self.look(tasks);
            tokio::pin!(look);
            let looked = loop {
                tokio::select! {
                    looked = &mut look => break looked,
                    Some(()) = runs.next(), if !runs.is_empty() => {}
                }
            };
            for adhoc in looked.adhoc {
                runs.push(Box::pin(self.run_adhoc(adhoc)));
            }
            for (ticket, slot, cancel) in looked.claimed {
                let board = self.board.as_deref().expect("claimed on the board");
                runs.push(Box::pin(async move {
                    self.run_ticket(board, ticket, slot, &cancel).await
                }));
            }
            if runs.is_empty() && (looked.closed || until_idle) {
                return;
            }
            tokio::select! {
                Some(()) = runs.next(), if !runs.is_empty() => {}
                () = changed => {}
                () = tokio::time::sleep(SCAN_EVERY) => {}
            }
        }
    }

    /// One look at what to start: the background delegations started since
    /// the last, and the board's tasks that may start, each claimed with a
    /// slot of its agent taken.
    async fn look(&self, tasks: bool) -> Looked<'_> {
        let (board, all) = self.startable(tasks).await;
        // Agents whose tasks wait for a backend (`when_none: wait` and no
        // usable model), found before the lock is taken: asking a backend
        // can take seconds. Each backend is asked at most once a minute.
        let mut blocked: HashMap<&str, Option<String>> = HashMap::new();
        for task in all.iter().filter(|t| self.may_start(t, &all)) {
            let agent = task.ticket.agent.as_str();
            if !blocked.contains_key(agent) {
                let ready = blocking(|| self.ensemble.backend_ready(agent));
                blocked.insert(agent, ready.err());
            }
        }
        // Taken a slot for, to claim on the board.
        let mut picked = Vec::new();
        let (adhoc, closed) = {
            let mut inner = self.lock();
            let adhoc = std::mem::take(&mut inner.started);
            let closed = inner.closed;
            for task in all.iter().filter(|_| !closed) {
                let ticket = &task.ticket;
                if inner.live.contains_key(&ticket.id) || !self.may_start(task, &all) {
                    continue;
                }
                if let Some(Some(why)) = blocked.get(ticket.agent.as_str()) {
                    if inner.backend_waits.insert(ticket.id.clone())
                        && let Some(runs) = &self.runs
                    {
                        runs.log(&ticket.id, &format!("waiting for a backend: {why}"));
                    }
                    continue;
                }
                if self.over_budget(&inner, &ticket.agent).is_some() {
                    continue;
                }
                let Some(slot) = self.take(&mut inner, &ticket.agent) else {
                    continue;
                };
                let cancel = Arc::new(AtomicBool::new(false));
                inner.live.insert(
                    ticket.id.clone(),
                    Self::live(&ticket.agent, &ticket.title, cancel.clone(), false),
                );
                picked.push((ticket.clone(), slot, cancel));
            }
            (adhoc, closed)
        };
        let mut claimed = Vec::new();
        for (ticket, slot, cancel) in picked {
            let Some(board) = board else { break };
            let movable = self.movable(&ticket);
            match board
                .claim(&ticket.id, &self.node, self.lease, movable)
                .await
            {
                Ok(Claim::Taken) => {
                    let mut inner = self.lock();
                    // Checked under the lock above; a run that started since
                    // may have used the last place, which is counted all the
                    // same.
                    let _ = self.spend_locked(&mut inner, &ticket.agent);
                    inner.backend_waits.remove(&ticket.id);
                    claimed.push((ticket, slot, cancel));
                }
                // Another node was first, or the hub could not be asked: the
                // next look tries again.
                Ok(Claim::Refused(_)) | Err(_) => {
                    self.lock().live.remove(&ticket.id);
                    drop(slot);
                }
            }
        }
        Looked {
            adhoc,
            claimed,
            closed,
        }
    }

    /// Keep this node's claim on task `id` while it runs: renewed four times
    /// a lease. Someone cancelling the task, or the claim lost to another
    /// node (or not renewed for a whole lease), stops the run through
    /// `cancel`; a lost claim also sets `lost`. Never resolves.
    async fn hold(&self, board: &dyn Board, id: &str, cancel: &AtomicBool, lost: &AtomicBool) {
        let mut failing: Option<Instant> = None;
        loop {
            tokio::time::sleep(self.lease / 4).await;
            match board.renew(id, &self.node, self.lease).await {
                Ok(Renewal::Kept) => failing = None,
                Ok(Renewal::Cancel) => {
                    if let Some(live) = self.lock().live.get_mut(id) {
                        live.asked = true;
                    }
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(Renewal::Lost) => {
                    lost.store(true, Ordering::Relaxed);
                    cancel.store(true, Ordering::Relaxed);
                }
                Err(_) => {
                    if failing.get_or_insert_with(Instant::now).elapsed() >= self.lease {
                        lost.store(true, Ordering::Relaxed);
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// The repository `ticket` works in: the working folder, or its `repo`.
    fn project_of(&self, ticket: &Ticket) -> Result<PathBuf, String> {
        let workdir = self
            .ensemble
            .workdir()
            .map_err(|e| format!("no project directory to work in: {e}"))?;
        match ticket.repo.as_deref() {
            Some(repo) => ensemble_core::workspace::resolve_repo(&workdir, repo),
            None => Ok(workdir),
        }
    }

    /// Whether a run of `ticket` here could be carried on elsewhere: its agent
    /// works on a branch of its own, which can be pushed to the repository's
    /// remote. Anything else changes this computer's folder, so the task stays
    /// with this node once it has run.
    fn movable(&self, ticket: &Ticket) -> bool {
        let worktree = self
            .ensemble
            .config()
            .agent(&ticket.agent)
            .is_some_and(|a| a.isolation == Isolation::Worktree);
        worktree
            && self
                .project_of(ticket)
                .is_ok_and(|project| blocking(|| worktree::has_remote(&project)))
    }

    /// Push the task's branch, when its repository has a remote, and say
    /// where it is for the next run, wherever that is.
    fn push(&self, ticket: &Ticket, wt: &worktree::Worktree) -> Option<Progress> {
        if !blocking(|| worktree::has_remote(&wt.dir)) {
            return None;
        }
        match blocking(|| worktree::push(wt)) {
            Ok(commit) => {
                self.note(
                    &ticket.id,
                    &format!("pushed {} at {commit} to {}", wt.branch, worktree::REMOTE),
                );
                Some(Progress {
                    node: self.node.clone(),
                    branch: wt.branch.clone(),
                    commit,
                })
            }
            Err(e) => {
                self.note(
                    &ticket.id,
                    &format!("{} could not be pushed: {e}", wt.branch),
                );
                None
            }
        }
    }

    /// Tell the board how a run of `id` ended, trying again when the hub
    /// cannot be reached; a run whose end is never told is run again once its
    /// claim runs out.
    async fn tell(
        &self,
        board: &dyn Board,
        id: &str,
        end: Option<End>,
        progress: Option<Progress>,
    ) {
        let mut said = String::new();
        for attempt in 1..=FINISH_ATTEMPTS {
            let told = match &end {
                Some(end) => {
                    board
                        .finish(id, &self.node, end.clone(), progress.clone())
                        .await
                }
                None => board.release(id, &self.node, progress.clone()).await,
            };
            match told {
                Ok(()) => return,
                Err(e) => said = e,
            }
            if attempt < FINISH_ATTEMPTS {
                tokio::time::sleep(SCAN_EVERY).await;
            }
        }
        self.note(
            id,
            &format!("the hub was not told how this run ended: {said}"),
        );
        eprintln!(
            "ensemble: {}: cannot tell {} how {id} ended: {said}",
            self.node,
            board.place()
        );
    }

    async fn run_ticket(
        &self,
        board: &dyn Board,
        ticket: Ticket,
        slot: Slot<'_>,
        cancel: &AtomicBool,
    ) {
        let since = SystemTime::now();
        // What it asked and was answered, what earlier agents handed on, and
        // where the work of an earlier run was pushed.
        let detail = board.get(&ticket.id).await.ok().flatten();
        let carried = detail
            .as_ref()
            .and_then(|d| d.progress.as_ref())
            .map(|p| p.commit.clone());
        let place = self.project_of(&ticket).and_then(|project| {
            match self.ensemble.config().agent(&ticket.agent) {
                Some(a) if a.isolation == Isolation::Worktree => {
                    self.note(
                        &ticket.id,
                        &format!("{}: preparing a git worktree", ticket.agent),
                    );
                    let root = self
                        .runs
                        .as_ref()
                        .map(|r| r.root().to_path_buf())
                        .ok_or("no state directory for a worktree")?;
                    blocking(|| {
                        worktree::prepare_from(&project, &root, &ticket.id, carried.as_deref())
                    })
                    .map(|wt| (wt.dir.clone(), Some(wt)))
                }
                _ => Ok((project, None)),
            }
        });
        let lost = AtomicBool::new(false);
        let work = async {
            match &place {
                Ok((dir, _)) => self.work_on(&ticket, detail.as_ref(), dir, cancel).await,
                Err(e) => (ToolResult::err(e.clone()), Vec::new()),
            }
        };
        let (mut result, mut extra) = tokio::select! {
            done = work => done,
            () = self.hold(board, &ticket.id, cancel, &lost) => unreachable!("holding never ends"),
        };
        drop(slot);
        let lost = lost.load(Ordering::Relaxed);
        // Commit the work on the task's branch, unless the session ended under
        // it: then the worktree is left as it is, for the next session.
        let asked = self.lock().live.get(&ticket.id).is_some_and(|l| l.asked);
        let interrupted_now = cancel.load(Ordering::Relaxed) && !asked;
        // A question pauses the task: its worktree stays as it is until the
        // answer comes and the task goes on.
        let question = (!result.is_error && !cancel.load(Ordering::Relaxed))
            .then(|| question_in(&result.text))
            .flatten();
        // Handing the task on is not finishing it: it goes to another agent with
        // a note, and its worktree stays as it is for that agent.
        let mut handoff = None;
        if question.is_none()
            && !result.is_error
            && !cancel.load(Ordering::Relaxed)
            && let Some((target, note)) = handoff_in(&result.text)
        {
            match self.ensemble.config().agent(&target) {
                Some(_) if target != ticket.agent => handoff = Some((target.clone(), note)),
                Some(_) => {}
                None => {
                    result = ToolResult::err(format!(
                        "{}\n\n[it tried to hand the task to '{target}', which is not an agent. Agents: {}.]",
                        result.text,
                        self.ensemble.config().agent_names().join(", ")
                    ));
                }
            }
        }
        if let Some(question) = &question {
            self.note(
                &ticket.id,
                &format!("{}: needs input: {question}", ticket.agent),
            );
        }
        if let Ok((_, Some(wt))) = &place
            && !interrupted_now
            && question.is_none()
            && handoff.is_none()
        {
            extra.push(("branch", wt.branch.clone()));
            let message = format!("{}: {}", ticket.id, ticket.title);
            let note = match blocking(|| worktree::commit(wt, &message)) {
                Ok(Some(sha)) => {
                    extra.push(("commit", sha.clone()));
                    format!(
                        "[work committed on branch {} as {sha}; Ensemble does not merge it]",
                        wt.branch
                    )
                }
                Ok(None) => format!("[no changes to commit on branch {}]", wt.branch),
                Err(e) => format!(
                    "[the work in {} could not be committed: {e}]",
                    wt.dir.display()
                ),
            };
            result.text = format!("{}\n{note}", result.text);
        }
        // A run that stops unfinished keeps what it did so far on its branch,
        // and the branch goes to the remote, for the next run wherever it is.
        let progress = match &place {
            Ok((_, Some(wt))) if !lost => {
                if interrupted_now || question.is_some() || handoff.is_some() {
                    let message = format!("{}: work so far", ticket.id);
                    if let Err(e) = blocking(|| worktree::commit(wt, &message)) {
                        self.note(
                            &ticket.id,
                            &format!("the work so far was not committed: {e}"),
                        );
                    }
                }
                self.push(&ticket, wt)
            }
            _ => None,
        };
        let end = if lost {
            // Another node runs it now; what this run did stays here.
            self.note(
                &ticket.id,
                &format!(
                    "{}: stopped: this node's claim on the task ran out, and another node runs it now",
                    ticket.agent
                ),
            );
            None
        } else if interrupted_now {
            // The session ended under it: put back, to run again.
            Some(None)
        } else if let Some(question) = question {
            Some(Some(End::Ask {
                question,
                reply: result.text.clone(),
            }))
        } else if let Some((to, note)) = handoff {
            self.note(
                &ticket.id,
                &format!("{} handed the task to {to}", ticket.agent),
            );
            Some(Some(End::Handoff {
                from: ticket.agent.clone(),
                to,
                note,
                reply: result.text.clone(),
            }))
        } else {
            let status = if asked {
                Status::Cancelled
            } else if result.is_error {
                Status::Failed
            } else {
                Status::Done
            };
            Some(Some(End::Result {
                status,
                agent: ticket.agent.clone(),
                started: utc(since),
                extra: extra.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                text: result.text.clone(),
            }))
        };
        if let Some(end) = end {
            self.tell(board, &ticket.id, end, progress).await;
        }
        self.lock().live.remove(&ticket.id);
        self.changed.notify_waiters();
    }

    /// Run the agent on `ticket` in `dir`, then its gate if it has one. While
    /// the gate fails and attempts remain, the agent runs again with the
    /// gate's output. Returns the outcome and front matter for the result.
    async fn work_on(
        &self,
        ticket: &Ticket,
        detail: Option<&Detail>,
        dir: &Path,
        cancel: &AtomicBool,
    ) -> (ToolResult, Vec<(&'static str, String)>) {
        let note = |line: &str| self.note(&ticket.id, line);
        let mut base = ticket.prompt();
        if let Some((questions, reply)) = detail.and_then(|d| d.conversation.as_ref()) {
            base.push_str(&format!(
                "\n\nYou stopped earlier to ask. The questions and their answers:\n\n{}\n\nYour reply when you last stopped:\n{}\n\nGo on from there.",
                questions.trim(),
                reply.trim()
            ));
        }
        if let Some(p) = detail
            .and_then(|d| d.progress.as_ref())
            .filter(|p| p.node != self.node)
        {
            base.push_str(&format!(
                "\n\nThis task was started on {}: its work so far is in this folder, committed on {} at {}. Go on from there; do not start over.",
                p.node, p.branch, p.commit
            ));
        }
        if let Some(earlier) = detail.and_then(|d| d.handoffs.as_ref()) {
            base.push_str(&format!(
                "\n\nThis task was handed to you. What earlier agents did and left for you:\n\n{}",
                earlier.trim()
            ));
        }
        base.push_str(&format!(
            "\n\nIf you cannot finish without a decision that only the person who asked can make, stop and end your reply with one line: {ASK_MARKER} <your question>\nIf the next part belongs to another agent, end your reply with one line: {HANDOFF_MARKER} <agent> <what remains>"
        ));
        // Recalled once: a gate's retry is the same task.
        let context = self
            .with_notes(
                &ticket.agent,
                &format!("{}\n\n{}", ticket.title, ticket.prompt()),
                ticket.context.as_deref(),
                &note,
            )
            .await;
        let delegate = async |prompt: &str| {
            run_delegation(
                self.ensemble,
                &ticket.agent,
                prompt,
                context.as_deref(),
                Some(dir),
                &note,
                cancel,
            )
            .await
        };
        let Some(gate) = &ticket.gate else {
            return (delegate(&base).await, Vec::new());
        };
        let config = self.ensemble.config();
        let Some(check) = config.gate_for(&ticket.agent, &gate.name) else {
            return (
                ToolResult::err(format!(
                    "gate '{}' is not defined under `gates` in config.yaml, so the task did not run.",
                    gate.name
                )),
                Vec::new(),
            );
        };
        let attempts = gate.attempts.max(1);
        let name = serde_json::to_string(&gate.name).expect("a string serializes");
        let mut prompt = base.clone();
        let mut attempt = 1;
        loop {
            let reported = |verdict: &str| {
                vec![
                    ("gate", name.clone()),
                    ("gate_result", verdict.to_string()),
                    ("attempts", attempt.to_string()),
                ]
            };
            let result = delegate(&prompt).await;
            if result.is_error
                || cancel.load(Ordering::Relaxed)
                || question_in(&result.text).is_some()
            {
                return (result, reported("not run"));
            }
            note(&format!("gate {}: running `{}`", gate.name, check.command));
            let verdict = gate::run(check, dir, cancel).await;
            if verdict.passed {
                let text = format!(
                    "{}\n\n[gate {}: passed on attempt {attempt} of {attempts}]",
                    result.text, gate.name
                );
                return (ToolResult::ok(text), reported("passed"));
            }
            note(&format!(
                "gate {}: failed ({}) on attempt {attempt} of {attempts}",
                gate.name, verdict.ended
            ));
            if attempt >= attempts || cancel.load(Ordering::Relaxed) {
                let text = format!(
                    "{}\n\n[gate {}: failed ({}) on attempt {attempt} of {attempts}]\n```\n{}\n```",
                    result.text, gate.name, verdict.ended, verdict.output
                );
                return (ToolResult::err(text), reported("failed"));
            }
            if let Err(refused) = self.spend(&ticket.agent) {
                let text = format!(
                    "{}\n\n[gate {}: failed ({}) on attempt {attempt} of {attempts}; no retry: {refused}]\n```\n{}\n```",
                    result.text, gate.name, verdict.ended, verdict.output
                );
                return (ToolResult::err(text), reported("failed"));
            }
            attempt += 1;
            prompt = format!(
                "{}\n\nYour previous attempt did not pass the check `{}` ({}). Its output:\n```\n{}\n```\n\nYour previous reply was:\n{}\n\nFix what the check reports.",
                base, check.command, verdict.ended, verdict.output, result.text
            );
        }
    }

    /// Keep a new task on the hub for a worker to run, and return at once.
    pub async fn submit(&self, ticket: &Ticket) -> ToolResult {
        let board = match self.tasks() {
            Ok(board) => board,
            Err(refused) => return refused,
        };
        match board.create(ticket).await {
            Ok(id) => {
                self.changed.notify_waiters();
                let stays = if ticket.agent.is_empty() || self.movable(ticket) {
                    ""
                } else {
                    " Its agent changes this folder directly (or its repository has no \
                     remote), so once it starts it stays with this computer."
                };
                ToolResult::ok(format!(
                    "task {id} queued. `status` shows where it is; `wait` with task {id} \
                     returns its result.{stays}"
                ))
            }
            Err(e) => ToolResult::err(format!("cannot keep the task on {}: {e}", board.place())),
        }
    }

    /// One task with what it said, from the hub.
    pub async fn detail(&self, id: &str) -> Result<Option<Detail>, String> {
        match self.board.as_deref() {
            Some(board) => board.get(id).await,
            None => Ok(None),
        }
    }

    /// What one task or background delegation is doing, or all of them.
    pub async fn status(&self, id: Option<&str>) -> ToolResult {
        let all = match self.board.as_deref() {
            Some(board) => match self.all(board).await {
                Ok(all) => all,
                Err(refused) => return refused,
            },
            None => Vec::new(),
        };
        let inner = self.lock();
        if let Some(id) = id {
            if let Some(live) = inner.live.get(id) {
                return ToolResult::ok(describe_live(id, live, KEPT_LINES));
            }
            if let Some(ended) = inner.ended.get(id) {
                return ToolResult::ok(describe_ended(id, ended));
            }
            return match all.iter().find(|t| t.id() == id) {
                Some(task) => ToolResult::ok(self.describe(&inner, task, &all)),
                None => ToolResult::err(unknown(&inner, &all, id)),
            };
        }

        let mut lines: Vec<String> = inner
            .live
            .iter()
            .map(|(id, live)| describe_live(id, live, LISTED_LINES))
            .collect();
        let mut over = Vec::new();
        for task in all.iter().filter(|t| !inner.live.contains_key(t.id())) {
            if task.status.is_some() {
                over.push(self.describe(&inner, task, &all));
            } else {
                lines.push(self.describe(&inner, task, &all));
            }
        }
        let hidden = over.len().saturating_sub(LISTED_ENDED);
        if hidden > 0 {
            lines.push(format!("({hidden} older ended task(s) not shown)"));
        }
        lines.extend(over.into_iter().skip(hidden));
        lines.extend(inner.ended.iter().map(|(id, e)| describe_ended(id, e)));
        let idle = (self.board.is_some() && inner.live.is_empty())
            .then(|| idle_line(&all))
            .flatten();
        drop(inner);
        lines.extend(idle);
        if lines.is_empty() {
            ToolResult::ok("No tasks.")
        } else {
            ToolResult::ok(lines.join("\n"))
        }
    }

    fn describe(&self, inner: &Inner, task: &Task, all: &[Task]) -> String {
        let wait = match task.status {
            None => self.over_budget(inner, &task.ticket.agent),
            Some(_) => None,
        };
        entry_line(task, all, wait)
    }

    /// The result of a task or background delegation, waiting up to
    /// `timeout` for it to end. One still going at the end of that is
    /// reported, not failed: the caller asks again.
    pub async fn wait(&self, id: &str, timeout: Duration) -> ToolResult {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.watch();
            {
                let inner = self.lock();
                if let Some(ended) = inner.ended.get(id) {
                    return match ended.status {
                        Status::Cancelled => ToolResult::err(format!(
                            "{id} was cancelled before it finished.\n{}",
                            ended.text
                        )),
                        status => ToolResult {
                            text: ended.text.clone(),
                            is_error: status == Status::Failed,
                        },
                    };
                }
            }
            let running_here = self.lock().live.contains_key(id);
            let detail = match self.detail(id).await {
                Ok(detail) => detail,
                Err(e) => return ToolResult::err(format!("cannot read task {id}: {e}")),
            };
            if !running_here && detail.is_none() {
                let all = match self.board.as_deref() {
                    Some(board) => board.scan().await.unwrap_or_default(),
                    None => Vec::new(),
                };
                return ToolResult::err(unknown(&self.lock(), &all, id));
            }
            if let Some(detail) = &detail {
                if !running_here && let Some(q) = &detail.task.question {
                    return ToolResult::ok(format!(
                        "{id} needs input: {q}\nAnswer it with `answer`; the task then goes on."
                    ));
                }
                if let Some(outcome) = &detail.outcome {
                    return ToolResult {
                        text: outcome.text.clone(),
                        is_error: outcome.status != Status::Done,
                    };
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let inner = self.lock();
                return ToolResult::ok(match (inner.live.get(id), &detail) {
                    (Some(live), _) => format!(
                        "{id} is still running; call `wait` again.\n{}",
                        describe_live(id, live, KEPT_LINES)
                    ),
                    (None, Some(d)) if d.task.running_on.is_some() => format!(
                        "{id} is running on {}; call `wait` again.",
                        d.task.running_on.as_deref().unwrap_or("another node")
                    ),
                    _ => format!(
                        "{id} is queued: it starts when a slot of its agent is free. Call `wait` again."
                    ),
                });
            }
            // A task another node runs ends without a word to this one, so
            // the hub is asked again every few seconds too.
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep(SCAN_EVERY) => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Stop a run, or cancel a task that has not started.
    pub async fn cancel(&self, id: &str) -> ToolResult {
        {
            let mut inner = self.lock();
            if let Some(live) = inner.live.get_mut(id) {
                live.asked = true;
                live.cancel.store(true, Ordering::Relaxed);
                return ToolResult::ok(format!("{id} is being cancelled."));
            }
            if inner.ended.contains_key(id) {
                return ToolResult::ok(format!("{id} had already ended."));
            }
        }
        let board = match self.tasks() {
            Ok(board) => board,
            Err(_) => return ToolResult::err(unknown(&self.lock(), &[], id)),
        };
        let cancelled = board.cancel(id).await;
        self.changed.notify_waiters();
        match cancelled {
            Ok(Cancelled::Before) => {
                ToolResult::ok(format!("{id} is cancelled; it had not started."))
            }
            Ok(Cancelled::Running(node)) => {
                ToolResult::ok(format!("{id} is being cancelled: {node} stops it."))
            }
            Ok(Cancelled::Ended) => ToolResult::ok(format!("{id} had already ended.")),
            Err(e) => self.refused(board, id, e).await,
        }
    }

    /// What the board refused about task `id`: a task it does not have is
    /// named among those it has.
    async fn refused(&self, board: &dyn Board, id: &str, e: String) -> ToolResult {
        if e.starts_with("no task") {
            let all = board.scan().await.unwrap_or_default();
            return ToolResult::err(unknown(&self.lock(), &all, id));
        }
        ToolResult::err(e)
    }

    /// Assign task `id` to `agent`: from the backlog, or to someone else. A
    /// task that is running or has ended is left alone.
    pub async fn assign(&self, id: &str, agent: &str) -> ToolResult {
        let board = match self.tasks() {
            Ok(board) => board,
            Err(refused) => return refused,
        };
        let to = match self.ensemble.config().agent(agent) {
            Some(a) => a.name.clone(),
            None => {
                return ToolResult::err(format!(
                    "cannot assign {id}: no agent '{agent}'. Agents: {}.",
                    self.ensemble.config().agent_names().join(", ")
                ));
            }
        };
        if self.lock().live.contains_key(id) {
            return ToolResult::err(format!("{id} is running; assign it when it stops."));
        }
        match board.assign(id, &to).await {
            Ok(()) => {
                self.changed.notify_waiters();
                ToolResult::ok(format!("{id} is assigned to {to}."))
            }
            Err(e) => self.refused(board, id, e).await,
        }
    }

    /// A line saying nothing is active, and what waits for the owner, when
    /// that is so: no run is going and no task is assigned and ready.
    pub async fn idle(&self) -> Option<String> {
        let board = self.board.as_deref()?;
        if !self.lock().live.is_empty() {
            return None;
        }
        idle_line(&board.scan().await.ok()?)
    }

    /// Answer the question a task is waiting on, so it goes on.
    pub async fn answer(&self, id: &str, answer: &str) -> ToolResult {
        let board = match self.tasks() {
            Ok(board) => board,
            Err(refused) => return refused,
        };
        match board.answer(id, answer).await {
            Ok(()) => {
                self.changed.notify_waiters();
                ToolResult::ok(format!("{id} is answered and queued to go on."))
            }
            Err(e) => self.refused(board, id, e).await,
        }
    }

    /// Stop every run, and anything started later, for when the caller is
    /// gone. Tasks that were running are put back on the hub, and those that
    /// had not started stay there, for the next run.
    pub fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        for live in inner.live.values() {
            live.cancel.store(true, Ordering::Relaxed);
        }
        drop(inner);
        self.changed.notify_waiters();
    }
}

const HOUR: u64 = 3600;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn over_budget_message(agent: &str, wait: u64) -> String {
    format!(
        "agent '{agent}' has used its budget for this hour; the next run is allowed in {}. \
         Use another agent, or try again then.",
        minutes(wait)
    )
}

fn unknown(inner: &Inner, all: &[Task], id: &str) -> String {
    let mut known: Vec<&str> = inner.live.keys().map(String::as_str).collect();
    known.extend(inner.ended.keys().map(String::as_str));
    known.extend(all.iter().map(Task::id));
    known.sort();
    known.dedup();
    if known.is_empty() {
        format!("no task '{id}': there are no tasks.")
    } else {
        format!("no task '{id}'. Tasks: {}.", known.join(", "))
    }
}

/// A run as a line, then its latest `lines` activity lines.
fn describe_live(id: &str, live: &Live, lines: usize) -> String {
    let state = if live.cancel.load(Ordering::Relaxed) {
        "cancelling"
    } else if live.waiting {
        "waiting for a slot"
    } else {
        "running"
    };
    let mut out = format!(
        "{id} · {} · {state} · {}s · {}",
        live.agent,
        live.started.elapsed().as_secs(),
        live.title
    );
    let skip = live.recent.len().saturating_sub(lines);
    for line in live.recent.iter().skip(skip) {
        out.push_str("\n  ");
        out.push_str(line);
    }
    out
}

fn describe_ended(id: &str, e: &Ended) -> String {
    format!(
        "{id} · {} · {} · {}s · {}",
        e.agent,
        e.status.as_str(),
        e.seconds,
        e.title
    )
}
