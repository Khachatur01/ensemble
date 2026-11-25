//! Where the runner keeps its settings, the keys of its providers and its hub
//! tokens: in typed tables of its core's database,
//! `<config-dir>/runner/core/database.db`, so that its operations change
//! them, and where it runs, such as the NixOS module, only runs it. The
//! ANN pool, the keys, sealed by the `master.key` beside the database, and
//! the audit log are kept as a hub's are ([`ensemble_store::kept`]), and its
//! harnesses in tables of their own.
//!
//! A variable is looked for in the environment, then among the keys kept.

use ensemble_config::kept::{Change, Keep};
use ensemble_config::{
    BuiltinHarness, CliHarness, HarnessKind, HarnessModel, HarnessesSetting, Runner, RunnerSettings,
};
use ensemble_core::state::RunnerDir;
use ensemble_link::tokens::normal;
use ensemble_store::kept::{self, audited, int, named, num, pool_rows, read_pool, some_text, txt};
use ensemble_store::{optional, rows, text, transaction};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What keeps a provider's key, for what is said.
const PROGRAM: &str = "ensemble-runner";

/// What the runner's settings are called in what is said.
pub const SETTINGS_NAME: &str = "the runner's settings";

/// The tables of a runner's settings, besides its ANN pool: one row of
/// them, one for each harness a person added and one for each model of its
/// own vendor's added to Claude Code or Codex; and of its hub tokens, each
/// sealed by the master key.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runner_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    name TEXT NOT NULL,
    workspace TEXT NOT NULL,
    hub TEXT
);
CREATE TABLE IF NOT EXISTS harnesses (
    kind TEXT PRIMARY KEY,
    missing INTEGER NOT NULL,
    command TEXT,
    max_seconds INTEGER NOT NULL,
    max_steps INTEGER NOT NULL,
    max_concurrent INTEGER NOT NULL,
    max_tokens INTEGER
);
CREATE TABLE IF NOT EXISTS harness_models (
    harness TEXT NOT NULL,
    id TEXT NOT NULL,
    tier TEXT NOT NULL,
    abilities TEXT,
    PRIMARY KEY (harness, id)
);
CREATE TABLE IF NOT EXISTS hub_tokens (
    url TEXT PRIMARY KEY,
    sealed TEXT NOT NULL
);
";

/// Open the database of the runner's core in `dir`, with its tables, and
/// run `work` on it.
fn core<T>(
    dir: &Path,
    work: impl AsyncFnOnce(&ensemble_store::Connection) -> Result<T, String>,
) -> Result<T, String> {
    ensemble_store::kept_with(dir, SCHEMA, work)
}

/// The folder of this computer's runner, `runner/` in `--config-dir`: a
/// computer runs one runner.
pub fn dir() -> Result<RunnerDir, String> {
    RunnerDir::this()
}

/// The settings of the runner in `dir`, in its core's database.
pub fn settings(dir: &RunnerDir) -> RunnerStore {
    RunnerStore::new(&dir.core())
}

/// The keys the runner in `dir` keeps, sealed in its core's database.
pub fn keys(dir: &RunnerDir) -> kept::Keys {
    kept::Keys::new(&dir.core(), PROGRAM)
}

/// Where the runner in `dir` looks for a variable: the environment, then
/// the keys it keeps.
pub fn variables(dir: &RunnerDir) -> Result<kept::Variables, String> {
    kept::Variables::new(&keys(dir))
}

/// `settings` as the runner in `dir`, with the keys they name.
pub fn resolve(settings: &RunnerSettings, dir: &RunnerDir) -> Result<Runner, String> {
    let env = variables(dir)?;
    settings.resolve(dir, &env).map_err(|e| e.to_string())
}

/// This computer's runner, with the keys its settings name; none before it
/// has settings.
pub fn load() -> Result<Option<Runner>, String> {
    let dir = dir()?;
    match settings(&dir).read()? {
        Some(s) => resolve(&s, &dir).map(Some),
        None => Ok(None),
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
        core(&self.dir, async |c| {
            let found = rows(
                c,
                "SELECT name, workspace, hub FROM runner_settings WHERE id = 1",
                vec![],
            )
            .await?;
            let Some(r) = found.first() else {
                return Ok(None);
            };
            Ok(Some(RunnerSettings {
                name: some_text(&r[0], "name")?,
                workspace: some_text(&r[1], "workspace")?,
                hub: text(&r[2]),
                ann_pool: read_pool(c).await?,
                harnesses: read_harnesses(c).await?,
            }))
        })
    }

    fn write(&self, s: &RunnerSettings, change: &Change) -> Result<(), String> {
        let mut statements = vec![
            ("DELETE FROM runner_settings", vec![]),
            (
                "INSERT INTO runner_settings (id, name, workspace, hub) VALUES (1, ?1, ?2, ?3)",
                vec![txt(&s.name), txt(&s.workspace), optional(s.hub.as_deref())],
            ),
        ];
        statements.extend(pool_rows(&s.ann_pool));
        statements.extend(harness_rows(&s.harnesses));
        statements.push(audited(change));
        core(&self.dir, async |c| transaction(c, statements).await)
    }
}

