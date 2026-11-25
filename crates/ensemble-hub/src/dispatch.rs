//! The hub's task picker at work: it chooses the worker of each task, a
//! runner of the task's account and the harness and the model it runs it
//! on, by asking a decision model, and offers the task to that runner.
//!
//! A task is chosen for once it may start: made, answered, its gate
//! confirmed, what it waits for done, and not running. The decision models of
//! the hub's `task_picker` are asked in order, the next when one fails, with
//! the task (its title, text, definition of done, project, and what came of
//! its earlier runs) as the state: first the questions a person wrote, then
//! which worker, among the workers of the account's runners connected now
//! that every answer's effects allow, those that gave the task back left
//! out. Its workers most likely first, those at least as likely as the
//! picker's least confidence are its candidates, a worker preferred by an
//! answer first; the first of them that is free is offered the task, and a
//! task whose candidates are all busy waits for one. An answer less likely
//! than its question's least confidence, or no candidate, leaves the choice
//! to the task's account (`needs_choice`), who chooses among the workers
//! ranked; that choice is kept with the decision, to learn from.
//!
//! With no task picker, or no decision model that answers, a task waits,
//! saying why, and is tried again later. A change to the task picker, its
//! decision models or the workers connected sends every task decided and
//! not yet offered, or waiting for a choice, back to be chosen again; a
//! runner that goes away before it ran a task offered to it has the task
//! chosen again without it.

use crate::board::{DbBoard, boards};
use ensemble_backends::decide::{self, Answer, Question};
use ensemble_config::picker::{NO, WORKER, YES};
use ensemble_config::{
    Effect, HarnessKind, LlmBackend, PickerQuestion, QuestionKind, TaskPickerSetting, Tier,
};
use ensemble_tasks::tasks::{Answered, Decision, Pick, Ranked, Task, Worker, utc, waiting_for};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// The most workers one question offers: Laya takes 100 options at most.
pub const MAX_OPTIONS: usize = 100;

/// How often the tasks are looked at.
const EVERY: Duration = Duration::from_secs(1);

/// How long a task that could not be chosen for waits before it is tried
/// again, at first and at most.
const FIRST_WAIT: Duration = Duration::from_secs(10);
const LONGEST_WAIT: Duration = Duration::from_secs(300);

/// What the worker choice asks.
const WORKER_INSTRUCTIONS: &str = "Which of these workers should work on this task? Each is a harness, the program that works in the repository, and the model it drives; choose the one whose model fits the task best, by how strong it is and what it is good at.";

/// A worker of a runner connected now, with what its runner says of its
/// model.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerNow {
    pub worker: Worker,
    /// The provider of the runner's pool its model is on; none for one of
    /// the harness's own.
    pub provider: Option<String>,
    pub tier: Tier,
    pub context_window: Option<u32>,
    pub tools: Option<bool>,
    pub abilities: Option<String>,
}

impl WorkerNow {
    /// It, as the decision model reads it.
    fn describe(&self) -> String {
        let w = &self.worker;
        let mut said = format!(
            "{} driving {}, a {} model",
            w.harness,
            w.model,
            self.tier.as_str()
        );
        if let Some(n) = self.context_window {
            said.push_str(&format!(" with {n} tokens of context"));
        }
        match self.tools {
            Some(true) => said.push_str(", that makes tool calls"),
            Some(false) => said.push_str(", that makes no tool calls"),
            None => {}
        }
        said.push_str(&format!(", on the computer {}.", w.runner));
        if let Some(a) = self.abilities.as_deref().filter(|a| !a.trim().is_empty()) {
            said.push_str(&format!(" What it is good and bad at: {}", a.trim()));
        }
        said
    }

    /// Whether `effect` allows it.
    fn fits(&self, effect: &Effect) -> bool {
        match effect {
            Effect::MinTier { tier } => self.tier >= *tier,
            Effect::MinContext { tokens } => self.context_window.is_none_or(|n| n >= *tokens),
            Effect::NeedsTools => self.tools == Some(true),
            Effect::Avoid { .. } => !self.named(effect),
            Effect::Prefer { .. } => true,
        }
    }

    /// Whether `effect`, a preference or an avoidance, names its harness or
    /// its model.
    fn named(&self, effect: &Effect) -> bool {
        let (Effect::Prefer { name } | Effect::Avoid { name }) = effect else {
            return false;
        };
        match effect.harness() {
            Some(h) => HarnessKind::parse(&self.worker.harness) == Some(h),
            None => self.worker.model == *name,
        }
    }
}

/// A runner connected now: its workers, and how busy it is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunnerNow {
    pub owner: String,
    pub name: String,
    pub workers: Vec<WorkerNow>,
    /// The most runs at once of each harness.
    pub harness_limits: BTreeMap<String, u32>,
    /// The most calls at once to each provider of its pool.
    pub provider_limits: BTreeMap<String, u32>,
    /// Runs going on now, on each harness and holding a call of each
    /// provider.
    pub busy_harnesses: BTreeMap<String, u32>,
    pub busy_providers: BTreeMap<String, u32>,
}

