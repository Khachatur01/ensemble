//! Getting started: finding what is installed, writing this computer's runner,
//! and saying what is wrong and how to fix it. The pieces that touch nothing outside take
//! what they need as arguments, so tests can run them without a terminal or a
//! network.

use ensemble_agents::picker::Means;
use ensemble_config::{HarnessKind, HarnessesSetting};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Local model servers worth looking for, with where each listens by default.
pub const LOCAL_SERVERS: [(&str, u16, &str); 4] = [
    ("ollama", 11434, "http://127.0.0.1:11434/v1"),
    ("llama-server", 8080, "http://127.0.0.1:8080/v1"),
    ("lm-studio", 1234, "http://127.0.0.1:1234/v1"),
    ("unsloth", 8888, "http://127.0.0.1:8888/v1"),
];

/// What the machine has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    pub claude: bool,
    pub opencode: bool,
    pub codex: bool,
    /// Local model servers that answered, as (name, base URL).
    pub servers: Vec<(String, String)>,
}

/// Is `program` an executable file on `PATH`? Nothing is run.
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let file = dir.join(program);
        file.is_file() && is_executable(&file)
    })
}

/// Is `command` a program this computer can run: a path to an executable
/// file, or a name on `PATH`? Nothing is run.
pub fn found(command: &str) -> bool {
    let command = command.trim();
    if command.contains(std::path::MAIN_SEPARATOR) {
        let file = Path::new(command);
        file.is_file() && is_executable(file)
    } else {
        !command.is_empty() && on_path(command)
    }
}

/// Whether the CLI of `kind` is logged in, as far as a file of its own or a
/// key in the environment says without running it: Claude Code's
/// credentials or `ANTHROPIC_API_KEY`, Codex's `auth.json` or
/// `OPENAI_API_KEY`. None where nothing says: OpenCode and `builtin` run on
/// the ANN pool, and Claude Code on macOS keeps its login in the keychain.
pub fn logged_in(kind: HarnessKind) -> Option<bool> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    let dir = |variable: &str, default: &str| {
        std::env::var_os(variable)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(default)))
    };
    match kind {
        HarnessKind::ClaudeCode => {
            let file = dir("CLAUDE_CONFIG_DIR", ".claude").map(|d| d.join(".credentials.json"));
            if set("ANTHROPIC_API_KEY") || file.is_some_and(|f| f.is_file()) {
                Some(true)
            } else if cfg!(target_os = "macos") {
                None
            } else {
                Some(false)
            }
        }
        HarnessKind::Codex => {
            let file = dir("CODEX_HOME", ".codex").map(|d| d.join("auth.json"));
            Some(set("OPENAI_API_KEY") || file.is_some_and(|f| f.is_file()))
        }
        HarnessKind::OpenCode | HarnessKind::Builtin => None,
    }
}

/// Mark each CLI harness of `harnesses` found or missing, as its command is
/// now; say which changed.
pub fn look_for(harnesses: &mut HarnessesSetting) -> Vec<String> {
    let mut said = Vec::new();
    for kind in HarnessKind::ALL {
        if let Some(h) = harnesses.cli_mut(kind) {
            let missing = !found(&h.command);
            if missing != h.missing {
                said.push(format!(
                    "{} {}",
                    kind.as_str(),
                    if missing {
                        "is missing now"
                    } else {
                        "is found"
                    }
                ));
                h.missing = missing;
            }
        }
    }
    said
}

#[cfg(unix)]
fn is_executable(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    true
}

