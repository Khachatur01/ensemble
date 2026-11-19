//! `runner.yaml`: one runner, a worker that gives this computer's power to its
//! owner and, when it works for the hub, to the people of a hub server:
//! `~/.config/ensemble/runner/runner.yaml`. A computer runs one runner.
//!
//! A runner says who it works for (`works_for`: its owner alone, or also the
//! accounts of its hub whose role has `compute`, with the agents of its teams
//! that run on `llm_backends`), where it works (its workspace, a folder of git
//! repositories, each a project named for its folder, where it clones the
//! repository of a task whose project it has no copy of), and the LLM
//! backends its teams' agents run on (`llm`): its own, those it offers
//! through the hub, and others' it reaches through the hub. Its teams are the
//! folders of its teams directory (`teams`).

use super::llm::{RawRunnerLlm, runner_llm};
use super::validate::{explain, read_env_file};
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
    /// The LLM backends its agents run on, and those it offers.
    pub llm: RunnerLlm,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRunner {
    pub(crate) version: u32,
    pub(crate) name: String,
    pub(crate) works_for: WorksFor,
    pub(crate) workspace: String,
    pub(crate) hub: Option<String>,
    pub(crate) llm: RawRunnerLlm,
}

/// The runner file in `dir`, usually `~/.config/ensemble/runner`.
pub fn runner_path(dir: &Path) -> PathBuf {
    dir.join(RUNNER_FILENAME)
}

/// The runner file at `path`, or none when there is no such file, with
/// variables the environment does not set taken from `env_file`, usually the
/// runner's `.env`.
pub fn load_runner(
    path: &Path,
    env_file: Option<&Path>,
    env: &dyn Env,
) -> Result<Option<Runner>, ConfigError> {
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
    parse_runner_with(&text, path, env_file, env).map(Some)
}

/// `text` as the runner file at `path`, with variables the environment does
/// not set taken from `env_file`.
pub fn parse_runner_with(
    text: &str,
    path: &Path,
    env_file: Option<&Path>,
    env: &dyn Env,
) -> Result<Runner, ConfigError> {
    let file_vars = read_env_file(env_file)?;
    let env = env::Layered {
        first: env,
        then: &file_vars,
    };
    parse_runner(text, path, &env)
}

pub fn parse_runner(text: &str, path: &Path, env: &dyn Env) -> Result<Runner, ConfigError> {
    // Say what changed rather than which key is unknown.
    if let Ok(v) = serde_norway::from_str::<serde_norway::Value>(text)
        && v.get("interfaces").is_some()
    {
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems: vec![
                "interfaces: is gone: the command line is always on and has no settings; remove the block".into(),
            ],
        });
    }
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
    let llm = runner_llm(&raw.llm, hub.is_some(), env, &mut problems);
    if problems.is_empty() {
        Ok(Runner {
            version: raw.version,
            name,
            works_for: raw.works_for,
            workspace,
            hub,
            llm,
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
