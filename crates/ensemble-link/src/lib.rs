//! The link between a runner and a hub: what both sides share. Each member's
//! runner opens WebSockets to the hub, signed in with a token: to lease and run
//! the tasks the hub holds for its account, and as a runner that offers them,
//! to run agents and serve models for others.
//!
//! What runner and hub say is in `protocol`; `client` is the runner's side,
//! which keeps its tokens in `tokens`. The hub's side is `ensemble_hub`.

pub mod client;
pub mod protocol;
pub mod tokens;

pub use client::{Link, Token};
pub use tokens::Tokens;
