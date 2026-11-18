//! An agent a node of a hub server offers. The task and a copy of the files
//! git tracks in the working folder go to the hub, which hands them to the
//! first of the agent's nodes that is connected and may be used; the node's
//! own agent works on them in a folder of its own, and what it changed comes
//! back and is written here. Gates, worktrees and commits all stay here: to
//! the rest of Ensemble this is an agent that changed some files.
//!
//! The agent that runs is the node owner's, from their helper team; nothing
//! of this agent's definition but its name goes to the node.

use super::AgentRun;
use ensemble_config::{AgentConfig, NODE_AGENT, NodeAdapter};
use ensemble_core::port::{Error, Usage};
use ensemble_core::runtime;
use ensemble_core::snapshot;
use ensemble_core::workspace::{Change, Changes};
use ensemble_link::client::{Connection, Failure};
use ensemble_link::protocol::{RUN, RUN_CANCEL, RUN_DONE, RUN_PROGRESS};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Time past the agent's `max_seconds` to wait for the node's own answer,
/// which carries what it did before its limit.
const GRACE: Duration = Duration::from_secs(30);

/// Run the task on a node, and write what the agent there changed into
/// `workdir`.
pub async fn run(
    agent: &AgentConfig,
    na: &NodeAdapter,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let backend = || format!("{NODE_AGENT} {}", na.nodes.join(" or "));
    let unsupported = |detail: String| Error::Unsupported {
        backend: backend(),
        detail,
    };
    let tokens = ensemble_link::Tokens::load().map_err(unsupported)?;
    let token = tokens.get(&na.hub).ok_or_else(|| {
        unsupported(format!(
            "no token for the hub {}; sign in first: ensemble runner login {} --user <name>",
            na.hub, na.hub
        ))
    })?;
    let files = runtime::blocking(|| snapshot::pack(workdir)).map_err(unsupported)?;
    progress(&format!(
        "{} · sending {} files to the hub",
        agent.name,
        files.len()
    ));
    let transport = |detail: String| Error::Transport {
        backend: backend(),
        detail,
    };
    let mut link = Connection::open(&na.hub, token).await.map_err(transport)?;
    let started = link
        .call(
            RUN,
            json!({
                "agent": na.agent,
                "nodes": na.nodes,
                "task": task,
                "context": context,
                "files": files,
            }),
        )
        .await
        .map_err(|f| match f {
            Failure::Hub(e) => unsupported(e),
            Failure::Broken(e) => transport(e),
        })?;
    let run = started["run"].as_str().unwrap_or("").to_string();
    let node = started["node"].as_str().unwrap_or("?").to_string();
    progress(&format!("{} · running on {node}", agent.name));
    let limit = Duration::from_secs(u64::from(agent.max_seconds));
    let deadline = tokio::time::Instant::now() + limit + GRACE;
    let unfinished = |why: String| AgentRun {
        text: format!("[{why} before {node} answered; the work is unfinished]"),
        model: na.label.clone(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
        changes: Changes::new(),
        commands: Vec::new(),
    };
    loop {
        let event = tokio::select! {
            () = runtime::until_set(cancel) => Err("cancelled".to_string()),
            () = tokio::time::sleep_until(deadline) => {
                Err(format!("stopped at the {}s time limit", agent.max_seconds))
            }
            event = link.next_event() => Ok(event),
        };
        let event = match event {
            Ok(event) => event
                .map_err(|e| transport(format!("the link to the hub broke during the run: {e}")))?,
            Err(why) => {
                let _ = link.send(RUN_CANCEL, json!({ "run": run })).await;
                link.close().await;
                return Ok(unfinished(why));
            }
        };
        if event.data["run"].as_str() != Some(run.as_str()) {
            continue;
        }
        match event.event.as_str() {
            RUN_PROGRESS => {
                if let Some(line) = event.data["line"].as_str() {
                    progress(&format!("{node}: {line}"));
                }
            }
            RUN_DONE => {
                link.close().await;
                return runtime::blocking(|| finished(&event.data, &node, na, workdir, backend()));
            }
            _ => {}
        }
    }
}

/// What the node said when the run ended, with its changes written here.
fn finished(
    done: &Value,
    node: &str,
    na: &NodeAdapter,
    workdir: &Path,
    backend: String,
) -> Result<AgentRun, Error> {
    if done["ok"] != json!(true) {
        let detail = format!(
            "{node}: {}",
            done["error"]
                .as_str()
                .unwrap_or("the run failed, and the node said no more")
        );
        return Err(if done["retryable"] == json!(true) {
            Error::Transport { backend, detail }
        } else {
            Error::Unsupported { backend, detail }
        });
    }
    let carried: Vec<snapshot::Change> =
        serde_json::from_value(done["changes"].clone()).map_err(|e| Error::Protocol {
            backend: backend.clone(),
            detail: format!("the changes {node} sent back: {e}"),
        })?;
    let mut changes = Changes::new();
    for c in &carried {
        let change = match (&c.data, workdir.join(&c.path).exists()) {
            (None, _) => Change::Deleted,
            (Some(_), true) => Change::Modified,
            (Some(_), false) => Change::Added,
        };
        changes.insert(c.path.clone(), change);
    }
    snapshot::apply(&carried, workdir).map_err(|e| Error::Protocol {
        backend,
        detail: format!("the changes {node} sent back: {e}"),
    })?;
    let count = |k: &str| done[k].as_u64().unwrap_or(0);
    Ok(AgentRun {
        text: done["text"].as_str().unwrap_or("").to_string(),
        model: done["model"]
            .as_str()
            .map_or_else(|| na.label.clone(), |m| format!("{m} on {node}")),
        usage: Usage {
            prompt_tokens: count("prompt_tokens") as u32,
            completion_tokens: count("completion_tokens") as u32,
        },
        steps: count("steps") as u32,
        tool_calls: count("tool_calls") as u32,
        changes,
        commands: done["commands"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Can the agent run now: is there a token for the hub, does the hub answer,
/// and is one of the nodes connected and offering the agent to this account?
pub fn health(na: &NodeAdapter) -> Result<(), Error> {
    let backend = || format!("{NODE_AGENT} {}", na.nodes.join(" or "));
    let tokens = ensemble_link::Tokens::load().map_err(|detail| Error::Unsupported {
        backend: backend(),
        detail,
    })?;
    let token = tokens.get(&na.hub).ok_or_else(|| Error::Unsupported {
        backend: backend(),
        detail: format!(
            "no token for the hub {}; ensemble runner login {} --user <name>",
            na.hub, na.hub
        ),
    })?;
    let listed = runtime::block_on(async {
        let mut link = Connection::open(&na.hub, token).await?;
        let listed = link
            .call(ensemble_link::protocol::NODES, json!({}))
            .await
            .map_err(|f| f.message());
        link.close().await;
        listed
    })
    .map_err(|detail| Error::Transport {
        backend: backend(),
        detail,
    })?;
    let nodes = listed["nodes"].as_array().cloned().unwrap_or_default();
    let offers = |n: &Value| {
        n["agents"]
            .as_array()
            .is_some_and(|a| a.iter().any(|a| a["name"] == json!(na.agent)))
    };
    if na
        .nodes
        .iter()
        .any(|id| nodes.iter().any(|n| n["node"] == json!(id) && offers(n)))
    {
        return Ok(());
    }
    let seen: Vec<&str> = nodes.iter().filter_map(|n| n["node"].as_str()).collect();
    Err(Error::Unsupported {
        backend: backend(),
        detail: format!(
            "none of {} is connected and offers `{}` to you; nodes you may use now: {}",
            na.nodes.join(", "),
            na.agent,
            if seen.is_empty() {
                "none".to_string()
            } else {
                seen.join(", ")
            }
        ),
    })
}
