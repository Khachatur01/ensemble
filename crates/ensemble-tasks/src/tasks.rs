//! Tasks as files: tickets the calling agent, a person or an inbox writes,
//! that agents work through while a worker runs for the folder (an
//! `ensemble mcp --team` session, a node, or `ensemble serve`).
//!
//! A task is a directory under the state directory's `tasks/`, named by its id
//! (`0007-add-retry`), holding files a person can read, like a ticket and its
//! resolution:
//!
//! - `task.md`: YAML front matter (`title`, `agent`, and optionally `context`,
//!   `goal`, `constraints`, `files`, `done_when`, `after`, `gate`), then the
//!   task itself. Written once; nothing rewrites it.
//! - `result.md`: written once, when the task ends: front matter (`status`,
//!   `agent`, `started`, `ended`), then the agent's reply. A task with a
//!   `result.md` is over; delete it to run the task again.
//!
//! An agent that cannot go on without a decision ends its reply with
//! `NEEDS INPUT: <question>`. The question is added to `questions.md` beside
//! the ticket, and the task waits until it is answered; then
//! it runs again with the questions and answers so far.
//!
//! What a run does while it runs (its activity log, whether it was
//! interrupted) lives in `runs/<id>/` beside `tasks/`, which ignores itself
//! should the state directory be put in a repository.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const TASKS_DIR: &str = "tasks";
pub const RUNS_DIR: &str = "runs";
pub const TASK_FILE: &str = "task.md";
pub const RESULT_FILE: &str = "result.md";
pub const QUESTIONS_FILE: &str = "questions.md";
const STATE_FILE: &str = "state.json";
const LOG_FILE: &str = "log.txt";
const REPLY_FILE: &str = "reply.md";
/// When each agent with a budget started its recent runs, across sessions.
const USAGE_FILE: &str = "usage.json";
/// Who a task is assigned to now, beside its immutable `task.md`.
const ASSIGNEE_FILE: &str = "assignee.txt";
/// What agents handed on, in order.
const HANDOFFS_FILE: &str = "handoffs.md";

/// How an agent asks: a line of its reply starting with this.
pub const ASK_MARKER: &str = "NEEDS INPUT:";
const ANSWER_HEADING: &str = "### Answer";

/// What `ensemble init` writes in `tasks/`, so a person can write a task
/// without reading the README.
pub const TASKS_README: &str = "\
# Tasks

One directory per task, named by its id (`0001-add-retry`). Agents work
through them while `ensemble mcp` runs, oldest id first, each within its
agent's `max_concurrent`.

`task.md` is the ticket; write one by hand or let the manager `submit` it:

    ---
    title: Add retry to the fetcher
    agent: coder
    # The rest is optional.
    context: The fetcher is src/fetch.rs; it uses ureq.
    goal: A request survives one dropped connection.
    constraints: [No new dependency]
    files: [src/fetch.rs]
    done_when: [cargo test passes]
    after: [0001-add-ureq]     # tasks that must be done first
    repo: backend           # a folder of the working directory, when it holds several repos
    gate: { name: test, attempts: 3 }   # a check from `gates` in config.yaml
    ---
    Retry a failed request three times, waiting 1s, 2s, 4s.

If the agent stops to ask, its question is added to `questions.md`; answer it
with the manager's `answer` tool, or write `### Answer` and the answer under
it, and the task goes on.

`result.md` appears when the task ends, with its status (done, failed or
cancelled) and the agent's reply. Delete it to run the task again.

Live state and activity logs are in ../runs/, which git ignores.
";

/// A ticket's front matter, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    title: String,
    /// Its assignee. Without one the task is in the backlog.
    agent: Option<String>,
    context: Option<String>,
    goal: Option<String>,
    constraints: Option<Vec<String>>,
    files: Option<Vec<String>>,
    done_when: Option<Vec<String>>,
    after: Option<Vec<String>>,
    repo: Option<String>,
    gate: Option<GateRef>,
}

/// How an agent hands a task on: a line of its reply starting with this, then
/// the agent and what remains.
pub const HANDOFF_MARKER: &str = "HAND OFF:";

/// A task's gate: which of the config's `gates` its work must pass, and how
/// many runs of the agent it gets to pass it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct GateRef {
    pub name: String,
    /// Runs of the agent in all, the first included.
    pub attempts: u32,
}

/// One task, read from its `task.md`. The optional parts are empty when the
/// ticket does not have them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    /// Its assignee. Empty for a task in the backlog, which no agent works on
    /// until it is assigned.
    pub agent: String,
    /// Background for the agent, passed as the delegation's context.
    pub context: Option<String>,
    /// The outcome wanted, in a sentence.
    pub goal: Option<String>,
    /// What the agent must not do or change.
    pub constraints: Vec<String>,
    /// Where the work is.
    pub files: Vec<String>,
    /// How to tell the task is done.
    pub done_when: Vec<String>,
    /// Tasks that must be done before this one starts.
    pub after: Vec<String>,
    /// A path inside the working directory where the task works, when that
    /// holds several repositories.
    pub repo: Option<String>,
    pub gate: Option<GateRef>,
    pub text: String,
}

