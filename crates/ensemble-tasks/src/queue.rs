//! The worker of one repository: it runs the tasks the hub keeps for the
//! repository's project ([`crate::board`]).
//!
//! While the worker runs ([`Queue::work`], in the runner), it claims every
//! task of the project that may start, oldest id first, each on a harness and
//! an LLM the picker takes for it of what is free on the runner, renews its
//! claim while the run works, and tells the hub how the run ended. A task the
//! worker's end interrupts is put back, and a runner that dies stops
//! renewing, so either way another run takes it up. What each run does as it
//! goes is kept in its activity log ([`Runs`]).
//!
//! Its tests run it against the tasks the hub keeps, in `ensemble-hub`
//! (`tests/queue.rs`).

use super::delegation::{RunResult, run_delegation};
use crate::board::{Board, Claim, End, LEASE, Progress, Renewal};
use crate::runs::Runs;
use crate::tasks::{ASK_MARKER, Detail, Status, Task, Ticket, question_in, utc, waiting_for};
use ensemble_agents::external::gate;
use ensemble_agents::picker::{Taken, above};
use ensemble_agents::router::Ensemble;
use ensemble_agents::runtime::blocking;
use ensemble_agents::worktree;
use ensemble_config::Tier;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{BTreeMap, HashSet};
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

/// How often a retry looks again for a pair strong enough to be free.
const WAIT_FOR_A_PAIR: Duration = Duration::from_secs(1);

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
struct Looked {
    /// Tasks claimed on the board, each with the pair it runs on and the flag
    /// that stops it.
    claimed: Vec<(Ticket, Taken, Arc<AtomicBool>)>,
    /// The worker is closing: nothing new starts.
    closed: bool,
}

#[derive(Default)]
struct Inner {
    /// Set by `close`: nothing new starts.
    closed: bool,
    live: BTreeMap<String, Live>,
    /// Tasks already noted as waiting for a free harness and model, so the
    /// note is written once per wait, not once per scan.
    waits: HashSet<String>,
    /// Whether the hub could not be reached at the last look, so that is said
    /// once, not every few seconds.
    board_down: bool,
}

/// What the last run of a task was on, for its result.
struct RanOn {
    harness: String,
    model: String,
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
    /// What this runner keeps of its runs here: logs and worktrees.
    runs: Runs,
    inner: Mutex<Inner>,
    /// Signalled whenever something starts or ends.
    changed: Notify,
}

