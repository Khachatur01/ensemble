//! How an agent talks to Ensemble: MCP over stdio.
//!
//! The agent that calls Ensemble (Claude Code, OpenCode, ...) is the manager.
//! It sees the roles as tools and decides what to delegate; Ensemble runs the
//! role and returns the work. `delegation` holds what a role is and what a
//! failed delegation says; `mcp` is the JSON-RPC transport around it.

pub mod delegation;
pub mod mcp;
