//! How an agent talks to Ensemble: MCP over stdio.
//!
//! The agent that calls Ensemble (Claude Code, OpenCode, ...) sees the helper
//! team as tools and decides what to delegate; Ensemble runs the agent and
//! returns the work. `delegation` holds the tools and what a failed delegation
//! says, and `queue` the work nobody waits for; both live in `ensemble_tasks`.
//! `mcp` is the JSON-RPC transport around them, and `serve` the worker with no
//! session.

pub use ensemble_tasks::{delegation, lock, queue};
pub mod mcp;
pub mod serve;
