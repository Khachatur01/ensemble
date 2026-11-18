//! Getting started: finding what is installed, writing this computer's node,
//! registering Ensemble with the agent CLI you talk to (the client), and saying
//! what is wrong and how to fix it. The pieces that touch nothing outside take
//! what they need as arguments, so tests can run them without a terminal or a
//! network.

use crate::config::node::{NODE_FILENAME, NODE_SCHEMA_URL_LINE, NODE_SUPPORTED_VERSION};
use crate::config::{Adapter, Budget, CLAUDE_CODE_AGENT, CODEX_AGENT, Config, OPENCODE_AGENT};
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

/// The agent CLIs `connect` knows how to register with: the clients.
pub const CLIENTS: [&str; 3] = [CLAUDE_CODE_AGENT, OPENCODE_AGENT, CODEX_AGENT];

/// What registers Ensemble with a client: the program and its arguments.
#[derive(Debug)]
pub struct Registration {
    pub program: &'static str,
    pub args: Vec<String>,
}

impl Registration {
    /// As one line a shell would read back the same way.
    pub fn line(&self) -> String {
        std::iter::once(self.program)
            .chain(self.args.iter().map(String::as_str))
            .map(shell_word)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@,+".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// How to register `ensemble mcp` with `client`: each session it starts is a
/// session of this computer's node.
pub fn registration(client: &str, ensemble: &str, global: bool) -> Result<Registration, String> {
    let (program, mut args): (&'static str, Vec<String>) = match client {
        CLAUDE_CODE_AGENT => (
            "claude",
            vec![
                "mcp".into(),
                "add".into(),
                "--scope".into(),
                if global { "user" } else { "local" }.into(),
                "ensemble".into(),
                "--".into(),
            ],
        ),
        OPENCODE_AGENT => {
            let mut args = vec!["mcp".to_string(), "add".into()];
            if global {
                args.push("--global".into());
            }
            args.push("ensemble".into());
            args.push("--".into());
            ("opencode", args)
        }
        CODEX_AGENT => {
            // Codex keeps its servers in one config of its own, so there is
            // no scope to choose.
            (
                "codex",
                vec!["mcp".into(), "add".into(), "ensemble".into(), "--".into()],
            )
        }
        other => {
            return Err(format!(
                "unknown client '{other}'. Clients: {}.",
                CLIENTS.join(", ")
            ));
        }
    };
    args.push(ensemble.to_string());
    args.push("mcp".into());
    Ok(Registration { program, args })
}

/// The text of a node file: the node `name`, private, offering nothing.
/// Checked by loading it.
pub fn node_file(name: &str) -> Result<String, String> {
    let quoted = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    let mut text = format!(
        "{NODE_SCHEMA_URL_LINE}\n\n# This computer's node, which `ensemble node serve` runs. `ensemble node init\n# --stdout` prints every setting.\n\nversion: {NODE_SUPPORTED_VERSION}\n\nname: {}\naccess: private\n",
        quoted(name)
    );
    text.push_str("offers: {}\n");
    crate::config::parse_node(&text, Path::new(NODE_FILENAME)).map_err(|e| e.to_string())?;
    Ok(text)
}

/// The `ensemble` to register: the one on `PATH` when there is one, else this
/// very program.
pub fn ensemble_command() -> String {
    if on_path("ensemble") {
        "ensemble".into()
    } else {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "ensemble".into())
    }
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
        if a.can_change_files() && a.isolation == crate::config::Isolation::Shared {
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
                "`ensemble node agent stats` shows `-` for an agent whose runs report none; its budget never fills",
            ));
        }
    }
    out
}