/// Look for the agent CLIs, and for local model servers when `probe` is set.
pub fn detect(probe: bool) -> Detected {
    let servers = if probe {
        LOCAL_SERVERS
            .iter()
            .filter(|(_, port, _)| {
                let addr = SocketAddr::from(([127, 0, 0, 1], *port));
                TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
            })
            .map(|(name, _, url)| (name.to_string(), url.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    Detected {
        claude: on_path("claude"),
        opencode: on_path("opencode"),
        codex: on_path("codex"),
        servers,
    }
}

/// The workspace `setup` offers: a folder in the home directory.
pub const WORKSPACE: &str = "~/ensemble";

/// The settings of a new runner: `name`, working for its owner in
/// `workspace`, offering nothing, with the harnesses it finds, each off,
/// once they load.
pub fn starting(name: &str, workspace: &str) -> Result<ensemble_config::RunnerSettings, String> {
    let mut settings = ensemble_config::RunnerSettings::starting(name.trim(), workspace.trim());
    look_for(&mut settings.harnesses);
    settings
        .resolve(
            &ensemble_core::state::RunnerDir::new(""),
            &ensemble_config::SystemEnv,
        )
        .map_err(|e| e.to_string())?;
    Ok(settings)
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Worth knowing, and nothing to fix before using Ensemble.
    Note,
    Problem,
}

/// What `doctor` found, one line at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub what: String,
    /// What to do about it, as a command or a sentence.
    pub fix: Option<String>,
}

impl Finding {
    fn ok(what: impl Into<String>) -> Self {
        Finding {
            level: Level::Ok,
            what: what.into(),
            fix: None,
        }
    }

    fn note(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Finding {
            level: Level::Note,
            what: what.into(),
            fix: Some(fix.into()),
        }
    }

    fn bad(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Finding {
            level: Level::Problem,
            what: what.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Checks that start nothing: the runner's harnesses, whether it has a pair
/// to run a task on, and whether its `workspace` holds a project.
pub fn structure(runner: &ensemble_config::Runner, means: &Means) -> Vec<Finding> {
    let mut out = Vec::new();
    let on: Vec<_> = runner
        .harnesses
        .each()
        .into_iter()
        .filter(|h| h.on)
        .collect();
    for h in &on {
        let name = h.kind.as_str();
        if h.command.as_deref().is_some_and(|c| !found(c)) {
            out.push(Finding::bad(
                format!(
                    "{name} is on, and `{}` is not found",
                    h.command.as_deref().unwrap_or_default()
                ),
                format!(
                    "install it, or name it with `ensemble-runner harness set {name} --command <path>`, or `ensemble-runner harness off {name}`"
                ),
            ));
        } else if logged_in(h.kind) == Some(false) {
            out.push(Finding::bad(
                format!("{name} is on, and its CLI is not logged in"),
                format!(
                    "log in with `{}`, or `ensemble-runner harness off {name}`",
                    h.command.as_deref().unwrap_or_default()
                ),
            ));
        }
    }
    let pairs = means.pairs();
    if pairs.is_empty() && on.is_empty() {
        out.push(Finding::note(
            "the runner runs no task yet: every harness is off",
            "turn one on with `ensemble-runner harness on <harness>`, and, for opencode or builtin, a model of the pool with `ensemble-runner model on <provider>:<model>`",
        ));
    } else if pairs.is_empty() {
        out.push(Finding::bad(
            "the runner runs no task: no harness is on with a model it can drive",
            "turn a harness on with `ensemble-runner harness on <harness>`, and, for opencode or builtin, a model of the pool with `ensemble-runner model on <provider>:<model>`",
        ));
    } else {
        out.push(Finding::ok(format!(
            "it runs tasks on: {}",
            pairs
                .iter()
                .map(|p| format!("{} ({})", p.name(), p.tier.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let repositories = std::fs::read_dir(&runner.workspace)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().join(".git").exists())
                .count()
        })
        .unwrap_or(0);
    if repositories == 0 {
        out.push(Finding::note(
            format!(
                "the workspace {} holds no git repository yet",
                runner.workspace.display()
            ),
            "clone the repositories its tasks are for into it, or name each task's `repository` so the runner clones it",
        ));
    }
    out
}

/// Whether each provider of the pool with a model on answers and serves
/// those models, as findings.
pub fn providers(pool: &ensemble_config::AnnPool) -> Vec<Finding> {
    let mut wanted: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for m in pool.models.iter().filter(|m| m.on) {
        wanted.entry(&m.provider).or_default().push(&m.id);
    }
    let mut out = Vec::new();
    for (name, models) in wanted {
        let Some(backend) = pool.direct.get(name) else {
            continue;
        };
        match ensemble_backends::models::models(backend) {
            Ok(served) => {
                let gone: Vec<&str> = models
                    .into_iter()
                    .filter(|m| !served.iter().any(|s| s == m))
                    .collect();
                if gone.is_empty() {
                    out.push(Finding::ok(format!("{name}: ok")));
                } else {
                    out.push(Finding::bad(
                        format!("{name} does not serve {}", gone.join(", ")),
                        format!(
                            "`ensemble-runner provider refresh {name}`, then turn on a model it serves"
                        ),
                    ));
                }
            }
            Err(e) => out.push(Finding::bad(
                format!("{name}: {e}"),
                format!(
                    "start the model server, or correct its address with `ensemble-runner provider`; until then no run takes a model of {name}"
                ),
            )),
        }
    }
    out
}

/// The directory `~` stands for.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn a_new_runner_is_complete_and_loads() {
        let settings = starting("desk-2", "/code").unwrap();
        let runner = settings
            .resolve(&ensemble_core::state::RunnerDir::new(""), &BTreeMap::new())
            .unwrap();
        assert_eq!(runner.name, "desk-2");
        assert_eq!(runner.workspace, Path::new("/code"));
        assert!(runner.llm.direct.is_empty() && runner.llm.models.is_empty());
        assert!(starting("a/b", "/code").is_err(), "a name is checked");
        assert!(starting("desk", "").is_err(), "a workspace is needed");
    }

    #[test]
    fn a_runner_with_nothing_to_run_a_task_on_is_told_how_to_fix_it() {
        let mut settings = ensemble_config::RunnerSettings::starting("desk", "/nowhere");
        settings.harnesses.codex.on = true;
        settings.harnesses.codex.command = "/nonexistent/codex".into();
        let runner = settings
            .resolve(&ensemble_core::state::RunnerDir::new(""), &BTreeMap::new())
            .unwrap();
        let means = Means::new(Vec::new(), runner.llm.clone(), Default::default(), |_| {
            Vec::new()
        });
        let found = structure(&runner, &means);
        let said: Vec<&str> = found.iter().map(|f| f.what.as_str()).collect();
        assert!(
            said[0].starts_with("codex is on, and `/nonexistent/codex` is not found"),
            "{said:?}"
        );
        assert!(
            said.contains(&"the runner runs no task: no harness is on with a model it can drive")
        );
        assert!(
            found
                .iter()
                .any(|f| f.level == Level::Note && f.what.contains("no git repository"))
        );
    }
}
