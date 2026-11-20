//! Where the hub keeps its settings and the keys of its backends: in its
//! database, as a GitLab instance keeps its application settings, so that
//! the operations, the REST API and a web UI change them, and a deployment,
//! such as the NixOS module, only runs the hub.
//!
//! The settings are one YAML document, as `hub.yaml` was, with its comments,
//! each change of it kept in its history. A hub that still has a `hub.yaml`
//! takes it in the first time its settings are read, and the file is renamed
//! `hub.yaml.imported`.
//!
//! A key is kept sealed, AES-256-GCM, by the hub's master key, `master.key`,
//! a file beside the database made the first time a key is kept: the
//! database alone gives no key away, and without the master key no key in it
//! can be read. The backends name their keys by variable, as before, and a
//! variable is looked for in the environment, then among the keys kept, then
//! in the hub's `.env` (or `--env-file`), where keys of a secrets manager are.

use ensemble_config::settings_file::Store;
use ensemble_config::{Env, Hub};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The name the hub's settings are kept by in the database.
const SETTINGS: &str = "hub";
/// The file the master key is in, beside the database.
pub const MASTER_KEY_FILE: &str = "master.key";
/// What a hub kept its settings in before its database did.
const OLD_FILE: &str = ensemble_config::hub::HUB_FILENAME;
/// What a sealed key starts with: the way it was sealed.
const SEALED: &str = "v1:";

/// What the hub's settings are called in what is said.
pub const SETTINGS_NAME: &str = "the hub's settings";

/// Where the hub in `dir` is said to keep its settings, for what is said of
/// them, and what their relative paths, such as the vault's, are relative to:
/// its database.
pub fn source(dir: &Path) -> PathBuf {
    dir.join(ensemble_store::DB_FILE)
}

/// The settings of the hub in `dir`, in its database.
pub struct Settings {
    dir: PathBuf,
}

impl Settings {
    pub fn new(dir: &Path) -> Self {
        Settings {
            dir: dir.to_path_buf(),
        }
    }
}

impl Store for Settings {
    fn read(&self) -> Result<Option<String>, String> {
        match ensemble_store::settings(&self.dir, SETTINGS)? {
            Some(text) => Ok(Some(text)),
            None => adopt(&self.dir),
        }
    }

    fn write(&self, text: &str) -> Result<(), String> {
        ensemble_store::keep_settings(&self.dir, SETTINGS, text, &now())
    }
}

/// The settings of a hub that kept them in `hub.yaml`, taken into its
/// database, with the keys Ensemble kept in its `.env`; the file is renamed
/// so that it is not taken in again, nor mistaken for the settings.
fn adopt(dir: &Path) -> Result<Option<String>, String> {
    let old = dir.join(OLD_FILE);
    let text = match std::fs::read_to_string(&old) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", old.display())),
    };
    let keys = Keys::new(dir);
    let env = dir.join(ensemble_core::state::ENV_FILE);
    let ours: Vec<(String, String)> = match std::fs::read_to_string(&env) {
        Ok(vars) => ensemble_config::parse_dotenv(&vars, &env)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|(name, _)| is_ours(name))
            .collect(),
        Err(_) => Vec::new(),
    };
    for (name, key) in &ours {
        keys.keep(name, Some(key))?;
    }
    ensemble_store::keep_settings(dir, SETTINGS, &text, &now())?;
    for (name, _) in &ours {
        ensemble_config::set_dotenv(&env, name, None)?;
    }
    let renamed = old.with_extension("yaml.imported");
    std::fs::rename(&old, &renamed).map_err(|e| format!("cannot rename {}: {e}", old.display()))?;
    eprintln!(
        "ensemble: {} is kept in the hub's database now, and renamed {}",
        old.display(),
        renamed.display()
    );
    Ok(Some(text))
}

/// Whether `name` is a variable Ensemble names for a backend's key.
fn is_ours(name: &str) -> bool {
    name.starts_with("ENSEMBLE_BACKEND_") && name.ends_with("_KEY")
}

fn now() -> String {
    ensemble_core::time::utc(SystemTime::now())
}

/// Write `text` as the settings of the hub in `dir`, as `ensemble-hub init`
/// does, unless it has some and not `force`.
pub fn init(dir: &Path, text: &str, force: bool) -> Result<bool, String> {
    if !force && Settings::new(dir).read()?.is_some() {
        return Ok(false);
    }
    ensemble_store::keep_settings(dir, SETTINGS, text, &now())?;
    Ok(true)
}

/// The settings of the hub in `dir`, loaded, with the keys they name: from
/// the environment, the keys kept, or `env_file`.
pub fn load(dir: &Path, env_file: Option<&Path>) -> Result<Hub, String> {
    let text = Settings::new(dir).read()?.ok_or_else(|| {
        format!(
            "there are no hub settings in {}. Write them with `ensemble-hub init`, or run the hub on the computer that has them.",
            dir.display()
        )
    })?;
    let env = Variables::new(dir, env_file)?;
    ensemble_config::parse_hub(&text, &source(dir), &env).map_err(|e| e.to_string())
}

/// Where the hub in a folder looks for a variable: the environment, the keys
/// it keeps, then its `.env` or `--env-file`.
pub struct Variables {
    keys: BTreeMap<String, String>,
    file: BTreeMap<String, String>,
}

