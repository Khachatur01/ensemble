//! Tasks: tickets the calling agent, a person or the hub's inbox writes, that
//! agents work through. A task is kept on the hub ([`crate::board`]), not on
//! the computer that runs it, so any of the person's runners with its agent may
//! run it, and another takes it over when that runner stops. What a run does as
//! it goes, its log and its worktree, stays on the runner that runs it
//! ([`crate::runs`]).
//!
//! A ticket says what to do: a `title`, the `agent` it is for (none: the
//! backlog), the task itself, and optionally its `context`, `goal`,
//! `constraints`, `files`, `done_when`, the tasks it comes `after`, the `repo`
//! it works in and the `gate` its work must pass.
//!
//! An agent that cannot go on without a decision ends its reply with
//! `NEEDS INPUT: <question>`. The task waits until the question is answered;
//! then it runs again with the questions and answers so far.

use serde::{Deserialize, Serialize};

/// How an agent asks: a line of its reply starting with this.
pub const ASK_MARKER: &str = "NEEDS INPUT:";

/// How an agent hands a task on: a line of its reply starting with this, then
/// the agent and what remains.
pub const HANDOFF_MARKER: &str = "HAND OFF:";

/// A task's gate: which of the config's `gates` its work must pass, and how
/// many runs of the agent it gets to pass it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GateRef {
    pub name: String,
    /// Runs of the agent in all, the first included.
    pub attempts: u32,
}

/// One task's ticket. The optional parts are empty when it does not have
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Ticket {
    /// Given by the hub when the task is made.
    pub id: String,
    pub title: String,
    /// Its assignee, `<team>/<agent>`. Empty for a task in the backlog, which
    /// no agent works on until it is assigned.
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

    /// What is wrong with it as a new task, if anything: what the hub refuses
    /// before it keeps one.
    pub fn check(&self) -> Result<(), String> {
        if self.title.trim().is_empty() || self.title.contains('\n') {
            return Err("the title is one non-empty line".into());
        }
        if self.text.trim().is_empty() {
            return Err("there is no task: its text is empty".into());
        }
        if let Some(bad) = self.after.iter().find(|a| !valid_id(a)) {
            return Err(format!("`after`: '{bad}' is not a task id"));
        }
        if let Some(gate) = &self.gate {
            if gate.name.trim().is_empty() {
                return Err("`gate.name` is empty".into());
            }
            if gate.attempts == 0 {
                return Err("`gate.attempts` must be at least 1".into());
            }
        }
        Ok(())
    }
}

/// "3 more minutes".
pub fn minutes(seconds: u64) -> String {
    let m = seconds.div_ceil(60);
    format!("{m} more minute{}", if m == 1 { "" } else { "s" })
}

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
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

/// How a task ended, and what it said: front matter (`status`, `agent`,
/// `started`, `ended`, and what the run added, such as its `branch`), then
/// the agent's reply.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Outcome {
    pub status: Status,
    /// Front matter and reply.
    pub text: String,
}

impl Outcome {
    /// The reply, without the front matter.
    pub fn reply(&self) -> &str {
        front_matter(&self.text).map_or(self.text.as_str(), |(_, reply)| reply)
    }

    /// A value of its front matter, such as `branch`.
    pub fn field(&self, key: &str) -> Option<&str> {
        let (front, _) = front_matter(&self.text)?;
        front
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(str::trim)
    }
}

/// A task as the hub has it now.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Task {
    pub ticket: Ticket,
    /// How it ended, once it has.
    pub status: Option<Status>,
    /// The question it waits on, while nobody has answered it.
    pub question: Option<String>,
    /// The runner running it now.
    pub running_on: Option<String>,
    /// The runner that was running it when it stopped unfinished; a runner runs
    /// it again.
    pub interrupted_on: Option<String>,
    /// The runner whose folder its work is in, which alone may go on with it.
    pub pinned_to: Option<String>,
}

impl Task {
    pub fn id(&self) -> &str {
        &self.ticket.id
    }
}

/// A task with what it said: its result, its questions, what earlier agents
/// handed on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Detail {
    pub task: Task,
    pub outcome: Option<Outcome>,
    /// The questions and answers so far, and the reply that last asked.
    pub conversation: Option<(String, String)>,
    pub handoffs: Option<String>,
    /// Where its work was pushed when a run of it last stopped.
    pub progress: Option<crate::board::Progress>,
}

