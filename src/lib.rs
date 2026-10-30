//! Ensemble: a team of coding agents behind one MCP server.
//!
//! ```text
//!        Your agent (Claude Code, OpenCode, ...)  = the manager
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
//! (or, for a light `llm` agent, one chat request to an LLM backend),
//! started headless in the project with the task; Ensemble reads what it does
//! as it works and reports what it changed. The harness knows the router and
//! nothing about the CLIs; the router knows agents, and the adapters in
//! `external` know how each CLI is started and read.

pub mod config;
pub mod external;
pub mod harness;
pub mod llm_backend;
pub mod port;
pub mod router;
pub mod tasks;
pub mod workspace;
pub mod worktree;