impl Ticket {
    /// What the agent is asked: the task, then its goal, constraints, files
    /// and acceptance criteria as sections it can check its work against.
    pub fn prompt(&self) -> String {
        let mut out = self.text.clone();
        if let Some(goal) = &self.goal {
            out.push_str(&format!("\n\nGoal: {goal}"));
        }
        for (heading, items) in [
            ("Constraints", &self.constraints),
            ("Files to work on", &self.files),
            ("Done when", &self.done_when),
        ] {
            if !items.is_empty() {
                out.push_str(&format!("\n\n{heading}:"));
                for item in items {
                    out.push_str(&format!("\n- {item}"));
                }
            }
        }
        out
    }
}

/// "3 more minutes".
pub fn minutes(seconds: u64) -> String {
    let m = seconds.div_ceil(60);
    format!("{m} more minute{}", if m == 1 { "" } else { "s" })
}

/// One line about a task found on disk: its id, agent, where it stands and its
/// title. `budget_wait` is the seconds until its agent may start another run,
/// when it may not now. `worker_elsewhere` says another process is working
/// through the tasks, so a task recorded as running is running, not
/// interrupted. The tool `status` and the commands `ensemble tasks` and
/// `ensemble task` all show a task this way.
pub fn entry_line(
    store: Option<&Store>,
    entry: &Entry,
    entries: &[Entry],
    budget_wait: Option<u64>,
    worker_elsewhere: bool,
) -> String {
    match entry {
        Entry::Task(t, Some(status)) => {
            format!("{} · {} · {} · {}", t.id, t.agent, status.as_str(), t.title)
        }
        Entry::Task(t, None) if t.agent.is_empty() => {
            format!("{} · - · backlog · {}", t.id, t.title)
        }
        Entry::Task(t, None) => {
            let recorded = store.and_then(|s| s.state(&t.id));
            let running = worker_elsewhere && recorded.as_deref() == Some("running");
            let interrupted = !running
                && recorded
                    .as_deref()
                    .is_some_and(|s| s == "interrupted" || s == "running");
            let blockers = waiting_for(t, entries);
            let question = store.and_then(|s| s.open_question(&t.id));
            let state = if let Some(q) = question {
                format!("needs input: {q}")
            } else if running {
                "running".into()
            } else if let Some(wait) = budget_wait {
                format!("queued, over its agent's budget for {}", minutes(wait))
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

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Done,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "done" => Some(Status::Done),
            "failed" => Some(Status::Failed),
            "cancelled" => Some(Status::Cancelled),
            _ => None,
        }
    }
}

/// A task's `result.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    /// The whole file, front matter and reply.
    pub text: String,
}

impl Outcome {
    /// The reply, without the front matter.
    pub fn reply(&self) -> &str {
        front_matter(&self.text).map_or(self.text.as_str(), |(_, reply)| reply)
    }
}

/// A task directory, as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A ticket, and its outcome once it has one.
    Task(Box<Ticket>, Option<Status>),
    /// A `task.md` that could not be read, and why. It never stops a scan.
    Unreadable { id: String, error: String },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Task(t, _) => &t.id,
            Entry::Unreadable { id, .. } => id,
        }
    }
}

/// The tasks and runs of the work done in one directory: `tasks/` and `runs/`
/// under its state directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    tasks: PathBuf,
    runs: PathBuf,
    /// Where usage counters are kept when not in `runs/`: the directory of
    /// the helper team, shared by every project that uses it.
    usage: Option<PathBuf>,
}

impl Store {
    /// The store in `dir`, a state directory. Nothing is created
    /// until something is written.
    pub fn new(dir: &Path) -> Self {
        Store {
            root: dir.to_path_buf(),
            tasks: dir.join(TASKS_DIR),
            runs: dir.join(RUNS_DIR),
            usage: None,
        }
    }

    /// Keep usage counters in `dir` instead of beside the runs.
    pub fn with_usage_dir(mut self, dir: PathBuf) -> Self {
        self.usage = Some(dir);
        self
    }

    fn usage_path(&self) -> PathBuf {
        self.usage.as_ref().unwrap_or(&self.runs).join(USAGE_FILE)
    }

