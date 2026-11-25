//! Running a task once: the harnesses (`external`), how one run is started
//! (`spec`), the picker that chooses a harness and an LLM for it of what the
//! runner has (`picker`), gates, a git worktree per task (`worktree`), and
//! the router that runs what the picker chose (`router`). Runs are async, on
//! `runtime`.

pub mod external;
pub mod picker;
pub mod router;
pub use ensemble_core::runtime;
pub mod spec;
pub mod worktree;
