//! Where the hub keeps its settings and the keys of its backends: in its
//! database, as a GitLab instance keeps its application settings, so that
//! the operations, the REST API and a web UI change them, and a deployment,
//! such as the NixOS module, only runs the hub ([`ensemble_store::kept`]).
//!
//! A variable is looked for in the environment, then among the keys kept,
//! then in the hub's `.env` (or `--env-file`), where keys of a secrets
//! manager are.

use ensemble_config::Hub;
use ensemble_config::settings_file::Store;
use ensemble_store::kept;
use std::path::Path;

pub use ensemble_store::kept::{MASTER_KEY_FILE, source};

/// The name the hub's settings are kept by in the database.
const SETTINGS: &str = "hub";
/// What keeps a backend's key, for what is said.
const PROGRAM: &str = "ensemble-hub";

/// What the hub's settings are called in what is said.
pub const SETTINGS_NAME: &str = "the hub's settings";

/// The settings of the hub in `dir`, in its database.
pub fn settings(dir: &Path) -> kept::Settings {
    kept::Settings::new(dir, SETTINGS)
}

/// The keys the hub in `dir` keeps, sealed in its database.
pub fn keys(dir: &Path) -> kept::Keys {
    kept::Keys::new(dir, PROGRAM)
}

/// Where the hub in `dir` looks for a variable: the environment, the keys it
/// keeps, then its `.env` or `--env-file`.
pub fn variables(dir: &Path, env_file: Option<&Path>) -> Result<kept::Variables, String> {
    kept::Variables::new(&keys(dir), env_file)
}

/// The settings of the hub in `dir`, loaded, with the keys they name: from
/// the environment, the keys kept, or `env_file`.
pub fn load(dir: &Path, env_file: Option<&Path>) -> Result<Hub, String> {
    let text = settings(dir).read()?.ok_or_else(|| {
        format!(
            "there are no hub settings in {}. Write them with `ensemble-hub init`, or run the hub on the computer that has them.",
            dir.display()
        )
    })?;
    let env = variables(dir, env_file)?;
    ensemble_config::parse_hub(&text, &source(dir), &env).map_err(|e| e.to_string())
}
