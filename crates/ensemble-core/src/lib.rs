//! What every other part of Ensemble shares: the error vocabulary (`port`), the
//! per-user state directory (`state`), the working folder and what a run changed
//! in it (`workspace`), the record of runs (`records`), the lock that keeps one
//! worker per directory (`lock`), dates in UTC (`time`), and the files a run on
//! a node of a hub server carries there and back (`snapshot`), and the folders
//! a node has worked in, by project (`folders`).

pub mod folders;
pub mod lock;
pub mod port;
pub mod records;
pub mod snapshot;
pub mod state;
pub mod time;
pub mod workspace;
