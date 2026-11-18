//! The worker of one repository: it runs the tasks the hub keeps for the
//! repository's project ([`crate::board`]).
//!
//! While the worker runs ([`Queue::work`], in the runner), it claims every
//! task of the project that may start, oldest id first, as its agent's slots
//! and budget allow, renews its claim while the agent works, and tells the hub
//! how the run ended. A task the worker's end interrupts is put back, and a
//! runner that dies stops renewing, so either way another run takes it up.
//! What each run does as it goes is kept in its activity log ([`Runs`]).

use super::delegation::{RunResult, run_delegation};
use crate::board::{Board, Claim, End, LEASE, Progress, Renewal};
use crate::runs::Runs;
use crate::tasks::{
    ASK_MARKER, Detail, HANDOFF_MARKER, Status, Task, Ticket, handoff_in, minutes, question_in,
    utc, waiting_for,
};
use ensemble_agents::external::gate;
use ensemble_agents::router::Ensemble;
use ensemble_agents::runtime::blocking;
use ensemble_agents::worktree;
use ensemble_config::{Budget, Isolation};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

/// How often the worker looks at the hub for tasks that may start.
const SCAN_EVERY: Duration = Duration::from_secs(2);
/// How many times a run's end is told to the hub before it is given up.
const FINISH_ATTEMPTS: u32 = 3;

/// A task that runs here.
struct Live {
    cancel: Arc<AtomicBool>,
    /// Set when someone cancelled it, as opposed to the worker ending.
    asked: bool,
}

/// One run of [`Queue::work`].
type Run<'q> = Pin<Box<dyn Future<Output = ()> + Send + 'q>>;

/// What one look found to start.
struct Looked<'q> {
    /// Tasks claimed on the board, each with a slot of its agent and the flag
    /// that stops it.
    claimed: Vec<(Ticket, Slot<'q>, Arc<AtomicBool>)>,
    /// The worker is closing: nothing new starts.
    closed: bool,
}

#[derive(Default)]
struct Inner {
    /// Set by `close`: nothing new starts.
    closed: bool,
    live: BTreeMap<String, Live>,
    /// Slots in use, per agent.
    busy: HashMap<String, u32>,
    /// When each agent with a budget started its runs in the last hour.
    usage: BTreeMap<String, Vec<u64>>,
    /// Tasks already noted as waiting for a backend, so the note is written
    /// once per wait, not once per scan.
    backend_waits: HashSet<String>,
    /// Whether the hub could not be reached at the last look, so that is said
    /// once, not every few seconds.
    board_down: bool,
}

/// The tasks of one repository.
pub struct Queue<'e> {
    ensemble: &'e Ensemble,
    /// Where the project's tasks are kept: on the runner's hub.
    board: Box<dyn Board + 'e>,
    /// This runner's name, which its claims on tasks carry.
    runner: String,
    /// How long a claim lasts unless renewed.
    lease: Duration,
    /// What this runner keeps of its runs here: logs, worktrees, usage.
    runs: Runs,
    inner: Mutex<Inner>,
    /// Signalled whenever something starts, ends or frees a slot.
    changed: Notify,
}

