//! The helper teams a runner holds: one directory, `teams/`, with a folder per
//! team named for it (`~/.local/state/ensemble/runner/teams/web/`). A team is there
//! in one of three ways:
//!
//! - **made here**: a folder with its `config.yaml`, written by a person
//!   (`ensemble-runner team new`) or by an agent at the person's request, which waits
//!   for approval (a `pending` file beside it) until the person approves it;
//! - **linked**: a symbolic link to a folder elsewhere, such as a checkout of
//!   the team's repository (`ensemble-runner team link`);
//! - **pulled**: a folder with `pulled.json`, naming a git repository and the
//!   commit it is pinned to (`ensemble-runner team pull`); the team itself is the
//!   clone in the cache, checked and trusted per commit.
//!
//! The runner runs every team that is ready at once, as one [`Config`]: each
//! name of a team (its agents and limits) becomes `<team>/<name>`, so
//! agents are named `web/coder`. A team names the models its agents run
//! on by their provider in the runner's ANN pool and their id there, so a team runs on any
//! runner that has the models it names on.

use super::*;

/// The file that marks a team written by an agent: it runs once approved.
pub const PENDING_FILE: &str = "pending";
/// The file that names the repository and commit of a pulled team.
pub const PULLED_FILE: &str = "pulled.json";

/// How a team came to be in the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A folder here with its `config.yaml`.
    Made,
    /// A link to a folder elsewhere.
    Linked(PathBuf),
    /// A git repository, pinned to a commit.
    Pulled { url: String, commit: String },
}

/// One team in the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamEntry {
    pub name: String,
    pub origin: Origin,
    /// Written by an agent and not approved yet: it does not run.
    pub pending: bool,
    /// Its place in the directory.
    pub path: PathBuf,
}

impl TeamEntry {
    /// The folder with its `config.yaml`, when it is on this computer as it
    /// is: a pulled team's is the clone the caller fetches and checks.
    pub fn config_dir(&self) -> Option<PathBuf> {
        match &self.origin {
            Origin::Made => Some(self.path.clone()),
            Origin::Linked(target) => Some(target.clone()),
            Origin::Pulled { .. } => None,
        }
    }

    /// Where it comes from, in a few words.
    pub fn describe(&self) -> String {
        let origin = match &self.origin {
            Origin::Made => "made here".to_string(),
            Origin::Linked(target) => format!("linked to {}", target.display()),
            Origin::Pulled { url, commit } => format!("pulled from {url}#{commit}"),
        };
        if self.pending {
            format!(
                "{origin}, waiting for approval: ensemble-runner team approve {}",
                self.name
            )
        } else {
            origin
        }
    }
}

/// Is `name` usable as a team's name: letters, digits, `-`, `_` and `.`, not
/// starting with a dot?
pub fn valid_team_name(name: &str) -> bool {
    crate::runner::valid_name(name) && !name.starts_with('.')
}

/// The teams in `dir`, by name. A folder whose name is not a team's, or that
/// is none of the three kinds, is left out.
pub fn list(dir: &Path) -> Vec<TeamEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<TeamEntry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !valid_team_name(&name) {
                return None;
            }
            entry(dir, &name)
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The team called `name` in `dir`, if there is one.
pub fn entry(dir: &Path, name: &str) -> Option<TeamEntry> {
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    let origin = if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&path).ok()?;
        Origin::Linked(if target.is_absolute() {
            target
        } else {
            dir.join(target)
        })
    } else if path.join(PULLED_FILE).is_file() {
        let text = std::fs::read_to_string(path.join(PULLED_FILE)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        Origin::Pulled {
            url: v["url"].as_str()?.to_string(),
            commit: v["commit"].as_str()?.to_string(),
        }
    } else if path.join(CONFIG_FILENAME).is_file() {
        Origin::Made
    } else {
        return None;
    };
    Some(TeamEntry {
        name: name.to_string(),
        pending: path.join(PENDING_FILE).exists(),
        origin,
        path,
    })
}