/// Checks that need no running agent: the structure of the helper team.
pub fn structure(config: &Config, workdir: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    if config.agents.is_empty() {
        out.push(Finding::bad(
            "the node has no helper team with agents",
            "make one with `ensemble node team new <name>`, pull one with `ensemble node team pull <url>`, or ask your agent to make one",
        ));
    }
    let wants_git = config
        .agents
        .values()
        .any(|a| a.isolation == crate::config::Isolation::Worktree);
    if wants_git && !workdir.join(".git").exists() {
        out.push(Finding::note(
            format!(
                "{} is not a git repository, and some agents run tasks in worktrees",
                workdir.display()
            ),
            "open a repository, or give tasks a `repo` that is one",
        ));
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
    result: &Result<(), crate::port::Error>,
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
    format!("ensemble node agent remove {agent}")
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
    use crate::config::parse;
    use std::collections::BTreeMap;

    fn loads(text: &str) -> Config {
        parse(text, Path::new("config.yaml"), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    /// A team of a coder on OpenCode and a reviewer on Claude Code.
    fn team() -> String {
        use crate::config::edit::{NewAgent, add_agent, agent_block};
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
    fn a_registration_starts_a_session_of_the_node_in_the_clients_scope() {
        let r = registration("claude-code", "ensemble", false).unwrap();
        assert_eq!(
            r.line(),
            "claude mcp add --scope local ensemble -- ensemble mcp"
        );
        let r = registration("claude-code", "/opt/e b/ensemble", true).unwrap();
        assert_eq!(
            r.line(),
            "claude mcp add --scope user ensemble -- '/opt/e b/ensemble' mcp"
        );
        let r = registration("opencode", "ensemble", true).unwrap();
        assert_eq!(
            r.line(),
            "opencode mcp add --global ensemble -- ensemble mcp"
        );
        let r = registration("codex", "ensemble", true).unwrap();
        assert_eq!(r.line(), "codex mcp add ensemble -- ensemble mcp");
        assert!(
            registration("emacs", "ensemble", false)
                .unwrap_err()
                .contains("Clients: claude-code")
        );
    }

    #[test]
    fn a_node_file_is_written_complete_and_loads() {
        let text = node_file("desk-2").unwrap();
        let node = crate::config::parse_node(&text, Path::new("node.yaml")).unwrap();
        assert_eq!(node.name, "desk-2");
        assert_eq!(node.access, crate::config::Access::Private);
        assert!(node.offers.runs.is_none() && node.offers.models.is_none());
        assert!(node_file("a/b").is_err(), "a name is checked");
    }

    #[test]
    fn a_failed_health_check_names_the_agents_to_drop_by_their_team() {
        let c = crate::config::teams::combine(
            vec![("web".into(), loads(&team()))],
            PathBuf::from("/teams"),
        );
        let failed = Err(crate::port::Error::Spawn {
            backend: "opencode".into(),
            program: "opencode".into(),
            detail: "not installed".into(),
        });
        let finding = health_finding("opencode (opencode)", &failed, &c);
        assert_eq!(finding.level, Level::Problem);
        let fix = finding.fix.unwrap();
        assert!(
            fix.contains("ensemble node agent remove web/coder"),
            "{fix}"
        );
    }

    #[test]
    fn the_structure_check_names_what_is_wrong() {
        let c = loads(&team());
        let tmp = crate::workspace::testing::TempDir::new();
        let findings = structure(&c, &tmp.0);
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Note && f.what.contains("not a git repository")),
            "{findings:?}"
        );
        std::fs::create_dir(tmp.0.join(".git")).unwrap();
        let findings = structure(&c, &tmp.0);
        assert!(
            findings.iter().all(|f| f.level == Level::Ok),
            "{findings:?}"
        );

        let bare = crate::config::edit::remove_agent(
            &crate::config::edit::remove_agent(&team(), "coder").unwrap(),
            "reviewer",
        )
        .unwrap();
        let findings = structure(&loads(&bare), &tmp.0);
        assert!(
            findings.iter().any(|f| f.level == Level::Problem
                && f.what.contains("no helper team")
                && f.fix
                    .as_deref()
                    .is_some_and(|x| x.contains("ensemble node team new <name>"))),
            "{findings:?}"
        );
    }
}
