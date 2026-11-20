//! The settings and keys a hub or a runner keeps in its database, as a GitLab
//! instance keeps its application settings, so that the operations, the REST
//! API and a web UI change them, and a deployment, such as the NixOS module,
//! only runs it.
//!
//! The settings are one YAML document, with its comments, each change of it
//! kept in its history.
//!
//! A key is kept sealed, AES-256-GCM, by a master key, `master.key`, a file
//! beside the database made the first time a key is kept: the database alone
//! gives no key away, and without the master key no key in it can be read.
//! The backends name their keys by variable, and a variable is looked for in
//! the environment, then among the keys kept, then in a file of `NAME=value`
//! lines (`.env` or `--env-file`), where keys of a secrets manager are.

use crate::{DB_FILE, keep_secret, keep_settings, secrets, settings};
use ensemble_config::Env;
use ensemble_config::settings_file::Store;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The file the master key is in, beside the database.
pub const MASTER_KEY_FILE: &str = "master.key";
/// What a sealed key starts with: the way it was sealed.
const SEALED: &str = "v1:";

/// Where the settings in `dir` are said to be kept, for what is said of
/// them, and what their relative paths are relative to: its database.
pub fn source(dir: &Path) -> PathBuf {
    dir.join(DB_FILE)
}

/// The settings kept as `name` in the database in `dir`.
pub struct Settings {
    dir: PathBuf,
    name: &'static str,
}

impl Settings {
    pub fn new(dir: &Path, name: &'static str) -> Self {
        Settings {
            dir: dir.to_path_buf(),
            name,
        }
    }

    /// Write `text` as the settings, unless there are some and not `force`;
    /// whether it was written.
    pub fn init(&self, text: &str, force: bool) -> Result<bool, String> {
        if !force && self.read()?.is_some() {
            return Ok(false);
        }
        self.write(text)?;
        Ok(true)
    }
}

impl Store for Settings {
    fn read(&self) -> Result<Option<String>, String> {
        settings(&self.dir, self.name)
    }

    fn write(&self, text: &str) -> Result<(), String> {
        keep_settings(&self.dir, self.name, text, &now())
    }
}

fn now() -> String {
    ensemble_core::time::utc(SystemTime::now())
}

/// Where a variable is looked for: the environment, the keys kept in a
/// folder, then a file of `NAME=value` lines.
pub struct Variables {
    keys: BTreeMap<String, String>,
    file: BTreeMap<String, String>,
}

impl Variables {
    pub fn new(keys: &Keys, env_file: Option<&Path>) -> Result<Self, String> {
        let file = match env_file {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                ensemble_config::parse_dotenv(&text, path).map_err(|e| e.to_string())?
            }
            None => BTreeMap::new(),
        };
        Ok(Variables {
            keys: keys.all()?,
            file,
        })
    }
}

impl Env for Variables {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.keys.get(key).cloned())
            .or_else(|| self.file.get(key).cloned())
    }
}

/// The keys kept in the database in a folder, sealed by its master key.
pub struct Keys {
    dir: PathBuf,
    /// The command that keeps a backend's key, `ensemble-hub` or
    /// `ensemble-runner`, for what is said.
    program: &'static str,
}

impl Keys {
    pub fn new(dir: &Path, program: &'static str) -> Self {
        Keys {
            dir: dir.to_path_buf(),
            program,
        }
    }

    /// Keep `key` as the variable `name`, sealed, or with none, forget it.
    pub fn keep(&self, name: &str, key: Option<&str>) -> Result<(), String> {
        let sealed = match key {
            Some(key) => Some(self.seal(name, key)?),
            None => None,
        };
        keep_secret(&self.dir, name, sealed.as_deref())
    }

    /// Every key kept, by its variable. With none kept, no master key is
    /// needed, nor made.
    pub fn all(&self) -> Result<BTreeMap<String, String>, String> {
        let sealed = secrets(&self.dir)?;
        if sealed.is_empty() {
            return Ok(BTreeMap::new());
        }
        let master = self.master(false)?;
        sealed
            .into_iter()
            .map(|(name, s)| self.open(&master, &name, &s).map(|key| (name, key)))
            .collect()
    }

    /// Whether a key can be kept: the master key is there, or can be made.
    pub fn ready(&self) -> Result<(), String> {
        self.master(true).map(|_| ())
    }

