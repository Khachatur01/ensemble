//! The protocol's fixed parts: its version, the server's name, and the tool catalogue.

use super::super::delegation;
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "ensemble";

/// The shared tool catalogue, wrapped in MCP's envelope.
///
/// `agents` is the whole team.
pub(super) fn tool_definitions(agents: &[&str]) -> Value {
    Value::Array(
        delegation::tools(agents)
            .into_iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.parameters,
                })
            })
            .collect(),
    )
}
