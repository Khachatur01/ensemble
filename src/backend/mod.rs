//! LlmBackend construction: config instance -> a live [`ModelPort`].
//!
//! This is the only place that knows which adapter a type id means. Everything
//! above it holds `Arc<dyn ModelPort>` and cannot tell adapters apart.

pub mod openai;

use crate::config::{BackendSettings, LlmBackend};
use crate::port::{Error, ModelPort};
use std::sync::Arc;

/// Build the adapter for one resolved backend.
pub fn build(backend: &LlmBackend) -> Result<Arc<dyn ModelPort>, Error> {
    match &backend.settings {
        BackendSettings::OpenAiCompatible { .. } => Ok(Arc::new(openai::OpenAiAdapter::new(
            backend,
            Box::new(openai::UreqHttp),
        )?)),
    }
}
