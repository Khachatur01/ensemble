//! The link between a runner and a hub: what both sides share. Each member's
//! runner opens WebSockets to the hub, signed in with a token: to lease and run
//! the tasks the hub holds for its account, and to say it is connected, with
//! its agents; and one without a token, to sign in
//! for one, with a password or a join code.
//!
//! What runner and hub say is in `protocol`, with bytes as text in `base64`;
//! `client` is the runner's side, which keeps its tokens in `tokens`. The
//! hub's side is `ensemble_hub`.

pub mod base64;
pub mod client;
pub mod loopback;
pub mod protocol;
pub mod relay;
pub mod tokens;

pub use client::{Link, Token};
pub use tokens::Tokens;
