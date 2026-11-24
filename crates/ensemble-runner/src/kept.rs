//! Where the runner keeps its settings, the keys of its backends and its hub
//! tokens: in typed tables of its core's database,
//! `<config-dir>/runner/core/database.db`, so that its operations change
//! them, and where it runs, such as the NixOS module, only runs it. The
//! backends, the keys, sealed by the `master.key` beside the database, and
//! the audit log are kept as a hub's are ([`ensemble_store::kept`]). Its
//! teams are the folders of its teams directory, beside its core.
//!
//! A variable is looked for in the environment, then among the keys kept.

use ensemble_config::kept::{Change, Keep};
use ensemble_config::{Runner, RunnerSettings};
use ensemble_core::state::RunnerDir;
use ensemble_link::tokens::normal;
use ensemble_store::kept::{self, audited, backend_rows, int, num, read_backends, some_text, txt};
use ensemble_store::{optional, rows, text, transaction};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What keeps a backend's key, for what is said.
const PROGRAM: &str = "ensemble-runner";

/// What the runner's settings are called in what is said.
pub const SETTINGS_NAME: &str = "the runner's settings";

/// The tables of a runner's settings, besides the backends, and of its hub
/// tokens, each sealed by the master key.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runner_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    name TEXT NOT NULL,
    workspace TEXT NOT NULL,
    hub TEXT,
    max_shared_calls INTEGER NOT NULL
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
                "SELECT name, workspace, hub, max_shared_calls FROM runner_settings WHERE id = 1",
                vec![],
            )
            .await?;
            let Some(r) = found.first() else {
                return Ok(None);
            };
            let (backends, shares) = read_backends(c).await?;
            Ok(Some(RunnerSettings {
                name: some_text(&r[0], "name")?,
                workspace: some_text(&r[1], "workspace")?,
                hub: text(&r[2]),
                backends,
                shares,
                max_shared_calls: num(&r[3], "max_shared_calls")?,
            }))
        })
    }

    fn write(&self, s: &RunnerSettings, change: &Change) -> Result<(), String> {
        let mut statements = vec![
            ("DELETE FROM runner_settings", vec![]),
            (
                "INSERT INTO runner_settings (id, name, workspace, hub, max_shared_calls) VALUES (1, ?1, ?2, ?3, ?4)",
                vec![
                    txt(&s.name),
                    txt(&s.workspace),
                    optional(s.hub.as_deref()),
                    int(s.max_shared_calls),
                ],
            ),
        ];
        statements.extend(backend_rows(&s.backends, &s.shares));
        statements.push(audited(change));
        core(&self.dir, async |c| transaction(c, statements).await)
    }
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

/// What the token of the hub at `url` is sealed as: no key of a backend's.
fn sealed_as(url: &str) -> String {
    format!("hub token {url}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::BackendSetting;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn a_runners_settings_are_kept_with_its_shares_and_no_table_of_the_hub() {
        let tmp = TempDir::new();
        let dir = RunnerDir::new(tmp.0.join("runner"));
        let store = settings(&dir);
        assert_eq!(store.read().unwrap(), None);
        let mut runner = RunnerSettings::starting("laptop", "~/ensemble");
        runner.hub = Some("ws://127.0.0.1:7700".into());
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
