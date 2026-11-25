//! The work a runner does: tasks (`tasks`), kept on the hub and leased by the
//! runners that run them (`board`); what a runner keeps of the runs it does
//! (`runs`); the worker that runs a repository's tasks on the harness and
//! the LLM the hub chose (`queue`); one run on a task and what it came to
//! (`delegation`); and the one-worker-per-folder lock (`lock`).

pub mod board;
pub mod delegation;
pub use ensemble_core::lock;
pub mod queue;
pub mod runs;
pub mod tasks;
