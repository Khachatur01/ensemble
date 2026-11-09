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

/// What the server tells the manager about itself when it connects, so that the
/// manager needs no paragraph in its own instructions file: the teams, one
/// line each, and how to hand work over. The agents of each team come from the
/// `agents` tool, so a large organization does not fill the manager's context.
pub(super) fn instructions(config: &crate::config::Config) -> String {
    let mut out = String::from(
        "This server gives you teams of agents to delegate work to. Each team is for one kind of work:\n",
    );
    for team in config.teams.values() {
        out.push_str(&format!(
            "- {}: {} ({} agents)\n",
            team.name,
            team.summary(),
            team.agents.len()
        ));
    }
    out.push_str(
        "\nCall `agents` to see who is in a team (`agents` with `team` lists one team). \
         Hand work over with `delegate`, naming the agent as team/agent and giving it everything \
         it needs: it cannot see this conversation. For work that may take more than a minute, \
         use `delegate` with `background`, or `submit` a task, then follow it with `status` and \
         `wait`. When the folder you work in holds several repositories, pass `repo` to work \
         in one of them.",
    );
    out
}
