//! A hub server and the link to it. A team runs one hub on a server; each
//! member's node opens one WebSocket to it, signs in with a token, and shares
//! to and recalls from the organization's vault there.
//!
//! The hub's users and their tokens are kept in `users`: state, not config.
//! What node and hub say is in `protocol`, and `server` is the hub's side.

pub mod protocol;
pub mod server;
pub mod users;

pub use server::Server;
pub use users::{User, Users};
