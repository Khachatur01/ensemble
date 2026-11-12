//! The half of the inbox that lives where the work happens: an issue made into
//! a task in a folder's state directory, and what the task has to say back. An
//! embedded hub's inbox uses it in the same process as the tracker; on a team,
//! a member's node uses it, and the hub server writes what it says on the
//! issue.

use crate::tracker::{Issue, Status};
use ensemble_tasks::tasks::{Status as TaskStatus, Store, Ticket};

/// What a task has to say back on its issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// It stopped to ask this.
    Question(String),
    /// It ended, with this reply.
    Result { status: TaskStatus, reply: String },
}

/// Make `issue` a task in `store`: for the agent a `<label>:<agent>` label
/// names, when `agents` has it, else in the backlog. `assignee` is who the
/// issue is assigned to, and `place` where the folder is, both for the comment.
/// Returns the task's id and the comment to post on the issue.
pub fn make_task(
    store: &Store,
    issue: &Issue,
    label: &str,
    agents: &[String],
    assignee: &str,
    place: &str,
) -> Result<(String, String), String> {
    let prefix = format!("{label}:");
    let named: Vec<&str> = issue
        .labels
        .iter()
        .filter_map(|l| l.strip_prefix(&prefix))
        .collect();
    let agent = named
        .iter()
        .find(|a| agents.iter().any(|known| known == *a))
        .map(|a| a.to_string())
        .unwrap_or_default();
    let unknown: Vec<&str> = named
        .into_iter()
        .filter(|a| !agents.iter().any(|known| known == a))
        .collect();
    let ticket = Ticket {
        title: issue.title.clone(),
        agent: agent.clone(),
        context: Some(format!(
            "Issue {} in the tracker, assigned to {assignee}. Report what you did; Ensemble posts it on the issue.",
            issue.id
        )),
        text: if issue.body.is_empty() {
            issue.title.clone()
        } else {
            issue.body.clone()
        },
        ..Ticket::default()
    };
    let task = store
        .create(&ticket)
        .map_err(|e| format!("cannot write the task: {e}"))?;
    let mut comment = format!(
        "Picked up as task {task} {place}, {}.",
        if agent.is_empty() {
            "in the backlog, for the helper team to assign".to_string()
        } else {
            format!("for {agent}")
        }
    );
    if !unknown.is_empty() {
        comment.push_str(&format!(
            " The helper team has no agent called {}.",
            unknown.join(" or ")
        ));
    }
    Ok((task, comment))
}

/// What `task` has to say that was not said yet: its result once it has one,
/// else a question other than `asked`.
pub fn follow(store: &Store, task: &str, asked: Option<&str>) -> Option<Update> {
    if let Some(outcome) = store.outcome(task) {
        return Some(Update::Result {
            status: outcome.status,
            reply: outcome.reply().trim().to_string(),
        });
    }
    store
        .open_question(task)
        .filter(|q| asked != Some(q.as_str()))
        .map(Update::Question)
}

/// Pass the person's answer to the question `task` stopped to ask.
pub fn answer(store: &Store, task: &str, text: &str) -> Result<(), String> {
    store.answer(task, text)
}

/// The comment and the issue's status for an update of `task`.
pub fn on_issue(task: &str, update: &Update) -> (String, Status) {
    match update {
        Update::Question(q) => (
            format!(
                "The helper team asks:\n\n{}\n\nAnswer with a comment here.",
                q.trim()
            ),
            Status::Blocked,
        ),
        Update::Result { status, reply } => {
            let (word, issue) = match status {
                TaskStatus::Done => ("is done", Status::Review),
                TaskStatus::Failed => ("failed", Status::Blocked),
                TaskStatus::Cancelled => ("was cancelled", Status::Blocked),
            };
            (format!("Task {task} {word}:\n\n{reply}"), issue)
        }
    }
}
