//! An issue made into a task of the person it is assigned to, and what the
//! task has to say back on the issue.

use super::tracker::{Comment, Issue, Status};
use ensemble_tasks::tasks::{Status as TaskStatus, Ticket};

/// Who the inbox's comments are by.
pub const AUTHOR: &str = "ensemble";

/// The first comment by `person` after the first `seen` comments: their reply
/// to a question.
pub fn reply_after(comments: &[Comment], seen: usize, person: &str) -> Option<String> {
    comments
        .iter()
        .skip(seen)
        .find(|c| c.author == person)
        .map(|c| c.text.clone())
}

/// What a task has to say back on its issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// It stopped to ask this.
    Question(String),
    /// It ended, with this reply.
    Result { status: TaskStatus, reply: String },
}

/// The task `issue` becomes, for the person's runners. It has no gate: anyone who may write in the tracker writes its issues, so only the
/// task's account gives it one.
pub fn ticket_for(issue: &Issue) -> Ticket {
    Ticket {
        title: issue.title.clone(),
        context: Some(format!(
            "Issue {} in the tracker, assigned to {}. Report what you did; Ensemble posts it on the issue.",
            issue.id, issue.assignee
        )),
        text: if issue.body.trim().is_empty() {
            issue.title.clone()
        } else {
            issue.body.clone()
        },
        ..Ticket::default()
    }
}

/// The comment and the issue's status for an update of `task`.
pub fn on_issue(task: &str, update: &Update) -> (String, Status) {
    match update {
        Update::Question(q) => (
            format!(
                "The task asks:\n\n{}\n\nAnswer with a comment here.",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(labels: &[&str], body: &str) -> Issue {
        Issue {
            id: "7".into(),
            title: "Fix the redirect".into(),
            assignee: "ana.p".into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            project: Some("shop".into()),
            status: Status::Open,
            body: body.into(),
        }
    }

    #[test]
    fn an_issue_is_a_task_of_its_body_and_without_one_of_its_title() {
        let t = ticket_for(&issue(&["ensemble"], "It loops."));
        assert_eq!(t.text, "It loops.");
        assert_eq!(t.gate, None, "only the task's account gives one");
        assert!(
            t.context
                .unwrap()
                .contains("Issue 7 in the tracker, assigned to ana.p")
        );
        let t = ticket_for(&issue(&["ensemble"], ""));
        assert_eq!(t.text, "Fix the redirect");
    }
}
