//! Getting started: finding what is installed, writing this computer's runner,
//! and saying what is wrong and how to fix it. The pieces that touch nothing outside take
//! what they need as arguments, so tests can run them without a terminal or a
//! network.

use ensemble_config::runner::{RUNNER_FILENAME, RUNNER_SCHEMA_URL_LINE, RUNNER_SUPPORTED_VERSION};
use ensemble_config::{Adapter, Budget, CLAUDE_CODE_AGENT, Config, OPENCODE_AGENT};
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

/// The text of a runner file: the runner `name`, private, working in
/// `workspace`, offering nothing. Checked by loading it.
pub fn runner_file(name: &str, workspace: &str) -> Result<String, String> {
    let quoted = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    let mut text = format!(
        "{RUNNER_SCHEMA_URL_LINE}\n\n# This computer's runner, which `ensemble-runner serve` runs. `ensemble-runner\n# init --stdout` prints every setting.\n\nversion: {RUNNER_SUPPORTED_VERSION}\n\nname: {}\naccess: private\n\n# Each git repository directly inside it is a project the runner works on.\nworkspace: {}\n",
        quoted(name),
        quoted(workspace)
    );
    text.push_str("offers: {}\n\n# How the runner is managed: `ensemble-runner …` on this computer.\ninterfaces:\n  cli: {}\n");
    ensemble_config::parse_runner(&text, Path::new(RUNNER_FILENAME)).map_err(|e| e.to_string())?;
    Ok(text)
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

/// What a config lets its agents do that deserves a second look. Ensemble adds
/// no sandbox of its own, so these are the agents' CLIs' own permissions.
pub fn risks(config: &Config) -> Vec<Finding> {
    let mut out = Vec::new();
    for a in config.agents.values() {
        if let Adapter::Codex(c) = &a.adapter
            && c.sandbox == "danger-full-access"
        {
            out.push(Finding::note(
                format!(
                    "{} runs with sandbox 'danger-full-access', which limits nothing",
                    a.name
                ),
                "use `workspace-write`, or `read-only` for an agent that only reads",
            ));
        }
        if let Adapter::ClaudeCode(c) = &a.adapter
            && matches!(c.permission_mode.as_str(), "bypassPermissions" | "auto")
        {
            out.push(Finding::note(
                format!(
                    "{} uses permission mode '{}', which lets it act without asking",
                    a.name, c.permission_mode
                ),
                "use `dontAsk` with a list of `tools`, which is what limits it",
            ));
        }
        if a.can_change_files() && a.isolation == ensemble_config::Isolation::Shared {
            out.push(Finding::note(
                format!(
                    "{} can change files directly in your working folder (isolation: shared)",
                    a.name
                ),
                "set `isolation: worktree` so its tasks work on a branch of their own",
            ));
        }
        if let Budget::PerHourTokens { .. } = a.budget {
            out.push(Finding::note(
                format!(
                    "{} has a token budget, which counts only the tokens its runs report",
                    a.name
                ),
                "`ensemble-runner agent stats` shows `-` for an agent whose runs report none; its budget never fills",
            ));
        }
    }
    out
}

/// Checks that need no running agent: the structure of the helper team, and
/// whether the runner's `workspace`, when it has one, holds a project.
pub fn structure(config: &Config, workspace: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    if config.agents.is_empty() {
        out.push(Finding::bad(
            "the runner has no helper team with agents",
            "make one with `ensemble-runner team new <name>`, or pull one with `ensemble-runner team pull <url>`",
        ));
    }
    if let Some(workspace) = workspace {
        let repositories = std::fs::read_dir(workspace)
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
                    "the workspace {} holds no git repository, so the runner has no project to work on",
                    workspace.display()
                ),
                "clone the repositories its tasks are for into it",
            ));
        }
    }
    out.extend(risks(config));
    if out.iter().all(|f| f.level != Level::Problem) {
        out.push(Finding::ok(format!("{} agents", config.agents.len())));
    }
    out
}