impl Variables {
    pub fn new(dir: &Path, env_file: Option<&Path>) -> Result<Self, String> {
        let file = match env_file {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                ensemble_config::parse_dotenv(&text, path).map_err(|e| e.to_string())?
            }
            None => BTreeMap::new(),
        };
        Ok(Variables {
            keys: Keys::new(dir).all()?,
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

/// The keys the hub in a folder keeps, sealed in its database by its master
/// key.
pub struct Keys {
    dir: PathBuf,
}

impl Keys {
    pub fn new(dir: &Path) -> Self {
        Keys {
            dir: dir.to_path_buf(),
        }
    }

    /// Keep `key` as the variable `name`, sealed, or with none, forget it.
    pub fn keep(&self, name: &str, key: Option<&str>) -> Result<(), String> {
        let sealed = match key {
            Some(key) => Some(self.seal(name, key)?),
            None => None,
        };
        ensemble_store::keep_secret(&self.dir, name, sealed.as_deref())
    }

    /// Every key kept, by its variable. With none kept, no master key is
    /// needed, nor made.
    pub fn all(&self) -> Result<BTreeMap<String, String>, String> {
        let sealed = ensemble_store::secrets(&self.dir)?;
        if sealed.is_empty() {
            return Ok(BTreeMap::new());
        }
        let master = self.master(false)?;
        sealed
            .into_iter()
            .map(|(name, s)| open(&master, &name, &s).map(|key| (name, key)))
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
        Ok(format!("{SEALED}{}", crate::users::hex(&all)))
    }

    /// The master key, made when there is none and `make`.
    fn master(&self, make: bool) -> Result<LessSafeKey, String> {
        let path = self.dir.join(MASTER_KEY_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && make => make_master(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "the hub keeps keys sealed by its master key, and {} is not there: put it back from where you keep it, or give each backend its key again with `ensemble-hub backend key`",
                    path.display()
                ));
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let bytes = crate::users::unhex(text.trim())
            .filter(|b| b.len() == 32)
            .ok_or_else(|| format!("{} is not a master key", path.display()))?;
        let key = UnboundKey::new(&AES_256_GCM, &bytes)
            .map_err(|_| format!("{} is not a master key", path.display()))?;
        Ok(LessSafeKey::new(key))
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
    let text = format!("{}\n", crate::users::hex(&bytes));
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

/// The key `sealed` holds for the variable `name`.
fn open(master: &LessSafeKey, name: &str, sealed: &str) -> Result<String, String> {
    let wrong = || {
        format!(
            "the key {name} cannot be opened with this master key: give the backend its key again with `ensemble-hub backend key`"
        )
    };
    let bytes = sealed
        .strip_prefix(SEALED)
        .and_then(crate::users::unhex)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn a_key_is_kept_sealed_and_opened_only_with_its_master_key_and_name() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        let keys = Keys::new(&dir);
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
        let sealed = ensemble_store::secrets(&dir).unwrap();
        assert!(!sealed["ENSEMBLE_BACKEND_CENTRAL_KEY"].contains("sk-1"));
        // Sealed for one name, it is no key of another.
        ensemble_store::keep_secret(
            &dir,
            "ENSEMBLE_BACKEND_OTHER_KEY",
            Some(&sealed["ENSEMBLE_BACKEND_CENTRAL_KEY"]),
        )
        .unwrap();
        assert!(keys.all().unwrap_err().contains("cannot be opened"));
        keys.keep("ENSEMBLE_BACKEND_OTHER_KEY", None).unwrap();
        // Without the master key, nothing kept can be read.
        std::fs::remove_file(dir.join(MASTER_KEY_FILE)).unwrap();
        assert!(keys.all().unwrap_err().contains("master key"));
    }

    #[test]
    fn a_hub_yaml_and_the_keys_of_its_env_are_taken_into_the_database_once() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(OLD_FILE), "# mine\nowner: ana\n").unwrap();
        std::fs::write(
            dir.join(".env"),
            "ENSEMBLE_BACKEND_CENTRAL_KEY=sk-1\nOTHER=x\n",
        )
        .unwrap();
        let settings = Settings::new(&dir);
        assert_eq!(
            settings.read().unwrap().as_deref(),
            Some("# mine\nowner: ana\n")
        );
        assert!(!dir.join(OLD_FILE).exists());
        assert!(dir.join("hub.yaml.imported").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "OTHER=x\n"
        );
        assert_eq!(
            Keys::new(&dir).all().unwrap()["ENSEMBLE_BACKEND_CENTRAL_KEY"],
            "sk-1"
        );
        // A key kept wins over the .env; the environment over both.
        let env = Variables::new(&dir, Some(&dir.join(".env"))).unwrap();
        assert_eq!(env.get("OTHER").as_deref(), Some("x"));
        assert_eq!(
            env.get("ENSEMBLE_BACKEND_CENTRAL_KEY").as_deref(),
            Some("sk-1")
        );
        settings.write("owner: bo\n").unwrap();
        assert_eq!(settings.read().unwrap().as_deref(), Some("owner: bo\n"));
        assert!(!init(&dir, "owner: x\n", false).unwrap());
        assert!(init(&dir, "owner: x\n", true).unwrap());
        assert_eq!(
            ensemble_store::settings_history(&dir, SETTINGS)
                .unwrap()
                .len(),
            3
        );
    }
}
