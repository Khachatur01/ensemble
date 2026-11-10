//! Reading the environment, `--env-file` files and where an organization's config lives.

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

/// The real environment, then the `.env` file: a variable that is set wins,
/// as dotenv files are meant to behave.
pub(super) struct Layered<'a> {
    pub(super) first: &'a dyn Env,
    pub(super) then: &'a BTreeMap<String, String>,
}

impl Env for Layered<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.first.get(key).or_else(|| self.then.get(key).cloned())
    }
}

/// Read a `.env` file: `NAME=value` per line, `#` comments, an optional
/// `export ` prefix, values optionally in single or double quotes. No
/// interpolation. Every malformed line is reported.
pub fn parse_dotenv(text: &str, path: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut vars = BTreeMap::new();
    let mut problems = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            problems.push(format!("line {}: expected NAME=value", i + 1));
            continue;
        };
        let name = name.trim();
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            problems.push(format!("line {}: '{name}' is not a variable name", i + 1));
            continue;
        }
        let value = value.trim();
        let value = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            inner
                .replace("\\n", "\n")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            inner.to_string()
        } else {
            // An unquoted value ends at a comment.
            value
                .split_once(" #")
                .map_or(value, |(v, _)| v)
                .trim_end()
                .to_string()
        };
        vars.insert(name.to_string(), value);
    }
    if problems.is_empty() {
        Ok(vars)
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}
