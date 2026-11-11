//! Where a node keeps its hub tokens: `~/.config/ensemble/hubs.json`
//! (`$XDG_CONFIG_HOME/ensemble/hubs.json` when that is set), one token per hub
//! URL. Only its user can read it, like an SSH key; `ensemble login` writes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TOKENS_FILE: &str = "hubs.json";

/// The token file and what is in it.
pub struct Tokens {
    path: PathBuf,
    tokens: BTreeMap<String, String>,
}

/// `$XDG_CONFIG_HOME/ensemble`, or `~/.config/ensemble`.
pub fn config_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or("cannot find a config directory: neither XDG_CONFIG_HOME nor HOME is set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("ensemble"))
}

/// A hub URL as tokens are kept by: without a trailing `/`.
pub fn normal(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

impl Tokens {
    /// The token file in the user's config directory.
    pub fn load() -> Result<Tokens, String> {
        Tokens::load_from(&config_dir()?)
    }

    /// The token file in `dir`.
    pub fn load_from(dir: &Path) -> Result<Tokens, String> {
        let path = dir.join(TOKENS_FILE);
        let tokens = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not readable: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Tokens { path, tokens })
    }

    pub fn get(&self, url: &str) -> Option<&str> {
        self.tokens.get(&normal(url)).map(String::as_str)
    }

    pub fn set(&mut self, url: &str, token: &str) {
        self.tokens.insert(normal(url), token.trim().to_string());
    }

    /// Forget the token of `url`, and say whether there was one.
    pub fn remove(&mut self, url: &str) -> bool {
        self.tokens.remove(&normal(url)).is_some()
    }

    /// Write the file, readable only by its owner, in a directory only its
    /// owner can open.
    pub fn save(&self) -> Result<(), String> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let json = serde_json::to_string_pretty(&self.tokens).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("tmp");
        crate::users::write_private(&tmp, &json)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn a_token_is_kept_per_hub_in_a_file_only_its_user_reads() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("ensemble");
        let mut tokens = Tokens::load_from(&dir).unwrap();
        assert!(tokens.get("wss://hub.acme.com").is_none());
        tokens.set("wss://hub.acme.com/", "ens_abc");
        tokens.save().unwrap();
        let mut tokens = Tokens::load_from(&dir).unwrap();
        assert_eq!(tokens.get("wss://hub.acme.com"), Some("ens_abc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join(TOKENS_FILE)), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
        assert!(tokens.remove("wss://hub.acme.com"));
        assert!(!tokens.remove("wss://hub.acme.com"));
    }
}
