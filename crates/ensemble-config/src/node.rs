//! `node.yaml`: one node, a worker that gives this computer's power to its
//! owner and, when shared, to the people of a hub server: `~/.config/ensemble/
//! node.yaml`. A computer runs one node.
//!
//! A node says who it works for (its owner alone, or also the others whose
//! role has `compute`) and what it offers: agents of its teams that run tasks
//! sent to it, named `<team>/<agent>`, and the models a server here serves.
//! Its teams are the folders of its teams directory (`teams`). It is tied to
//! no project: your agent CLI's sessions reach its teams from any folder
//! through `ensemble mcp`, and a task from a hub goes to the folder of
//! its project's name where a session last ran.

use super::validate::explain;
use super::*;
use serde::Deserialize;

pub const NODE_FILENAME: &str = "node.yaml";
pub const NODE_SUPPORTED_VERSION: u32 = 1;

/// The JSON Schema for `node.yaml`.
pub const NODE_SCHEMA: &str = include_str!("../../../node.schema.json");

/// The first line of a node file, which points an editor at its schema.
pub const NODE_SCHEMA_URL_LINE: &str = "# yaml-language-server: $schema=https://raw.githubusercontent.com/Khachatur01/ensemble/master/node.schema.json";

/// The file `ensemble node init` writes.
pub const NODE_SAMPLE: &str = include_str!("node_sample.yaml");

/// One node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub version: u32,
    /// Its name, which a hub knows as `<owner>/<name>`.
    pub name: String,
    pub access: Access,
    /// The hub server it connects to, `ws://` or `wss://`, signed in to with
    /// `ensemble node login`. None: it uses no hub.
    pub hub: Option<String>,
    pub offers: Offers,
    /// How it may be managed.
    pub interfaces: Interfaces,
    pub source: PathBuf,
}

/// Whom a node works for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    /// Its owner alone.
    Private,
    /// Its owner, and everyone whose role on the hub has `compute`.
    Shared,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Private => "private",
            Access::Shared => "shared",
        }
    }
}

/// What a node offers through a hub. Each block is there or not: absent, it
/// is not offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offers {
    pub runs: Option<OfferedRuns>,
    pub models: Option<OfferedModels>,
}

/// Agents of the node's teams, which run tasks sent to the node in a folder
/// of their own and send back what they changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedRuns {
    /// Which agents of its teams others may run, as `<team>/<agent>`.
    pub agents: Vec<String>,
}

/// Models an OpenAI-compatible server on this computer serves, reached
/// through the hub at `/nodes/v1/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedModels {
    pub base_url: String,
    /// The variable holding the server's API key, when it needs one.
    pub api_key_env: Option<String>,
    pub timeout_seconds: u64,
    /// The models offered, as the server names them.
    pub names: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawNode {
    pub(crate) version: u32,
    pub(crate) name: String,
    pub(crate) access: Access,
    pub(crate) hub: Option<String>,
    pub(crate) offers: RawOffers,
    pub(crate) interfaces: Option<RawNodeInterfaces>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOffers {
    pub(crate) runs: Option<RawOfferedRuns>,
    pub(crate) models: Option<RawOfferedModels>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOfferedRuns {
    pub(crate) agents: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOfferedModels {
    pub(crate) base_url: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) timeout_seconds: u64,
    pub(crate) names: Vec<String>,
}

/// The node file in `dir`, usually `~/.config/ensemble`.
pub fn node_path(dir: &Path) -> PathBuf {
    dir.join(NODE_FILENAME)
}

/// The node file at `path`, or none when there is no such file.
pub fn load_node(path: &Path) -> Result<Option<Node>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Unreadable {
                path: path.to_path_buf(),
                detail: e.to_string(),
            });
        }
    };
    parse_node(&text, path).map(Some)
}

pub fn parse_node(text: &str, path: &Path) -> Result<Node, ConfigError> {
    let raw: RawNode = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: explain(&e),
    })?;
    let mut problems = Vec::new();
    if raw.version != NODE_SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands node version {NODE_SUPPORTED_VERSION}",
            raw.version
        ));
    }
    let name = raw.name.trim().to_string();
    if !valid_name(&name) {
        problems.push(format!(
            "name: '{name}' must be letters, digits, '-', '_' or '.', such as laptop or gpu-box"
        ));
    }
    let hub = raw.hub.as_ref().map(|h| h.trim().to_string());
    if let Some(h) = &hub
        && !(h.starts_with("ws://") || h.starts_with("wss://"))
    {
        problems.push(format!(
            "hub: '{h}' must start with ws:// or wss://, such as wss://hub.example.com"
        ));
    }
    if hub.is_none() && (raw.offers.runs.is_some() || raw.offers.models.is_some()) {
        problems
            .push("offers: a node offers what it has through a hub, and there is no `hub`".into());
    }
    let runs = raw.offers.runs.as_ref().map(|r| {
        let agents: Vec<String> = r.agents.iter().map(|a| a.trim().to_string()).collect();
        if agents.is_empty() {
            problems.push("offers.runs.agents: name at least one agent of its teams".into());
        }
        for a in agents.iter().filter(|a| crate::teams::team_of(a).is_none()) {
            problems.push(format!(
                "offers.runs.agents: '{a}' needs its team, as `<team>/<agent>`, such as web/{a}"
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for a in &agents {
            if a.is_empty() || !seen.insert(a) {
                problems.push(format!(
                    "offers.runs.agents: '{a}' is empty or listed twice"
                ));
            }
        }
        OfferedRuns { agents }
    });
    let models = raw.offers.models.as_ref().map(|m| {
        let base_url = m.base_url.trim().trim_end_matches('/').to_string();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            problems.push(format!(
                "offers.models.base_url: '{base_url}' must start with http:// or https://, such as http://127.0.0.1:11434/v1"
            ));
        }
        if m.timeout_seconds == 0 {
            problems.push("offers.models: `timeout_seconds` must be greater than 0".into());
        }
        let names: Vec<String> = m.names.iter().map(|n| n.trim().to_string()).collect();
        if names.is_empty() || names.iter().any(String::is_empty) {
            problems.push(
                "offers.models.names: name at least one model, as the server serves it".into(),
            );
        }
        OfferedModels {
            base_url,
            api_key_env: m.api_key_env.as_ref().map(|e| e.trim().to_string()),
            timeout_seconds: m.timeout_seconds,
            names,
        }
    });
    let interfaces = node_interfaces(raw.interfaces.as_ref(), &mut problems);
    if problems.is_empty() {
        Ok(Node {
            version: raw.version,
            name,
            access: raw.access,
            hub,
            offers: Offers { runs, models },
            interfaces,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

/// A node's name: what a hub's users write after `<owner>/`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}
