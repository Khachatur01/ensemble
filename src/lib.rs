//! Ensemble: an agentic development system. A hub provides services (task
//! tracking, knowledge, accounts, integrations) that client agents reach over
//! MCP; runners provide computing power (agents, model servers) and work
//! through the hub's tasks.
//!
//! ```text
//!   client agents (Claude Code, OpenCode, Codex, ...)
//!         |  MCP over HTTP                  (ensemble-link: mcp)
//!         v
//!   the hub: knowledge, tasks, accounts    (ensemble-link, ensemble-knowledge)
//!         |  the link, a WebSocket the runner opens
//!         v
//!   ensemble runner serve                  (src/daemon.rs)
//!         |  a worker per repository of its workspace (ensemble-tasks: queue)
//!         v
//!   the router: agent -> adapter, model    (ensemble-agents: router)
//!         v
//!   claude -p, opencode run, codex exec, builtin, direct, node
//!                                          (ensemble-agents: external)
//! ```
//!
//! Each agent is a CLI the user already has, started headless in the
//! repository with the task, or `builtin` (a loop in this binary), `direct`
//! (one chat request), or `node` (a run on another computer through a hub
//! server). Ensemble reads what the agent does as it works and reports what
//! it changed. The worker knows the router and nothing about the CLIs; the
//! router knows agents, and the adapters in `external` know how each CLI is
//! started and read. `ARCHITECTURE.md` is the map of the whole.

pub mod remote;
pub mod setup;
pub mod teams;

pub use ensemble_agents::{external, router, runtime, worktree};
pub use ensemble_backends as backends;
pub use ensemble_config as config;
pub use ensemble_core::{lock, port, records, snapshot, state, workspace};
pub use ensemble_inbox as inbox;
pub use ensemble_knowledge as knowledge;
pub use ensemble_link as link;
pub use ensemble_node as node;
pub use ensemble_ops as ops;
pub use ensemble_tasks::{board, delegation, queue, runs, tasks};
