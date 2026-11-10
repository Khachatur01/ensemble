//! What every other part of Ensemble shares: the error vocabulary (`port`), the
//! per-user state directory (`state`), the working folder and what a run changed
//! in it (`workspace`), and the record of runs (`records`).

pub mod port;
pub mod records;
pub mod state;
pub mod workspace;
