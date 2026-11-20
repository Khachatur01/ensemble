//! The settings and keys a hub or a runner keeps in its database, as a GitLab
//! instance keeps its application settings: in typed tables, one row of
//! settings and a table for each kind of thing there are several of
//! (backends, roles, models, trackers), changed only through operations,
//! each change kept in the audit log with who made it. A deployment, such as
//! the NixOS module, only runs the hub or the runner.
//!
//! A key is kept sealed, AES-256-GCM, by a master key, `master.key`, a file
//! beside the database made the first time a key is kept: the database alone
//! gives no key away, and without the master key no key in it can be read.
//! The backends name their keys by variable, and a variable is looked for in
//! the environment, then among the keys kept, then in a file of `NAME=value`
//! lines (`.env` or `--env-file`), where keys of a secrets manager are.

use crate::{DB_FILE, Value, keep_secret, optional, rows, secrets, text, transaction};
use ensemble_config::kept::{Change, Keep};
use ensemble_config::{
    BackendSetting, EmbedSetting, Env, HubSettings, ModelSetting, ModelsSetting, Permission,
    RegistrationSetting, RunnerSettings, SourceFilterSetting, TrackerSetting, WhenNone, WorksFor,
};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The file the master key is in, beside the database.
pub const MASTER_KEY_FILE: &str = "master.key";
/// What a sealed key starts with: the way it was sealed.
const SEALED: &str = "v1:";

/// The tables of a hub's settings, besides the backends.
pub(crate) const HUB_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hub_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    owner TEXT NOT NULL,
    listen TEXT NOT NULL,
    registration TEXT NOT NULL,
    max_pending INTEGER,
    session_hours INTEGER NOT NULL,
    lockout_failures INTEGER NOT NULL,
    lockout_minutes INTEGER NOT NULL,
    vault TEXT NOT NULL,
    distill_when_none TEXT NOT NULL,
    distill_max_tokens INTEGER NOT NULL,
    recall_max_notes INTEGER NOT NULL,
    recall_max_chars INTEGER NOT NULL,
    embed_backend TEXT,
    embed_model TEXT,
    embed_min_similarity REAL,
    filter_when_none TEXT,
    filter_max_tokens INTEGER,
    filter_prompt TEXT
);
CREATE TABLE IF NOT EXISTS roles (
    name TEXT PRIMARY KEY,
    permissions TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS models (
    purpose TEXT NOT NULL,
    position INTEGER NOT NULL,
    backend TEXT NOT NULL,
    model TEXT NOT NULL,
    PRIMARY KEY (purpose, position)
);
CREATE TABLE IF NOT EXISTS trackers (
    name TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    dir TEXT NOT NULL,
    label TEXT NOT NULL,
    poll_seconds INTEGER NOT NULL
);
";

/// The table of a runner's settings, besides the backends.
const RUNNER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runner_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    name TEXT NOT NULL,
    works_for TEXT NOT NULL,
    workspace TEXT NOT NULL,
    hub TEXT,
    max_shared_calls INTEGER NOT NULL
);
";

/// What the distiller's models are kept as, and the source filter's.
const DISTILL: &str = "distill";
const FILTER: &str = "source_filter";

/// Where the settings in `dir` are said to be kept, for what is said of
/// them, and what their relative paths are relative to: its database.
pub fn source(dir: &Path) -> PathBuf {
    dir.join(DB_FILE)
}

fn now() -> String {
    ensemble_core::time::utc(SystemTime::now())
}

fn int(n: impl Into<i64>) -> Value {
    Value::Integer(n.into())
}

