//! What every other part of Ensemble shares: the error vocabulary (`port`), the
//! per-user state directory (`state`), the working folder and what a run changed
//! in it (`workspace`), the record of runs (`records`), the lock that keeps one
//! worker per directory (`lock`), dates in UTC (`time`), the files a run on a
//! node of a hub server carries there and back (`snapshot`), and the async
//! runtime with its bridges to code that blocks (`runtime`).

pub mod lock;
pub mod port;
pub mod records;
pub mod runtime;
pub mod snapshot;
pub mod state;
pub mod time;
pub mod workspace;
