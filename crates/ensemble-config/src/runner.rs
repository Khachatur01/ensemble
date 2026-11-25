//! The settings of one runner ([`RunnerSettings`]), a worker that gives this
//! computer's power to its owner, kept in typed tables of its database
//! (`ensemble_store::kept`) and changed only through its operations. A
//! computer runs one runner.
//!
//! A runner says where it works (its workspace, a folder of git
//! repositories, each a project named for its folder, where it clones the
//! repository of a task whose project it has no copy of), and its ANN pool:
//! the providers its teams' agents run on, its own and other runners it
//! reaches through the hub, the models they serve, and which of its own it
//! shares through the hub. Its teams are the folders of its teams directory
//! (`teams`).

use super::ann::AnnPoolSetting;
use super::*;
use ensemble_core::state::RunnerDir;
use serde::{Deserialize, Serialize};

/// One runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    /// Its name, which a hub knows as `<owner>/<name>`.
    pub name: String,
    /// The folder it works in: each git repository directly inside it is a
    /// project, named for its folder, whose tasks the runner runs; it clones
    /// a task's repository here for a project it has none of.
    pub workspace: PathBuf,
    /// The hub server it connects to, `ws://` or `wss://`, signed in to with
    /// `ensemble-runner login`. None: it uses no hub.
    pub hub: Option<String>,
    /// Its ANN pool: the providers its agents run on, and the models it shares.
    pub llm: AnnPool,
    pub source: PathBuf,
}

/// The runner's settings as they are kept, in typed tables of its database
/// (`ensemble_store::kept`), and as `export` and `import` write them: every
/// setting written, none with a default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerSettings {
    /// Its name, which a hub knows as `<owner>/<name>`.
    pub name: String,
    /// The folder it works in: `~/` is the home directory, and a relative
    /// path is in the runner's folder.
    pub workspace: String,
    /// The hub it connects to, `ws://` or `wss://`. None: no hub.
    pub hub: Option<String>,
    /// The providers its teams' agents run on and the models they serve.
    pub ann_pool: AnnPoolSetting,
}

impl RunnerSettings {
    /// The settings a new runner starts with: `name`, working in `workspace`,
    /// with no hub and no provider.
    pub fn starting(name: &str, workspace: &str) -> Self {
        RunnerSettings {
            name: name.to_string(),
            workspace: workspace.to_string(),
            hub: None,
            ann_pool: AnnPoolSetting::default(),
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
        let llm = crate::ann::pool(&self.ann_pool, true, env, &mut problems);
        if hub.is_none() && llm.models.iter().any(|m| m.shared) {
            problems.push(
                "ann_pool: a runner shares its models through a hub, and it names no hub".into(),
            );
        }
        if hub.is_none() && !llm.relayed.is_empty() {
            problems.push(
                "ann_pool: a provider that is another runner is reached through a hub, and it names no hub"
                    .into(),
            );
        }
        if problems.is_empty() {
            Ok(Runner {
                name,
                workspace,
                hub,
                llm,
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
