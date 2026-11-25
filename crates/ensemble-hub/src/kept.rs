//! Where the hub keeps its settings and the keys of its providers: in typed
//! tables of its core's database (`<config-dir>/hub/core/database.db`), as a
//! GitLab instance keeps its application settings, so that the operations,
//! the REST API and a web UI change them, and a deployment, such as the
//! NixOS module, only runs the hub. The ANN pool, its keys, sealed by the
//! `master.key` beside the database, and the audit log are kept as a
//! runner's are ([`ensemble_store::kept`]).
//!
//! A variable is looked for in the environment, then among the keys kept.

use ensemble_config::kept::{Change, Keep};
use ensemble_config::{
    EmbedSetting, Hub, HubSettings, ModelSetting, ModelsSetting, Permission, RegistrationSetting,
    SourceFilterSetting, TrackerSetting, WhenNone,
};
use ensemble_core::state::HubDir;
use ensemble_store::kept::{
    self, audited, int, name_of, named, num, pool_rows, read_pool, some_text, txt,
};
use ensemble_store::{Value, rows, text, transaction};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use ensemble_store::kept::MASTER_KEY_FILE;

/// What keeps a provider's key, for what is said.
const PROGRAM: &str = "ensemble-hub";

/// What the hub's settings are called in what is said.
pub const SETTINGS_NAME: &str = "the hub's settings";

/// The hub's accounts and the tokens they sign in with.
const ACCOUNTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    name TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    registered TEXT NOT NULL,
    password_rounds INTEGER,
    password_salt TEXT,
    password_hash TEXT,
    tracker_user TEXT
);
CREATE TABLE IF NOT EXISTS tokens (
    sha256 TEXT PRIMARY KEY,
    user TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    scopes TEXT,
    expires TEXT,
    made TEXT NOT NULL
);
";

/// The tables of the hub's settings, besides the backends.
const SETTINGS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hub_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    owner TEXT NOT NULL,
    registration TEXT NOT NULL,
    max_pending INTEGER,
    session_hours INTEGER NOT NULL,
    lockout_failures INTEGER NOT NULL,
    lockout_minutes INTEGER NOT NULL,
    distill_when_none TEXT NOT NULL,
    distill_max_tokens INTEGER NOT NULL,
    recall_max_notes INTEGER NOT NULL,
    recall_max_chars INTEGER NOT NULL,
    embed_provider TEXT,
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
    provider TEXT NOT NULL,
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

/// What the distiller's models are kept as, and the source filter's.
const DISTILL: &str = "distill";
const FILTER: &str = "source_filter";

/// Open the database of the hub's core in `dir`, with its tables, and run
/// `work` on it: its settings, its accounts and their tokens, and what a
/// runner's core keeps too.
pub fn core<T>(
    dir: &Path,
    work: impl AsyncFnOnce(&ensemble_store::Connection) -> Result<T, String>,
) -> Result<T, String> {
    ensemble_store::kept_with(dir, ACCOUNTS_SCHEMA, async |c| {
        c.execute_batch(SETTINGS_SCHEMA)
            .await
            .map_err(|e| e.to_string())?;
        work(c).await
    })
}

/// The settings of the hub in `dir`, in its core's database.
pub fn settings(dir: &HubDir) -> HubStore {
    HubStore::new(&dir.core())
}

/// The keys the hub in `dir` keeps, sealed in its core's database.
pub fn keys(dir: &HubDir) -> kept::Keys {
    kept::Keys::new(&dir.core(), PROGRAM)
}

/// Where the hub in `dir` looks for a variable: the environment, then the
/// keys it keeps.
pub fn variables(dir: &HubDir) -> Result<kept::Variables, String> {
    kept::Variables::new(&keys(dir))
}