/// Why `name` cannot be a new team in `dir`, if it cannot.
pub fn check_new(dir: &Path, name: &str) -> Result<(), String> {
    if !valid_team_name(name) {
        return Err(format!(
            "'{name}' cannot be a team's name: use letters, digits, '-', '_' or '.', such as web or pcb-design"
        ));
    }
    if std::fs::symlink_metadata(dir.join(name)).is_ok() {
        return Err(format!(
            "there is a team '{name}' already, in {}",
            dir.join(name).display()
        ));
    }
    Ok(())
}

/// Write the team `name` into `dir` from the text of its `config.yaml`, as an
/// agent asked for it: checked first, and waiting for approval, so it does
/// not run until the person has read it.
pub fn create_pending(dir: &Path, name: &str, text: &str, by: &str) -> Result<PathBuf, String> {
    check_new(dir, name)?;
    let path = dir.join(name);
    parse(text, &path.join(CONFIG_FILENAME)).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let written = std::fs::write(path.join(CONFIG_FILENAME), text).and_then(|()| {
        std::fs::write(
            path.join(PENDING_FILE),
            format!(
                "Written by {by}. It runs once you approve it: ensemble-runner team approve {name}\n"
            ),
        )
    });
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(&path);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    Ok(path)
}

/// The teams `(name, config)` as one helper team on `backends`, the providers
/// of the runner's ANN pool, which each team was checked against
/// ([`Config::with_pool`]): every name of a team becomes `<team>/<name>`,
/// but for the providers, which are the runner's. `source` is where they were
/// read from.
pub fn combine(
    teams: Vec<(String, Config)>,
    backends: BTreeMap<String, LlmBackend>,
    source: PathBuf,
) -> Config {
    let mut out = Config {
        version: SUPPORTED_VERSION,
        llm_backends: backends,
        limits: BTreeMap::new(),
        agents: BTreeMap::new(),
        source,
    };
    for (team, config) in teams {
        let named = |name: &str| format!("{team}/{name}");
        for (name, limit) in config.limits {
            out.limits.insert(named(&name), limit);
        }
        for (name, mut agent) in config.agents {
            agent.name = named(&name);
            agent.limit = agent.limit.map(|l| named(&l));
            out.agents.insert(named(&name), agent);
        }
    }
    out
}