/// What `ticket` still waits for: each task in its `after` that is not done,
/// and why. Empty when it may start.
pub fn waiting_for(ticket: &Ticket, all: &[Task]) -> Vec<String> {
    ticket
        .after
        .iter()
        .filter_map(|dep| {
            let why = match all.iter().find(|t| t.id() == dep) {
                Some(Task {
                    status: Some(Status::Done),
                    ..
                }) => return None,
                Some(Task {
                    status: Some(status),
                    ..
                }) => status.as_str(),
                Some(_) => "not done yet",
                None => "no such task",
            };
            Some(format!("{dep} ({why})"))
        })
        .collect()
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

/// A task id is one word of letters, digits and dashes, such as
/// `0007-add-retry`: it names a branch and a folder.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

/// A YAML scalar that reads back as exactly `s`.
pub(crate) fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

/// Lowercase words of the title, joined by dashes, at most 40 characters.
pub(crate) fn slug(title: &str) -> String {
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

pub use ensemble_core::time::utc;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A ticket with only what every ticket has.
    pub(crate) fn draft(title: &str, agent: &str, text: &str) -> Ticket {
        Ticket {
            title: title.into(),
            agent: agent.into(),
            text: text.into(),
            ..Ticket::default()
        }
    }

    /// A task as the hub would list it, not started.
    fn queued(id: &str, after: &[&str]) -> Task {
        Task {
            ticket: Ticket {
                id: id.into(),
                after: after.iter().map(|a| a.to_string()).collect(),
                ..draft("t", "coder", "x")
            },
            status: None,
            question: None,
            running_on: None,
            interrupted_on: None,
            pinned_to: None,
        }
    }

    #[test]
    fn a_ticket_is_asked_with_its_sections() {
        let t = Ticket {
            goal: Some("Requests survive a flaky network.".into()),
            constraints: vec!["No new dependency".into()],
            files: vec!["src/fetch.rs".into()],
            done_when: vec!["cargo test passes".into()],
            ..draft("Add retry", "coder", "Retry three times.")
        };
        assert_eq!(
            t.prompt(),
            "Retry three times.\n\nGoal: Requests survive a flaky network.\n\nConstraints:\n- No new dependency\n\nFiles to work on:\n- src/fetch.rs\n\nDone when:\n- cargo test passes"
        );
    }

    #[test]
    fn a_ticket_that_cannot_be_a_task_says_why() {
        assert!(draft("Look", "scout", "Find it.").check().is_ok());
        for (ticket, said) in [
            (draft("", "scout", "t"), "title"),
            (draft("a\nb", "scout", "t"), "title"),
            (draft("Look", "scout", "  "), "no task"),
            (
                Ticket {
                    after: vec!["../x".into()],
                    ..draft("Look", "scout", "t")
                },
                "not a task id",
            ),
            (
                Ticket {
                    gate: Some(GateRef {
                        name: "test".into(),
                        attempts: 0,
                    }),
                    ..draft("Look", "scout", "t")
                },
                "at least 1",
            ),
        ] {
            let err = ticket.check().unwrap_err();
            assert!(err.contains(said), "{err}");
        }
    }

    #[test]
    fn a_task_waits_for_every_task_in_after_to_be_done() {
        let mut done = queued("1", &[]);
        done.status = Some(Status::Done);
        let mut failed = queued("2", &[]);
        failed.status = Some(Status::Failed);
        let all = vec![
            done,
            failed,
            queued("3", &[]),
            queued("4", &["1", "2", "3", "9"]),
        ];
        assert_eq!(
            waiting_for(&all[3].ticket, &all),
            ["2 (failed)", "3 (not done yet)", "9 (no such task)"]
        );
        assert!(waiting_for(&all[0].ticket, &all).is_empty());
    }

    #[test]
    fn an_outcome_has_its_reply_and_its_fields() {
        let o = Outcome {
            status: Status::Done,
            text: "---\nstatus: done\nbranch: ensemble/0001-a\n---\nAll done.\n".into(),
        };
        assert_eq!(o.reply(), "All done.\n");
        assert_eq!(o.field("branch"), Some("ensemble/0001-a"));
        assert_eq!(o.field("commit"), None);
    }

    #[test]
    fn an_id_is_one_word() {
        assert!(valid_id("0007-add-retry"));
        for bad in ["", "..", "../x", "a/b", "a b"] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert_eq!(
            slug("Add retry: to the fetcher!"),
            "add-retry-to-the-fetcher"
        );
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

    #[test]
    fn the_last_hand_off_line_names_the_agent_and_what_remains() {
        assert_eq!(handoff_in("done"), None);
        assert_eq!(
            handoff_in("x\nHAND OFF: web/reviewer — check the tests\n").unwrap(),
            ("web/reviewer".to_string(), "check the tests".to_string())
        );
        assert_eq!(
            handoff_in("HAND OFF: web/reviewer").unwrap(),
            ("web/reviewer".to_string(), String::new())
        );
    }
}