/// The settings of the hub in `dir`, loaded, with the keys they name: from
/// the environment or the keys kept.
pub fn load(dir: &HubDir) -> Result<Hub, String> {
    let settings = settings(dir).read()?.ok_or_else(|| {
        format!(
            "there are no hub settings in {}. Make them with `ensemble-hub setup` or `ensemble-hub init`, or name the folder they are in with --config-dir.",
            dir.database().display()
        )
    })?;
    let env = variables(dir)?;
    settings.resolve(dir, &env).map_err(|e| e.to_string())
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
        core(&self.dir, async |c| {
            let found = rows(
                c,
                "SELECT owner, registration, max_pending, session_hours, lockout_failures, lockout_minutes, distill_when_none, distill_max_tokens, recall_max_notes, recall_max_chars, embed_provider, embed_model, embed_min_similarity, filter_when_none, filter_max_tokens, filter_prompt FROM hub_settings WHERE id = 1",
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
                "SELECT purpose, provider, model FROM models ORDER BY purpose, position",
                vec![],
            )
            .await?
            {
                models
                    .entry(some_text(&m[0], "models.purpose")?)
                    .or_default()
                    .push(ModelSetting {
                        provider: some_text(&m[1], "models.provider")?,
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
            let registration = match some_text(&r[1], "registration")?.as_str() {
                "closed" => RegistrationSetting::Closed,
                "approval" => RegistrationSetting::Approval {
                    max_pending: num(&r[2], "registration.max_pending")?,
                },
                other => return Err(format!("registration: '{other}' is unknown")),
            };
            let embed = match text(&r[10]) {
                Some(provider) => Some(EmbedSetting {
                    provider,
                    model: some_text(&r[11], "embed.model")?,
                    min_similarity: match &r[12] {
                        Value::Real(x) => *x,
                        Value::Integer(n) => *n as f64,
                        _ => return Err("embed.min_similarity: is not a number".into()),
                    },
                }),
                None => None,
            };
            let source_filter = match text(&r[13]) {
                Some(_) => Some(SourceFilterSetting {
                    models: ModelsSetting {
                        candidates: models.remove(FILTER).unwrap_or_default(),
                        when_none: named(&r[13], "source_filter.when_none")?,
                        max_tokens: num(&r[14], "source_filter.max_tokens")?,
                    },
                    prompt: some_text(&r[15], "source_filter.prompt")?,
                }),
                None => None,
            };
            Ok(Some(HubSettings {
                owner: some_text(&r[0], "owner")?,
                registration,
                session_hours: num(&r[3], "session_hours")?,
                lockout_failures: num(&r[4], "lockout_failures")?,
                lockout_minutes: num(&r[5], "lockout_minutes")?,
                roles,
                ann_pool: read_pool(c).await?,
                distill: ModelsSetting {
                    candidates: models.remove(DISTILL).unwrap_or_default(),
                    when_none: named::<WhenNone>(&r[6], "distill.when_none")?,
                    max_tokens: num(&r[7], "distill.max_tokens")?,
                },
                embed,
                recall_max_notes: num(&r[8], "recall_max_notes")?,
                recall_max_chars: num(&r[9], "recall_max_chars")?,
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
        let (embed_provider, embed_model, embed_min) = match &s.embed {
            Some(e) => (
                txt(&e.provider),
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
                "INSERT INTO hub_settings (id, owner, registration, max_pending, session_hours, lockout_failures, lockout_minutes, distill_when_none, distill_max_tokens, recall_max_notes, recall_max_chars, embed_provider, embed_model, embed_min_similarity, filter_when_none, filter_max_tokens, filter_prompt) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                vec![
                    txt(&s.owner),
                    txt(registration),
                    max_pending,
                    int(s.session_hours),
                    int(s.lockout_failures),
                    int(s.lockout_minutes),
                    txt(&name_of(&s.distill.when_none)),
                    int(s.distill.max_tokens),
                    int(s.recall_max_notes),
                    int(s.recall_max_chars),
                    embed_provider,
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
                    "INSERT INTO models (purpose, position, provider, model) VALUES (?1, ?2, ?3, ?4)",
                    vec![txt(purpose), int(i as i64), txt(&m.provider), txt(&m.model)],
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
        statements.extend(pool_rows(&s.ann_pool));
        statements.push(audited(change));
        core(&self.dir, async |c| transaction(c, statements).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::{ModelKind, ProviderSetting, Tier};
    use ensemble_core::workspace::testing::TempDir;
    use ensemble_store::kept::{audit, is_new};

    #[test]
    fn a_hubs_settings_are_kept_in_tables_whole_and_each_change_in_the_audit_log() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub/core");
        let store = HubStore::new(&dir);
        assert_eq!(store.read().unwrap(), None);
        assert!(is_new(&dir).unwrap());
        let mut hub = HubSettings::starting("Ana");
        store
            .write(&hub, &Change::by_this_computer("init"))
            .unwrap();
        assert_eq!(store.read().unwrap().as_ref(), Some(&hub), "as it starts");
        hub.registration = RegistrationSetting::Approval { max_pending: 3 };
        let pool = &mut hub.ann_pool;
        pool.providers.insert(
            "local".into(),
            ProviderSetting::OpenAiCompatible {
                base_url: "http://127.0.0.1:1/v1".into(),
                api_key_env: Some("ENSEMBLE_PROVIDER_LOCAL_KEY".into()),
                timeout_seconds: 5,
                max_concurrent: 2,
            },
        );
        pool.providers.insert(
            "claude".into(),
            ProviderSetting::Anthropic {
                base_url: "https://api.anthropic.com".into(),
                api_key_env: None,
                timeout_seconds: 9,
                max_concurrent: 4,
            },
        );
        pool.providers.insert(
            "gpu".into(),
            ProviderSetting::Runner {
                runner: "kim/box".into(),
            },
        );
        let llm = |tier| ModelKind::Llm {
            tier,
            context_window: Some(32768),
            tools: Some(false),
        };
        pool.add("local", "small", llm(Tier::Small));
        pool.add("local", "nomic", ModelKind::Embedding { dimensions: 768 });
        pool.add("gpu", "local:big", llm(Tier::Strong));
        pool.add("claude", "haiku", llm(Tier::Medium));
        pool.model_mut("local", "nomic").unwrap().missing = true;
        // Kept in the order of their providers and ids.
        pool.models
            .sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
        let model = |provider: &str, model: &str| ModelSetting {
            provider: provider.into(),
            model: model.into(),
        };
        hub.distill.candidates = vec![model("local", "small"), model("gpu", "local:big")];
        hub.embed = Some(EmbedSetting {
            provider: "local".into(),
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
            name: "provider.key",
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
}