/// One slot of an agent, given back when dropped.
struct Slot<'q> {
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
    /// The tasks of `board`, run with `ensemble` as the runner `runner`,
    /// keeping what it runs in `runs`.
    pub fn new(ensemble: &'e Ensemble, runs: Runs, board: impl Board + 'e, runner: &str) -> Self {
        let usage = runs.usage();
        Queue {
            ensemble,
            board: Box::new(board),
            runner: runner.to_string(),
            lease: LEASE,
            runs,
            inner: Mutex::new(Inner {
                usage,
                ..Inner::default()
            }),
            changed: Notify::new(),
        }
    }

    /// Claims that last `lease` unless renewed, in place of [`LEASE`].
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
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

    /// How many runs of `agent` may go on at once. An unknown agent fails as
    /// soon as it starts, so it needs no limit.
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
        self.runs.save_usage(&inner.usage);
        Ok(())
    }

    /// [`Self::spend_locked`], as a refusal one can read.
    fn spend(&self, agent: &str) -> Result<(), String> {
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

    /// Record what the agent of task `id` is doing now.
    fn note(&self, id: &str, line: &str) {
        self.runs.log(id, line);
    }

    /// Run every task of the board that may start, oldest id first, as its
    /// agent's slots allow, until [`Queue::close`] and the runs left end. The
    /// board is looked at again every few seconds, and at once when a run
    /// ends.
    pub async fn work(&self) {
        let mut runs: FuturesUnordered<Run<'_>> = FuturesUnordered::new();
        loop {
            let changed = self.watch();
            // The runs go on while the board is looked at: a run renewing its
            // claim and the look share one link to the hub.
            let look = self.look();
            tokio::pin!(look);
            let looked = loop {
                tokio::select! {
                    looked = &mut look => break looked,
                    Some(()) = runs.next(), if !runs.is_empty() => {}
                }
            };
            for (ticket, slot, cancel) in looked.claimed {
                runs.push(Box::pin(async move {
                    self.run_ticket(ticket, slot, &cancel).await
                }));
            }
            if runs.is_empty() && looked.closed {
                return;
            }
            tokio::select! {
                Some(()) = runs.next(), if !runs.is_empty() => {}
                () = changed => {}
                () = tokio::time::sleep(SCAN_EVERY) => {}
            }
        }
    }

    /// Whether `task` may start here: assigned to an agent of this runner's
    /// teams, not ended, not asking, not waiting for another task, and not
    /// running on another runner. A task for an agent this runner does not
    /// have is left for one of the account's runners that does.
    fn may_start(&self, task: &Task, all: &[Task]) -> bool {
        task.status.is_none()
            && self.ensemble.config().agent(&task.ticket.agent).is_some()
            && task.question.is_none()
            && task.running_on.as_ref().is_none_or(|n| *n == self.runner)
            && waiting_for(&task.ticket, all).is_empty()
    }

    /// The board's tasks. A board that cannot be reached is said once, until
    /// it can again.
    async fn scan(&self) -> Vec<Task> {
        match self.board.scan().await {
            Ok(all) => {
                self.lock().board_down = false;
                all
            }
            Err(e) => {
                let mut inner = self.lock();
                if !inner.board_down {
                    inner.board_down = true;
                    eprintln!(
                        "ensemble: cannot read the tasks on {}: {e}; trying again",
                        self.board.place()
                    );
                }
                Vec::new()
            }
        }
    }

    /// One look at what to start: the board's tasks that may start, each
    /// claimed with a slot of its agent taken.
    async fn look(&self) -> Looked<'_> {
        let all = self.scan().await;
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
        let closed = {
            let mut inner = self.lock();
            let closed = inner.closed;
            for task in all.iter().filter(|_| !closed) {
                let ticket = &task.ticket;
                if inner.live.contains_key(&ticket.id) || !self.may_start(task, &all) {
                    continue;
                }
                if let Some(Some(why)) = blocked.get(ticket.agent.as_str()) {
                    if inner.backend_waits.insert(ticket.id.clone()) {
                        self.note(&ticket.id, &format!("waiting for a backend: {why}"));
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
                    Live {
                        cancel: cancel.clone(),
                        asked: false,
                    },
                );
                picked.push((ticket.clone(), slot, cancel));
            }
            closed
        };
        let mut claimed = Vec::new();
        for (ticket, slot, cancel) in picked {
            let movable = self.movable(&ticket);
            match self
                .board
                .claim(&ticket.id, &self.runner, self.lease, movable)
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
                // Another runner was first, or the hub could not be asked:
                // the next look tries again.
                Ok(Claim::Refused(_)) | Err(_) => {
                    self.lock().live.remove(&ticket.id);
                    drop(slot);
                }
            }
        }
        Looked { claimed, closed }
    }

    /// Keep this runner's claim on task `id` while it runs: renewed four times
    /// a lease. Someone cancelling the task, or the claim lost to another
    /// runner (or not renewed for a whole lease), stops the run through
    /// `cancel`; a lost claim also sets `lost`. Never resolves.
    async fn hold(&self, id: &str, cancel: &AtomicBool, lost: &AtomicBool) {
        let mut failing: Option<Instant> = None;
        loop {
            tokio::time::sleep(self.lease / 4).await;
            match self.board.renew(id, &self.runner, self.lease).await {
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

    /// The repository `ticket` works in: the worker's, or a folder of it the
    /// ticket's `repo` names.
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
    /// with this runner once it has run.
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
                    node: self.runner.clone(),
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
    async fn tell(&self, id: &str, end: Option<End>, progress: Option<Progress>) {
        let mut said = String::new();
        for attempt in 1..=FINISH_ATTEMPTS {
            let told = match &end {
                Some(end) => {
                    self.board
                        .finish(id, &self.runner, end.clone(), progress.clone())
                        .await
                }
                None => self.board.release(id, &self.runner, progress.clone()).await,
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
            self.runner,
            self.board.place()
        );
    }

    async fn run_ticket(&self, ticket: Ticket, slot: Slot<'_>, cancel: &AtomicBool) {
        let since = SystemTime::now();
        // What it asked and was answered, what earlier agents handed on, and
        // where the work of an earlier run was pushed.
        let detail = self.board.get(&ticket.id).await.ok().flatten();
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
                    let root = self.runs.root();
                    blocking(|| {
                        worktree::prepare_from(&project, root, &ticket.id, carried.as_deref())
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
                Err(e) => (RunResult::err(e.clone()), Vec::new()),
            }
        };
        let (mut result, mut extra) = tokio::select! {
            done = work => done,
            () = self.hold(&ticket.id, cancel, &lost) => unreachable!("holding never ends"),
        };
        drop(slot);
        let lost = lost.load(Ordering::Relaxed);
        // Commit the work on the task's branch, unless the worker ended under
        // it: then the worktree is left as it is, for the next run.
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
                    result = RunResult::err(format!(
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
            // Another runner runs it now; what this run did stays here.
            self.note(
                &ticket.id,
                &format!(
                    "{}: stopped: this runner's claim on the task ran out, and another runner runs it now",
                    ticket.agent
                ),
            );
            None
        } else if interrupted_now {
            // The worker ended under it: put back, to run again.
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
            self.tell(&ticket.id, end, progress).await;
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
    ) -> (RunResult, Vec<(&'static str, String)>) {
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
            .filter(|p| p.node != self.runner)
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
        let delegate = async |prompt: &str| {
            run_delegation(
                self.ensemble,
                &ticket.agent,
                prompt,
                ticket.context.as_deref(),
                dir,
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
                RunResult::err(format!(
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
                return (RunResult::ok(text), reported("passed"));
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
                return (RunResult::err(text), reported("failed"));
            }
            if let Err(refused) = self.spend(&ticket.agent) {
                let text = format!(
                    "{}\n\n[gate {}: failed ({}) on attempt {attempt} of {attempts}; no retry: {refused}]\n```\n{}\n```",
                    result.text, gate.name, verdict.ended, verdict.output
                );
                return (RunResult::err(text), reported("failed"));
            }
            attempt += 1;
            prompt = format!(
                "{}\n\nYour previous attempt did not pass the check `{}` ({}). Its output:\n```\n{}\n```\n\nYour previous reply was:\n{}\n\nFix what the check reports.",
                base, check.command, verdict.ended, verdict.output, result.text
            );
        }
    }

    /// Stop every run, and start nothing more, for when the worker ends.
    /// Tasks that were running are put back on the hub, and those that had
    /// not started stay there, for the next run.
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
        "agent '{agent}' has used its budget for this hour; the next run is allowed in {}.",
        minutes(wait)
    )
}

#[cfg(test)]
mod tests;