    /// The state directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tasks_dir(&self) -> &Path {
        &self.tasks
    }

    /// Write `ticket` as a new task and return its id: the next number, then
    /// a slug of the title. The ticket's own `id` is not used.
    pub fn create(&self, ticket: &Ticket) -> io::Result<String> {
        let folder = &self.tasks;
        fs::create_dir_all(folder)?;
        let mut front = format!("title: {}\n", yaml_str(&ticket.title));
        if !ticket.agent.is_empty() {
            front.push_str(&format!("agent: {}\n", yaml_str(&ticket.agent)));
        }
        for (key, value) in [
            ("context", &ticket.context),
            ("goal", &ticket.goal),
            ("repo", &ticket.repo),
        ] {
            if let Some(value) = value {
                front.push_str(&format!("{key}: {}\n", yaml_str(value)));
            }
        }
        for (key, items) in [
            ("constraints", &ticket.constraints),
            ("files", &ticket.files),
            ("done_when", &ticket.done_when),
            ("after", &ticket.after),
        ] {
            if !items.is_empty() {
                let list = serde_json::to_string(items).expect("strings serialize");
                front.push_str(&format!("{key}: {list}\n"));
            }
        }
        if let Some(gate) = &ticket.gate {
            let gate = serde_json::to_string(gate).expect("a gate serializes");
            front.push_str(&format!("gate: {gate}\n"));
        }
        let body = format!("---\n{front}---\n{}\n", ticket.text.trim_end());
        let slug = slug(&ticket.title);
        let mut number = self.next_number();
        loop {
            let id = if slug.is_empty() {
                format!("{number:04}")
            } else {
                format!("{number:04}-{slug}")
            };
            // `create_dir` fails on an existing directory, so two writers
            // never share an id.
            match fs::create_dir(folder.join(&id)) {
                Ok(()) => {
                    write_atomically(&folder.join(&id).join(TASK_FILE), &body)?;
                    return Ok(id);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => number += 1,
                Err(e) => return Err(e),
            }
        }
    }

    /// The directory of task `id`. Ids are numbered across the folder, so an
    /// id names one task.
    fn dir(&self, id: &str) -> PathBuf {
        if valid_id(id) {
            return self.tasks.join(id);
        }
        // Not a task: a path that is not there.
        self.tasks.join("-").join("-")
    }

    /// One past the highest number any task id starts with.
    fn next_number(&self) -> u32 {
        dir_names(&self.tasks)
            .into_iter()
            .filter_map(|name| {
                let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u32>().ok()
            })
            .max()
            .map_or(1, |n| n + 1)
    }

    /// Before version 3 tasks lived in a folder per team, `tasks/<team>/<id>`.
    /// A folder under `tasks/` that is no task itself but holds tasks is one of
    /// those: its tasks are moved up, once, so none is lost. A task that is
    /// already there under the same id stays, and the old one is left where it
    /// is and said so.
    fn adopt_team_folders(&self) {
        for team in dir_names(&self.tasks) {
            let folder = self.tasks.join(&team);
            if folder.join(TASK_FILE).is_file() {
                continue;
            }
            for id in dir_names(&folder) {
                let old = folder.join(&id);
                if !old.join(TASK_FILE).is_file() {
                    continue;
                }
                let new = self.tasks.join(&id);
                if new.exists() {
                    eprintln!(
                        "ensemble: tasks/{team}/{id} was not moved to tasks/{id}: a task of that id is already there"
                    );
                    continue;
                }
                match fs::rename(&old, &new) {
                    Ok(()) => eprintln!("ensemble: moved tasks/{team}/{id} to tasks/{id}"),
                    Err(e) => {
                        eprintln!("ensemble: cannot move tasks/{team}/{id} to tasks/{id}: {e}")
                    }
                }
            }
            // An emptied team folder goes; one with anything else in it stays.
            let _ = fs::remove_dir(&folder);
        }
    }

    /// Every task, by id. A directory without a `task.md` is not a task yet.
    pub fn scan(&self) -> Vec<Entry> {
        self.adopt_team_folders();
        let mut ids: Vec<String> = dir_names(&self.tasks)
            .into_iter()
            .filter(|id| self.dir(id).join(TASK_FILE).is_file())
            .collect();
        ids.sort();
        ids.into_iter()
            .map(|id| match self.read(&id) {
                Ok(ticket) => {
                    let status = self.outcome(&id).map(|o| o.status);
                    Entry::Task(Box::new(ticket), status)
                }
                Err(error) => Entry::Unreadable { id, error },
            })
            .collect()
    }

    pub fn exists(&self, id: &str) -> bool {
        valid_id(id) && self.dir(id).join(TASK_FILE).is_file()
    }

    /// Read a ticket. The error names the file and what is wrong with it.
    pub fn read(&self, id: &str) -> Result<Ticket, String> {
        if !valid_id(id) {
            return Err(format!("'{id}' is not a task id"));
        }
        let dir = self.dir(id);
        let path = dir.join(TASK_FILE);
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut ticket = parse_ticket(id, &text).map_err(|e| format!("{}: {e}", path.display()))?;
        // A ticket from before version 3 names its agent as `team/agent`; the
        // team is gone, so the agent is the part after the slash.
        let plain = |agent: &str| agent.rsplit('/').next().unwrap_or(agent).to_string();
        ticket.agent = plain(&ticket.agent);
        // Who it is assigned to now, if it was assigned or handed on since.
        if let Ok(now) = fs::read_to_string(dir.join(ASSIGNEE_FILE))
            && !now.trim().is_empty()
        {
            ticket.agent = plain(now.trim());
        }
        Ok(ticket)
    }

    /// A task's `result.md`, once it has one.
    pub fn outcome(&self, id: &str) -> Option<Outcome> {
        if !valid_id(id) {
            return None;
        }
        let text = fs::read_to_string(self.dir(id).join(RESULT_FILE)).ok()?;
        let status = front_matter(&text)
            .and_then(|(front, _)| {
                front
                    .lines()
                    .find_map(|l| l.strip_prefix("status:"))
                    .and_then(|s| Status::parse(s.trim()))
            })
            // A result someone edited into nonsense still ends the task.
            .unwrap_or(Status::Failed);
        Some(Outcome { status, text })
    }

    /// Write a task's `result.md`, with `extra` front matter after the usual.
    /// It is written once: a second call for the same task leaves the first
    /// result in place.
    pub fn write_result(
        &self,
        id: &str,
        agent: &str,
        status: Status,
        started: SystemTime,
        extra: &[(&str, String)],
        reply: &str,
    ) -> io::Result<()> {
        let path = self.dir(id).join(RESULT_FILE);
        if path.exists() {
            return Ok(());
        }
        let extra: String = extra
            .iter()
            .map(|(key, value)| format!("{key}: {value}\n"))
            .collect();
        let text = format!(
            "---\nstatus: {}\nagent: {}\nstarted: {}\nended: {}\n{extra}---\n{}\n",
            status.as_str(),
            yaml_str(agent),
            utc(started),
            utc(SystemTime::now()),
            reply.trim_end()
        );
        write_atomically(&path, &text)
    }

    /// `runs/`, made on first use, with the `.gitignore` that keeps every
    /// run out of git.
    fn runs_dir(&self) -> io::Result<&Path> {
        fs::create_dir_all(&self.runs)?;
        let ignore = self.runs.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n")?;
        }
        Ok(&self.runs)
    }

    /// The run directory of `id`, made on first use.
    fn run_dir(&self, id: &str) -> io::Result<PathBuf> {
        let dir = self.runs_dir()?.join(id);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Add a line to a run's activity log.
    pub fn log(&self, id: &str, line: &str) {
        let append = || -> io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.run_dir(id)?.join(LOG_FILE))?;
            writeln!(f, "{line}")
        };
        if let Err(e) = append() {
            eprintln!("ensemble: cannot write the log of {id}: {e}");
        }
    }

    /// Record a run's state: `running`, or `interrupted` when the session
    /// ended under it.
    pub fn set_state(&self, id: &str, state: &str, started: SystemTime) {
        let write = || -> io::Result<()> {
            let value = json!({ "state": state, "started": utc(started), "updated": utc(SystemTime::now()) });
            write_atomically(&self.run_dir(id)?.join(STATE_FILE), &value.to_string())
        };
        if let Err(e) = write() {
            eprintln!("ensemble: cannot write the state of {id}: {e}");
        }
    }

    /// Add a question to the task's `questions.md`, and keep the reply that
    /// asked it for the run after the answer.
    pub fn ask(&self, id: &str, question: &str, reply: &str) -> io::Result<()> {
        let path = self.dir(id).join(QUESTIONS_FILE);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let n = text.matches("## Question ").count() + 1;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        write!(
            f,
            "## Question {n} ({})\n{}\n\n",
            utc(SystemTime::now()),
            question.trim()
        )?;
        write_atomically(&self.run_dir(id)?.join(REPLY_FILE), reply)
    }

    /// The question the task is waiting on, if one is not answered yet.
    pub fn open_question(&self, id: &str) -> Option<String> {
        if !valid_id(id) {
            return None;
        }
        let text = fs::read_to_string(self.dir(id).join(QUESTIONS_FILE)).ok()?;
        let (_, last) = text.rsplit_once("## Question ")?;
        if last.contains(ANSWER_HEADING) {
            return None;
        }
        let question = last.split_once('\n').map_or("", |(_, q)| q).trim();
        Some(question.to_string())
    }

    /// Answer the open question of a task.
    pub fn answer(&self, id: &str, answer: &str) -> Result<(), String> {
        if self.open_question(id).is_none() {
            return Err(format!("{id} has no question waiting for an answer."));
        }
        let path = self.dir(id).join(QUESTIONS_FILE);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        write!(f, "{ANSWER_HEADING}\n{}\n\n", answer.trim())
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The questions and answers so far, and the reply that last asked, for
    /// the run after an answer.
    pub fn conversation(&self, id: &str) -> Option<(String, String)> {
        let questions = fs::read_to_string(self.dir(id).join(QUESTIONS_FILE)).ok()?;
        let reply = fs::read_to_string(self.runs.join(id).join(REPLY_FILE)).unwrap_or_default();
        Some((questions, reply))
    }

    /// The state a run last recorded, if any.
    pub fn state(&self, id: &str) -> Option<String> {
        let text = fs::read_to_string(self.runs.join(id).join(STATE_FILE)).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        value["state"].as_str().map(str::to_string)
    }

    /// When each agent started its recent runs, in seconds since the epoch.
    /// Assign task `id` to `agent`, whoever has it now. The
    /// ticket itself is not rewritten: the assignee is kept beside it.
    pub fn assign(&self, id: &str, agent: &str) -> io::Result<()> {
        if !self.exists(id) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no task '{id}'"),
            ));
        }
        write_atomically(&self.dir(id).join(ASSIGNEE_FILE), agent.trim())
    }

    /// Note that `from` handed task `id` on to `to`, saying what remains, with
    /// what it had said.
    pub fn append_handoff(
        &self,
        id: &str,
        from: &str,
        to: &str,
        note: &str,
        reply: &str,
    ) -> io::Result<()> {
        let path = self.dir(id).join(HANDOFFS_FILE);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        write!(
            f,
            "## {from} handed this to {to}\n{}\n\nWhat {from} had said:\n{}\n\n",
            if note.trim().is_empty() {
                "(no note)"
            } else {
                note.trim()
            },
            reply.trim()
        )
    }

    /// What earlier agents handed on, for the next one.
    pub fn handoffs(&self, id: &str) -> Option<String> {
        fs::read_to_string(self.dir(id).join(HANDOFFS_FILE)).ok()
    }

    pub fn usage(&self) -> BTreeMap<String, Vec<u64>> {
        fs::read_to_string(self.usage_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save_usage(&self, usage: &BTreeMap<String, Vec<u64>>) {
        let write = || -> io::Result<()> {
            let text = serde_json::to_string(usage).expect("usage serializes");
            match &self.usage {
                Some(dir) => fs::create_dir_all(dir)?,
                None => {
                    self.runs_dir()?;
                }
            }
            write_atomically(&self.usage_path(), &text)
        };
        if let Err(e) = write() {
            eprintln!("ensemble: cannot record usage: {e}");
        }
    }

    /// The highest `adhoc-N` a run directory has, so a new session does not
    /// reuse one.
    pub fn last_adhoc(&self) -> u64 {
        dir_names(&self.runs)
            .iter()
            .filter_map(|n| n.strip_prefix("adhoc-")?.parse().ok())
            .max()
            .unwrap_or(0)
    }
}

/// The question an agent's reply asks, if it asks one: the last line that
/// starts with [`ASK_MARKER`].
pub fn question_in(reply: &str) -> Option<String> {
    reply
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(ASK_MARKER))
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
}

