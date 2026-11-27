//! The task inbox. Issues live in a tracker and are assigned to people, never
//! to agents; an issue that carries the hub's label is for the runners of the
//! person it is assigned to. The hub's inbox (`relay`) makes it a task the
//! hub keeps for that person's account, in the project the issue names, which
//! the first of their runners to claim it runs, in a repository of that name
//! in its workspace; and the task's progress, questions and result are
//! written back on the issue. A run never calls a tracker itself.
//!
//! An **issue** is a tracker's item, assigned to a person; a **task** is a
//! runner's ticket (`ensemble_tasks`). What the inbox needs from a tracker is
//! the `IssueTracker` trait (`tracker`); local markdown files are the one
//! implementation so far (`local_markdown`), the task tracker's one integration.

pub mod local_markdown;
pub mod relay;
pub mod tracker;
pub mod work;

pub use local_markdown::LocalMarkdown;
pub use relay::ServerInbox;
pub use tracker::{Comment, Issue, IssueTracker, Status};

/// The tracker an integration of the task tracker reaches.
pub fn tracker_for(integration: &ensemble_config::Integration) -> Box<dyn IssueTracker + Send> {
    match &integration.kind {
        ensemble_config::IssueTracker::LocalMarkdown { dir } => Box::new(LocalMarkdown::new(dir)),
    }
}
