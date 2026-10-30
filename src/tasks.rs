//! Tasks as files: tickets the manager or a person writes, that agents work
//! through while `ensemble mcp` runs.
//!
//! A task is a directory under `.ensemble/tasks/`, named by its id
//! (`0007-add-retry`), holding two files that are meant to be committed, like
//! a ticket and its resolution:
//!
//! - `task.md`: YAML front matter (`title`, `agent`, optional `context`), then
//!   the task itself. Written once; nothing rewrites it.
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
    context: The fetcher is src/fetch.rs.   # optional
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
}

/// One task, read from its `task.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    pub agent: String,
    pub context: Option<String>,
    pub text: String,
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
    Task(Ticket, Option<Status>),
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
    tasks: PathBuf,
    runs: PathBuf,
}

impl Store {
    /// The store in `dir`, the project's `.ensemble/`. Nothing is created
    /// until something is written.
    pub fn new(dir: &Path) -> Self {
        Store {
            tasks: dir.join(TASKS_DIR),
            runs: dir.join(RUNS_DIR),
        }
    }

    pub fn tasks_dir(&self) -> &Path {
        &self.tasks
    }

    /// Write a new ticket and return its id: the next number, then a slug of
    /// the title.
    pub fn create(
        &self,
        title: &str,
        agent: &str,
        context: Option<&str>,
        text: &str,
    ) -> io::Result<String> {
        fs::create_dir_all(&self.tasks)?;
        let mut front = format!("title: {}\nagent: {}\n", yaml_str(title), yaml_str(agent));
        if let Some(context) = context {
            front.push_str(&format!("context: {}\n", yaml_str(context)));
        }
        let body = format!("---\n{front}---\n{}\n", text.trim_end());
        let slug = slug(title);
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
                    Entry::Task(ticket, status)
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

    /// Write a task's `result.md`. It is written once: a second call for the
    /// same task leaves the first result in place.
    pub fn write_result(
        &self,
        id: &str,
        agent: &str,
        status: Status,
        started: SystemTime,
        reply: &str,
    ) -> io::Result<()> {
        let path = self.tasks.join(id).join(RESULT_FILE);
        if path.exists() {
            return Ok(());
        }
        let text = format!(
            "---\nstatus: {}\nagent: {}\nstarted: {}\nended: {}\n---\n{}\n",
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
    Ok(Ticket {
        id: id.to_string(),
        title: front.title.trim().to_string(),
        agent: front.agent.trim().to_string(),
        context: front.context.filter(|c| !c.trim().is_empty()),
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

    #[test]
    fn a_created_ticket_reads_back_as_written() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        let id = store
            .create(
                "Add retry: to the fetcher!",
                "coder",
                Some("see \"src/fetch.rs\"\nline two"),
                "Retry three times.\n\n---\nNot front matter.",
            )
            .unwrap();
        assert_eq!(id, "0001-add-retry-to-the-fetcher");
        let t = store.read(&id).unwrap();
        assert_eq!(t.title, "Add retry: to the fetcher!");
        assert_eq!(t.agent, "coder");
        assert_eq!(t.context.as_deref(), Some("see \"src/fetch.rs\"\nline two"));
        assert_eq!(t.text, "Retry three times.\n\n---\nNot front matter.");
        assert_eq!(store.scan(), vec![Entry::Task(t, None)]);
    }

    #[test]
    fn ids_count_up_past_the_highest_and_skip_a_taken_one() {
        let tmp = TempDir::new();
        let store = Store::new(&tmp.0);
        fs::create_dir_all(tmp.0.join("tasks/0007-by-hand")).unwrap();
        assert_eq!(store.create("Next", "a", None, "t").unwrap(), "0008-next");
        fs::create_dir_all(tmp.0.join("tasks/0009-same")).unwrap();
        assert_eq!(store.create("Same", "a", None, "t").unwrap(), "0010-same");
        assert_eq!(store.create("", "a", None, "t").unwrap(), "0011");
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
        let id = store.create("Look", "scout", None, "t").unwrap();
        let started = UNIX_EPOCH + Duration::from_secs(1_793_448_000);
        store
            .write_result(&id, "scout", Status::Failed, started, "it broke\n")
            .unwrap();
        store
            .write_result(&id, "scout", Status::Done, started, "second")
            .unwrap();
        let out = store.outcome(&id).unwrap();
        assert_eq!(out.status, Status::Failed);
        assert!(
            out.text.contains("started: 2026-10-31T12:00:00Z"),
            "{}",
            out.text
        );
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
