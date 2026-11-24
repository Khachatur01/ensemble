//! Reading the environment, and where an organization's config lives.

use super::*;

/// How the environment is read. Injected so validation is testable without
/// `set_var`, which is `unsafe` in edition 2024 and racy across threads.
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct SystemEnv;

impl Env for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Env for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        BTreeMap::get(self, key).cloned()
    }
}

/// The config file of the organization in directory `org`: `config.yaml`.
/// Parent directories are deliberately not searched, and there is no
/// user-level fallback: the organization is named, never found.
pub fn config_path(org: &Path) -> PathBuf {
    org.join(CONFIG_FILENAME)
}

/// The variable that holds the API key of the source `name` when Ensemble
/// keeps the key itself, sealed in its database:
/// `ENSEMBLE_SOURCE_<NAME>_KEY`, its letters upper case and anything else `_`.
pub fn source_key_variable(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("ENSEMBLE_SOURCE_{name}_KEY")
}
