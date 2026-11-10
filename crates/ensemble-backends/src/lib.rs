//! Talking to LLM backends: one chat request (`chat`), asking a backend which
//! models it serves (`models`), and choosing the first usable model of a `try`
//! list (`resolve`). Nothing here knows about agents.

pub mod chat;
pub mod models;
pub mod resolve;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;
