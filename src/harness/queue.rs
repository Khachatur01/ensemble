//! Work the caller does not wait for: tasks from `.ensemble/tasks/`, and
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
use crate::config::Isolation;
use crate::external::gate;
use crate::router::Ensemble;
use crate::tasks::{ASK_MARKER, Entry, Status, Store, Ticket, question_in, waiting_for};
use crate::worktree;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// Work to run on a thread of the session, so it may borrow the session's data.
pub type Task<'s> = Box<dyn FnOnce() + Send + 's>;

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

#[derive(Default)]
struct Inner {
    /// Set by `close`: the caller is gone, and nothing new starts.
    closed: bool,
    last_adhoc: u64,
    live: BTreeMap<String, Live>,
    /// Background delegations that ended in this session.
    ended: BTreeMap<String, Ended>,
    /// Slots in use, per agent.
    busy: HashMap<String, u32>,
}

/// The tasks and background delegations of one session.
pub struct Queue<'e> {
    ensemble: &'e Ensemble,
    /// Where tasks live. Without one (a request handled outside a session)
    /// there are no tasks, and background delegations are kept in memory.
    store: Option<Store>,
    inner: Mutex<Inner>,
    /// Signalled whenever something starts, ends, frees a slot or is
    /// submitted.
    changed: Condvar,
}

/// One slot of an agent, given back when dropped.
pub struct Slot<'q> {
    inner: &'q Mutex<Inner>,
    changed: &'q Condvar,
    agent: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = inner.busy.get_mut(&self.agent) {
            *n = n.saturating_sub(1);
        }
        drop(inner);
        self.changed.notify_all();
    }
}

