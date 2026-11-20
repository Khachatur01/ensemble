//! The settings of one runner ([`RunnerSettings`]), a worker that gives this
//! computer's power to its owner and, when it works for the hub, to the people
//! of a hub server, kept in typed tables of its database
//! (`ensemble_store::kept`) and changed only through its operations. A
//! computer runs one runner.
//!
//! A runner says who it works for (`works_for`: its owner alone, or also the
//! accounts of its hub whose role has `compute`, with the agents of its teams
//! that run on `llm_backends`), where it works (its workspace, a folder of git
//! repositories, each a project named for its folder, where it clones the
//! repository of a task whose project it has no copy of), and the LLM
//! backends its teams' agents run on: its own, those it offers
//! through the hub, and others' it reaches through the hub. Its teams are the
//! folders of its teams directory (`teams`).

use super::llm::BackendSetting;
use super::*;
use ensemble_core::state::RunnerDir;
use serde::{Deserialize, Serialize};

/// One runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
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
    /// The LLM backends its agents run on, and those it offers.
    pub llm: RunnerLlm,
    pub source: PathBuf,
}

/// Whom a runner works for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorksFor {
    /// Its owner alone: the tasks of its owner's account.
    Owner,
    /// Its owner, and every account of its hub whose role has `compute`: their
    /// tasks that no runner of their own takes, with the agents of its teams
    /// on LLM backends alone (`AgentConfig::lendable`), and the backends it
    /// shares.
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

/// The runner's settings as they are kept, in typed tables of its database
/// (`ensemble_store::kept`), and as `export` and `import` write them: every
/// setting written, none with a default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerSettings {
    /// Its name, which a hub knows as `<owner>/<name>`.
    pub name: String,
    pub works_for: WorksFor,
    /// The folder it works in: `~/` is the home directory, and a relative
    /// path is in the runner's folder.
    pub workspace: String,
    /// The hub it connects to, `ws://` or `wss://`. None: no hub.
    pub hub: Option<String>,
    /// The LLM backends its teams' agents run on, by name.
    pub backends: BTreeMap<String, BackendSetting>,
    /// Those of its own backends it offers through the hub.
    pub shares: Vec<String>,
    /// The most calls of others to them at once.
    pub max_shared_calls: u32,
}

impl RunnerSettings {
    /// The settings a new runner starts with: `name`, working for its owner
    /// in `workspace`, with no hub and no backend.
    pub fn starting(name: &str, workspace: &str) -> Self {
        RunnerSettings {
            name: name.to_string(),
            works_for: WorksFor::Owner,
            workspace: workspace.to_string(),
            hub: None,
            backends: BTreeMap::new(),
            shares: Vec::new(),
            max_shared_calls: 1,
        }
    }

    /// The runner in `dir` these settings make, checked, with the keys they
    /// name from `env`: a relative workspace is in its folder.
    pub fn resolve(&self, dir: &RunnerDir, env: &dyn Env) -> Result<Runner, ConfigError> {
        let source = &dir.database();
        let mut problems = Vec::new();
        let name = self.name.trim().to_string();
        if !valid_name(&name) {
            problems.push(format!(
                "name: '{name}' must be letters, digits, '-', '_' or '.', such as laptop or gpu-box"
            ));
        }
        let workspace = workspace_path(self.workspace.trim(), dir.path()).unwrap_or_else(|e| {
            problems.push(format!("workspace: {e}"));
            PathBuf::new()
        });
        let hub = self.hub.as_ref().map(|h| h.trim().to_string());
        if let Some(h) = &hub
            && !(h.starts_with("ws://") || h.starts_with("wss://"))
        {
            problems.push(format!(
                "hub: '{h}' must start with ws:// or wss://, such as wss://hub.example.com"
            ));
        }
        if hub.is_none() && self.works_for == WorksFor::Hub {
            problems.push(
                "works_for: a runner works for the accounts of a hub, and it names no hub; say `owner`, or name the hub".into(),
            );
        }
        let (direct, relayed) = crate::llm::backends(&self.backends, env, &mut problems);
        let mut shares: Vec<String> = Vec::new();
        for name in &self.shares {
            let name = name.trim().to_string();
            match self.backends.get(&name) {
                None => problems.push(format!("shares: '{name}' is not one of its backends")),
                Some(BackendSetting::OpenAiCompatible { .. }) => {}
                Some(_) => problems.push(format!(
                    "shares: '{name}' is not an {OPENAI_COMPATIBLE_API} backend of its own, the one kind a runner offers"
                )),
            }
            if shares.contains(&name) {
                problems.push(format!("shares: '{name}' is listed twice"));
            }
            shares.push(name);
        }
        if self.max_shared_calls == 0 {
            problems.push("max_shared_calls: must be greater than 0".into());
        }
        if hub.is_none() && !shares.is_empty() {
            problems.push(
                "shares: a runner offers its backends through a hub, and it names no hub".into(),
            );
        }
        if hub.is_none() && !relayed.is_empty() {
            problems.push(
                "backends: those of type runner are reached through a hub, and it names no hub"
                    .into(),
            );
        }
        if problems.is_empty() {
            Ok(Runner {
                name,
                works_for: self.works_for,
                workspace,
                hub,
                llm: RunnerLlm {
                    direct,
                    shares,
                    max_shared_calls: self.max_shared_calls,
                    hub: relayed,
                },
                source: source.to_path_buf(),
            })
        } else {
            Err(ConfigError::Invalid {
                path: source.to_path_buf(),
                problems,
            })
        }
    }
}

/// The folder `workspace` names for the runner in `dir`: `~/…` in the home
/// directory, and a relative path in the runner's folder.
fn workspace_path(workspace: &str, dir: &Path) -> Result<PathBuf, String> {
    if workspace.is_empty() {
        return Err("is empty: name the folder the runner works in, such as ~/ensemble".into());
    }
    if let Some(rest) = workspace.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| format!("'{workspace}' starts with ~, and HOME is not set"))?;
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(dir.join(workspace))
}

/// A runner's name: what a hub's users write after `<owner>/`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}
