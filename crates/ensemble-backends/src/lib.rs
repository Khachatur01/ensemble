//! Talking to the sources of the ANN pool: one chat request (`chat`), texts as vectors
//! (`embed`), asking a source which models it serves and what each is (`models`,
//! with what Ensemble knows of models before it asks, `catalog`), and choosing
//! the first usable model of a `try` list (`resolve`). Nothing here knows about
//! agents.

pub mod catalog;
pub mod chat;
pub mod embed;
pub mod models;
pub mod ops;
pub mod resolve;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;