/// Where an agent's reply hands the task: the last line that starts with
/// [`HANDOFF_MARKER`], as the agent named and the note after it.
pub fn handoff_in(reply: &str) -> Option<(String, String)> {
    let rest = reply
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(HANDOFF_MARKER))?
        .trim();
    let (agent, note) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let note = note.trim().trim_start_matches(['—', '-', ':']).trim();
    (!agent.is_empty()).then(|| (agent.to_string(), note.to_string()))
}

/// How many tasks are active: assigned, not ended, and not waiting on an answer
/// or on a task that ended without being done.
pub fn active_tasks(entries: &[Entry], store: Option<&Store>) -> usize {
    entries
        .iter()
        .filter(|e| match e {
            Entry::Task(t, None) => {
                !t.agent.is_empty()
                    && store.is_none_or(|s| s.open_question(&t.id).is_none())
                    && !waiting_for(t, entries).iter().any(|w| {
                        w.ends_with("(failed)")
                            || w.ends_with("(cancelled)")
                            || w.ends_with("(no such task)")
                            || w.ends_with("(unreadable)")
                    })
            }
            _ => false,
        })
        .count()
}

/// A line saying the helper team has nothing active, and what is waiting for
/// the owner; `None` while something is.
pub fn idle_line(entries: &[Entry], store: Option<&Store>) -> Option<String> {
    if active_tasks(entries, store) > 0 {
        return None;
    }
    let unfinished: Vec<&Ticket> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Task(t, None) => Some(&**t),
            _ => None,
        })
        .collect();
    let backlog = unfinished.iter().filter(|t| t.agent.is_empty()).count();
    let asking = unfinished
        .iter()
        .filter(|t| store.is_some_and(|s| s.open_question(&t.id).is_some()))
        .count();
    let mut waiting = Vec::new();
    if backlog > 0 {
        waiting.push(format!("{backlog} in the backlog, to be assigned"));
    }
    if asking > 0 {
        waiting.push(format!("{asking} waiting for an answer"));
    }
    Some(format!(
        "Idle: no active tasks{}. The helper team waits for planning: tell your agent what to do next.",
        if waiting.is_empty() {
            String::new()
        } else {
            format!(" ({})", waiting.join(", "))
        }
    ))
}

