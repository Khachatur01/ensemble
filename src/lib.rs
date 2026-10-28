//! Ensemble: one interface for interacting with different models, delegating
//! work between them.
//!
//! ```text
//!         Harness (OpenCode, etc.)
//!                   |
//!                   v          MCP over stdio  (src/harness/mcp.rs)
//!              Ensemble                        (src/router.rs)
//!                   |
//!                   v          ModelPort       (src/port.rs)
//!               /         \
//!              v           v
//!        Claude Code    OpenAI-compatible      (src/backend/)
//!            CLI              API
//! ```
//!
//! The layering is the design, and it is load-bearing in one direction: a
//! backend-specific detail may never travel upward. The router knows roles and
//! `ModelPort`; it does not know that one adapter spawns a process and the
//! other opens a socket. The MCP server knows the router; it does not know
//! there are backends at all.

pub mod backend;
pub mod config;
pub mod harness;
pub mod port;
pub mod router;