/// A health result as a finding, with what to do about it.
pub fn health_finding(
    name: &str,
    result: &Result<(), ensemble_core::port::Error>,
    config: &Config,
) -> Finding {
    match result {
        Ok(()) => Finding::ok(format!("{name}: ok")),
        Err(e) => {
            let what = format!("{name}: {e}");
            let kind = name.split(' ').next().unwrap_or(name);
            let users: Vec<&str> = config
                .agents
                .values()
                .filter(|a| a.kind() == kind || a.llm_backends().iter().any(|(b, _)| *b == name))
                .map(|a| a.name.as_str())
                .collect();
            let remove = users
                .iter()
                .map(|u| remove_command(u))
                .collect::<Vec<_>>()
                .join("; ");
            let fix = if kind == CLAUDE_CODE_AGENT || kind == OPENCODE_AGENT {
                format!(
                    "install `{kind}` and put it on PATH, or change `command` in config.yaml, or drop the agents that use it: {remove}"
                )
            } else {
                format!(
                    "start the model server, or correct `base_url` and the model names in config.yaml, or drop the agents that use it: {remove}"
                )
            };
            Finding::bad(what, fix)
        }
    }
}

/// The command that takes `agent`, such as `web/coder`, out of its team.
fn remove_command(agent: &str) -> String {
    format!("ensemble-runner agent remove {agent}")
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
    use ensemble_config::parse;
    use std::collections::BTreeMap;

    fn loads(text: &str) -> Config {
        parse(text, Path::new("config.yaml"), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    /// A team of a coder on OpenCode and a reviewer on Claude Code.
    fn team() -> String {
        use ensemble_config::edit::{NewAgent, add_agent, agent_block};
        let mut text =
            String::from("version: 1\nllm_backends: {}\nlimits: {}\ngates: {}\nagents: {}\n");
        for (name, adapter, writes) in [
            ("coder", OPENCODE_AGENT, true),
            ("reviewer", CLAUDE_CODE_AGENT, false),
        ] {
            let block = agent_block(
                name,
                &NewAgent {
                    adapter,
                    prompt: Some("Does the work."),
                    model: None,
                    backend: None,
                    writes,
                },
            )
            .unwrap();
            text = add_agent(&text, name, &block).unwrap();
        }
        text
    }

    #[test]
    fn a_runner_file_is_written_complete_and_loads() {
        let text = runner_file("desk-2", "/code").unwrap();
        let runner = ensemble_config::parse_runner(&text, Path::new("runner.yaml")).unwrap();
        assert_eq!(runner.name, "desk-2");
        assert_eq!(runner.access, ensemble_config::Access::Private);
        assert_eq!(runner.workspace, Path::new("/code"));
        assert!(runner.offers.runs.is_none() && runner.offers.models.is_none());
        assert!(runner_file("a/b", "/code").is_err(), "a name is checked");
        assert!(runner_file("desk", "").is_err(), "a workspace is needed");
    }

    #[test]
    fn a_failed_health_check_names_the_agents_to_drop_by_their_team() {
        let c = ensemble_config::teams::combine(
            vec![("web".into(), loads(&team()))],
            PathBuf::from("/teams"),
        );
        let failed = Err(ensemble_core::port::Error::Spawn {
            backend: "opencode".into(),
            program: "opencode".into(),
            detail: "not installed".into(),
        });
        let finding = health_finding("opencode (opencode)", &failed, &c);
        assert_eq!(finding.level, Level::Problem);
        let fix = finding.fix.unwrap();
        assert!(
            fix.contains("ensemble-runner agent remove web/coder"),
            "{fix}"
        );
    }

    #[test]
    fn the_structure_check_names_what_is_wrong() {
        let c = loads(&team());
        let tmp = ensemble_core::workspace::testing::TempDir::new();
        let findings = structure(&c, Some(&tmp.0));
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Note && f.what.contains("holds no git repository")),
            "{findings:?}"
        );
        std::fs::create_dir_all(tmp.0.join("shop/.git")).unwrap();
        let findings = structure(&c, Some(&tmp.0));
        assert!(
            findings.iter().all(|f| f.level == Level::Ok),
            "{findings:?}"
        );

        let bare = ensemble_config::edit::remove_agent(
            &ensemble_config::edit::remove_agent(&team(), "coder").unwrap(),
            "reviewer",
        )
        .unwrap();
        let findings = structure(&loads(&bare), None);
        assert!(
            findings.iter().any(|f| f.level == Level::Problem
                && f.what.contains("no helper team")
                && f.fix
                    .as_deref()
                    .is_some_and(|x| x.contains("ensemble-runner team new <name>"))),
            "{findings:?}"
        );
    }
}
