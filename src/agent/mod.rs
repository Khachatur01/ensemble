//! Ensemble's own agent: a model, a tool loop, and a fenced-in project.
//!
//! An agent with `tools:` in `ensemble.yaml` is run here rather than as one
//! completion. [`run`] offers the agent's tools to its model, executes the calls
//! the model makes against the project directory ([`tools`], confined by
//! [`sandbox`]), feeds the results back, and repeats until the model answers
//! without asking for a tool. Any `openai-compatible-api` model can be the
//! brain; nothing else needs installing.

pub mod fetch;
pub mod run;
pub mod sandbox;
pub mod shell;
pub mod tools;

pub use run::{AgentRun, run};
