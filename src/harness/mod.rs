//! How a harness talks to Ensemble.
//!
//! Two surfaces, and they are not alternatives of equal standing:
//!
//! * `serve` — an OpenAI-compatible HTTP API. The harness selects
//!   `ensemble/manager` as its model and Ensemble owns the manager, the model
//!   selection and the delegation behind it. Nothing is registered, and no
//!   harness-owned agent sits in front of it.
//! * `mcp` — a JSON-RPC server on stdio, offering the same tools to a harness
//!   agent that drives them itself. Optional, kept because it opens no port, is
//!   not OpenCode-specific, and works with a `claude-code-cli` manager.
//!
//! Both are built from `delegation`, so what a role is and what a failed
//! delegation says cannot drift between them. See `docs/HARNESS.md`.

pub mod delegation;
pub mod http;
pub mod mcp;
pub mod serve;