/// The task picker, as the hub's settings say now, with its providers.
#[derive(Debug, Clone, PartialEq)]
struct Picking {
    setting: TaskPickerSetting,
    backends: BTreeMap<String, LlmBackend>,
}

/// What a decision came to.
struct Chosen {
    decision: Decision,
    pick: Pick,
}

/// Tasks that could not be chosen for, by account, project and id: when to
/// try again, and how long the last wait was.
type Later = HashMap<(String, String, String), (Instant, Duration)>;

/// The runners connected now.
type Runners = dyn Fn() -> Vec<RunnerNow> + Send + Sync;

/// The hub's task picker at work, on the tasks in `dir`.
pub struct Dispatcher {
    dir: PathBuf,
    runners: Box<Runners>,
    picking: Mutex<Option<Picking>>,
    /// Set when what tasks were chosen of changed.
    stale: AtomicBool,
    /// The workers connected at the last look, to see them change.
    seen: Mutex<Vec<(String, String, Vec<WorkerNow>)>>,
    /// Tasks that could not be chosen for, by account, project and id: when
    /// to try again, and how long the last wait was.
    later: Mutex<Later>,
}

impl Dispatcher {
    /// The task picker on the tasks in `dir`, choosing of the workers
    /// `runners` says are connected; with none until [`Dispatcher::apply`].
    pub fn new(dir: &Path, runners: impl Fn() -> Vec<RunnerNow> + Send + Sync + 'static) -> Self {
        Dispatcher {
            dir: dir.to_path_buf(),
            runners: Box::new(runners),
            picking: Mutex::new(None),
            stale: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
            later: Mutex::new(HashMap::new()),
        }
    }

    fn later(&self) -> MutexGuard<'_, Later> {
        self.later.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Choose with `picker`, its decision models reached as `backends` say,
    /// or with none. When it changed, every task decided and not yet
    /// offered is chosen for again.
    pub fn apply(
        &self,
        picker: Option<&TaskPickerSetting>,
        backends: &BTreeMap<String, LlmBackend>,
    ) {
        let now = picker.map(|setting| Picking {
            setting: setting.clone(),
            backends: setting
                .decision_models
                .iter()
                .filter_map(|m| Some((m.provider.clone(), backends.get(&m.provider)?.clone())))
                .collect(),
        });
        let mut picking = self.picking.lock().unwrap_or_else(|e| e.into_inner());
        if *picking != now {
            *picking = now;
            self.stale.store(true, Ordering::Relaxed);
        }
    }

