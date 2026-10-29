//! Ensemble: one interface for interacting with different models, delegating
//! work between them.
//!
//! ```text
//!              Harness (OpenCode, etc.)
//!                 |                 |
//!        selects  |                 |  or spawns
//!      ensemble/  v                 v
//!       manager   chat API     MCP over stdio   (src/harness/)
//!                     \         /
//!                      v       v
//!                      Ensemble                 (src/router.rs)
//!                          |
//!                          v      ModelPort     (src/port.rs)
//!                    /          \
//!                   v            v
//!             Claude Code   OpenAI-compatible   (src/backend/)
//!                 CLI             API
//! ```
//!
//! The layering is the design, and it is load-bearing in one direction: a
//! backend-specific detail may never travel upward. The router knows roles and
//! `ModelPort`; it does not know that one adapter spawns a process and the
//! other opens a socket. Both harness surfaces know the router; neither knows
//! there are backends at all.
//!
//! That rule is why the manager's loop lives in `harness/serve`, which may
//! depend on `harness/delegation`, rather than in the router, which may not.
//! The router's contribution is `Ensemble::turn`: one model call for one role,
//! with the caller's own conversation and tools. Who runs a tool, and what the
//! tools mean, is the harness's business.

pub mod agent;
pub mod backend;
pub mod config;
pub mod harness;
pub mod port;
pub mod router;
