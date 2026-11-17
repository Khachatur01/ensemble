//! The protocol's fixed parts: its version, the server's name, and the tool catalogue.

use ensemble_knowledge::KnowledgeAccess;
use ensemble_tasks::delegation;
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "ensemble";

/// The shared tool catalogue, wrapped in MCP's envelope.
///
/// `agents` is the whole helper team; `memory`, a hub's knowledge, adds its
/// tools.
pub(super) async fn tool_definitions(
    agents: &[&str],
    memory: Option<&dyn KnowledgeAccess>,
) -> Value {
    let knowledge = match memory {
        Some(m) => m.tools().await,
        None => Vec::new(),
    };
    let team = delegation::tools(agents)
        .into_iter()
        .map(|t| (t.name, t.description, t.parameters));
    let knowledge = knowledge
        .into_iter()
        .map(|t| (t.name, t.description, t.parameters));
    Value::Array(
        team.chain(knowledge)
            .map(|(name, description, parameters)| {
                json!({
                    "name": name,
                    "description": description,
                    "inputSchema": parameters,
                })
            })
            .collect(),
    )
}

/// What the server tells the calling agent about itself when it connects, so
/// that it needs no paragraph in its own instructions file: the helper team,
/// one agent per line, and how to hand work over.
pub(super) fn instructions(config: &ensemble_config::Config) -> String {
    let mut out = String::from(
        "This server gives you a helper team of agents to delegate work to. Each agent is for one kind of work:\n",
    );
    for agent in config.agents.values() {
        let purpose = agent
            .prompt
            .as_deref()
            .and_then(|p| p.lines().next())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or("(no description configured)");
        out.push_str(&format!("- {}: {purpose}\n", agent.name));
    }
    out.push_str(
        "\n`agents` lists them again. Hand work over with `delegate`, naming the agent and \
         giving it everything it needs: it cannot see this conversation. For work that may take more than a minute, \
         use `delegate` with `background`, or `submit` a task, then follow it with `status` and \
         `wait`. When the folder you work in holds several repositories, pass `repo` to work \
         in one of them.\n\n\
         The person you talk to owns the work and need not know who does what. Plan it with \
         them however suits the work, with or without an issue of theirs: then delegate parts \
         directly, make tasks, or both, or neither. Tasks are for work that goes on without \
         you: `submit` with an `agent` assigns one, `submit` with no agent puts it in the \
         backlog, and `assign` gives a backlog task to an agent. A task runs only once it has \
         an agent, and an agent may hand a task on to another. `status` says when no task is \
         active.",
    );
    out
}
