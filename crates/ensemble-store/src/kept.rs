//! What a hub's core and a runner's both keep in their database, as a GitLab
//! instance keeps its application settings: the ANN pool, the keys of its
//! providers, sealed, and the audit log of every change of their settings, made
//! only through operations. The rest of their settings, typed tables of
//! their own, the hub and the runner each keep themselves.
//!
//! A key is kept sealed, AES-256-GCM, by a master key, `master.key`, a file
//! beside the database made the first time a key is kept: without the
//! master key no key in it can be read. The providers name their keys by
//! variable, and a variable is looked for in the environment, then among the
//! keys kept.

use crate::{DB_FILE, Value, keep_secret, optional, rows, secrets, text};
use ensemble_config::kept::Change;
use ensemble_config::{AnnPoolSetting, Env, ModelKind, PoolModel, ProviderSetting};
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

fn now() -> String {
    ensemble_core::time::utc(SystemTime::now())
}

pub fn int(n: impl Into<i64>) -> Value {
    Value::Integer(n.into())
}

pub fn txt(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// A whole number kept, as the type wanted.
pub fn num<T: TryFrom<i64>>(value: &Value, what: &str) -> Result<T, String> {
    match value {
        Value::Integer(n) => T::try_from(*n).map_err(|_| format!("{what}: {n} is out of range")),
        _ => Err(format!("{what}: is not a number")),
    }
}

pub fn some_text(value: &Value, what: &str) -> Result<String, String> {
    text(value).ok_or_else(|| format!("{what}: is empty"))
}

/// A value kept as its JSON name, such as `wait` or `owner`.
pub fn named<T: serde::de::DeserializeOwned>(value: &Value, what: &str) -> Result<T, String> {
    let name = some_text(value, what)?;
    serde_json::from_value(serde_json::Value::String(name.clone()))
        .map_err(|_| format!("{what}: '{name}' is unknown"))
}

/// What a value is kept as: its JSON name.
pub fn name_of<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// The statement that keeps `change` in the audit log.
pub fn audited(change: &Change) -> (&'static str, Vec<Value>) {
    (
        "INSERT INTO settings_audit (n, at, who, operation, input) SELECT COALESCE(MAX(n), 0) + 1, ?1, ?2, ?3, ?4 FROM settings_audit",
        vec![
            txt(&now()),
            txt(&change.who),
            txt(&change.operation),
            txt(&change.input.to_string()),
        ],
    )
}
/// The statements that keep `pool` in place of the ANN pool kept.
pub fn pool_rows(pool: &AnnPoolSetting) -> Vec<(&'static str, Vec<Value>)> {
    let mut out = vec![
        ("DELETE FROM ann_providers", vec![]),
        ("DELETE FROM ann_models", vec![]),
    ];
    for (name, s) in &pool.providers {
        let row = match s {
            ProviderSetting::OpenAiCompatible {
                base_url,
                api_key_env,
                timeout_seconds,
                max_concurrent,
            }
            | ProviderSetting::Anthropic {
                base_url,
                api_key_env,
                timeout_seconds,
                max_concurrent,
            }
            | ProviderSetting::Decision {
                base_url,
                api_key_env,
                timeout_seconds,
                max_concurrent,
            } => vec![
                txt(name),
                txt(s.type_id()),
                txt(base_url),
                optional(api_key_env.as_deref()),
                int(*timeout_seconds as i64),
                int(*max_concurrent),
                Value::Null,
            ],
            ProviderSetting::Runner { runner } => vec![
                txt(name),
                txt(s.type_id()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                txt(runner),
            ],
        };
        out.push((
            "INSERT INTO ann_providers (name, type, base_url, api_key_env, timeout_seconds, max_concurrent, runner) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            row,
        ));
    }
    for m in &pool.models {
        let flag = |on: bool| int(i64::from(on));
        let (tier, context_window, tools, dimensions) = match m.kind {
            ModelKind::Llm {
                tier,
                context_window,
                tools,
            } => (
                txt(tier.as_str()),
                context_window.map_or(Value::Null, int),
                tools.map_or(Value::Null, flag),
                Value::Null,
            ),
            ModelKind::Embedding { dimensions } => {
                (Value::Null, Value::Null, Value::Null, int(dimensions))
            }
            ModelKind::Decision => (Value::Null, Value::Null, Value::Null, Value::Null),
        };
        out.push((
            "INSERT INTO ann_models (provider, id, kind, tier, context_window, tools, dimensions, missing, shared, abilities) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            vec![
                txt(&m.provider),
                txt(&m.id),
                txt(m.kind.type_id()),
                tier,
                context_window,
                tools,
                dimensions,
                flag(m.missing),
                flag(m.shared),
                optional(m.abilities.as_deref()),
            ],
        ));
    }
    out
}

/// The ANN pool kept: its providers, and their models in the order of their
/// providers and ids.
pub async fn read_pool(c: &turso::Connection) -> Result<AnnPoolSetting, String> {
    let mut pool = AnnPoolSetting::default();
    for r in rows(
        c,
        "SELECT name, type, base_url, api_key_env, timeout_seconds, max_concurrent, runner FROM ann_providers ORDER BY name",
        vec![],
    )
    .await?
    {
        let name = some_text(&r[0], "ann_providers.name")?;
        let at = format!("ann_providers.{name}");
        let kind = some_text(&r[1], &at)?;
        let s = if kind == ensemble_config::ann::RUNNER_PROVIDER {
            ProviderSetting::Runner {
                runner: some_text(&r[6], &format!("{at}.runner"))?,
            }
        } else {
            let base_url = some_text(&r[2], &format!("{at}.base_url"))?;
            let api_key_env = text(&r[3]);
            let timeout_seconds = num(&r[4], &format!("{at}.timeout_seconds"))?;
            let max_concurrent = num(&r[5], &format!("{at}.max_concurrent"))?;
            match kind.as_str() {
                ensemble_config::ANTHROPIC_API => ProviderSetting::Anthropic {
                    base_url,
                    api_key_env,
                    timeout_seconds,
                    max_concurrent,
                },
                ensemble_config::OPENAI_COMPATIBLE_API => ProviderSetting::OpenAiCompatible {
                    base_url,
                    api_key_env,
                    timeout_seconds,
                    max_concurrent,
                },
                ensemble_config::DECISION_API => ProviderSetting::Decision {
                    base_url,
                    api_key_env,
                    timeout_seconds,
                    max_concurrent,
                },
                other => return Err(format!("{at}: the type '{other}' is unknown")),
            }
        };
        pool.providers.insert(name, s);
    }
    for r in rows(
        c,
        "SELECT provider, id, kind, tier, context_window, tools, dimensions, missing, shared, abilities FROM ann_models ORDER BY provider, id",
        vec![],
    )
    .await?
    {
        let provider = some_text(&r[0], "ann_models.provider")?;
        let id = some_text(&r[1], "ann_models.id")?;
        let at = format!("ann_models.{provider}:{id}");
        let flag = |v: &Value, what: &str| num::<i64>(v, &format!("{at}.{what}")).map(|n| n != 0);
        let kind = match some_text(&r[2], &format!("{at}.kind"))?.as_str() {
            "llm" => ModelKind::Llm {
                tier: named(&r[3], &format!("{at}.tier"))?,
                context_window: match &r[4] {
                    Value::Null => None,
                    v => Some(num(v, &format!("{at}.context_window"))?),
                },
                tools: match &r[5] {
                    Value::Null => None,
                    v => Some(flag(v, "tools")?),
                },
            },
            "embedding" => ModelKind::Embedding {
                dimensions: num(&r[6], &format!("{at}.dimensions"))?,
            },
            "decision" => ModelKind::Decision,
            other => return Err(format!("{at}: the kind '{other}' is unknown")),
        };
        pool.models.push(PoolModel {
            provider,
            id,
            kind,
            missing: flag(&r[7], "missing")?,
            shared: flag(&r[8], "shared")?,
            abilities: text(&r[9]),
        });
    }
    Ok(pool)
}

/// One change of the settings, as the audit log keeps it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Audited {
    pub at: String,
    pub who: String,
    pub operation: String,
    pub input: serde_json::Value,
}

