//! Ensemble: an agentic development system. A hub provides services (task
//! tracking, knowledge, accounts, integrations); nodes provide computing power
//! (agents, model servers), and work spreads across them. An agent CLI reaches
//! a node over MCP.
//!
//! ```text
//!   your agent (Claude Code, OpenCode, Codex, ...)
//!         |  MCP over stdio                 (ensemble-mcp)
//!         v
//!   ensemble mcp -> the node               (src/main.rs, src/daemon.rs)
//!         |  the tools: delegate, submit, ... (ensemble-tasks)
//!         v
//!   the router: agent -> adapter, model    (ensemble-agents: router)
//!         v
//!   claude -p, opencode run, codex exec, builtin, direct, node
//!                                          (ensemble-agents: external)
//! ```
//!
//! Each agent is a CLI the user already has, started headless in the project
//! with the task, or `builtin` (a loop in this binary), `direct` (one chat
//! request), or `node` (a run on another computer through a hub server).
//! Ensemble reads what the agent does as it works and reports what it
//! changed. The MCP session knows the router and nothing about the CLIs; the
//! router knows agents, and the adapters in `external` know how each CLI is
//! started and read. `ARCHITECTURE.md` is the map of the whole.

pub mod hooks;
pub mod remote;
pub mod setup;
pub mod teams;

pub use ensemble_agents::{external, router, runtime, worktree};
pub use ensemble_backends as backends;
pub use ensemble_config as config;
pub use ensemble_core::{folders, lock, port, records, snapshot, state, workspace};
pub use ensemble_inbox as inbox;
pub use ensemble_knowledge as knowledge;
pub use ensemble_link as link;
pub use ensemble_mcp as mcp;
pub use ensemble_node as node;
pub use ensemble_tasks::{board, delegation, plans, queue, review, runs, tasks};