    fn seal(&self, name: &str, key: &str) -> Result<String, String> {
        let master = self.master(true)?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "no randomness to seal a key with".to_string())?;
        let mut sealed = key.as_bytes().to_vec();
        master
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(name.as_bytes()),
                &mut sealed,
            )
            .map_err(|_| format!("could not seal the key {name}"))?;
        let mut all = nonce.to_vec();
        all.extend(sealed);
        Ok(format!("{SEALED}{}", hex(&all)))
    }

    /// The master key, made when there is none and `make`.
    fn master(&self, make: bool) -> Result<LessSafeKey, String> {
        let path = self.dir.join(MASTER_KEY_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && make => make_master(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "the keys of {} are sealed by its master key, and {} is not there: put it back from where you keep it, or give each backend its key again with `{} backend key`",
                    self.dir.display(),
                    path.display(),
                    self.program
                ));
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let bytes = unhex(text.trim())
            .filter(|b| b.len() == 32)
            .ok_or_else(|| format!("{} is not a master key", path.display()))?;
        let key = UnboundKey::new(&AES_256_GCM, &bytes)
            .map_err(|_| format!("{} is not a master key", path.display()))?;
        Ok(LessSafeKey::new(key))
    }

    /// The key `sealed` holds for the variable `name`.
    fn open(&self, master: &LessSafeKey, name: &str, sealed: &str) -> Result<String, String> {
        let wrong = || {
            format!(
                "the key {name} cannot be opened with this master key: give the backend its key again with `{} backend key`",
                self.program
            )
        };
        let bytes = sealed
            .strip_prefix(SEALED)
            .and_then(unhex)
            .filter(|b| b.len() > NONCE_LEN)
            .ok_or_else(wrong)?;
        let (nonce, rest) = bytes.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| wrong())?;
        let mut rest = rest.to_vec();
        let key = master
            .open_in_place(nonce, Aad::from(name.as_bytes()), &mut rest)
            .map_err(|_| wrong())?;
        String::from_utf8(key.to_vec()).map_err(|_| wrong())
    }
}

/// Make a master key at `path`, readable only by its owner, unless another
/// process just did; what is there then.
fn make_master(path: &Path) -> Result<String, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "no randomness to make a master key with".to_string())?;
    let text = format!("{}\n", hex(&bytes));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => file
            .write_all(text.as_bytes())
            .map(|()| text)
            .map_err(|e| format!("cannot write {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display())),
        Err(e) => Err(format!("cannot write {}: {e}", path.display())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn a_key_is_kept_sealed_and_opened_only_with_its_master_key_and_name() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        let keys = Keys::new(&dir, "ensemble-hub");
        assert!(keys.all().unwrap().is_empty());
        assert!(!dir.join(MASTER_KEY_FILE).exists(), "made only when needed");
        keys.keep("ENSEMBLE_BACKEND_CENTRAL_KEY", Some("sk-1"))
            .unwrap();
        let mode = std::fs::metadata(dir.join(MASTER_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(keys.all().unwrap()["ENSEMBLE_BACKEND_CENTRAL_KEY"], "sk-1");
        let sealed = secrets(&dir).unwrap();
        assert!(!sealed["ENSEMBLE_BACKEND_CENTRAL_KEY"].contains("sk-1"));
        // Sealed for one name, it is no key of another.
        keep_secret(
            &dir,
            "ENSEMBLE_BACKEND_OTHER_KEY",
            Some(&sealed["ENSEMBLE_BACKEND_CENTRAL_KEY"]),
        )
        .unwrap();
        assert!(keys.all().unwrap_err().contains("cannot be opened"));
        keys.keep("ENSEMBLE_BACKEND_OTHER_KEY", None).unwrap();
        // Without the master key, nothing kept can be read.
        std::fs::remove_file(dir.join(MASTER_KEY_FILE)).unwrap();
        let err = keys.all().unwrap_err();
        assert!(err.contains("master key") && err.contains("`ensemble-hub backend key`"));
    }

    #[test]
    fn settings_are_written_once_unless_forced_and_a_kept_key_comes_after_the_environment() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("runner");
        let settings = Settings::new(&dir, "runner");
        assert_eq!(settings.read().unwrap(), None);
        assert!(settings.init("name: a\n", false).unwrap());
        assert!(!settings.init("name: b\n", false).unwrap());
        assert_eq!(settings.read().unwrap().as_deref(), Some("name: a\n"));
        assert!(settings.init("name: b\n", true).unwrap());
        assert_eq!(crate::settings_history(&dir, "runner").unwrap().len(), 2);
        // Only the tables of what is kept: a runner's database has no accounts.
        let tables = crate::open(&source(&dir), "", async |c| {
            crate::rows(
                c,
                "SELECT name FROM sqlite_master WHERE type = 'table'",
                vec![],
            )
            .await
        })
        .unwrap();
        assert!(
            !tables
                .iter()
                .any(|r| crate::text(&r[0]).as_deref() == Some("users"))
        );

        let keys = Keys::new(&dir, "ensemble-runner");
        keys.keep("ENSEMBLE_BACKEND_CENTRAL_KEY", Some("sk-1"))
            .unwrap();
        let env_file = dir.join(".env");
        std::fs::write(&env_file, "OTHER=x\nENSEMBLE_BACKEND_CENTRAL_KEY=old\n").unwrap();
        let env = Variables::new(&keys, Some(&env_file)).unwrap();
        assert_eq!(env.get("OTHER").as_deref(), Some("x"));
        assert_eq!(
            env.get("ENSEMBLE_BACKEND_CENTRAL_KEY").as_deref(),
            Some("sk-1")
        );
    }
}
