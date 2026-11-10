//! What every other part of Ensemble shares: the error vocabulary (`port`), the
//! per-user state directory (`state`), the working folder and what a run changed
//! in it (`workspace`), the record of runs (`records`), the lock that keeps one
//! worker per directory (`lock`), and dates in UTC (`time`).

pub mod lock;
pub mod port;
pub mod records;
pub mod state;
pub mod time;
pub mod workspace;