/// Every change of the settings in `dir`, oldest first.
pub fn audit(dir: &Path) -> Result<Vec<Audited>, String> {
    let found = crate::kept_with(dir, "", async |c| {
        rows(
            c,
            "SELECT at, who, operation, input FROM settings_audit ORDER BY n",
            vec![],
        )
        .await
    })?;
    found
        .iter()
        .map(|r| {
            Ok(Audited {
                at: some_text(&r[0], "at")?,
                who: some_text(&r[1], "who")?,
                operation: some_text(&r[2], "operation")?,
                input: serde_json::from_str(&some_text(&r[3], "input")?)
                    .map_err(|e| e.to_string())?,
            })
        })
        .collect()
}

/// Whether no settings were kept in `dir` yet: not a single change.
pub fn is_new(dir: &Path) -> Result<bool, String> {
    let found = crate::kept_with(dir, "", async |c| {
        rows(c, "SELECT n FROM settings_audit LIMIT 1", vec![]).await
    })?;
    Ok(found.is_empty())
}

/// Where a variable is looked for: the environment, then the keys kept in a
/// folder.
pub struct Variables {
    keys: BTreeMap<String, String>,
}

impl Variables {
    pub fn new(keys: &Keys) -> Result<Self, String> {
        Ok(Variables { keys: keys.all()? })
    }
}