impl<'e> Queue<'e> {
    pub fn new(ensemble: &'e Ensemble, store: Option<Store>) -> Self {
        let last_adhoc = store.as_ref().map_or(0, Store::last_adhoc);
        Queue {
            ensemble,
            store,
            inner: Mutex::new(Inner {
                last_adhoc,
                ..Inner::default()
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many delegations to `agent` may run at once. An unknown agent
    /// fails as soon as it starts, so it needs no limit.
    fn limit(&self, agent: &str) -> u32 {
        self.ensemble
            .config()
            .agent(agent)
            .map_or(u32::MAX, |a| a.max_concurrent)
    }

    /// Take a slot of `agent` if one is free.
    fn take(&self, inner: &mut Inner, agent: &str) -> Option<Slot<'_>> {
        let used = inner.busy.entry(agent.to_string()).or_default();
        if *used >= self.limit(agent) {
            return None;
        }
        *used += 1;
        Some(Slot {
            inner: &self.inner,
            changed: &self.changed,
            agent: agent.to_string(),
        })
    }

    /// Wait for a slot of `agent`, calling `waiting` once if there is none
    /// free. `None` when `cancel` is set first. The session's end frees slots
    /// by stopping background work, so a blocking call still gets one.
    pub fn slot(&self, agent: &str, cancel: &AtomicBool, waiting: &dyn Fn()) -> Option<Slot<'_>> {
        let mut told = false;
        let mut inner = self.lock();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(slot) = self.take(&mut inner, agent) {
                return Some(slot);
            }
            if !told {
                told = true;
                drop(inner);
                waiting();
                inner = self.lock();
                continue;
            }
            inner = self
                .changed
                .wait_timeout(inner, SLOT_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
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

    /// Start a background delegation on a thread `spawn` makes, and return
    /// its id at once.
    pub fn start_adhoc<'s>(
        &'s self,
        agent: &str,
        task: &str,
        context: Option<&str>,
        spawn: &dyn Fn(Task<'s>),
    ) -> String
    where
        'e: 's,
    {
        let (id, cancel) = {
            let mut inner = self.lock();
            inner.last_adhoc += 1;
            let id = format!("adhoc-{}", inner.last_adhoc);
            // One registered after the caller left starts out cancelled.
            let cancel = Arc::new(AtomicBool::new(inner.closed));
            inner
                .live
                .insert(id.clone(), Self::live(agent, task, cancel.clone(), true));
            (id, cancel)
        };
        let (run, agent, task, context) = (
            id.clone(),
            agent.to_string(),
            task.to_string(),
            context.map(str::to_string),
        );
        spawn(Box::new(move || {
            let result = match self.slot(&agent, &cancel, &|| {
                self.note(&run, &format!("{agent}: waiting for a free slot"))
            }) {
                Some(_slot) => {
                    if let Some(live) = self.lock().live.get_mut(&run) {
                        live.waiting = false;
                    }
                    run_delegation(
                        self.ensemble,
                        &agent,
                        &task,
                        context.as_deref(),
                        None,
                        &|line| self.note(&run, line),
                        &cancel,
                    )
                }
                None => ToolResult::err("it was cancelled before it started."),
            };
            let mut inner = self.lock();
            if let Some(live) = inner.live.remove(&run) {
                let status = if live.cancel.load(Ordering::Relaxed) {
                    Status::Cancelled
                } else if result.is_error {
                    Status::Failed
                } else {
                    Status::Done
                };
                inner.ended.insert(
                    run,
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
            self.changed.notify_all();
        }));
        id
    }

    /// Run tasks until the session ends: every task without a result, oldest
    /// id first, as its agent's slots allow. Tasks are looked for again every
    /// few seconds, and at once when one is submitted or a run ends.
    pub fn work<'s>(&'s self, spawn: &dyn Fn(Task<'s>))
    where
        'e: 's,
    {
        let Some(store) = &self.store else {
            return;
        };
        loop {
            let entries = store.scan();
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            for entry in &entries {
                let Entry::Task(ticket, None) = entry else {
                    continue;
                };
                // Ended since the scan: a result is written before its run
                // leaves `live`, under this lock.
                if inner.live.contains_key(&ticket.id)
                    || store.outcome(&ticket.id).is_some()
                    || !waiting_for(ticket, &entries).is_empty()
                    || store.open_question(&ticket.id).is_some()
                {
                    continue;
                }
                let ticket = ticket.clone();
                let Some(slot) = self.take(&mut inner, &ticket.agent) else {
                    continue;
                };
                let cancel = Arc::new(AtomicBool::new(false));
                inner.live.insert(
                    ticket.id.clone(),
                    Self::live(&ticket.agent, &ticket.title, cancel.clone(), false),
                );
                spawn(Box::new(move || {
                    self.run_ticket(store, *ticket, slot, &cancel)
                }));
            }
            drop(
                self.changed
                    .wait_timeout(inner, SCAN_EVERY)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }

    fn run_ticket(&self, store: &Store, ticket: Ticket, slot: Slot<'_>, cancel: &AtomicBool) {
        let since = SystemTime::now();
        store.set_state(&ticket.id, "running", since);
        let place = self
            .ensemble
            .workdir()
            .map_err(|e| format!("no project directory to work in: {e}"))
            .and_then(
                |project| match self.ensemble.config().agent(&ticket.agent) {
                    Some(a) if a.isolation == Isolation::Worktree => {
                        self.note(
                            &ticket.id,
                            &format!("{}: preparing a git worktree", ticket.agent),
                        );
                        worktree::prepare(&project, store.root(), &ticket.id)
                            .map(|wt| (wt.dir.clone(), Some(wt)))
                    }
                    _ => Ok((project, None)),
                },
            );
        let (mut result, mut extra) = match &place {
            Ok((dir, _)) => self.work_on(store, &ticket, dir, cancel),
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
        {
            extra.push(("branch", wt.branch.clone()));
            let note = match worktree::commit(wt, &format!("{}: {}", ticket.id, ticket.title)) {
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
        self.changed.notify_all();
    }

    /// Run the agent on `ticket` in `dir`, then its gate if it has one. While
    /// the gate fails and attempts remain, the agent runs again with the
    /// gate's output. Returns the outcome and front matter for `result.md`.
    fn work_on(
        &self,
        store: &Store,
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
        base.push_str(&format!(
            "\n\nIf you cannot finish without a decision that only the person who asked can make, stop and end your reply with one line: {ASK_MARKER} <your question>"
        ));
        let delegate = |prompt: &str| {
            run_delegation(
                self.ensemble,
                &ticket.agent,
                prompt,
                ticket.context.as_deref(),
                Some(dir),
                &note,
                cancel,
            )
        };
        let Some(gate) = &ticket.gate else {
            return (delegate(&base), Vec::new());
        };
        let Some(check) = self.ensemble.config().gates.get(&gate.name) else {
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
            let result = delegate(&prompt);
            if result.is_error
                || cancel.load(Ordering::Relaxed)
                || question_in(&result.text).is_some()
            {
                return (result, reported("not run"));
            }
            note(&format!("gate {}: running `{}`", gate.name, check.command));
            let verdict = gate::run(check, dir, cancel);
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
                self.changed.notify_all();
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
                Some(entry) => ToolResult::ok(self.describe_entry(entry, &entries)),
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
                Entry::Task(_, Some(_)) => over.push(self.describe_entry(entry, &entries)),
                _ => lines.push(self.describe_entry(entry, &entries)),
            }
        }
        let hidden = over.len().saturating_sub(LISTED_ENDED);
        if hidden > 0 {
            lines.push(format!("({hidden} older ended task(s) not shown)"));
        }
        lines.extend(over.into_iter().skip(hidden));
        lines.extend(inner.ended.iter().map(|(id, e)| describe_ended(id, e)));
        if lines.is_empty() {
            ToolResult::ok("No tasks.")
        } else {
            ToolResult::ok(lines.join("\n"))
        }
    }

    fn describe_entry(&self, entry: &Entry, entries: &[Entry]) -> String {
        match entry {
            Entry::Task(t, Some(status)) => {
                format!("{} · {} · {} · {}", t.id, t.agent, status.as_str(), t.title)
            }
            Entry::Task(t, None) => {
                let interrupted = self
                    .store
                    .as_ref()
                    .and_then(|s| s.state(&t.id))
                    .is_some_and(|s| s == "interrupted" || s == "running");
                let blockers = waiting_for(t, entries);
                let question = self.store.as_ref().and_then(|s| s.open_question(&t.id));
                let state = if let Some(q) = question {
                    format!("needs input: {q}")
                } else if !blockers.is_empty() {
                    format!("blocked by {}", blockers.join(", "))
                } else if interrupted {
                    "queued (interrupted last session)".into()
                } else {
                    "queued".into()
                };
                format!("{} · {} · {state} · {}", t.id, t.agent, t.title)
            }
            Entry::Unreadable { id, error } => format!("{id} · unreadable · {error}"),
        }
    }

    /// The result of a task or background delegation, waiting up to
    /// `timeout` for it to end. One still going at the end of that is
    /// reported, not failed: the caller asks again.
    pub fn wait(&self, id: &str, timeout: Duration) -> ToolResult {
        let exists = |inner: &Inner| {
            inner.live.contains_key(id)
                || inner.ended.contains_key(id)
                || self.store.as_ref().is_some_and(|s| s.exists(id))
        };
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        if !exists(&inner) {
            let entries = self.store.as_ref().map(Store::scan).unwrap_or_default();
            return ToolResult::err(unknown(&inner, &entries, id));
        }
        loop {
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
            let now = Instant::now();
            if now >= deadline {
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
            inner = self
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
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
        self.changed.notify_all();
        match written {
            Ok(()) => ToolResult::ok(format!("{id} is cancelled; it had not started.")),
            Err(e) => ToolResult::err(format!("cannot cancel {id}: {e}")),
        }
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
                self.changed.notify_all();
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
        self.changed.notify_all();
    }
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
