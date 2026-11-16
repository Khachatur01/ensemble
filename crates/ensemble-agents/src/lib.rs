//! Running one agent once: the adapters that start each CLI and read what it
//! does (`external`), the `direct` and `builtin` adapters, gates, a git
//! worktree per task (`worktree`), and the router that picks an agent's
//! adapter and keeps its slots and budget (`router`). Runs are async, on
//! `runtime`.

pub mod external;
pub mod router;
pub use ensemble_core::runtime;
pub mod worktree;