impl Env for Variables {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.keys.get(key).cloned())
    }
}

/// The keys kept in the database in a folder, sealed by its master key.
pub struct Keys {
    dir: PathBuf,
    /// The command that keeps a provider's key, `ensemble-hub` or
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

    /// `key`, sealed by the master key for `name`, made when there is none:
    /// it opens only as `name`'s.
    pub fn seal(&self, name: &str, key: &str) -> Result<String, String> {
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

    /// What `sealed` holds for `name`, opened by the master key.
    pub fn unseal(&self, name: &str, sealed: &str) -> Result<String, String> {
        self.open(&self.master(false)?, name, sealed)
    }

    /// The master key, made when there is none and `make`.
    fn master(&self, make: bool) -> Result<LessSafeKey, String> {
        let path = self.dir.join(MASTER_KEY_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && make => make_master(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "the keys of {} are sealed by its master key, and {} is not there: put it back from where you keep it, or give each provider its key again with `{} provider key`",
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
                "the key {name} cannot be opened with this master key: give the provider its key again with `{} provider key`",
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
        keys.keep("ENSEMBLE_PROVIDER_CENTRAL_KEY", Some("sk-1"))
            .unwrap();
        let mode = std::fs::metadata(dir.join(MASTER_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(keys.all().unwrap()["ENSEMBLE_PROVIDER_CENTRAL_KEY"], "sk-1");
        let sealed = secrets(&dir).unwrap();
        assert!(!sealed["ENSEMBLE_PROVIDER_CENTRAL_KEY"].contains("sk-1"));
        // Sealed for one name, it is no key of another.
        keep_secret(
            &dir,
            "ENSEMBLE_PROVIDER_OTHER_KEY",
            Some(&sealed["ENSEMBLE_PROVIDER_CENTRAL_KEY"]),
        )
        .unwrap();
        assert!(keys.all().unwrap_err().contains("cannot be opened"));
        keys.keep("ENSEMBLE_PROVIDER_OTHER_KEY", None).unwrap();
        // Without the master key, nothing kept can be read.
        std::fs::remove_file(dir.join(MASTER_KEY_FILE)).unwrap();
        let err = keys.all().unwrap_err();
        assert!(err.contains("master key") && err.contains("`ensemble-hub provider key`"));
    }

    #[test]
    fn a_variable_is_the_environments_then_a_key_kept() {
        let tmp = TempDir::new();
        let keys = Keys::new(&tmp.0.join("runner"), "ensemble-runner");
        keys.keep("ENSEMBLE_TEST_KEPT_KEY", Some("sk-1")).unwrap();
        let env = Variables::new(&keys).unwrap();
        assert_eq!(env.get("ENSEMBLE_TEST_KEPT_KEY").as_deref(), Some("sk-1"));
        assert_eq!(env.get("ENSEMBLE_TEST_NO_KEY"), None);
        assert_eq!(env.get("HOME"), std::env::var("HOME").ok());
    }
}
