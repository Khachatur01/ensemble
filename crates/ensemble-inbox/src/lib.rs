//! The task inbox. Tasks live in a tracker and are assigned to people, never to
//! agents; an issue assigned to the hub's person that carries the hub's label
//! is for their helper team. The inbox turns such an issue into a task in the
//! folder its project maps to, and writes the task's progress, questions and
//! result back on the issue. The helper team never calls a tracker itself.
//!
//! An **issue** is a tracker's item, assigned to a person; a **task** is an
//! agent's ticket in a folder's state directory (`ensemble_tasks`). What the
//! inbox needs from a tracker is the `IssueTracker` trait (`tracker`); local
//! markdown files are the one implementation so far (`local_markdown`). The bridge
//! (`bridge`) turns issues into tasks and reports them back.

pub mod bridge;
pub mod local_markdown;
pub mod tracker;

pub use bridge::Inbox;
pub use local_markdown::LocalMarkdown;
pub use tracker::{Comment, Issue, IssueTracker, Status};

/// The tracker a hub's `tasks` block names.
pub fn tracker_for(tasks: &ensemble_config::Tasks) -> Box<dyn IssueTracker + Send> {
    match &tasks.tracker {
        ensemble_config::IssueTracker::LocalMarkdown { dir } => Box::new(LocalMarkdown::new(dir)),
    }
}
