//! Running one agent once: the adapters that start each CLI and read what it
//! does (`external`), the `direct` adapter, gates, a git worktree per task
//! (`worktree`), and the router that picks an agent's adapter and keeps its
//! slots and budget (`router`).

pub mod external;
pub mod router;
pub mod worktree;