/// The statements that keep `harnesses` in place of those kept: a row for
/// each a person added, and one for each model of its own vendor's.
fn harness_rows(harnesses: &HarnessesSetting) -> Vec<(&'static str, Vec<ensemble_store::Value>)> {
    let mut out = vec![
        ("DELETE FROM harnesses", vec![]),
        ("DELETE FROM harness_models", vec![]),
    ];
    for h in harnesses.each() {
        out.push((
            "INSERT INTO harnesses (kind, missing, command, max_seconds, max_steps, max_concurrent, max_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            vec![
                txt(h.kind.as_str()),
                int(i64::from(h.missing)),
                optional(h.command.as_deref()),
                int(h.max_seconds as i64),
                int(h.max_steps),
                int(h.max_concurrent),
                h.max_tokens.map_or(ensemble_store::Value::Null, int),
            ],
        ));
        for m in &h.models {
            out.push((
                "INSERT INTO harness_models (harness, id, tier, abilities) VALUES (?1, ?2, ?3, ?4)",
                vec![
                    txt(h.kind.as_str()),
                    txt(&m.id),
                    txt(m.tier.as_str()),
                    optional(m.abilities.as_deref()),
                ],
            ));
        }
    }
    out
}

/// The harnesses kept: those a person added, each with the models of its own
/// vendor's added to it.
async fn read_harnesses(c: &ensemble_store::Connection) -> Result<HarnessesSetting, String> {
    let mut models: BTreeMap<HarnessKind, Vec<HarnessModel>> = BTreeMap::new();
    for r in rows(
        c,
        "SELECT harness, id, tier, abilities FROM harness_models ORDER BY harness, id",
        vec![],
    )
    .await?
    {
        let name = some_text(&r[0], "harness_models.harness")?;
        let kind = HarnessKind::parse(&name)
            .ok_or_else(|| format!("harness_models: the harness '{name}' is unknown"))?;
        let id = some_text(&r[1], &format!("harness_models.{name}.id"))?;
        let tier = named(&r[2], &format!("harness_models.{name}:{id}.tier"))?;
        models.entry(kind).or_default().push(HarnessModel {
            id,
            tier,
            abilities: text(&r[3]),
        });
    }
    let mut harnesses = HarnessesSetting::default();
    for r in rows(
        c,
        "SELECT kind, missing, command, max_seconds, max_steps, max_concurrent, max_tokens FROM harnesses",
        vec![],
    )
    .await?
    {
        let name = some_text(&r[0], "harnesses.kind")?;
        let kind = HarnessKind::parse(&name)
            .ok_or_else(|| format!("harnesses: the kind '{name}' is unknown"))?;
        let at = |what: &str| format!("harnesses.{name}.{what}");
        let max_seconds = num(&r[3], &at("max_seconds"))?;
        let max_steps = num(&r[4], &at("max_steps"))?;
        let max_concurrent = num(&r[5], &at("max_concurrent"))?;
        match harnesses.cli_slot(kind) {
            Some(slot) => {
                *slot = Some(CliHarness {
                    missing: num::<i64>(&r[1], &at("missing"))? != 0,
                    command: some_text(&r[2], &at("command"))?,
                    max_seconds,
                    max_steps,
                    max_concurrent,
                    models: models.remove(&kind).unwrap_or_default(),
                });
            }
            None => {
                harnesses.builtin = Some(BuiltinHarness {
                    max_seconds,
                    max_steps,
                    max_concurrent,
                    max_tokens: num(&r[6], &at("max_tokens"))?,
                });
            }
        }
    }
    if let Some(kind) = models.keys().next() {
        return Err(format!(
            "harness_models: models of {}, which is not added",
            kind.as_str()
        ));
    }
    Ok(harnesses)
}

/// The runner's hub tokens, one per hub URL, each sealed by its master key
/// for its URL: `ensemble-runner login` and `ensemble-runner join` keep them.
pub struct Tokens {
    dir: RunnerDir,
    tokens: BTreeMap<String, String>,
}

impl Tokens {
    /// The tokens of this computer's runner.
    pub fn load() -> Result<Tokens, String> {
        Tokens::load_from(&dir()?)
    }

    /// The tokens of the runner in `dir`.
    pub fn load_from(dir: &RunnerDir) -> Result<Tokens, String> {
        let sealed = core(&dir.core(), async |c| {
            rows(c, "SELECT url, sealed FROM hub_tokens", vec![]).await
        })?;
        let keys = keys(dir);
        let mut tokens = BTreeMap::new();
        for row in sealed {
            let (Some(url), Some(sealed)) = (text(&row[0]), text(&row[1])) else {
                continue;
            };
            let token = keys.unseal(&sealed_as(&url), &sealed)?;
            tokens.insert(url, token);
        }
        Ok(Tokens {
            dir: dir.clone(),
            tokens,
        })
    }

