//! Reading the environment, `.env` files and where a project's config lives.

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

/// Where a project's config lives: `.ensemble/config.yaml`.
pub fn config_path(project: &Path) -> PathBuf {
    project.join(CONFIG_DIR).join(CONFIG_FILENAME)
}

/// The project's own `.ensemble/config.yaml`, and nothing else.
///
/// Parent directories are deliberately not searched: a team is a property of
/// one project, and a file in `~/projects/` silently configuring every
/// checkout below it is exactly the shared, out-of-sight config this rules
/// out. There is no user-level fallback either.
pub fn discover(project: &Path) -> Option<PathBuf> {
    let candidate = config_path(project);
    candidate.is_file().then_some(candidate)
}

/// The `.env` beside a config file.
pub fn env_path(config: &Path) -> PathBuf {
    config.with_file_name(ENV_FILENAME)
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
