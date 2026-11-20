//! Where the runner keeps its settings and the keys of its backends: in its
//! database, `~/.config/ensemble/runner/database.db`, as the hub keeps its own
//! ([`ensemble_store::kept`]), so that its operations change them, and where
//! it runs, such as the NixOS module, only runs it. Its teams are the folders
//! of its teams directory, beside it.
//!
//! A variable is looked for in the environment, then among the keys kept,
//! then in the runner's `.env` (or `--env-file`), where keys of a secrets
//! manager are.

use ensemble_config::Runner;
use ensemble_config::settings_file::Store;
use ensemble_core::state;
use ensemble_store::kept;
use std::path::{Path, PathBuf};

pub use ensemble_store::kept::source;

/// The name the runner's settings are kept by in the database.
const SETTINGS: &str = "runner";
/// What keeps a backend's key, for what is said.
const PROGRAM: &str = "ensemble-runner";

/// What the runner's settings are called in what is said.
pub const SETTINGS_NAME: &str = "the runner's settings";

/// The folder of this computer's runner, `~/.config/ensemble/runner/`: a
/// computer runs one runner.
pub fn dir() -> Result<PathBuf, String> {
    state::runner_config_dir()
}

/// The settings of the runner in `dir`, in its database.
pub fn settings(dir: &Path) -> kept::Settings {
    kept::Settings::new(dir, SETTINGS)
}

/// The keys the runner in `dir` keeps, sealed in its database.
pub fn keys(dir: &Path) -> kept::Keys {
    kept::Keys::new(dir, PROGRAM)
}

/// Where the runner in `dir` looks for a variable: the environment, the keys
/// it keeps, then its `.env` or `--env-file`.
pub fn variables(dir: &Path, env_file: Option<&Path>) -> Result<kept::Variables, String> {
    kept::Variables::new(&keys(dir), env_file)
}

/// `text` as the settings of the runner in `dir`, with the keys they name.
pub fn parse(text: &str, dir: &Path, env_file: Option<&Path>) -> Result<Runner, String> {
    let env = variables(dir, env_file)?;
    ensemble_config::parse_runner(text, &source(dir), &env).map_err(|e| e.to_string())
}

/// This computer's runner, with the keys its settings name, read from
/// `env_file` where the environment and the keys kept lack them; none before
/// it has settings.
pub fn load(env_file: Option<&Path>) -> Result<Option<Runner>, String> {
    let dir = dir()?;
    match settings(&dir).read()? {
        Some(text) => parse(&text, &dir, env_file).map(Some),
        None => Ok(None),
    }
}
