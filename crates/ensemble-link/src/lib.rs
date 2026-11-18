//! A hub server and the link to it. A team runs one hub on a server. Client
//! agents reach it over MCP (`mcp`), signed in with an account's token, to
//! share to and recall from the organization's vault and to make and follow
//! tasks. Each member's runner opens WebSockets to it, signed in with a token:
//! to lease and run the tasks the hub holds for its account, and as a runner
//! that offers them, to run agents and serve models for others (`nodes`).
//!
//! The hub's users and their tokens are kept in `users`: state, not config.
//! What runner and hub say is in `protocol`; `server` is the hub's side, and
//! `client` the runner's, which keeps its tokens in `tokens`. The same port
//! serves a REST API (`api`, over a little `http`) for signing up and for
//! admins.

pub mod accounts;
mod api;
pub mod client;
mod http;
mod mcp;
mod nodes;
pub mod protocol;
pub mod server;
pub mod tokens;
pub mod users;

pub use client::{Link, Token};
pub use ensemble_store as store;
pub use server::Server;
pub use tokens::Tokens;
pub use users::{User, Users};
