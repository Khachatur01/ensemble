//! Work the caller does not wait for: tasks from the state directory's `tasks/`, and
//! background delegations.
//!
//! A blocking `delegate` holds the caller's tool call open for as long as the
//! agent runs, and the caller's MCP client may give up on it long before that.
//! Two things here avoid that:
//!
//! - A **task** is a ticket on disk ([`crate::tasks`]). The manager writes one
//!   with `submit`, or a person writes one by hand. While the session runs, a
//!   worker starts every task that has no result yet, oldest id first, as its
//!   agent's slots allow, and writes `result.md` when it ends. A task the
//!   session's end interrupts has no result, so it runs again next session.
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
use crate::tasks::{
    ASK_MARKER, Entry, HANDOFF_MARKER, Status, Store, Ticket, handoff_in, idle_line, minutes,
    question_in, waiting_for,
};
use crate::tracker::Tracker;
use ensemble_agents::external::gate;
use ensemble_agents::router::Ensemble;
use ensemble_agents::runtime::blocking;
use ensemble_agents::worktree;
use ensemble_config::{Budget, Isolation};
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
/// How often the worker looks for tasks written by hand.
const SCAN_EVERY: Duration = Duration::from_secs(2);
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
pub type Recall<'e> = dyn Fn(&str, usize) -> Result<Option<String>, String> + Sync + 'e;

/// The most of a task's text used as the topic of a recall.
const MAX_TOPIC_CHARS: usize = 1000;