fn txt(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// A whole number kept, as the type wanted.
fn num<T: TryFrom<i64>>(value: &Value, what: &str) -> Result<T, String> {
    match value {
        Value::Integer(n) => T::try_from(*n).map_err(|_| format!("{what}: {n} is out of range")),
        _ => Err(format!("{what}: is not a number")),
    }
}

fn some_text(value: &Value, what: &str) -> Result<String, String> {
    text(value).ok_or_else(|| format!("{what}: is empty"))
}

/// A value kept as its JSON name, such as `wait` or `owner`.
fn named<T: serde::de::DeserializeOwned>(value: &Value, what: &str) -> Result<T, String> {
    let name = some_text(value, what)?;
    serde_json::from_value(serde_json::Value::String(name.clone()))
        .map_err(|_| format!("{what}: '{name}' is unknown"))
}

fn name_of<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// The statement that keeps `change` in the audit log.
fn audited(change: &Change) -> (&'static str, Vec<Value>) {
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

/// The statements that keep `backends` in place of those kept, `shares`
/// marked as shared.
fn backend_rows(
    backends: &BTreeMap<String, BackendSetting>,
    shares: &[String],
) -> Vec<(&'static str, Vec<Value>)> {
    let mut out = vec![("DELETE FROM backends", vec![])];
    for (name, b) in backends {
        let shared = int(i64::from(shares.contains(name)));
        let row = match b {
            BackendSetting::OpenAiCompatible {
                base_url,
                api_key_env,
                timeout_seconds,
            }
            | BackendSetting::Anthropic {
                base_url,
                api_key_env,
                timeout_seconds,
            } => vec![
                txt(name),
                txt(b.type_id()),
                txt(base_url),
                optional(api_key_env.as_deref()),
                int(*timeout_seconds as i64),
                Value::Null,
                Value::Null,
                shared,
            ],
            BackendSetting::Runner { runner, backend } => vec![
                txt(name),
                txt(b.type_id()),
                Value::Null,
                Value::Null,
                Value::Null,
                txt(runner),
                txt(backend),
                shared,
            ],
        };
        out.push((
            "INSERT INTO backends (name, type, base_url, api_key_env, timeout_seconds, runner, backend, shared) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            row,
        ));
    }
    out
}

/// The backends kept, and the names of those shared.
async fn read_backends(
    c: &turso::Connection,
) -> Result<(BTreeMap<String, BackendSetting>, Vec<String>), String> {
    let mut backends = BTreeMap::new();
    let mut shares = Vec::new();
    for r in rows(
        c,
        "SELECT name, type, base_url, api_key_env, timeout_seconds, runner, backend, shared FROM backends ORDER BY name",
        vec![],
    )
    .await?
    {
        let name = some_text(&r[0], "backends.name")?;
        let at = format!("backends.{name}");
        let kind = some_text(&r[1], &at)?;
        let b = if kind == ensemble_config::llm::RUNNER_BACKEND {
            BackendSetting::Runner {
                runner: some_text(&r[5], &format!("{at}.runner"))?,
                backend: some_text(&r[6], &format!("{at}.backend"))?,
            }
        } else {
            let base_url = some_text(&r[2], &format!("{at}.base_url"))?;
            let api_key_env = text(&r[3]);
            let timeout_seconds = num(&r[4], &format!("{at}.timeout_seconds"))?;
            match kind.as_str() {
                ensemble_config::ANTHROPIC_API => BackendSetting::Anthropic {
                    base_url,
                    api_key_env,
                    timeout_seconds,
                },
                ensemble_config::OPENAI_COMPATIBLE_API => BackendSetting::OpenAiCompatible {
                    base_url,
                    api_key_env,
                    timeout_seconds,
                },
                other => return Err(format!("{at}: the type '{other}' is unknown")),
            }
        };
        if num::<i64>(&r[7], &format!("{at}.shared"))? != 0 {
            shares.push(name.clone());
        }
        backends.insert(name, b);
    }
    Ok((backends, shares))
}

/// The settings of the hub in a folder, in its database.
pub struct HubStore {
    dir: PathBuf,
}

impl HubStore {
    pub fn new(dir: &Path) -> Self {
        HubStore {
            dir: dir.to_path_buf(),
        }
    }
}

impl Keep<HubSettings> for HubStore {
    fn read(&self) -> Result<Option<HubSettings>, String> {
        crate::with(&self.dir, async |c| {
            let found = rows(
                c,
                "SELECT owner, listen, registration, max_pending, session_hours, lockout_failures, lockout_minutes, vault, distill_when_none, distill_max_tokens, recall_max_notes, recall_max_chars, embed_backend, embed_model, embed_min_similarity, filter_when_none, filter_max_tokens, filter_prompt FROM hub_settings WHERE id = 1",
                vec![],
            )
            .await?;
            let Some(r) = found.first() else {
                return Ok(None);
            };
            let mut roles = BTreeMap::new();
            for role in rows(c, "SELECT name, permissions FROM roles", vec![]).await? {
                let name = some_text(&role[0], "roles.name")?;
                let permissions: Vec<Permission> =
                    serde_json::from_str(&some_text(&role[1], &name)?)
                        .map_err(|e| format!("roles.{name}: {e}"))?;
                roles.insert(name, permissions);
            }
            let mut models: BTreeMap<String, Vec<ModelSetting>> = BTreeMap::new();
            for m in rows(
                c,
                "SELECT purpose, backend, model FROM models ORDER BY purpose, position",
                vec![],
            )
            .await?
            {
                models
                    .entry(some_text(&m[0], "models.purpose")?)
                    .or_default()
                    .push(ModelSetting {
                        backend: some_text(&m[1], "models.backend")?,
                        model: some_text(&m[2], "models.model")?,
                    });
            }
            let mut trackers = BTreeMap::new();
            for t in rows(
                c,
                "SELECT name, type, dir, label, poll_seconds FROM trackers",
                vec![],
            )
            .await?
            {
                let name = some_text(&t[0], "trackers.name")?;
                let at = format!("trackers.{name}");
                let kind = some_text(&t[1], &at)?;
                if kind != ensemble_config::hub::LOCAL_MARKDOWN_TRACKER {
                    return Err(format!("{at}: the type '{kind}' is unknown"));
                }
                trackers.insert(
                    name,
                    TrackerSetting::LocalMarkdown {
                        dir: some_text(&t[2], &at)?,
                        label: some_text(&t[3], &at)?,
                        poll_seconds: num(&t[4], &at)?,
                    },
                );
            }
            let (backends, _) = read_backends(c).await?;
            let registration = match some_text(&r[2], "registration")?.as_str() {
                "closed" => RegistrationSetting::Closed,
                "approval" => RegistrationSetting::Approval {
                    max_pending: num(&r[3], "registration.max_pending")?,
                },
                other => return Err(format!("registration: '{other}' is unknown")),
            };
            let embed = match text(&r[12]) {
                Some(backend) => Some(EmbedSetting {
                    backend,
                    model: some_text(&r[13], "embed.model")?,
                    min_similarity: match &r[14] {
                        Value::Real(x) => *x,
                        Value::Integer(n) => *n as f64,
                        _ => return Err("embed.min_similarity: is not a number".into()),
                    },
                }),
                None => None,
            };
            let source_filter = match text(&r[15]) {
                Some(_) => Some(SourceFilterSetting {
                    models: ModelsSetting {
                        candidates: models.remove(FILTER).unwrap_or_default(),
                        when_none: named(&r[15], "source_filter.when_none")?,
                        max_tokens: num(&r[16], "source_filter.max_tokens")?,
                    },
                    prompt: some_text(&r[17], "source_filter.prompt")?,
                }),
                None => None,
            };
            Ok(Some(HubSettings {
                owner: some_text(&r[0], "owner")?,
                listen: some_text(&r[1], "listen")?,
                registration,
                session_hours: num(&r[4], "session_hours")?,
                lockout_failures: num(&r[5], "lockout_failures")?,
                lockout_minutes: num(&r[6], "lockout_minutes")?,
                roles,
                backends,
                vault: some_text(&r[7], "vault")?,
                distill: ModelsSetting {
                    candidates: models.remove(DISTILL).unwrap_or_default(),
                    when_none: named::<WhenNone>(&r[8], "distill.when_none")?,
                    max_tokens: num(&r[9], "distill.max_tokens")?,
                },
                embed,
                recall_max_notes: num(&r[10], "recall_max_notes")?,
                recall_max_chars: num(&r[11], "recall_max_chars")?,
                trackers,
                source_filter,
            }))
        })
    }

    fn write(&self, s: &HubSettings, change: &Change) -> Result<(), String> {
        let (registration, max_pending) = match s.registration {
            RegistrationSetting::Closed => ("closed", Value::Null),
            RegistrationSetting::Approval { max_pending } => ("approval", int(max_pending)),
        };
        let (embed_backend, embed_model, embed_min) = match &s.embed {
            Some(e) => (
                txt(&e.backend),
                txt(&e.model),
                Value::Real(e.min_similarity),
            ),
            None => (Value::Null, Value::Null, Value::Null),
        };
        let (filter_when_none, filter_max_tokens, filter_prompt) = match &s.source_filter {
            Some(f) => (
                txt(&name_of(&f.models.when_none)),
                int(f.models.max_tokens),
                txt(&f.prompt),
            ),
            None => (Value::Null, Value::Null, Value::Null),
        };
        let mut statements = vec![
            ("DELETE FROM hub_settings", vec![]),
            (
                "INSERT INTO hub_settings (id, owner, listen, registration, max_pending, session_hours, lockout_failures, lockout_minutes, vault, distill_when_none, distill_max_tokens, recall_max_notes, recall_max_chars, embed_backend, embed_model, embed_min_similarity, filter_when_none, filter_max_tokens, filter_prompt) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                vec![
                    txt(&s.owner),
                    txt(&s.listen),
                    txt(registration),
                    max_pending,
                    int(s.session_hours),
                    int(s.lockout_failures),
                    int(s.lockout_minutes),
                    txt(&s.vault),
                    txt(&name_of(&s.distill.when_none)),
                    int(s.distill.max_tokens),
                    int(s.recall_max_notes),
                    int(s.recall_max_chars),
                    embed_backend,
                    embed_model,
                    embed_min,
                    filter_when_none,
                    filter_max_tokens,
                    filter_prompt,
                ],
            ),
            ("DELETE FROM roles", vec![]),
            ("DELETE FROM models", vec![]),
            ("DELETE FROM trackers", vec![]),
        ];
        for (name, permissions) in &s.roles {
            let permissions = serde_json::to_string(permissions).map_err(|e| e.to_string())?;
            statements.push((
                "INSERT INTO roles (name, permissions) VALUES (?1, ?2)",
                vec![txt(name), txt(&permissions)],
            ));
        }
        let filter = s.source_filter.iter().map(|f| (FILTER, &f.models));
        for (purpose, models) in std::iter::once((DISTILL, &s.distill)).chain(filter) {
            for (i, m) in models.candidates.iter().enumerate() {
                statements.push((
                    "INSERT INTO models (purpose, position, backend, model) VALUES (?1, ?2, ?3, ?4)",
                    vec![txt(purpose), int(i as i64), txt(&m.backend), txt(&m.model)],
                ));
            }
        }
        for (name, t) in &s.trackers {
            let TrackerSetting::LocalMarkdown {
                dir,
                label,
                poll_seconds,
            } = t;
            statements.push((
                "INSERT INTO trackers (name, type, dir, label, poll_seconds) VALUES (?1, ?2, ?3, ?4, ?5)",
                vec![
                    txt(name),
                    txt(ensemble_config::hub::LOCAL_MARKDOWN_TRACKER),
                    txt(dir),
                    txt(label),
                    int(*poll_seconds),
                ],
            ));
        }
        statements.extend(backend_rows(&s.backends, &[]));
        statements.push(audited(change));
        crate::with(&self.dir, async |c| transaction(c, statements).await)
    }
}

/// The settings of the runner in a folder, in its database.
pub struct RunnerStore {
    dir: PathBuf,
}

impl RunnerStore {
    pub fn new(dir: &Path) -> Self {
        RunnerStore {
            dir: dir.to_path_buf(),
        }
    }
}

impl Keep<RunnerSettings> for RunnerStore {
    fn read(&self) -> Result<Option<RunnerSettings>, String> {
        crate::kept_with(&self.dir, RUNNER_SCHEMA, async |c| {
            let found = rows(
                c,
                "SELECT name, works_for, workspace, hub, max_shared_calls FROM runner_settings WHERE id = 1",
                vec![],
            )
            .await?;
            let Some(r) = found.first() else {
                return Ok(None);
            };
            let (backends, shares) = read_backends(c).await?;
            Ok(Some(RunnerSettings {
                name: some_text(&r[0], "name")?,
                works_for: named::<WorksFor>(&r[1], "works_for")?,
                workspace: some_text(&r[2], "workspace")?,
                hub: text(&r[3]),
                backends,
                shares,
                max_shared_calls: num(&r[4], "max_shared_calls")?,
            }))
        })
    }

    fn write(&self, s: &RunnerSettings, change: &Change) -> Result<(), String> {
        let mut statements = vec![
            ("DELETE FROM runner_settings", vec![]),
            (
                "INSERT INTO runner_settings (id, name, works_for, workspace, hub, max_shared_calls) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                vec![
                    txt(&s.name),
                    txt(s.works_for.as_str()),
                    txt(&s.workspace),
                    optional(s.hub.as_deref()),
                    int(s.max_shared_calls),
                ],
            ),
        ];
        statements.extend(backend_rows(&s.backends, &s.shares));
        statements.push(audited(change));
        crate::kept_with(&self.dir, RUNNER_SCHEMA, async |c| {
            transaction(c, statements).await
        })
    }
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
    fn a_hubs_settings_are_kept_in_tables_whole_and_each_change_in_the_audit_log() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        let store = HubStore::new(&dir);
        assert_eq!(store.read().unwrap(), None);
        assert!(is_new(&dir).unwrap());
        let mut hub = HubSettings::starting("Ana");
        store
            .write(&hub, &Change::by_this_computer("init"))
            .unwrap();
        assert_eq!(store.read().unwrap().as_ref(), Some(&hub), "as it starts");
        hub.registration = RegistrationSetting::Approval { max_pending: 3 };
        hub.backends.insert(
            "local".into(),
            BackendSetting::OpenAiCompatible {
                base_url: "http://127.0.0.1:1/v1".into(),
                api_key_env: Some("ENSEMBLE_BACKEND_LOCAL_KEY".into()),
                timeout_seconds: 5,
            },
        );
        hub.backends.insert(
            "claude".into(),
            BackendSetting::Anthropic {
                base_url: "https://api.anthropic.com".into(),
                api_key_env: None,
                timeout_seconds: 9,
            },
        );
        hub.backends.insert(
            "gpu".into(),
            BackendSetting::Runner {
                runner: "kim/box".into(),
                backend: "big".into(),
            },
        );
        let model = |backend: &str, model: &str| ModelSetting {
            backend: backend.into(),
            model: model.into(),
        };
        hub.distill.candidates = vec![model("local", "small"), model("gpu", "big")];
        hub.embed = Some(EmbedSetting {
            backend: "local".into(),
            model: "nomic".into(),
            min_similarity: 0.45,
        });
        hub.source_filter = Some(SourceFilterSetting {
            models: ModelsSetting {
                candidates: vec![model("claude", "haiku")],
                when_none: WhenNone::Fail,
                max_tokens: 200,
            },
            prompt: "Salaries.".into(),
        });
        hub.trackers.insert(
            "issues".into(),
            TrackerSetting::LocalMarkdown {
                dir: "issues".into(),
                label: "ensemble".into(),
                poll_seconds: 30,
            },
        );
        hub.roles.remove("reader");
        let caller = ensemble_ops::Caller::Account {
            name: "ana".into(),
            permissions: vec![],
            session: true,
            token: "t".into(),
        };
        const KEY: &[ensemble_ops::Field] = &[ensemble_ops::Field::secret("key", "")];
        let spec = ensemble_ops::Spec {
            name: "backend.key",
            summary: "",
            access: ensemble_ops::Access::Local,
            input: KEY,
        };
        store
            .write(
                &hub,
                &Change::new(&caller, &spec, serde_json::json!({ "key": "sk-1" })),
            )
            .unwrap();
        assert_eq!(store.read().unwrap().as_ref(), Some(&hub), "every setting");
        let log = audit(&dir).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(
            (log[0].who.as_str(), log[0].operation.as_str()),
            ("local", "init")
        );
        assert_eq!(log[1].who, "ana");
        assert_eq!(log[1].input, serde_json::json!({ "key": "(hidden)" }));
        assert!(!is_new(&dir).unwrap());
    }

    #[test]
    fn a_runners_settings_are_kept_with_its_shares_and_no_table_of_the_hub() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("runner");
        let store = RunnerStore::new(&dir);
        assert_eq!(store.read().unwrap(), None);
        let mut runner = RunnerSettings::starting("laptop", "~/ensemble");
        runner.hub = Some("ws://127.0.0.1:7700".into());
        runner.works_for = WorksFor::Hub;
        runner.backends.insert(
            "local".into(),
            BackendSetting::OpenAiCompatible {
                base_url: "http://127.0.0.1:1/v1".into(),
                api_key_env: None,
                timeout_seconds: 5,
            },
        );
        runner.shares = vec!["local".into()];
        store
            .write(&runner, &Change::by_this_computer("init"))
            .unwrap();
        assert_eq!(store.read().unwrap(), Some(runner));
        let tables = crate::open(&source(&dir), "", async |c| {
            rows(
                c,
                "SELECT name FROM sqlite_master WHERE type = 'table'",
                vec![],
            )
            .await
        })
        .unwrap();
        let tables: Vec<String> = tables.iter().filter_map(|r| text(&r[0])).collect();
        assert!(
            tables.contains(&"runner_settings".to_string()),
            "{tables:?}"
        );
        assert!(
            !tables.iter().any(|t| t == "users" || t == "hub_settings"),
            "{tables:?}"
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
            Some("sk-1"),
            "a key kept comes before the file"
        );
    }
}