/// What `ticket` still waits for: each task in its `after` that is not done,
/// and why. Empty when it may start.
pub fn waiting_for(ticket: &Ticket, entries: &[Entry]) -> Vec<String> {
    ticket
        .after
        .iter()
        .filter_map(|dep| {
            let why = match entries.iter().find(|e| e.id() == dep) {
                Some(Entry::Task(_, Some(Status::Done))) => return None,
                Some(Entry::Task(_, Some(status))) => status.as_str(),
                Some(Entry::Task(_, None)) => "not done yet",
                Some(Entry::Unreadable { .. }) => "unreadable",
                None => "no such task",
            };
            Some(format!("{dep} ({why})"))
        })
        .collect()
}

/// Tasks whose `after` leads back to themselves, which can never start.
pub fn cyclic(entries: &[Entry]) -> Vec<String> {
    let after = |id: &str| -> Vec<String> {
        match entries.iter().find(|e| e.id() == id) {
            Some(Entry::Task(t, _)) => t.after.clone(),
            _ => Vec::new(),
        }
    };
    entries
        .iter()
        .map(Entry::id)
        .filter(|start| {
            let mut seen = std::collections::BTreeSet::new();
            let mut todo = after(start);
            while let Some(id) = todo.pop() {
                if id == *start {
                    return true;
                }
                if seen.insert(id.clone()) {
                    todo.extend(after(&id));
                }
            }
            false
        })
        .map(str::to_string)
        .collect()
}

