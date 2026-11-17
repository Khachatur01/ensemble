//! Work the calling agent hands over: tasks as files (`tasks`), the queue that
//! runs them and background delegations within each agent's slots and budget
//! (`queue`), the tools that delegate and follow work (`delegation`), the
//! one-worker-per-folder lock (`lock`), and the review and merge of a worktree
//! task's branch (`review`).

pub mod delegation;
pub use ensemble_core::lock;
pub mod queue;
pub mod review;
pub mod tasks;
