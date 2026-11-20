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

/// The variable that holds the API key of the LLM backend `name` when Ensemble
/// keeps the key itself, in the `.env` beside the settings file:
/// `ENSEMBLE_BACKEND_<NAME>_KEY`, its letters upper case and anything else `_`.
pub fn backend_key_variable(name: &str) -> String {
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
    format!("ENSEMBLE_BACKEND_{name}_KEY")
}

/// Set `name` to `value` in the `.env` file at `path`, or take it out with
/// `None`. Its other lines stay as they are; it is made when it is not there,
/// and written readable by its owner alone, through a file beside it, so a
/// reader never sees half of it.
pub fn set_dotenv(path: &Path, name: &str, value: Option<&str>) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let names = |line: &str| {
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line);
        line.split_once('=').is_some_and(|(n, _)| n.trim() == name)
    };
    let line = value.map(|v| format!("{name}={}", dotenv_value(v)));
    let mut lines: Vec<String> = Vec::new();
    let mut placed = false;
    for l in text.lines() {
        if names(l) {
            if let (false, Some(line)) = (placed, &line) {
                lines.push(line.clone());
            }
            placed = true;
        } else {
            lines.push(l.to_string());
        }
    }
    if let (false, Some(line)) = (placed, line) {
        lines.push(line);
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let written = {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        open.open(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(out.as_bytes())
        })
    };
    #[cfg(unix)]
    let written = written.and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
    });
    written
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// `value` as a `.env` value that [`parse_dotenv`] reads back as it is: as it
/// is when it is plain, else in double quotes.
fn dotenv_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:/+=@~,%".contains(c));
    if plain {
        return value.to_string();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}