/// A task id is one directory name: no separators, no `..`.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\'])
}

fn dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect()
}

/// `---` front matter, then the rest.
fn front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest
        .find("\n---\n")
        .map(|i| (i, i + 5))
        .or_else(|| rest.find("\n---\r\n").map(|i| (i, i + 6)))
        .or_else(|| rest.strip_suffix("\n---").map(|r| (r.len(), rest.len())))?;
    Some((&rest[..end.0 + 1], &rest[end.1..]))
}

fn parse_ticket(id: &str, text: &str) -> Result<Ticket, String> {
    let (front, body) = front_matter(text)
        .ok_or("it must start with front matter between `---` lines (title, agent)")?;
    let front: FrontMatter = serde_norway::from_str(front).map_err(|e| e.to_string())?;
    if front.title.trim().is_empty() {
        return Err("`title` is empty".into());
    }
    let text = body.trim();
    if text.is_empty() {
        return Err("there is no task below the front matter".into());
    }
    let after = front.after.unwrap_or_default();
    if let Some(bad) = after.iter().find(|a| !valid_id(a)) {
        return Err(format!("`after`: '{bad}' is not a task id"));
    }
    if after.iter().any(|a| a == id) {
        return Err("`after` names the task itself".into());
    }
    if let Some(gate) = &front.gate {
        if gate.name.trim().is_empty() {
            return Err("`gate.name` is empty".into());
        }
        if gate.attempts == 0 {
            return Err("`gate.attempts` must be at least 1".into());
        }
    }
    let some = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    Ok(Ticket {
        id: id.to_string(),
        title: front.title.trim().to_string(),
        agent: front.agent.unwrap_or_default().trim().to_string(),
        context: some(front.context),
        goal: some(front.goal),
        constraints: front.constraints.unwrap_or_default(),
        files: front.files.unwrap_or_default(),
        done_when: front.done_when.unwrap_or_default(),
        repo: front.repo,
        after,
        gate: front.gate,
        text: text.to_string(),
    })
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

/// Write through a temporary file, so a reader never sees half of it.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

pub use ensemble_core::time::utc;

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;
    use std::time::{Duration, UNIX_EPOCH};

    /// A ticket with only what every ticket has.
    pub(crate) fn draft(title: &str, agent: &str, text: &str) -> Ticket {
        Ticket {
            title: title.into(),
            agent: agent.into(),
            text: text.into(),
            ..Ticket::default()
        }
    }

    #[test]
    fn a_created_ticket_reads_back_as_written() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let written = Ticket {
            context: Some("see \"src/fetch.rs\"\nline two".into()),
            goal: Some("Requests survive a flaky network.".into()),
            constraints: vec!["No new dependency".into(), "Keep: the API".into()],
            files: vec!["src/fetch.rs".into()],
            done_when: vec!["cargo test passes".into()],
            after: vec!["0000-setup".into()],
            gate: Some(GateRef {
                name: "test".into(),
                attempts: 3,
            }),
            ..draft(
                "Add retry: to the fetcher!",
                "coder",
                "Retry three times.\n\n---\nNot front matter.",
            )
        };
        let id = store.create(&written).unwrap();
        assert_eq!(id, "0001-add-retry-to-the-fetcher");
        let t = store.read(&id).unwrap();
        assert_eq!(
            t,
            Ticket {
                id: id.clone(),
                ..written
            }
        );
        assert_eq!(store.scan(), vec![Entry::Task(Box::new(t.clone()), None)]);
        assert_eq!(
            t.prompt(),
            "Retry three times.\n\n---\nNot front matter.\n\nGoal: Requests survive a flaky network.\n\nConstraints:\n- No new dependency\n- Keep: the API\n\nFiles to work on:\n- src/fetch.rs\n\nDone when:\n- cargo test passes"
        );
    }

    #[test]
    fn a_task_waits_for_every_task_in_after_to_be_done() {
        let task = |id: &str, after: &[&str], status: Option<Status>| {
            Entry::Task(
                Box::new(Ticket {
                    id: id.into(),
                    after: after.iter().map(|a| a.to_string()).collect(),
                    ..draft(id, "x", "t")
                }),
                status,
            )
        };
        let entries = vec![
            task("1", &[], Some(Status::Done)),
            task("2", &[], Some(Status::Failed)),
            task("3", &[], None),
            task("4", &["1", "2", "3", "9"], None),
            task("5", &["6"], None),
            task("6", &["7"], None),
            task("7", &["5"], None),
        ];
        let Entry::Task(four, _) = &entries[3] else {
            unreachable!()
        };
        assert_eq!(
            waiting_for(four, &entries),
            ["2 (failed)", "3 (not done yet)", "9 (no such task)"]
        );
        let Entry::Task(one, _) = &entries[0] else {
            unreachable!()
        };
        assert!(waiting_for(one, &entries).is_empty());
        assert_eq!(cyclic(&entries), ["5", "6", "7"]);
    }

    #[test]
    fn a_task_cannot_wait_for_itself_or_for_a_path() {
        assert!(
            parse_ticket(
                "0001-a",
                "---\ntitle: a\nagent: x\nafter: [0001-a]\n---\nt\n"
            )
            .unwrap_err()
            .contains("itself")
        );
        assert!(
            parse_ticket("0001-a", "---\ntitle: a\nagent: x\nafter: [../b]\n---\nt\n")
                .unwrap_err()
                .contains("not a task id")
        );
        assert!(
            parse_ticket(
                "0001-a",
                "---\ntitle: a\nagent: x\ngate: {name: t, attempts: 0}\n---\nt\n"
            )
            .unwrap_err()
            .contains("at least 1")
        );
        assert!(
            parse_ticket(
                "0001-a",
                "---\ntitle: a\nagent: x\ngate: {name: t}\n---\nt\n"
            )
            .unwrap_err()
            .contains("attempts")
        );
    }

    #[test]
    fn tasks_live_in_one_folder_and_ids_count_across_them() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let a = store.create(&draft("Wire it", "layout", "t")).unwrap();
        let b = store.create(&draft("Code it", "coder", "t")).unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("0001-wire-it", "0002-code-it"));
        assert!(tmp.0.join("tasks/0001-wire-it/task.md").is_file());
        assert!(tmp.0.join("tasks/0002-code-it/task.md").is_file());
        let agents: Vec<String> = store
            .scan()
            .iter()
            .map(|e| match e {
                Entry::Task(t, _) => t.agent.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(agents, ["layout", "coder"]);
        assert!(store.exists(&b));
        store.ask(&b, "which?", "reply").unwrap();
        assert_eq!(store.open_question(&b).as_deref(), Some("which?"));
        assert_eq!(store.open_question(&a), None);
    }

    #[test]
    fn tasks_of_the_old_team_folders_are_moved_up_once_and_their_agents_lose_the_team() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let write = |path: &str, agent: &str| {
            let dir = tmp.0.join("tasks").join(path);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(TASK_FILE),
                format!("---\ntitle: t\nagent: {agent}\n---\nDo it.\n"),
            )
            .unwrap();
        };
        write("software/0001-short", "coder");
        write("software/0002-full", "software/coder");
        write("hardware/0003-board", "hardware/layout");
        fs::write(
            tmp.0.join("tasks/hardware/0003-board").join(ASSIGNEE_FILE),
            "hardware/checker",
        )
        .unwrap();
        // One of that id is already in place: the old one stays where it is.
        write("0002-full", "coder");
        let ids: Vec<String> = store.scan().iter().map(|e| e.id().to_string()).collect();
        assert_eq!(ids, ["0001-short", "0002-full", "0003-board"]);
        assert!(tmp.0.join("tasks/0001-short/task.md").is_file());
        assert!(
            tmp.0.join("tasks/software/0002-full/task.md").is_file(),
            "the clash is left in place"
        );
        assert!(
            !tmp.0.join("tasks/hardware").exists(),
            "an emptied team folder goes"
        );
        let agent = |id: &str| store.read(id).unwrap().agent;
        assert_eq!(agent("0001-short"), "coder");
        assert_eq!(agent("0003-board"), "checker");
    }

    #[test]
    fn ids_count_up_past_the_highest_and_skip_a_taken_one() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        fs::create_dir_all(tmp.0.join("tasks/0007-by-hand")).unwrap();
        assert_eq!(store.create(&draft("Next", "a", "t")).unwrap(), "0008-next");
        fs::create_dir_all(tmp.0.join("tasks/0009-same")).unwrap();
        assert_eq!(store.create(&draft("Same", "a", "t")).unwrap(), "0010-same");
        assert_eq!(store.create(&draft("", "a", "t")).unwrap(), "0011");
    }

    #[test]
    fn a_hand_written_ticket_is_read_and_a_bad_one_does_not_stop_the_scan() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let write = |id: &str, text: &str| {
            fs::create_dir_all(tmp.0.join("tasks/default").join(id)).unwrap();
            fs::write(tmp.0.join("tasks/default").join(id).join(TASK_FILE), text).unwrap();
        };
        write(
            "a-good",
            "---\ntitle: Look\nagent: scout\n---\nFind the parser.\n",
        );
        write("b-no-front", "Find the parser.\n");
        write("c-typo", "---\ntitle: x\nagnet: scout\n---\nt\n");
        write("d-empty", "---\ntitle: x\nagent: scout\n---\n\n");
        fs::create_dir_all(tmp.0.join("tasks/e-not-yet")).unwrap();
        let entries = store.scan();
        assert_eq!(entries.len(), 4, "{entries:?}");
        assert!(matches!(&entries[0], Entry::Task(t, None) if t.text == "Find the parser."));
        let error = |i: usize| match &entries[i] {
            Entry::Unreadable { error, .. } => error.clone(),
            other => panic!("{other:?}"),
        };
        assert!(error(1).contains("front matter"), "{}", error(1));
        assert!(error(2).contains("agnet"), "{}", error(2));
        assert!(error(3).contains("no task"), "{}", error(3));
        assert!(error(1).contains("b-no-front/task.md"), "{}", error(1));
    }

    #[test]
    fn a_result_is_written_once_and_ends_the_task() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let id = store.create(&draft("Look", "scout", "t")).unwrap();
        let started = UNIX_EPOCH + Duration::from_secs(1_793_448_000);
        store
            .write_result(
                &id,
                "scout",
                Status::Failed,
                started,
                &[("attempts", "2".into())],
                "it broke\n",
            )
            .unwrap();
        store
            .write_result(&id, "scout", Status::Done, started, &[], "second")
            .unwrap();
        let out = store.outcome(&id).unwrap();
        assert_eq!(out.status, Status::Failed);
        assert!(
            out.text.contains("started: 2026-10-31T12:00:00Z"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Z\nattempts: 2\n---\n"), "{}", out.text);
        assert!(out.text.ends_with("---\nit broke\n"), "{}", out.text);
        assert!(matches!(
            &store.scan()[0],
            Entry::Task(_, Some(Status::Failed))
        ));
        assert!(
            !tmp.0
                .join("tasks/default")
                .join(&id)
                .join("result.tmp")
                .exists()
        );
    }

    #[test]
    fn runs_keep_a_log_and_a_state_and_ignore_themselves() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        store.log("adhoc-3", "scout · step 1");
        store.log("adhoc-3", "scout · step 2");
        store.set_state("0001-x", "interrupted", SystemTime::now());
        assert_eq!(
            fs::read_to_string(tmp.0.join("runs/adhoc-3/log.txt")).unwrap(),
            "scout · step 1\nscout · step 2\n"
        );
        assert_eq!(store.state("0001-x").as_deref(), Some("interrupted"));
        assert_eq!(
            fs::read_to_string(tmp.0.join("runs/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(store.last_adhoc(), 3);
    }

    #[test]
    fn an_id_cannot_leave_the_tasks_directory() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        assert!(!store.exists("../x"));
        assert!(store.outcome("..").is_none());
    }

    #[test]
    fn a_question_waits_for_its_answer_and_both_are_kept() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let id = store.create(&draft("Look", "scout", "t")).unwrap();
        assert_eq!(store.open_question(&id), None);
        assert!(store.answer(&id, "x").is_err());
        store
            .ask(
                &id,
                "Which parser?",
                "I found two.\nNEEDS INPUT: Which parser?",
            )
            .unwrap();
        assert_eq!(store.open_question(&id).as_deref(), Some("Which parser?"));
        store.answer(&id, "The new one.").unwrap();
        assert_eq!(store.open_question(&id), None);
        store.ask(&id, "Keep the old one?", "r2").unwrap();
        assert_eq!(
            store.open_question(&id).as_deref(),
            Some("Keep the old one?")
        );
        let (questions, reply) = store.conversation(&id).unwrap();
        assert!(questions.starts_with("## Question 1 ("), "{questions}");
        assert!(
            questions.contains("Which parser?\n\n### Answer\nThe new one.\n\n## Question 2 ("),
            "{questions}"
        );
        assert_eq!(reply, "r2");
    }

    #[test]
    fn the_last_marked_line_of_a_reply_is_its_question() {
        assert_eq!(question_in("done"), None);
        assert_eq!(
            question_in("x\n  NEEDS INPUT: a?\nNEEDS INPUT: b?\n\n[scout: 1 step(s)]").as_deref(),
            Some("b?")
        );
        assert_eq!(question_in("NEEDS INPUT:   "), None);
    }
}
