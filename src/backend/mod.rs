//! Backend construction: config instance -> a live [`ModelPort`].
//!
//! This is the only place that knows which adapter a type id means. Everything
//! above it holds `Arc<dyn ModelPort>` and cannot tell the two apart.

pub mod claude_code;
pub mod openai;

use crate::config::{Backend, BackendSettings};
use crate::port::{Error, ModelPort};
use std::path::PathBuf;
use std::sync::Arc;

/// Build the adapter for one resolved backend.
///
/// `workdir` is the harness's project directory. It reaches the Claude Code
/// adapter as the default `--add-dir`, because a chat-completions request has
/// nowhere to carry one and a planner reading the wrong tree is worse than one
/// that reads nothing.
pub fn build(backend: &Backend, workdir: Option<PathBuf>) -> Result<Arc<dyn ModelPort>, Error> {
    match &backend.settings {
        BackendSettings::ClaudeCodeCli { .. } => Ok(Arc::new(claude_code::ClaudeCodeAdapter::new(
            backend,
            workdir,
            Box::new(claude_code::ProcessRunner),
        )?)),
        BackendSettings::OpenAiCompatible { .. } => Ok(Arc::new(openai::OpenAiAdapter::new(
            backend,
            Box::new(openai::UreqHttp),
        )?)),
    }
}
