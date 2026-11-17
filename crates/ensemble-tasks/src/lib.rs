//! Work the calling agent hands over: tasks (`tasks`), kept on the hub and
//! leased by the nodes that run them (`board`); what a node keeps of the runs
//! it does (`runs`); the queue that runs tasks and background delegations
//! within each agent's slots and budget (`queue`); the tools that delegate and
//! follow work (`delegation`); the one-worker-per-folder lock (`lock`); and
//! the review and merge of a worktree task's branch (`review`).

pub mod board;
pub mod delegation;
pub use ensemble_core::lock;
pub mod queue;
pub mod review;
pub mod runs;
pub mod tasks;
