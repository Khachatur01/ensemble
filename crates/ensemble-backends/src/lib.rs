//! Talking to LLM backends: one chat request (`chat`), and asking a backend
//! which models it serves (`models`). Nothing here knows about agents.

pub mod chat;
pub mod models;
