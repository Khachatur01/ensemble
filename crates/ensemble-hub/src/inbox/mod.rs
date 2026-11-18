//! The task inbox. Issues live in a tracker and are assigned to people, never
//! to agents; an issue that carries the hub's label is for the helper teams of
//! the person it is assigned to. The hub's inbox (`relay`) makes it a task the
//! hub keeps for that person's account, in the project the issue names, which
//! the first of their runners with the agent and a repository of that name in
//! its workspace runs; and
//! the task's progress, questions and result are written back on the issue.
//! The helper team never calls a tracker itself.
//!
//! An **issue** is a tracker's item, assigned to a person; a **task** is an
//! agent's ticket (`ensemble_tasks`). What the inbox needs from a tracker is
//! the `IssueTracker` trait (`tracker`); local markdown files are the one
//! implementation so far (`local_markdown`).

pub mod local_markdown;
pub mod relay;
pub mod tracker;
pub mod work;

pub use local_markdown::LocalMarkdown;
pub use relay::ServerInbox;
pub use tracker::{Comment, Issue, IssueTracker, Status};

/// The tracker a hub's `tasks` block names.
pub fn tracker_for(tasks: &ensemble_config::Tasks) -> Box<dyn IssueTracker + Send> {
    match &tasks.tracker {
        ensemble_config::IssueTracker::LocalMarkdown { dir } => Box::new(LocalMarkdown::new(dir)),
    }
}
