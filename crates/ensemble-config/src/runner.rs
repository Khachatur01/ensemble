//! `runner.yaml`: one runner, a worker that gives this computer's power to its
//! owner and, when it works for the hub, to the people of a hub server:
//! `~/.config/ensemble/runner/runner.yaml`. A computer runs one runner.
//!
//! A runner says who it works for (`works_for`: its owner alone, or also the
//! accounts of its hub whose role has `compute`, with the agents of its teams
//! that run on `llm_backends`), where it works (its workspace, a folder of git
//! repositories, each a project named for its folder, where it clones the
//! repository of a task whose project it has no copy of), and what else it
//! offers: the models a server here serves. Its teams are the folders of its
//! teams directory (`teams`).

use super::validate::explain;
use super::*;
use serde::Deserialize;

pub const RUNNER_FILENAME: &str = "runner.yaml";
pub const RUNNER_SUPPORTED_VERSION: u32 = 1;

/// The JSON Schema for `runner.yaml`.
pub const RUNNER_SCHEMA: &str = include_str!("../../../runner.schema.json");

/// The first line of a runner file, which points an editor at its schema.
pub const RUNNER_SCHEMA_URL_LINE: &str = "# yaml-language-server: $schema=https://raw.githubusercontent.com/Khachatur01/ensemble/master/runner.schema.json";

/// The file `ensemble-runner init` writes.
pub const RUNNER_SAMPLE: &str = include_str!("runner_sample.yaml");

/// One runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub version: u32,
    /// Its name, which a hub knows as `<owner>/<name>`.
    pub name: String,
    pub works_for: WorksFor,
    /// The folder it works in: each git repository directly inside it is a
    /// project, named for its folder, whose tasks the runner runs; it clones
    /// a task's repository here for a project it has none of.
    pub workspace: PathBuf,
    /// The hub server it connects to, `ws://` or `wss://`, signed in to with
    /// `ensemble-runner login`. None: it uses no hub.
    pub hub: Option<String>,
    pub offers: Offers,
    /// How it may be managed.
    pub interfaces: Interfaces,
    pub source: PathBuf,
}

/// Whom a runner works for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorksFor {
    /// Its owner alone: the tasks of its owner's account.
    Owner,
    /// Its owner, and every account of its hub whose role has `compute`: their
    /// tasks that no runner of their own takes, with the agents of its teams
    /// on `llm_backends` alone (`AgentConfig::lendable`), and its models.
    /// Connecting it needs `host`.
    Hub,
}

impl WorksFor {
    pub fn as_str(self) -> &'static str {
        match self {
            WorksFor::Owner => "owner",
            WorksFor::Hub => "hub",
        }
    }
}

/// What else a runner offers through a hub. Each block is there or not:
/// absent, it is not offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offers {
    pub models: Option<OfferedModels>,
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
pub(crate) struct RawRunner {
    pub(crate) version: u32,
    pub(crate) name: String,
    pub(crate) works_for: WorksFor,
    pub(crate) workspace: String,
    pub(crate) hub: Option<String>,
    pub(crate) offers: RawOffers,
    pub(crate) interfaces: Option<RawRunnerInterfaces>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOffers {
    pub(crate) models: Option<RawOfferedModels>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOfferedModels {
    pub(crate) base_url: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) timeout_seconds: u64,
    pub(crate) names: Vec<String>,
}

/// The runner file in `dir`, usually `~/.config/ensemble/runner`.
pub fn runner_path(dir: &Path) -> PathBuf {
    dir.join(RUNNER_FILENAME)
}

/// The runner file at `path`, or none when there is no such file.
pub fn load_runner(path: &Path) -> Result<Option<Runner>, ConfigError> {
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
    parse_runner(&text, path).map(Some)
}

pub fn parse_runner(text: &str, path: &Path) -> Result<Runner, ConfigError> {
    let raw: RawRunner = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: explain(&e),
    })?;
    let mut problems = Vec::new();
    if raw.version != RUNNER_SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands runner version {RUNNER_SUPPORTED_VERSION}",
            raw.version
        ));
    }
    let name = raw.name.trim().to_string();
    if !valid_name(&name) {
        problems.push(format!(
            "name: '{name}' must be letters, digits, '-', '_' or '.', such as laptop or gpu-box"
        ));
    }
    let workspace = workspace_path(raw.workspace.trim(), path).unwrap_or_else(|e| {
        problems.push(format!("workspace: {e}"));
        PathBuf::new()
    });
    let hub = raw.hub.as_ref().map(|h| h.trim().to_string());
    if let Some(h) = &hub
        && !(h.starts_with("ws://") || h.starts_with("wss://"))
    {
        problems.push(format!(
            "hub: '{h}' must start with ws:// or wss://, such as wss://hub.example.com"
        ));
    }
    if hub.is_none() && raw.works_for == WorksFor::Hub {
        problems.push(
            "works_for: a runner works for the accounts of a hub, and there is no `hub`; say `owner`, or name the hub".into(),
        );
    }
    if hub.is_none() && raw.offers.models.is_some() {
        problems.push(
            "offers: a runner offers what it has through a hub, and there is no `hub`".into(),
        );
    }
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
    let interfaces = runner_interfaces(raw.interfaces.as_ref(), &mut problems);
    if problems.is_empty() {
        Ok(Runner {
            version: raw.version,
            name,
            works_for: raw.works_for,
            workspace,
            hub,
            offers: Offers { models },
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

/// The folder `workspace` names in the runner file at `path`: `~/…` in the
/// home directory, and a relative path beside the file, as hub.yaml's paths
/// are.
fn workspace_path(workspace: &str, path: &Path) -> Result<PathBuf, String> {
    if workspace.is_empty() {
        return Err("is empty: name the folder the runner works in, such as ~/ensemble".into());
    }
    if let Some(rest) = workspace.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| format!("'{workspace}' starts with ~, and HOME is not set"))?;
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(path.parent().unwrap_or(Path::new(".")).join(workspace))
}

/// A runner's name: what a hub's users write after `<owner>/`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}