/// The tasks and background delegations of one session.
pub struct Queue<'e> {
    ensemble: &'e Ensemble,
    /// Where tasks live. Without one (a request handled outside a session)
    /// there are no tasks, and background delegations are kept in memory.
    store: Option<Store>,
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
    pub fn new(ensemble: &'e Ensemble, store: Option<Store>) -> Self {
        let last_adhoc = store.as_ref().map_or(0, Store::last_adhoc);
        let usage = store.as_ref().map(Store::usage).unwrap_or_default();
        Queue {
            ensemble,
            store,
            recall: None,
            inner: Mutex::new(Inner {
                last_adhoc,
                usage,
                ..Inner::default()
            }),
            changed: Notify::new(),
        }
    }

    /// This queue, with the agents that have `recall` given what `recall`
    /// finds about each of their tasks.
    pub fn with_recall(
        mut self,
        recall: impl Fn(&str, usize) -> Result<Option<String>, String> + Sync + 'e,
    ) -> Self {
        self.recall = Some(Box::new(recall));
        self
    }

    /// The context `agent` is given with a task about `topic`: `context`, and
    /// after it the notes of the knowledge vault about the task when the agent
    /// has `recall`, within its `max_chars`. A vault that cannot be asked is
    /// told to `progress`, and the task goes on without notes.
    pub fn with_notes(
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
        let notes = match blocking(|| recall(&topic, limits.max_chars as usize)) {
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
        let runs = inner.usage.entry(self.key(agent)).or_default();
        runs.retain(|t| now < t + HOUR);
        runs.push(now);
        if let Some(store) = &self.store {
            store.save_usage(&inner.usage);
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
        if let Some(store) = &self.store {
            store.log(id, line);
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
    /// is left: background delegations, and with `tasks`, every task without
    /// a result, oldest id first, as its agent's slots allow. Tasks are
    /// looked for again every few seconds, and at once when one is submitted
    /// or a run ends. Only the one process that holds the folder's lock runs
    /// its tasks.
    pub async fn work(&self, tasks: bool) {
        self.drive(tasks, false).await;
    }

    /// Run the background delegations started so far to their end, for a
    /// request handled with no session around it.
    pub async fn finish(&self) {
        self.drive(false, true).await;
    }

    async fn drive(&self, tasks: bool, until_idle: bool) {
        let mut runs: FuturesUnordered<Run<'_>> = FuturesUnordered::new();
        loop {
            let changed = self.watch();
            let store = self.store.as_ref().filter(|_| tasks);
            let entries = store.map(Store::scan).unwrap_or_default();
            // Agents whose tasks wait for a backend (`when_none: wait` and no
            // usable model), found before the lock is taken: asking a backend
            // can take seconds. Each backend is asked at most once a minute.
            let mut blocked: HashMap<&str, Option<String>> = HashMap::new();
            for entry in &entries {
                if let Entry::Task(t, None) = entry
                    && !t.agent.is_empty()
                    && !blocked.contains_key(t.agent.as_str())
                {
                    let ready = blocking(|| self.ensemble.backend_ready(&t.agent));
                    blocked.insert(&t.agent, ready.err());
                }
            }
            {
                let mut inner = self.lock();
                for adhoc in inner.started.drain(..) {
                    runs.push(Box::pin(self.run_adhoc(adhoc)));
                }
                if runs.is_empty() && (inner.closed || until_idle) {
                    return;
                }
                let open = !inner.closed;
                for entry in entries.iter().filter(|_| open) {
                    let (Some(store), Entry::Task(ticket, None)) = (store, entry) else {
                        continue;
                    };
                    // Ended since the scan: a result is written before its run
                    // leaves `live`, under this lock.
                    if inner.live.contains_key(&ticket.id)
                        || ticket.agent.is_empty()
                        || store.outcome(&ticket.id).is_some()
                        || !waiting_for(ticket, &entries).is_empty()
                        || store.open_question(&ticket.id).is_some()
                    {
                        continue;
                    }
                    if let Some(Some(why)) = blocked.get(ticket.agent.as_str()) {
                        if inner.backend_waits.insert(ticket.id.clone()) {
                            store.log(&ticket.id, &format!("waiting for a backend: {why}"));
                        }
                        continue;
                    }
                    let ticket = ticket.clone();
                    if self.over_budget(&inner, &ticket.agent).is_some() {
                        continue;
                    }
                    let Some(slot) = self.take(&mut inner, &ticket.agent) else {
                        continue;
                    };
                    // Cannot fail: the budget was checked under this same lock.
                    let _ = self.spend_locked(&mut inner, &ticket.agent);
                    inner.backend_waits.remove(&ticket.id);
                    let cancel = Arc::new(AtomicBool::new(false));
                    inner.live.insert(
                        ticket.id.clone(),
                        Self::live(&ticket.agent, &ticket.title, cancel.clone(), false),
                    );
                    // The configured tracker keeps the tasks.
                    let tracker = crate::tracker::for_config(self.ensemble.config().tracker, store);
                    runs.push(Box::pin(async move {
                        self.run_ticket(tracker, *ticket, slot, &cancel).await
                    }));
                }
            }
            tokio::select! {
                Some(()) = runs.next(), if !runs.is_empty() => {}
                () = changed => {}
                () = tokio::time::sleep(SCAN_EVERY) => {}
            }
        }
    }

    async fn run_ticket(
        &self,
        store: &dyn Tracker,
        ticket: Ticket,
        slot: Slot<'_>,
        cancel: &AtomicBool,
    ) {
        let since = SystemTime::now();
        store.set_state(&ticket.id, "running", since);
        let place = self
            .ensemble
            .workdir()
            .map_err(|e| format!("no project directory to work in: {e}"))
            .and_then(|workdir| match ticket.repo.as_deref() {
                Some(repo) => ensemble_core::workspace::resolve_repo(&workdir, repo),
                None => Ok(workdir),
            })
            .and_then(
                |project| match self.ensemble.config().agent(&ticket.agent) {
                    Some(a) if a.isolation == Isolation::Worktree => {
                        self.note(
                            &ticket.id,
                            &format!("{}: preparing a git worktree", ticket.agent),
                        );
                        blocking(|| worktree::prepare(&project, store.root(), &ticket.id))
                            .map(|wt| (wt.dir.clone(), Some(wt)))
                    }
                    _ => Ok((project, None)),
                },
            );
        let (mut result, mut extra) = match &place {
            Ok((dir, _)) => self.work_on(store, &ticket, dir, cancel).await,
            Err(e) => (ToolResult::err(e.clone()), Vec::new()),
        };
        drop(slot);
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
            if let Err(e) = store.ask(&ticket.id, question, &result.text) {
                eprintln!("ensemble: cannot write the question of {}: {e}", ticket.id);
            }
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
        let mut inner = self.lock();
        if interrupted_now {
            // The session ended under it: no result, so the next session
            // runs it again.
            store.set_state(&ticket.id, "interrupted", since);
        } else if question.is_some() {
            store.set_state(&ticket.id, "needs-input", since);
        } else if let Some((to, note)) = &handoff {
            match store
                .append_handoff(&ticket.id, &ticket.agent, to, note, &result.text)
                .and_then(|()| store.assign(&ticket.id, to))
            {
                Ok(()) => {
                    // The lock is held here, so the log is written directly.
                    store.log(
                        &ticket.id,
                        &format!("{} handed the task to {to}", ticket.agent),
                    );
                    store.set_state(&ticket.id, "queued", since);
                }
                Err(e) => {
                    eprintln!("ensemble: cannot hand {} to {to}: {e}", ticket.id);
                    store.set_state(&ticket.id, "failed", since);
                    let text = format!(
                        "{}\n\n[the hand-off to {to} could not be written: {e}]",
                        result.text
                    );
                    let _ = store.write_result(
                        &ticket.id,
                        &ticket.agent,
                        Status::Failed,
                        since,
                        &extra,
                        &text,
                    );
                }
            }
        } else {
            let status = if asked {
                Status::Cancelled
            } else if result.is_error {
                Status::Failed
            } else {
                Status::Done
            };
            store.set_state(&ticket.id, status.as_str(), since);
            if let Err(e) = store.write_result(
                &ticket.id,
                &ticket.agent,
                status,
                since,
                &extra,
                &result.text,
            ) {
                eprintln!("ensemble: cannot write the result of {}: {e}", ticket.id);
            }
        }
        inner.live.remove(&ticket.id);
        drop(inner);
        self.changed.notify_waiters();
    }

    /// Run the agent on `ticket` in `dir`, then its gate if it has one. While
    /// the gate fails and attempts remain, the agent runs again with the
    /// gate's output. Returns the outcome and front matter for `result.md`.
    async fn work_on(
        &self,
        store: &dyn Tracker,
        ticket: &Ticket,
        dir: &Path,
        cancel: &AtomicBool,
    ) -> (ToolResult, Vec<(&'static str, String)>) {
        let note = |line: &str| self.note(&ticket.id, line);
        let mut base = ticket.prompt();
        if let Some((questions, reply)) = store.conversation(&ticket.id) {
            base.push_str(&format!(
                "\n\nYou stopped earlier to ask. The questions and their answers:\n\n{}\n\nYour reply when you last stopped:\n{}\n\nGo on from there.",
                questions.trim(),
                reply.trim()
            ));
        }
        if let Some(earlier) = store.handoffs(&ticket.id) {
            base.push_str(&format!(
                "\n\nThis task was handed to you. What earlier agents did and left for you:\n\n{}",
                earlier.trim()
            ));
        }
        base.push_str(&format!(
            "\n\nIf you cannot finish without a decision that only the person who asked can make, stop and end your reply with one line: {ASK_MARKER} <your question>\nIf the next part belongs to another agent, end your reply with one line: {HANDOFF_MARKER} <agent> <what remains>"
        ));
        // Recalled once: a gate's retry is the same task.
        let context = self.with_notes(
            &ticket.agent,
            &format!("{}\n\n{}", ticket.title, ticket.prompt()),
            ticket.context.as_deref(),
            &note,
        );
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
        let Some(check) = config.gate(&gate.name) else {
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

    /// Write a new task for the worker, and return at once.
    pub fn submit(&self, ticket: &Ticket) -> ToolResult {
        let Some(store) = &self.store else {
            return ToolResult::err("there is no task store in this session.");
        };
        if let Some(missing) = ticket.after.iter().find(|a| !store.exists(a)) {
            return ToolResult::err(format!(
                "`after` names '{missing}', which is not a task. Tasks are named by the id \
                 `submit` returned."
            ));
        }
        match store.create(ticket) {
            Ok(id) => {
                self.changed.notify_waiters();
                ToolResult::ok(format!(
                    "task {id} queued. `status` shows where it is; `wait` with task {id} \
                     returns its result."
                ))
            }
            Err(e) => ToolResult::err(format!(
                "cannot write the task in {}: {e}",
                store.tasks_dir().display()
            )),
        }
    }

    /// What one task or background delegation is doing, or all of them.
    pub fn status(&self, id: Option<&str>) -> ToolResult {
        let entries = self.store.as_ref().map(Store::scan).unwrap_or_default();
        let inner = self.lock();
        if let Some(id) = id {
            if let Some(live) = inner.live.get(id) {
                return ToolResult::ok(describe_live(id, live, KEPT_LINES));
            }
            if let Some(ended) = inner.ended.get(id) {
                return ToolResult::ok(describe_ended(id, ended));
            }
            return match entries.iter().find(|e| e.id() == id) {
                Some(entry) => ToolResult::ok(self.describe_entry(&inner, entry, &entries)),
                None => ToolResult::err(unknown(&inner, &entries, id)),
            };
        }

        let mut lines: Vec<String> = inner
            .live
            .iter()
            .map(|(id, live)| describe_live(id, live, LISTED_LINES))
            .collect();
        let mut over = Vec::new();
        for entry in entries.iter().filter(|e| !inner.live.contains_key(e.id())) {
            match entry {
                Entry::Task(_, Some(_)) => over.push(self.describe_entry(&inner, entry, &entries)),
                _ => lines.push(self.describe_entry(&inner, entry, &entries)),
            }
        }
        let hidden = over.len().saturating_sub(LISTED_ENDED);
        if hidden > 0 {
            lines.push(format!("({hidden} older ended task(s) not shown)"));
        }
        lines.extend(over.into_iter().skip(hidden));
        lines.extend(inner.ended.iter().map(|(id, e)| describe_ended(id, e)));
        drop(inner);
        if let Some(idle) = self.idle() {
            lines.push(idle);
        }
        if lines.is_empty() {
            ToolResult::ok("No tasks.")
        } else {
            ToolResult::ok(lines.join("\n"))
        }
    }

    fn describe_entry(&self, inner: &Inner, entry: &Entry, entries: &[Entry]) -> String {
        let wait = match entry {
            Entry::Task(t, None) => self.over_budget(inner, &t.agent),
            _ => None,
        };
        let elsewhere = self
            .store
            .as_ref()
            .and_then(|s| super::lock::holder(s.root()))
            .is_some_and(|pid| pid != std::process::id());
        crate::tasks::entry_line(self.store.as_ref(), entry, entries, wait, elsewhere)
    }

    /// The result of a task or background delegation, waiting up to
    /// `timeout` for it to end. One still going at the end of that is
    /// reported, not failed: the caller asks again.
    pub async fn wait(&self, id: &str, timeout: Duration) -> ToolResult {
        let exists = |inner: &Inner| {
            inner.live.contains_key(id)
                || inner.ended.contains_key(id)
                || self.store.as_ref().is_some_and(|s| s.exists(id))
        };
        let deadline = tokio::time::Instant::now() + timeout;
        {
            let inner = self.lock();
            if !exists(&inner) {
                let entries = self.store.as_ref().map(Store::scan).unwrap_or_default();
                return ToolResult::err(unknown(&inner, &entries, id));
            }
        }
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
                if !inner.live.contains_key(id)
                    && let Some(q) = self.store.as_ref().and_then(|s| s.open_question(id))
                {
                    return ToolResult::ok(format!(
                        "{id} needs input: {q}\nAnswer it with `answer`; the task then goes on."
                    ));
                }
                if let Some(outcome) = self.store.as_ref().and_then(|s| s.outcome(id)) {
                    return ToolResult {
                        text: outcome.text,
                        is_error: outcome.status != Status::Done,
                    };
                }
                if tokio::time::Instant::now() >= deadline {
                    return ToolResult::ok(match inner.live.get(id) {
                        Some(live) => format!(
                            "{id} is still running; call `wait` again.\n{}",
                            describe_live(id, live, KEPT_LINES)
                        ),
                        None => format!(
                            "{id} is queued: it starts when a slot of its agent is free. Call `wait` again."
                        ),
                    });
                }
            }
            // A task another process runs ends without a word to this one,
            // so its result is looked for every few seconds too.
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep(SCAN_EVERY) => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Stop a run, or cancel a task that has not started.
    pub fn cancel(&self, id: &str) -> ToolResult {
        let mut inner = self.lock();
        if let Some(live) = inner.live.get_mut(id) {
            live.asked = true;
            live.cancel.store(true, Ordering::Relaxed);
            return ToolResult::ok(format!("{id} is being cancelled."));
        }
        if inner.ended.contains_key(id) {
            return ToolResult::ok(format!("{id} had already ended."));
        }
        let Some(store) = self.store.as_ref().filter(|s| s.exists(id)) else {
            let entries = self.store.as_ref().map(Store::scan).unwrap_or_default();
            return ToolResult::err(unknown(&inner, &entries, id));
        };
        if store.outcome(id).is_some() {
            return ToolResult::ok(format!("{id} had already ended."));
        }
        let agent = store.read(id).map(|t| t.agent).unwrap_or_default();
        // Under the lock, so the worker cannot start it meanwhile.
        let written = store.write_result(
            id,
            &agent,
            Status::Cancelled,
            SystemTime::now(),
            &[],
            "Cancelled before it started.",
        );
        drop(inner);
        self.changed.notify_waiters();
        match written {
            Ok(()) => ToolResult::ok(format!("{id} is cancelled; it had not started.")),
            Err(e) => ToolResult::err(format!("cannot cancel {id}: {e}")),
        }
    }

    /// Assign task `id` to `agent`: from the backlog, or to someone else. A
    /// task that is running or has ended is left alone.
    pub fn assign(&self, id: &str, agent: &str) -> ToolResult {
        let Some(store) = &self.store else {
            return ToolResult::err("there is no task store in this session.");
        };
        if !store.exists(id) {
            let entries = store.scan();
            return ToolResult::err(unknown(&self.lock(), &entries, id));
        }
        let to = match self.ensemble.config().agent(agent) {
            Some(a) => a.name.clone(),
            None => {
                return ToolResult::err(format!(
                    "cannot assign {id}: no agent '{agent}'. Agents: {}.",
                    self.ensemble.config().agent_names().join(", ")
                ));
            }
        };
        if store.outcome(id).is_some() {
            return ToolResult::err(format!(
                "{id} has ended; it cannot be assigned. Submit it again."
            ));
        }
        if self.lock().live.contains_key(id) {
            return ToolResult::err(format!("{id} is running; assign it when it stops."));
        }
        match store.assign(id, &to) {
            Ok(()) => {
                self.changed.notify_waiters();
                ToolResult::ok(format!("{id} is assigned to {to}."))
            }
            Err(e) => ToolResult::err(format!("cannot assign {id}: {e}")),
        }
    }

    /// A line saying nothing is active, and what waits for the owner, when
    /// that is so: no run is going and no task is assigned and ready.
    pub fn idle(&self) -> Option<String> {
        let store = self.store.as_ref()?;
        if !self.lock().live.is_empty() {
            return None;
        }
        idle_line(&store.scan(), Some(store))
    }

    /// Answer the question a task is waiting on, so it goes on.
    pub fn answer(&self, id: &str, answer: &str) -> ToolResult {
        let Some(store) = &self.store else {
            return ToolResult::err("there is no task store in this session.");
        };
        if !store.exists(id) {
            let entries = store.scan();
            return ToolResult::err(unknown(&self.lock(), &entries, id));
        }
        match store.answer(id, answer) {
            Ok(()) => {
                self.changed.notify_waiters();
                ToolResult::ok(format!("{id} is answered and queued to go on."))
            }
            Err(e) => ToolResult::err(e),
        }
    }

    /// Stop every run, and anything started later, for when the caller is
    /// gone. Tasks that had not started stay on disk for the next session.
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

fn unknown(inner: &Inner, entries: &[Entry], id: &str) -> String {
    let mut known: Vec<&str> = inner.live.keys().map(String::as_str).collect();
    known.extend(inner.ended.keys().map(String::as_str));
    known.extend(entries.iter().map(Entry::id));
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