    /// Look at the tasks every second until `stop` is set.
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
        let mut failing = false;
        while !stop.load(Ordering::Relaxed) {
            match self.pass() {
                Ok(()) => failing = false,
                Err(e) if !failing => {
                    failing = true;
                    say(&format!("the task picker cannot read the tasks: {e}"));
                }
                Err(_) => {}
            }
            std::thread::sleep(EVERY);
        }
    }

    /// One look at every task: choose the worker of those that may start,
    /// and offer those chosen for to the first of their candidates that is
    /// free.
    pub fn pass(&self) -> Result<(), String> {
        let runners = (self.runners)();
        let seen: Vec<(String, String, Vec<WorkerNow>)> = runners
            .iter()
            .map(|r| (r.owner.clone(), r.name.clone(), r.workers.clone()))
            .collect();
        {
            let mut before = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if *before != seen {
                *before = seen;
                self.stale.store(true, Ordering::Relaxed);
            }
        }
        let stale = self.stale.swap(false, Ordering::Relaxed);
        if stale {
            self.later().clear();
        }
        let picking = self
            .picking
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut all: Vec<(DbBoard, Vec<Task>, String)> = Vec::new();
        for (owner, project) in boards(&self.dir)? {
            let board = DbBoard::new(&self.dir, &owner, &project);
            let mut tasks = board.list()?;
            if stale {
                let mut changed = false;
                for t in tasks.iter().filter(|t| t.status.is_none()) {
                    changed |= board.reconsider(t.id())?;
                }
                if changed {
                    tasks = board.list()?;
                }
            }
            all.push((board, tasks, project));
        }
        // Runs each runner has been offered and has not started, on each
        // harness and provider, counted as busy.
        let mut offered: HashMap<(String, String), u32> = HashMap::new();
        let mut offered_providers: HashMap<(String, String), u32> = HashMap::new();
        let provider_of = |w: &Worker, owner: &str| {
            runners
                .iter()
                .find(|r| r.owner == owner && r.name == w.runner)
                .and_then(|r| r.workers.iter().find(|x| x.worker == *w))
                .and_then(|x| x.provider.clone())
        };
        for (board, tasks, _) in &all {
            for t in tasks
                .iter()
                .filter(|t| t.status.is_none() && t.running_on.is_none())
            {
                if let Pick::Offered { worker } = &t.pick {
                    let owner = board.owner();
                    *offered
                        .entry((key(owner, &worker.runner), worker.harness.clone()))
                        .or_default() += 1;
                    if let Some(p) = provider_of(worker, owner) {
                        *offered_providers
                            .entry((key(owner, &worker.runner), p))
                            .or_default() += 1;
                    }
                }
            }
        }
        for (board, tasks, project) in &all {
            let owner = board.owner();
            let mine: Vec<&RunnerNow> = runners.iter().filter(|r| r.owner == owner).collect();
            for task in tasks.iter().filter(|t| t.status.is_none()) {
                match &task.pick {
                    Pick::Offered { worker } => {
                        if task.running_on.is_none()
                            && !mine.iter().any(|r| r.name == worker.runner)
                        {
                            board.withdraw(task.id(), &worker.runner)?;
                        }
                    }
                    Pick::Undecided { .. } => {
                        if !may_start(task, tasks) {
                            continue;
                        }
                        let at = (owner.to_string(), project.clone(), task.id().to_string());
                        if self
                            .later()
                            .get(&at)
                            .is_some_and(|(when, _)| Instant::now() < *when)
                        {
                            continue;
                        }
                        let chosen = match &picking {
                            None => Err(
                                "there is no task picker: an admin chooses its decision models (`picker set`)".to_string(),
                            ),
                            Some(p) => self.choose(p, task, project, &mine),
                        };
                        match chosen {
                            Ok(chosen) => {
                                self.later().remove(&at);
                                board.decide(
                                    task.id(),
                                    task.round,
                                    chosen.decision,
                                    chosen.pick,
                                )?;
                            }
                            Err(why) => {
                                let mut later = self.later();
                                let wait = later
                                    .get(&at)
                                    .map_or(FIRST_WAIT, |(_, w)| (*w * 2).min(LONGEST_WAIT));
                                later.insert(at, (Instant::now() + wait, wait));
                                drop(later);
                                board.wait(task.id(), task.round, &why)?;
                            }
                        }
                    }
                    Pick::Decided | Pick::NeedsChoice { .. } => {}
                }
            }
            // What was decided, now or before, is offered once a candidate
            // is free.
            for task in board.list()?.iter().filter(|t| t.status.is_none()) {
                if task.pick != Pick::Decided {
                    continue;
                }
                let Some(decision) = &task.decision else {
                    continue;
                };
                let present: Vec<&Worker> = decision
                    .candidates
                    .iter()
                    .filter(|w| {
                        mine.iter().any(|r| {
                            r.name == w.runner && r.workers.iter().any(|x| x.worker == **w)
                        })
                    })
                    .collect();
                if present.is_empty() {
                    // None of them is there any more: chosen for again.
                    board.reconsider(task.id())?;
                    continue;
                }
                let free = present.into_iter().find(|w| {
                    let Some(r) = mine.iter().find(|r| r.name == w.runner) else {
                        return false;
                    };
                    let k = key(owner, &r.name);
                    let runs = r.busy_harnesses.get(&w.harness).copied().unwrap_or(0)
                        + offered
                            .get(&(k.clone(), w.harness.clone()))
                            .copied()
                            .unwrap_or(0);
                    let harness_free =
                        runs < r.harness_limits.get(&w.harness).copied().unwrap_or(1);
                    let provider = r
                        .workers
                        .iter()
                        .find(|x| x.worker == **w)
                        .and_then(|x| x.provider.clone());
                    let provider_free = provider.as_ref().is_none_or(|p| {
                        let calls = r.busy_providers.get(p).copied().unwrap_or(0)
                            + offered_providers
                                .get(&(k.clone(), p.clone()))
                                .copied()
                                .unwrap_or(0);
                        r.provider_limits.get(p).is_none_or(|max| calls < *max)
                    });
                    harness_free && provider_free
                });
                if let Some(w) = free
                    && board.offer(task.id(), w)?
                {
                    let k = key(owner, &w.runner);
                    *offered.entry((k.clone(), w.harness.clone())).or_default() += 1;
                    if let Some(p) = provider_of(w, owner) {
                        *offered_providers.entry((k, p)).or_default() += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Ask the decision models of `picking`, in order, to choose the worker
    /// of `task` of `project`, among the workers of `runners`, its account's.
    /// Why none could, when none could.
    fn choose(
        &self,
        picking: &Picking,
        task: &Task,
        project: &str,
        runners: &[&RunnerNow],
    ) -> Result<Chosen, String> {
        let options: Vec<&WorkerNow> = runners
            .iter()
            .filter(|r| !task.given_back.iter().any(|g| g.runner == r.name))
            .filter(|r| task.pinned_to.as_ref().is_none_or(|p| *p == r.name))
            .flat_map(|r| r.workers.iter())
            .collect();
        if options.is_empty() {
            return Err(match &task.pinned_to {
                Some(p) => format!("its work is in {p}'s folder, and {p} is not connected"),
                None => "no runner of its account that may take it is connected".to_string(),
            });
        }
        let mut failed = Vec::new();
        for m in &picking.setting.decision_models {
            let name = format!("{}:{}", m.provider, m.model);
            let Some(backend) = picking.backends.get(&m.provider) else {
                failed.push(format!("{name}: its provider cannot be reached"));
                continue;
            };
            match ask(
                backend,
                &m.model,
                &picking.setting,
                task,
                project,
                &options,
                runners,
            ) {
                Ok(mut chosen) => {
                    chosen.decision.by = name;
                    return Ok(chosen);
                }
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }
        Err(format!("no decision model answered: {}", failed.join("; ")))
    }
}

/// `<owner>/<runner>`, a runner of an account.
fn key(owner: &str, runner: &str) -> String {
    format!("{owner}/{runner}")
}

/// Whether `task`, not ended, may start: answered, its gate confirmed, what
/// it waits for done, and not running.
fn may_start(task: &Task, all: &[Task]) -> bool {
    task.status.is_none()
        && task.question.is_none()
        && !task.gate_unconfirmed
        && task.running_on.is_none()
        && waiting_for(&task.ticket, all).is_empty()
}

/// The task, as a decision model is told of it.
fn state(task: &Task, project: &str) -> Value {
    let t = &task.ticket;
    let attempts: Vec<Value> = task
        .attempts
        .iter()
        .map(|a| json!({ "worker": a.worker.name(), "outcome": a.outcome }))
        .collect();
    json!({
        "project": project,
        "title": t.title,
        "task": t.text,
        "definition_of_done": t.definition_of_done,
        "goal": t.goal,
        "constraints": t.constraints,
        "earlier_runs": attempts,
    })
}

/// `q`, as it is asked.
fn question(q: &PickerQuestion) -> Question {
    let instructions = q.instructions.clone();
    match q.kind {
        QuestionKind::Choice => Question::Choice {
            instructions,
            criteria: q
                .answers
                .iter()
                .map(|a| (a.name.clone(), a.description.clone()))
                .collect(),
        },
        QuestionKind::Score => Question::Score {
            instructions: format!(
                "{instructions} Levels, lowest first: {}",
                q.answers
                    .iter()
                    .map(|a| format!("{}: {}", a.name, a.description))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            criteria: q.answers.iter().map(|a| a.name.clone()).collect(),
        },
        QuestionKind::Noul => Question::Noul { instructions },
    }
}

/// What `answer` answered `q`: the answer taken, its probability, and every
/// answer's.
fn read(q: &PickerQuestion, answer: &Answer) -> Result<Answered, String> {
    let names: Vec<&str> = q.answers.iter().map(|a| a.name.as_str()).collect();
    let (taken, probabilities): (String, BTreeMap<String, f64>) = match (q.kind, answer) {
        (
            QuestionKind::Choice,
            Answer::Choice {
                choice,
                probabilities,
                ..
            },
        ) => {
            if !names.contains(&choice.as_str()) {
                return Err(format!(
                    "{} was answered '{choice}', not one of its answers",
                    q.name
                ));
            }
            (choice.clone(), probabilities.clone())
        }
        (
            QuestionKind::Score,
            Answer::Score {
                probabilities,
                score,
                ..
            },
        ) => {
            // Each level's probability, by its number.
            let by_name: BTreeMap<String, f64> = names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    (
                        n.to_string(),
                        probabilities.get(&i.to_string()).copied().unwrap_or(0.0),
                    )
                })
                .collect();
            let level = if by_name.values().any(|p| *p > 0.0) {
                names
                    .iter()
                    .copied()
                    .max_by(|a, b| by_name[*a].total_cmp(&by_name[*b]))
                    .unwrap_or_default()
                    .to_string()
            } else {
                let i = (score.round().max(0.0) as usize).min(names.len().saturating_sub(1));
                names.get(i).copied().unwrap_or_default().to_string()
            };
            (level, by_name)
        }
        (QuestionKind::Noul, Answer::Noul { noul }) => {
            let taken = if *noul >= 0.5 { YES } else { NO };
            let probabilities =
                BTreeMap::from([(YES.to_string(), *noul), (NO.to_string(), 1.0 - noul)]);
            (taken.to_string(), probabilities)
        }
        _ => {
            return Err(format!(
                "{} was answered as another type of question",
                q.name
            ));
        }
    };
    Ok(Answered {
        question: q.name.clone(),
        probability: probabilities.get(&taken).copied().unwrap_or(0.0),
        answer: taken,
        probabilities,
    })
}

/// Ask the decision model `model` of `backend` to choose the worker of
/// `task` among `options`, by `picker`: its questions first, then the worker.
#[allow(clippy::too_many_arguments)]
fn ask(
    backend: &LlmBackend,
    model: &str,
    picker: &TaskPickerSetting,
    task: &Task,
    project: &str,
    options: &[&WorkerNow],
    runners: &[&RunnerNow],
) -> Result<Chosen, String> {
    let mut state = state(task, project);
    let mut answered = Vec::new();
    let mut unsure = Vec::new();
    let mut effects: Vec<Effect> = Vec::new();
    if !picker.questions.is_empty() {
        let asked: BTreeMap<String, Question> = picker
            .questions
            .iter()
            .map(|q| (q.name.clone(), question(q)))
            .collect();
        let answers = decide::decide(backend, model, &state, &asked).map_err(|e| e.to_string())?;
        for q in &picker.questions {
            let a = answers
                .answers
                .get(&q.name)
                .ok_or_else(|| format!("it did not answer {}", q.name))?;
            let a = read(q, a)?;
            if a.probability < q.min_confidence {
                unsure.push(format!(
                    "{} is {} at {:.2}, less than {}",
                    q.name, a.answer, a.probability, q.min_confidence
                ));
            }
            if let Some(found) = q.answers.iter().find(|x| x.name == a.answer) {
                effects.extend(found.effects.iter().cloned());
            }
            answered.push(a);
        }
        state["answers"] = json!(
            answered
                .iter()
                .map(|a| (a.question.clone(), json!(a.answer)))
                .collect::<serde_json::Map<_, _>>()
        );
    }
    let mut fitting: Vec<&WorkerNow> = options
        .iter()
        .copied()
        .filter(|w| effects.iter().all(|e| w.fits(e)))
        .collect();
    let decision = |ranked: Vec<Ranked>, candidates: Vec<Worker>| Decision {
        at: utc(SystemTime::now()),
        by: String::new(),
        answers: answered.clone(),
        ranked,
        candidates,
        picked_by_person: None,
    };
    if fitting.is_empty() {
        let written: Vec<String> = effects.iter().map(Effect::written).collect();
        let ranked = options
            .iter()
            .map(|w| Ranked {
                worker: w.worker.clone(),
                probability: 0.0,
            })
            .collect();
        return Ok(Chosen {
            decision: decision(ranked, Vec::new()),
            pick: Pick::NeedsChoice {
                why: format!(
                    "no worker connected fits the answers ({})",
                    written.join(", ")
                ),
            },
        });
    }
    if fitting.len() > MAX_OPTIONS {
        // The busiest and the weakest are left out first.
        let busy = |w: &WorkerNow| {
            runners
                .iter()
                .find(|r| r.name == w.worker.runner)
                .and_then(|r| r.busy_harnesses.get(&w.worker.harness).copied())
                .unwrap_or(0)
        };
        fitting.sort_by_key(|w| (busy(w), std::cmp::Reverse(w.tier)));
        fitting.truncate(MAX_OPTIONS);
    }
    let names: Vec<String> = fitting.iter().map(|w| w.worker.name()).collect();
    let asked = BTreeMap::from([(
        WORKER.to_string(),
        Question::Choice {
            instructions: WORKER_INSTRUCTIONS.into(),
            criteria: fitting
                .iter()
                .zip(&names)
                .map(|(w, n)| (n.clone(), w.describe()))
                .collect(),
        },
    )]);
    let answers = decide::decide(backend, model, &state, &asked).map_err(|e| e.to_string())?;
    let Some(Answer::Choice { probabilities, .. }) = answers.answers.get(WORKER) else {
        return Err("it did not choose a worker".into());
    };
    let mut ranked: Vec<Ranked> = fitting
        .iter()
        .zip(&names)
        .map(|(w, n)| Ranked {
            worker: w.worker.clone(),
            probability: probabilities.get(n).copied().unwrap_or(0.0),
        })
        .collect();
    ranked.sort_by(|a, b| b.probability.total_cmp(&a.probability));
    let likely: Vec<&Ranked> = ranked
        .iter()
        .filter(|r| r.probability >= picker.worker_min_confidence)
        .collect();
    // A worker an answer prefers goes first.
    let preferred = |r: &Ranked| {
        fitting.iter().any(|w| {
            w.worker == r.worker
                && effects
                    .iter()
                    .any(|e| matches!(e, Effect::Prefer { .. }) && w.named(e))
        })
    };
    let candidates: Vec<Worker> = likely
        .iter()
        .filter(|r| preferred(r))
        .chain(likely.iter().filter(|r| !preferred(r)))
        .map(|r| r.worker.clone())
        .collect();
    let pick = if !unsure.is_empty() {
        Pick::NeedsChoice {
            why: format!("the decision model is not sure: {}", unsure.join("; ")),
        }
    } else if candidates.is_empty() {
        Pick::NeedsChoice {
            why: format!(
                "no worker is at least {} likely: the likeliest, {}, is {:.2}",
                picker.worker_min_confidence,
                ranked[0].worker.name(),
                ranked[0].probability
            ),
        }
    } else {
        Pick::Decided
    };
    Ok(Chosen {
        decision: decision(ranked, candidates),
        pick,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_backends::testing::{answers, decision_server};
    use ensemble_config::picker::template;
    use ensemble_config::{BackendSettings, ModelSetting};
    use ensemble_core::workspace::testing::TempDir;
    use ensemble_tasks::tasks::Ticket;
    use std::sync::Arc;

    /// What the stub decision model answers: for each question, the answer
    /// whose name holds the text given for it, else the first, with the
    /// probability given.
    type Answering = Arc<Mutex<BTreeMap<String, (String, f64)>>>;

    struct Hub {
        tmp: TempDir,
        runners: Arc<Mutex<Vec<RunnerNow>>>,
        answering: Answering,
        dispatcher: Dispatcher,
        stub: ensemble_backends::testing::ModelServer,
    }

    impl Hub {
        fn new() -> Hub {
            let tmp = TempDir::new();
            let runners: Arc<Mutex<Vec<RunnerNow>>> = Arc::default();
            let answering: Answering = Arc::default();
            let said = answering.clone();
            let stub = decision_server(move |asked| {
                let said = said.lock().unwrap();
                answers(asked, |question, options| {
                    let (want, p) = said.get(question).cloned().unwrap_or(("".into(), 0.9));
                    let found = options
                        .iter()
                        .find(|o| !want.is_empty() && o.contains(&want))
                        .unwrap_or(&options[0]);
                    (found.clone(), p)
                })
            });
            let seen = runners.clone();
            let dispatcher = Dispatcher::new(&tmp.0, move || seen.lock().unwrap().clone());
            Hub {
                tmp,
                runners,
                answering,
                dispatcher,
                stub,
            }
        }

        /// The task picker: the stub, sure of the worker at least `min`, asking
        /// `questions` first.
        fn picker(&self, min: f64, questions: Vec<PickerQuestion>) {
            let setting = TaskPickerSetting {
                decision_models: vec![ModelSetting {
                    provider: "laya".into(),
                    model: "stub".into(),
                }],
                worker_min_confidence: min,
                questions,
            };
            let backends = BTreeMap::from([(
                "laya".to_string(),
                LlmBackend {
                    name: "laya".into(),
                    timeout_seconds: 5,
                    settings: BackendSettings::Decision {
                        base_url: self.stub.url.clone(),
                        api_key: None,
                    },
                },
            )]);
            self.dispatcher.apply(Some(&setting), &backends);
        }

        fn answer(&self, question: &str, with: &str, p: f64) {
            self.answering
                .lock()
                .unwrap()
                .insert(question.into(), (with.into(), p));
        }

        fn board(&self) -> DbBoard {
            DbBoard::new(&self.tmp.0, "ana", "shop")
        }

        fn task(&self, id: &str) -> Task {
            self.board().detail(id).unwrap().unwrap().task
        }

        fn add(&self) -> String {
            self.board()
                .add(&Ticket {
                    title: "Fix the cart".into(),
                    text: "The cart loses items.".into(),
                    definition_of_done: "Items stay.".into(),
                    ..Ticket::default()
                })
                .unwrap()
        }
    }

    fn worker(runner: &str, harness: &str, model: &str) -> Worker {
        Worker {
            runner: runner.into(),
            harness: harness.into(),
            model: model.into(),
        }
    }

    /// A runner of ana's with Claude Code on Sonnet, strong, and OpenCode on
    /// a small model of its local server, one run of each at once.
    fn runner(name: &str) -> RunnerNow {
        let now = |harness: &str, model: &str, provider: Option<&str>, tier| WorkerNow {
            worker: worker(name, harness, model),
            provider: provider.map(String::from),
            tier,
            context_window: provider.map(|_| 32768),
            tools: Some(true),
            abilities: None,
        };
        RunnerNow {
            owner: "ana".into(),
            name: name.into(),
            workers: vec![
                now("claude-code", "sonnet", None, Tier::Strong),
                now("opencode", "local:small", Some("local"), Tier::Small),
            ],
            harness_limits: BTreeMap::from([("claude-code".into(), 1), ("opencode".into(), 1)]),
            provider_limits: BTreeMap::from([("local".into(), 1)]),
            ..RunnerNow::default()
        }
    }

    #[test]
    fn with_no_task_picker_or_no_decision_model_answering_a_task_waits_saying_why() {
        let hub = Hub::new();
        hub.runners.lock().unwrap().push(runner("desk"));
        let id = hub.add();
        hub.dispatcher.pass().unwrap();
        let Pick::Undecided { why: Some(why) } = hub.task(&id).pick else {
            panic!("{:?}", hub.task(&id).pick)
        };
        assert!(why.contains("there is no task picker"), "{why}");
        let mut setting = TaskPickerSetting {
            decision_models: vec![ModelSetting {
                provider: "laya".into(),
                model: "stub".into(),
            }],
            worker_min_confidence: 0.5,
            questions: Vec::new(),
        };
        let down = BTreeMap::from([(
            "laya".to_string(),
            LlmBackend {
                name: "laya".into(),
                timeout_seconds: 2,
                settings: BackendSettings::Decision {
                    base_url: ensemble_backends::testing::nowhere(),
                    api_key: None,
                },
            },
        )]);
        hub.dispatcher.apply(Some(&setting), &down);
        hub.dispatcher.pass().unwrap();
        let task = hub.task(&id);
        let Pick::Undecided { why: Some(why) } = &task.pick else {
            panic!("{:?}", task.pick)
        };
        assert!(
            why.starts_with("no decision model answered: laya:stub: "),
            "{why}"
        );
        assert!(task.decision.is_none());
        // Waiting, it is not asked again at once.
        setting.worker_min_confidence = 0.5;
        hub.dispatcher.pass().unwrap();
        assert_eq!(hub.task(&id).round, task.round);
    }

    #[test]
    fn a_task_is_offered_to_the_worker_the_decision_model_is_sure_of() {
        let hub = Hub::new();
        hub.runners.lock().unwrap().push(runner("desk"));
        hub.picker(0.6, Vec::new());
        hub.answer(WORKER, "sonnet", 0.9);
        let id = hub.add();
        hub.dispatcher.pass().unwrap();
        let task = hub.task(&id);
        assert_eq!(
            task.pick,
            Pick::Offered {
                worker: worker("desk", "claude-code", "sonnet")
            }
        );
        let d = task.decision.unwrap();
        assert_eq!(d.by, "laya:stub");
        assert_eq!(d.ranked[0].worker.model, "sonnet");
        assert_eq!(d.ranked[0].probability, 0.9);
        assert_eq!(d.candidates.len(), 1, "only sonnet is likely enough");
        // The decision model was told the task and each worker.
        let asked: Value = serde_json::from_str(&hub.stub.requests.lock().unwrap()[0]).unwrap();
        assert_eq!(asked["state"]["title"], "Fix the cart");
        assert_eq!(asked["state"]["definition_of_done"], "Items stay.");
        let criteria = &asked["questions"]["worker"]["criteria"];
        assert!(
            criteria["claude-code on sonnet on desk"]
                .as_str()
                .unwrap()
                .contains("a strong model"),
            "{criteria}"
        );
        // The runner claims it there.
        assert_eq!(
            hub.board().take(&id, "desk", 60, true).unwrap(),
            ensemble_tasks::board::Claim::Taken
        );
    }

    #[test]
    fn when_the_decision_model_is_not_sure_the_tasks_account_chooses() {
        let hub = Hub::new();
        hub.runners.lock().unwrap().push(runner("desk"));
        hub.picker(0.6, Vec::new());
        hub.answer(WORKER, "sonnet", 0.5);
        let id = hub.add();
        hub.dispatcher.pass().unwrap();
        let Pick::NeedsChoice { why } = hub.task(&id).pick else {
            panic!("{:?}", hub.task(&id).pick)
        };
        assert!(why.contains("no worker is at least 0.6 likely"), "{why}");
        let small = worker("desk", "opencode", "local:small");
        hub.board().choose(&id, &small).unwrap();
        hub.dispatcher.pass().unwrap();
        let task = hub.task(&id);
        assert_eq!(
            task.pick,
            Pick::Offered {
                worker: small.clone()
            }
        );
        assert_eq!(task.decision.unwrap().picked_by_person, Some(small));
        let err = hub
            .board()
            .choose(&id, &worker("desk", "x", "y"))
            .unwrap_err();
        assert!(err.contains("does not wait for you to choose"), "{err}");
    }

    #[test]
    fn a_task_whose_worker_is_busy_waits_and_is_offered_once_it_is_free() {
        let hub = Hub::new();
        let mut desk = runner("desk");
        desk.busy_harnesses.insert("claude-code".into(), 1);
        hub.runners.lock().unwrap().push(desk.clone());
        hub.picker(0.6, Vec::new());
        hub.answer(WORKER, "sonnet", 0.9);
        let id = hub.add();
        let other = hub.add();
        hub.dispatcher.pass().unwrap();
        assert_eq!(hub.task(&id).pick, Pick::Decided);
        desk.busy_harnesses.clear();
        // The same workers: what was decided stays.
        *hub.runners.lock().unwrap() = vec![desk];
        hub.dispatcher.pass().unwrap();
        assert!(matches!(hub.task(&id).pick, Pick::Offered { .. }));
        // One run of Claude Code at once: the next waits for it.
        assert_eq!(hub.task(&other).pick, Pick::Decided);
        assert_eq!(hub.task(&id).round, 0, "decided once");
    }

    #[test]
    fn the_answers_narrow_the_workers_the_decision_model_chooses_of() {
        let hub = Hub::new();
        hub.runners.lock().unwrap().push(runner("desk"));
        hub.picker(0.6, template("basic").unwrap());
        hub.answer("difficulty", "hard", 0.8);
        hub.answer("tools", "yes", 0.9);
        hub.answer(WORKER, "small", 0.9);
        let id = hub.add();
        hub.dispatcher.pass().unwrap();
        let requests = hub.stub.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "the questions, then the worker");
        let worker_asked: Value = serde_json::from_str(&requests[1]).unwrap();
        let options: Vec<&String> = worker_asked["questions"]["worker"]["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(
            options,
            ["claude-code on sonnet on desk"],
            "a hard task wants a strong model"
        );
        assert_eq!(worker_asked["state"]["answers"]["difficulty"], "hard");
        let task = hub.task(&id);
        let d = task.decision.unwrap();
        assert_eq!(
            (d.answers[0].question.as_str(), d.answers[0].answer.as_str()),
            ("difficulty", "hard")
        );
        assert!(matches!(task.pick, Pick::Offered { .. }));

        // An answer it is not sure of leaves the choice to a person.
        hub.answer("difficulty", "easy", 0.4);
        let unsure = hub.add();
        hub.dispatcher.pass().unwrap();
        let Pick::NeedsChoice { why } = hub.task(&unsure).pick else {
            panic!()
        };
        assert!(
            why.contains("difficulty is easy at 0.40, less than 0.5"),
            "{why}"
        );
    }

    #[test]
    fn a_change_to_the_picker_sends_what_was_decided_back_to_be_chosen_again() {
        let hub = Hub::new();
        let mut desk = runner("desk");
        desk.busy_harnesses.insert("claude-code".into(), 1);
        hub.runners.lock().unwrap().push(desk);
        hub.picker(0.6, Vec::new());
        hub.answer(WORKER, "sonnet", 0.9);
        let id = hub.add();
        hub.dispatcher.pass().unwrap();
        assert_eq!(hub.task(&id).pick, Pick::Decided);
        hub.picker(0.7, Vec::new());
        hub.dispatcher.pass().unwrap();
        let task = hub.task(&id);
        assert_eq!(task.pick, Pick::Decided);
        assert_eq!(task.round, 1, "chosen again");
        assert_eq!(hub.stub.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_task_given_back_or_whose_runner_went_away_is_chosen_again_without_it() {
        let hub = Hub::new();
        hub.runners
            .lock()
            .unwrap()
            .extend([runner("desk"), runner("laptop")]);
        hub.picker(0.6, Vec::new());
        hub.answer(WORKER, "sonnet on desk", 0.9);
        let id = hub.add();
        hub.dispatcher.pass().unwrap();
        assert_eq!(
            hub.task(&id).pick,
            Pick::Offered {
                worker: worker("desk", "claude-code", "sonnet")
            }
        );
        hub.board()
            .give_back(&id, "desk", "no key for the repository")
            .unwrap();
        hub.answer(WORKER, "sonnet", 0.9);
        hub.dispatcher.pass().unwrap();
        let task = hub.task(&id);
        assert_eq!(
            task.pick,
            Pick::Offered {
                worker: worker("laptop", "claude-code", "sonnet")
            }
        );
        assert_eq!(task.attempts.len(), 1);
        assert!(task.attempts[0].outcome.contains("desk gave it back"));
        let asked: Value =
            serde_json::from_str(hub.stub.requests.lock().unwrap().last().unwrap()).unwrap();
        assert!(
            asked["state"]["earlier_runs"][0]["outcome"]
                .as_str()
                .unwrap()
                .contains("no key"),
            "{asked}"
        );

        // The laptop goes away before it runs it: it waits for a runner.
        hub.runners.lock().unwrap().retain(|r| r.name != "laptop");
        hub.dispatcher.pass().unwrap();
        // Its offer taken back, it is chosen for again.
        hub.dispatcher.pass().unwrap();
        let Pick::Undecided { why: Some(why) } = hub.task(&id).pick else {
            panic!("{:?}", hub.task(&id).pick)
        };
        assert!(why.contains("no runner of its account"), "{why}");
    }
}