    pub fn get(&self, url: &str) -> Option<&str> {
        self.tokens.get(&normal(url)).map(String::as_str)
    }

    pub fn set(&mut self, url: &str, token: &str) {
        self.tokens.insert(normal(url), token.trim().to_string());
    }

    /// The hubs it has a token for.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.tokens.keys().map(String::as_str)
    }

    /// Forget the token of `url`, and say whether there was one.
    pub fn remove(&mut self, url: &str) -> bool {
        self.tokens.remove(&normal(url)).is_some()
    }

    /// Keep them, each sealed, in place of those kept.
    pub fn save(&self) -> Result<(), String> {
        let keys = keys(&self.dir);
        let mut statements = vec![("DELETE FROM hub_tokens", vec![])];
        for (url, token) in &self.tokens {
            statements.push((
                "INSERT INTO hub_tokens (url, sealed) VALUES (?1, ?2)",
                vec![txt(url), txt(&keys.seal(&sealed_as(url), token)?)],
            ));
        }
        core(&self.dir.core(), async |c| transaction(c, statements).await)
    }
}

/// What the token of the hub at `url` is sealed as: no key of a provider's.
fn sealed_as(url: &str) -> String {
    format!("hub token {url}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::{ModelKind, ProviderSetting};
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn a_runners_settings_are_kept_with_its_pool_and_no_table_of_the_hub() {
        let tmp = TempDir::new();
        let dir = RunnerDir::new(tmp.0.join("runner"));
        let store = settings(&dir);
        assert_eq!(store.read().unwrap(), None);
        let mut runner = RunnerSettings::starting("laptop", "~/ensemble");
        runner.hub = Some("ws://127.0.0.1:7700".into());
        runner.ann_pool.providers.insert(
            "local".into(),
            ProviderSetting::OpenAiCompatible {
                base_url: "http://127.0.0.1:1/v1".into(),
                api_key_env: None,
                timeout_seconds: 5,
                max_concurrent: 2,
            },
        );
        runner.ann_pool.add(
            "local",
            "small",
            ModelKind::Llm {
                tier: ensemble_config::Tier::Small,
                context_window: Some(8192),
                tools: Some(true),
                max_answer_tokens: Some(2048),
            },
        );
        runner
            .ann_pool
            .add("local", "e", ModelKind::Embedding { dimensions: 8 });
        runner.ann_pool.model_mut("local", "small").unwrap().shared = true;
        runner.ann_pool.model_mut("local", "e").unwrap().missing = true;
        runner.harnesses.claude_code = Some(ensemble_config::CliHarness {
            missing: false,
            command: "claude".into(),
            max_seconds: 600,
            max_steps: 20,
            max_concurrent: 2,
            models: vec![HarnessModel {
                id: "sonnet".into(),
                tier: ensemble_config::Tier::Strong,
                abilities: None,
            }],
        });
        runner.harnesses.builtin = Some(BuiltinHarness {
            max_seconds: 900,
            max_steps: 30,
            max_concurrent: 1,
            max_tokens: 4000,
        });
        store
            .write(&runner, &Change::by_this_computer("init"))
            .unwrap();
        assert_eq!(store.read().unwrap(), Some(runner));
        let tables = ensemble_store::open(&dir.database(), "", async |c| {
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
    }

    #[test]
    fn a_hub_token_is_kept_sealed_per_hub() {
        let tmp = TempDir::new();
        let dir = RunnerDir::new(tmp.0.join("runner"));
        let mut tokens = Tokens::load_from(&dir).unwrap();
        assert!(tokens.get("wss://hub.acme.com").is_none());
        tokens.set("wss://hub.acme.com/", "ens_abc");
        tokens.save().unwrap();
        let mut tokens = Tokens::load_from(&dir).unwrap();
        assert_eq!(tokens.get("wss://hub.acme.com"), Some("ens_abc"));
        let kept = ensemble_store::open(&dir.database(), "", async |c| {
            rows(c, "SELECT sealed FROM hub_tokens", vec![]).await
        })
        .unwrap();
        assert!(!text(&kept[0][0]).unwrap().contains("ens_abc"), "sealed");
        assert!(dir.core().join(kept::MASTER_KEY_FILE).exists());
        assert!(tokens.remove("wss://hub.acme.com"));
        assert!(!tokens.remove("wss://hub.acme.com"));
        tokens.save().unwrap();
        assert!(
            Tokens::load_from(&dir)
                .unwrap()
                .get("wss://hub.acme.com")
                .is_none()
        );
    }
}