impl<'e> Queue<'e> {
    /// The tasks of `board`, run with `ensemble` as the runner `runner`,
    /// keeping what it runs in `runs`.
    pub fn new(ensemble: &'e Ensemble, runs: Runs, board: impl Board + 'e, runner: &str) -> Self {
        Queue {
            ensemble,
            board: Box::new(board),
            runner: runner.to_string(),
            lease: LEASE,
            runs,
            inner: Mutex::new(Inner::default()),
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

    /// Record what the run of task `id` is doing now.
    fn note(&self, id: &str, line: &str) {
        self.runs.log(id, line);
    }

    /// Run every task of the board that may start, oldest id first, as the
    /// runner's harnesses and models allow, until [`Queue::close`] and the
    /// runs left end. The board is looked at again every few seconds, and at
    /// once when a run ends.
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
            for (ticket, taken, cancel) in looked.claimed {
                runs.push(Box::pin(async move {
                    self.run_ticket(ticket, taken, &cancel).await
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

    /// Whether `task` may start here: not ended, not asking, not waiting for
    /// its gate to be confirmed or another task, not running on another
    /// runner, and not given back by this one.
    fn may_start(&self, task: &Task, all: &[Task]) -> bool {
        task.status.is_none()
            && task.question.is_none()
            && !task.gate_unconfirmed
            && task.running_on.as_ref().is_none_or(|n| *n == self.runner)
            && !task.given_back.iter().any(|g| g.runner == self.runner)
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
    /// claimed with the pair the picker took for it.
    async fn look(&self) -> Looked {
        let all = self.scan().await;
        let means = self.ensemble.means();
        // Taken a pair for, to claim on the board.
        let mut picked = Vec::new();
        let closed = {
            let mut inner = self.lock();
            let closed = inner.closed;
            for task in all.iter().filter(|_| !closed) {
                let ticket = &task.ticket;
                if inner.live.contains_key(&ticket.id) || !self.may_start(task, &all) {
                    continue;
                }
                let Some(taken) = means.take(None) else {
                    if inner.waits.insert(ticket.id.clone()) {
                        self.note(&ticket.id, "waiting for a harness and a model to be free");
                    }
                    break;
                };
                let cancel = Arc::new(AtomicBool::new(false));
                inner.live.insert(
                    ticket.id.clone(),
                    Live {
                        cancel: cancel.clone(),
                        asked: false,
                    },
                );
                picked.push((ticket.clone(), taken, cancel));
            }
            closed
        };
        let mut claimed = Vec::new();
        for (ticket, taken, cancel) in picked {
            let movable = self.movable(&ticket);
            match self
                .board
                .claim(&ticket.id, &self.runner, self.lease, movable)
                .await
            {
                Ok(Claim::Taken) => {
                    self.lock().waits.remove(&ticket.id);
                    claimed.push((ticket, taken, cancel));
                }
                // Another runner was first, or the hub could not be asked:
                // the next look tries again.
                Ok(Claim::Refused(_)) | Err(_) => {
                    self.lock().live.remove(&ticket.id);
                    drop(taken);
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

    /// Whether a run of `ticket` here could be carried on elsewhere: its
    /// branch can be pushed to the repository's remote. Without one, the
    /// work is in this computer's folder, so the task stays with this runner
    /// once it has run.
    fn movable(&self, ticket: &Ticket) -> bool {
        self.project_of(ticket)
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

    async fn run_ticket(&self, ticket: Ticket, taken: Taken, cancel: &AtomicBool) {
        let since = SystemTime::now();
        // What it asked and was answered, and where the work of an earlier
        // run was pushed.
        let detail = self.board.get(&ticket.id).await.ok().flatten();
        let carried = detail
            .as_ref()
            .and_then(|d| d.progress.as_ref())
            .map(|p| p.commit.clone());
        let place = self.project_of(&ticket).and_then(|project| {
            self.note(&ticket.id, "preparing a git worktree");
            let root = self.runs.root();
            blocking(|| worktree::prepare_from(&project, root, &ticket.id, carried.as_deref()))
        });
        let ran_on = RanOn {
            harness: taken.spec.kind().to_string(),
            model: taken.spec.model().to_string(),
        };
        let lost = AtomicBool::new(false);
        let work = async {
            match &place {
                Ok(wt) => {
                    self.work_on(&ticket, detail.as_ref(), &wt.dir, taken, cancel)
                        .await
                }
                Err(e) => (RunResult::err(e.clone()), Vec::new(), ran_on),
            }
        };
        let (mut result, mut extra, ran_on) = tokio::select! {
            done = work => done,
            () = self.hold(&ticket.id, cancel, &lost) => unreachable!("holding never ends"),
        };
        self.changed.notify_waiters();
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
        if let Some(question) = &question {
            self.note(&ticket.id, &format!("needs input: {question}"));
        }
        if let Ok(wt) = &place
            && !interrupted_now
            && question.is_none()
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
            Ok(wt) if !lost => {
                if interrupted_now || question.is_some() {
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
                "stopped: this runner's claim on the task ran out, and another runner runs it now",
            );
            None
        } else if interrupted_now {
            // The worker ended under it: put back, to run again.
            Some(None)
        } else if let Some(question) = question {
            Some(Some(End::Ask {
                question,
                reply: result.text.clone(),
                repository: false,
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
                harness: ran_on.harness,
                model: ran_on.model,
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

    /// A pair at least `at_least` strong, when one that strong exists, taken
    /// as soon as one is free; none once the run is cancelled.
    async fn take_when_free(&self, at_least: Tier, cancel: &AtomicBool) -> Option<Taken> {
        loop {
            if let Some(taken) = self.ensemble.means().take(Some(at_least)) {
                return Some(taken);
            }
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            tokio::time::sleep(WAIT_FOR_A_PAIR).await;
        }
    }

    /// Run `taken` on `ticket` in `dir`, then its gate if it has one. While
    /// the gate fails and attempts remain, it runs again with the gate's
    /// output, on a pair a tier stronger. Returns the outcome, front matter
    /// for the result, and what the last run was on.
    async fn work_on(
        &self,
        ticket: &Ticket,
        detail: Option<&Detail>,
        dir: &Path,
        mut taken: Taken,
        cancel: &AtomicBool,
    ) -> (RunResult, Vec<(&'static str, String)>, RanOn) {
        let note = |line: &str| self.note(&ticket.id, line);
        let ran_on = |taken: &Taken| RanOn {
            harness: taken.spec.kind().to_string(),
            model: taken.spec.model().to_string(),
        };
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
        base.push_str(&format!(
            "\n\nIf you cannot finish without a decision that only the person who asked can make, stop and end your reply with one line: {ASK_MARKER} <your question>"
        ));
        let delegate = async |taken: &Taken, prompt: &str| {
            note(&format!("running on {}", taken.spec.name));
            run_delegation(
                self.ensemble,
                taken,
                prompt,
                ticket.context.as_deref(),
                dir,
                &note,
                cancel,
            )
            .await
        };
        let Some(gate) = &ticket.gate else {
            let result = delegate(&taken, &base).await;
            return (result, Vec::new(), ran_on(&taken));
        };
        let attempts = gate.attempts.max(1);
        let command = serde_json::to_string(&gate.command).expect("a string serializes");
        let mut prompt = base.clone();
        let mut attempt = 1;
        loop {
            let reported = |verdict: &str| {
                vec![
                    ("gate", command.clone()),
                    ("gate_result", verdict.to_string()),
                    ("attempts", attempt.to_string()),
                ]
            };
            let result = delegate(&taken, &prompt).await;
            if result.is_error
                || cancel.load(Ordering::Relaxed)
                || question_in(&result.text).is_some()
            {
                return (result, reported("not run"), ran_on(&taken));
            }
            note(&format!("gate: running `{}`", gate.command));
            let verdict = gate::run(&gate.command, gate.timeout_seconds, dir, cancel).await;
            if verdict.passed {
                let text = format!(
                    "{}\n\n[gate: passed on attempt {attempt} of {attempts}]",
                    result.text
                );
                return (RunResult::ok(text), reported("passed"), ran_on(&taken));
            }
            note(&format!(
                "gate: failed ({}) on attempt {attempt} of {attempts}",
                verdict.ended
            ));
            let failed = |why: &str| {
                format!(
                    "{}\n\n[gate: failed ({}) on attempt {attempt} of {attempts}{why}]\n```\n{}\n```",
                    result.text, verdict.ended, verdict.output
                )
            };
            if attempt >= attempts || cancel.load(Ordering::Relaxed) {
                return (
                    RunResult::err(failed("")),
                    reported("failed"),
                    ran_on(&taken),
                );
            }
            // The next attempt asks for a tier above the last, and waits for
            // a pair that strong to be free.
            let at_least = above(taken.spec.tier);
            let last = ran_on(&taken);
            drop(taken);
            self.changed.notify_waiters();
            taken = match self.take_when_free(at_least, cancel).await {
                Some(next) => next,
                None => {
                    return (
                        RunResult::err(failed("; no retry: the task was stopped")),
                        reported("failed"),
                        last,
                    );
                }
            };
            attempt += 1;
            prompt = format!(
                "{}\n\nA previous attempt did not pass the check `{}` ({}). Its output:\n```\n{}\n```\n\nIts reply was:\n{}\n\nFix what the check reports.",
                base, gate.command, verdict.ended, verdict.output, result.text
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
