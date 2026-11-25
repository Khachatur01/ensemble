//! Running a task once: the harnesses (`external`), how one run is started
//! (`spec`), what the runner has to run it on and what is busy (`means`),
//! gates, a git worktree per task (`worktree`), and the router that runs the
//! pair the hub chose (`router`). Runs are async, on `runtime`.

pub mod external;
pub mod means;
pub mod router;
pub use ensemble_core::runtime;
pub mod spec;
pub mod worktree;