/// The team an agent of a combined helper team belongs to: `web` of
/// `web/coder`.
pub fn team_of(agent: &str) -> Option<&str> {
    agent.split_once('/').map(|(team, _)| team)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    const TEAM: &str = "version: 1\nlimits:\n  box: { max_concurrent: 1, budget: { type: unlimited } }\nagents:\n  coder:\n    prompt: Codes.\n    max_seconds: 60\n    limit: box\n    isolation: shared\n    adapter: { type: direct, try: [{ provider: local, model: small }], when_none: fail, max_tokens: 100 }\n";

    fn loads(text: &str) -> Config {
        parse(text, Path::new("config.yaml")).unwrap()
    }

    /// The runner's providers: `local`, and `claude`, on Anthropic's API.
    fn backends() -> BTreeMap<String, LlmBackend> {
        let backend = |name: &str, settings| LlmBackend {
            name: name.into(),
            timeout_seconds: 5,
            settings,
        };
        BTreeMap::from([
            (
                "local".to_string(),
                backend(
                    "local",
                    BackendSettings::OpenAiCompatible {
                        base_url: "http://127.0.0.1:1/v1".into(),
                        api_key: None,
                    },
                ),
            ),
            (
                "claude".to_string(),
                backend(
                    "claude",
                    BackendSettings::Anthropic {
                        base_url: "https://api.anthropic.com".into(),
                        api_key: None,
                    },
                ),
            ),
        ])
    }

    #[test]
    fn combined_teams_name_everything_by_their_team() {
        let both = combine(
            vec![("web".into(), loads(TEAM)), ("pcb".into(), loads(TEAM))],
            backends(),
            PathBuf::from("/teams"),
        );
        assert_eq!(both.agent_names(), vec!["pcb/coder", "web/coder"]);
        let coder = both.agent("web/coder").unwrap();
        assert_eq!(coder.name, "web/coder");
        assert_eq!(coder.limit.as_deref(), Some("web/box"));
        let Adapter::Direct(direct) = &coder.adapter else {
            panic!("a direct agent")
        };
        assert_eq!(
            direct.fallback.candidates[0].provider, "local",
            "a provider is the runner's, named alone"
        );
        assert_eq!(both.llm_backends["local"].name, "local");
        assert!(both.limits.contains_key("pcb/box"));
        assert_eq!(team_of("web/coder"), Some("web"));
    }

    #[test]
    fn a_team_runs_on_the_sources_the_runner_has() {
        let team = loads(TEAM);
        assert!(team.llm_backends.is_empty(), "a team defines none");
        let on = team.clone().with_backends(backends()).unwrap();
        assert_eq!(on.llm_backends.len(), 2);
        let elsewhere = TEAM.replace("provider: local", "provider: gpu");
        let err = loads(&elsewhere).with_backends(backends()).unwrap_err();
        assert!(
            err.to_string().contains(
                "agents.coder.adapter.try[0]: there is no provider 'gpu'. Its providers: claude, local."
            ),
            "{err}"
        );
        let opencode = "version: 1\nlimits: {}\nagents:\n  coder:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: { type: unlimited }\n    adapter:\n      type: opencode\n      model: { type: ann_pool, try: [{ provider: claude, name: haiku, context_window: 8 }], when_none: fail }\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n";
        let err = loads(opencode).with_backends(backends()).unwrap_err();
        assert!(
            err.to_string().contains(
                "agents.coder.adapter.model.try[0]: the provider 'claude' is of type 'anthropic-api'"
            ),
            "{err}"
        );
    }
    #[test]
    fn the_directory_holds_teams_made_linked_pulled_and_waiting() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("teams");
        std::fs::create_dir_all(dir.join("made")).unwrap();
        std::fs::write(dir.join("made/config.yaml"), TEAM).unwrap();
        let elsewhere = tmp.0.join("checkout");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("linked")).unwrap();
        std::fs::create_dir_all(dir.join("pulled")).unwrap();
        std::fs::write(
            dir.join("pulled").join(PULLED_FILE),
            r#"{"url": "github.com/acme/team", "commit": "abc"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("not-a-team")).unwrap();

        let written = create_pending(&dir, "asked", TEAM, "claude-code").unwrap();
        assert!(written.join(PENDING_FILE).is_file());
        assert!(
            create_pending(&dir, "asked", TEAM, "x")
                .unwrap_err()
                .contains("already")
        );
        assert!(create_pending(&dir, "bad/name", TEAM, "x").is_err());
        let err = create_pending(&dir, "broken", "version: 1\n", "x").unwrap_err();
        assert!(err.contains("required"), "{err}");
        assert!(
            !dir.join("broken").exists(),
            "nothing is left of a broken team"
        );

        let teams = list(&dir);
        let names: Vec<&str> = teams.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["asked", "linked", "made", "pulled"]);
        assert!(teams[0].pending && teams[0].describe().contains("team approve asked"));
        assert_eq!(teams[1].origin, Origin::Linked(elsewhere.clone()));
        assert_eq!(teams[1].config_dir(), Some(elsewhere));
        assert_eq!(teams[2].origin, Origin::Made);
        assert_eq!(
            teams[3].origin,
            Origin::Pulled {
                url: "github.com/acme/team".into(),
                commit: "abc".into()
            }
        );
        assert_eq!(teams[3].config_dir(), None);
    }
}
