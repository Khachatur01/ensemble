//! A hub server and the link to it. A team runs one hub on a server; each
//! member's node opens one WebSocket to it, signs in with a token, and shares
//! to and recalls from the organization's vault there.
//!
//! The hub's users and their tokens are kept in `users`: state, not config.
//! What node and hub say is in `protocol`; `server` is the hub's side, and
//! `client` the node's, which keeps its tokens in `tokens`. The same port
//! serves a REST API (`api`, over a little `http`) for signing up and for
//! admins.

mod api;
pub mod client;
mod http;
mod nodes;
pub mod protocol;
pub mod server;
pub mod tokens;
pub mod users;

pub use client::RemoteMemory;
pub use server::Server;
pub use tokens::Tokens;
pub use users::{User, Users};
