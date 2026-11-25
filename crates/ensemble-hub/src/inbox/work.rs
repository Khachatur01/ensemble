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

/// The heading of the section of an issue that says what must be true when
/// its task is done, any level, any case: `## Definition of done`.
pub const DEFINITION_OF_DONE: &str = "Definition of done";

/// The task `issue` becomes, for the person's runners: its body is the task,
/// without its definition of done, which is the task's. None when its body
/// has no such section, or an empty one: Ensemble does not make one up. It
/// has no gate: anyone who may write in the tracker writes its issues, so
/// only the task's account gives it one.
pub fn ticket_for(issue: &Issue) -> Option<Ticket> {
    let (text, done) = split_done(&issue.body)?;
    Some(Ticket {
        title: issue.title.clone(),
        definition_of_done: done,
        context: Some(format!(
            "Issue {} in the tracker, assigned to {}. Report what you did; Ensemble posts it on the issue.",
            issue.id, issue.assignee
        )),
        text: if text.is_empty() {
            issue.title.clone()
        } else {
            text
        },
        ..Ticket::default()
    })
}

/// `body` without its definition of done, and the section's text, trimmed:
/// the lines after its heading up to the next heading.
fn split_done(body: &str) -> Option<(String, String)> {
    let heading = |line: &str| line.trim_start().starts_with('#');
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.iter().position(|l| {
        heading(l)
            && l.trim()
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case(DEFINITION_OF_DONE)
    })?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| heading(l))
        .map_or(lines.len(), |n| start + 1 + n);
    let done = lines[start + 1..end].join("\n").trim().to_string();
    if done.is_empty() {
        return None;
    }
    let text = [&lines[..start], &lines[end..]].concat().join("\n");
    Some((text.trim().to_string(), done))
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
    fn an_issue_is_a_task_of_its_body_and_its_definition_of_done() {
        let t = ticket_for(&issue(
            &["ensemble"],
            "It loops.\n\n## Definition of done\n\nIt lands on the page it came from.\n\n## Notes\n\nSeen on Firefox.",
        ))
        .unwrap();
        assert_eq!(t.text, "It loops.\n\n## Notes\n\nSeen on Firefox.");
        assert_eq!(t.definition_of_done, "It lands on the page it came from.");
        assert_eq!(t.gate, None, "only the task's account gives one");
        assert!(
            t.context
                .unwrap()
                .contains("Issue 7 in the tracker, assigned to ana.p")
        );
        let t = ticket_for(&issue(&["ensemble"], "### definition of DONE\nIt works.")).unwrap();
        assert_eq!(
            (t.text.as_str(), t.definition_of_done.as_str()),
            ("Fix the redirect", "It works.")
        );
        assert_eq!(ticket_for(&issue(&["ensemble"], "It loops.")), None);
        assert_eq!(
            ticket_for(&issue(
                &["ensemble"],
                "It loops.\n## Definition of done\n\n## Notes"
            )),
            None,
            "an empty one is none"
        );
    }
}
