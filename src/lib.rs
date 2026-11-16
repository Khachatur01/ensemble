//! Ensemble: a helper team of coding agents behind one MCP server.
//!
//! ```text
//!        Your agent (Claude Code, OpenCode, ...)
//!                       |  MCP over stdio          (src/harness/)
//!                       v
//!                   Ensemble                      (src/router.rs)
//!                  /        \
//!                 v          v
//!       claude -p ...      opencode run ...       (src/external/)
//!                           |  provider from an LLM backend
//!                           v
//!                 OpenAI-compatible endpoint      (config: llm_backends)
//! ```
//!
//! Ensemble runs no agent loop itself. Each agent is a CLI the user already has
//! (or, for a light `direct` agent, one chat request to an LLM backend),
//! started headless in the project with the task; Ensemble reads what it does
//! as it works and reports what it changed. The harness knows the router and
//! nothing about the CLIs; the router knows agents, and the adapters in
//! `external` know how each CLI is started and read.

pub mod harness;
pub mod hooks;
pub mod remote;
pub mod setup;

pub use ensemble_agents::{external, router, worktree};
pub use ensemble_backends as backends;
pub use ensemble_config as config;
pub use ensemble_core::{port, records, state, workspace};
pub use ensemble_inbox as inbox;
pub use ensemble_knowledge as knowledge;
pub use ensemble_link as link;
pub use ensemble_tasks::{review, tasks, tracker};
