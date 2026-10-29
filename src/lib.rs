//! Ensemble: one interface for interacting with different models, delegating
//! work between them.
//!
//! ```text
//!        Agent (Claude Code, OpenCode, ...)  = the manager
//!                       |  MCP over stdio          (src/harness/)
//!                       v
//!                   Ensemble                      (src/router.rs)
//!                  /        \
//!                 v          v
//!       agent loop           one completion       (src/agent/)
//!    (tools in the project)  (think-only role)
//!                 \          /
//!                  v        v
//!                  ModelPort                      (src/port.rs)
//!                  /        \
//!                 v          v
//!       OpenAI-compatible   Claude Code           (src/backend/)
//!            API               CLI
//! ```
//!
//! The layering is the design: a backend-specific detail may never travel
//! upward. The router knows roles and `ModelPort`; it does not know that one
//! adapter spawns a process and the other opens a socket. The harness knows the
//! router and nothing about backends. The agent loop knows `ModelPort` and the
//! project directory, and nothing about which model is behind it.

pub mod agent;
pub mod backend;
pub mod config;
pub mod harness;
pub mod port;
pub mod router;
