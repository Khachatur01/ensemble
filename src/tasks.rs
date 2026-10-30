//! Tasks as files: tickets the manager or a person writes, that agents work
//! through while `ensemble mcp` runs.
//!
//! A task is a directory under `.ensemble/tasks/`, named by its id
//! (`0007-add-retry`), holding two files that are meant to be committed, like
//! a ticket and its resolution:
//!
//! - `task.md`: YAML front matter (`title`, `agent`, and optionally `context`,
//!   `goal`, `constraints`, `files`, `done_when`, `after`, `gate`), then the
//!   task itself. Written once; nothing rewrites it.
//! - `result.md`: written once, when the task ends: front matter (`status`,
//!   `agent`, `started`, `ended`), then the agent's reply. A task with a
//!   `result.md` is over; delete it to run the task again.
//!
//! What a run does while it runs (its activity log, whether it was
//! interrupted) is not worth committing, and lives in `.ensemble/runs/<id>/`,
//! which ignores itself.

use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TASKS_DIR: &str = "tasks";
pub const RUNS_DIR: &str = "runs";
pub const TASK_FILE: &str = "task.md";
pub const RESULT_FILE: &str = "result.md";
const STATE_FILE: &str = "state.json";
const LOG_FILE: &str = "log.txt";

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
    gate: { name: test, attempts: 3 }   # a check from config.yaml's gates
    ---
    Retry a failed request three times, waiting 1s, 2s, 4s.

`result.md` appears when the task ends, with its status (done, failed or
cancelled) and the agent's reply. Delete it to run the task again.

Live state and activity logs are in ../runs/, which git ignores.
";

/// A ticket's front matter, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    title: String,
    agent: String,
    context: Option<String>,
    goal: Option<String>,
    constraints: Option<Vec<String>>,
    files: Option<Vec<String>>,
    done_when: Option<Vec<String>>,
    after: Option<Vec<String>>,
    gate: Option<GateRef>,
}

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

    fn parse(s: &str) -> Option<Self> {
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

/// The tasks and runs of one project: `.ensemble/tasks/` and `.ensemble/runs/`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    tasks: PathBuf,
    runs: PathBuf,
}

impl Store {
    /// The store in `dir`, the project's `.ensemble/`. Nothing is created
    /// until something is written.
    pub fn new(dir: &Path) -> Self {
        Store {
            root: dir.to_path_buf(),
            tasks: dir.join(TASKS_DIR),
            runs: dir.join(RUNS_DIR),
        }
    }

    /// The project's `.ensemble/`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tasks_dir(&self) -> &Path {
        &self.tasks
    }

    /// Write `ticket` as a new task and return its id: the next number, then
    /// a slug of the title. The ticket's own `id` is not used.
    pub fn create(&self, ticket: &Ticket) -> io::Result<String> {
        fs::create_dir_all(&self.tasks)?;
        let mut front = format!(
            "title: {}\nagent: {}\n",
            yaml_str(&ticket.title),
            yaml_str(&ticket.agent)
        );
        for (key, value) in [("context", &ticket.context), ("goal", &ticket.goal)] {
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
            match fs::create_dir(self.tasks.join(&id)) {
                Ok(()) => {
                    write_atomically(&self.tasks.join(&id).join(TASK_FILE), &body)?;
                    return Ok(id);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => number += 1,
                Err(e) => return Err(e),
            }
        }
    }

    /// One past the highest number any task id starts with.
    fn next_number(&self) -> u32 {
        dir_names(&self.tasks)
            .iter()
            .filter_map(|name| {
                let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u32>().ok()
            })
            .max()
            .map_or(1, |n| n + 1)
    }

    /// Every task, by id. A directory without a `task.md` is not a task yet.
    pub fn scan(&self) -> Vec<Entry> {
        let mut ids = dir_names(&self.tasks);
        ids.sort();
        ids.into_iter()
            .filter(|id| self.tasks.join(id).join(TASK_FILE).is_file())
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
        valid_id(id) && self.tasks.join(id).join(TASK_FILE).is_file()
    }

    /// Read a ticket. The error names the file and what is wrong with it.
    pub fn read(&self, id: &str) -> Result<Ticket, String> {
        if !valid_id(id) {
            return Err(format!("'{id}' is not a task id"));
        }
        let path = self.tasks.join(id).join(TASK_FILE);
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        parse_ticket(id, &text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// A task's `result.md`, once it has one.
    pub fn outcome(&self, id: &str) -> Option<Outcome> {
        if !valid_id(id) {
            return None;
        }
        let text = fs::read_to_string(self.tasks.join(id).join(RESULT_FILE)).ok()?;
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
        let path = self.tasks.join(id).join(RESULT_FILE);
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

    /// The run directory of `id`, made on first use, with the `.gitignore`
    /// that keeps every run out of git.
    fn run_dir(&self, id: &str) -> io::Result<PathBuf> {
        let dir = self.runs.join(id);
        fs::create_dir_all(&dir)?;
        let ignore = self.runs.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n")?;
        }
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

    /// The state a run last recorded, if any.
    pub fn state(&self, id: &str) -> Option<String> {
        let text = fs::read_to_string(self.runs.join(id).join(STATE_FILE)).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        value["state"].as_str().map(str::to_string)
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
    if front.agent.trim().is_empty() {
        return Err("`agent` is empty".into());
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
        agent: front.agent.trim().to_string(),
        context: some(front.context),
        goal: some(front.goal),
        constraints: front.constraints.unwrap_or_default(),
        files: front.files.unwrap_or_default(),
        done_when: front.done_when.unwrap_or_default(),
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

/// `2026-10-31T12:00:00Z`.
pub fn utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;
    use std::time::Duration;

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
            fs::create_dir_all(tmp.0.join("tasks").join(id)).unwrap();
            fs::write(tmp.0.join("tasks").join(id).join(TASK_FILE), text).unwrap();
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
        assert!(!tmp.0.join("tasks").join(&id).join("result.tmp").exists());
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
    fn dates_are_utc() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }
}
