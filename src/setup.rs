//! Getting started: finding what is installed, writing a starter helper team,
//! registering Ensemble with the manager, and saying what is wrong and how to
//! fix it. The pieces that touch nothing outside take what they need as
//! arguments, so tests can run them without a terminal or a network.

use crate::config::edit::{NewAgent, add_agent, agent_block};
use crate::config::{
    Adapter, Budget, CLAUDE_CODE_AGENT, CODEX_AGENT, Config, OPENCODE_AGENT, SCHEMA_URL_LINE,
};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Local model servers worth looking for, with where each listens by default.
pub const LOCAL_SERVERS: [(&str, u16, &str); 3] = [
    ("ollama", 11434, "http://127.0.0.1:11434/v1"),
    ("llama-server", 8080, "http://127.0.0.1:8080/v1"),
    ("lm-studio", 1234, "http://127.0.0.1:1234/v1"),
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

/// A starter helper team: the agents it begins with.
#[derive(Debug)]
pub struct Starter {
    pub name: &'static str,
    /// (name, what it is for, whether it may change files)
    pub agents: &'static [(&'static str, &'static str, bool)],
}

pub const STARTERS: [Starter; 3] = [
    Starter {
        name: "software",
        agents: &[
            (
                "coder",
                "You implement exactly what is asked, in the style of the code around it. Read the relevant files first, make the change, and report what you changed.",
                true,
            ),
            (
                "reviewer",
                "You review code for correctness and report findings as file:line — problem — fix. You do not rewrite the code.",
                false,
            ),
        ],
    },
    Starter {
        name: "hardware",
        agents: &[
            (
                "designer",
                "You change KiCad schematics and board files as asked, and say what you changed and why.",
                true,
            ),
            (
                "checker",
                "You check KiCad projects for electrical and layout problems and report each with where it is. You do not edit.",
                false,
            ),
        ],
    },
    Starter {
        name: "docs",
        agents: &[
            (
                "writer",
                "You write documentation as asked, in the voice of what is already there, and say what you wrote.",
                true,
            ),
            (
                "editor",
                "You read documentation for mistakes, gaps and unclear passages and report each with where it is. You do not edit.",
                false,
            ),
        ],
    },
];

/// The starter called `name`.
pub fn starter(name: &str) -> Result<&'static Starter, String> {
    STARTERS.iter().find(|s| s.name == name).ok_or_else(|| {
        format!(
            "no starter '{name}'. Starters: {}.",
            STARTERS
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The text of a starter helper team built for what is `found`: agents that
/// change files run on OpenCode when it is there, reviewers on Claude Code, and
/// either stands in for the other. A local model server that answered becomes
/// an LLM backend, ready for an agent to name.
pub fn starter_team(starter: &Starter, found: &Detected) -> Result<String, String> {
    if !found.claude && !found.opencode && !found.codex {
        return Err("no agent CLI found on PATH. Install `claude` (Claude Code), `opencode` or `codex`, then run this again.".into());
    }
    let mut text = format!("{SCHEMA_URL_LINE}\n\nversion: 3\n\n");
    if found.servers.is_empty() {
        text.push_str("llm_backends: {}\n\n");
    } else {
        text.push_str("llm_backends:\n");
        for (name, url) in &found.servers {
            text.push_str(&format!(
                "  {name}:\n    type: openai-compatible-api\n    base_url: \"{url}\"\n    timeout_seconds: 600\n"
            ));
        }
        text.push('\n');
    }
    text.push_str("limits: {}\n\ntracker:\n  type: local_markdown\n\ngates: {}\n\nagents: {}\n");
    for (name, prompt, writes) in starter.agents {
        // Writers go to OpenCode, then Codex, then Claude Code; readers to Claude
        // Code, then Codex, then OpenCode: whichever of them is there.
        let order: [(&str, bool); 3] = if *writes {
            [
                (OPENCODE_AGENT, found.opencode),
                (CODEX_AGENT, found.codex),
                (CLAUDE_CODE_AGENT, found.claude),
            ]
        } else {
            [
                (CLAUDE_CODE_AGENT, found.claude),
                (CODEX_AGENT, found.codex),
                (OPENCODE_AGENT, found.opencode),
            ]
        };
        let adapter = order
            .iter()
            .find(|(_, there)| *there)
            .map_or(CLAUDE_CODE_AGENT, |(kind, _)| kind);
        let block = agent_block(
            name,
            &NewAgent {
                adapter,
                prompt: Some(prompt),
                model: None,
                backend: None,
                writes: *writes,
            },
        )?;
        text = add_agent(&text, name, &block)?;
    }
    Ok(text)
}

/// The manager CLIs `connect` knows how to register with.
pub const MANAGERS: [&str; 3] = [CLAUDE_CODE_AGENT, OPENCODE_AGENT, CODEX_AGENT];

/// What registers Ensemble with a manager: the program and its arguments.
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

/// How to register `ensemble mcp` with `manager`, serving the helper team at
/// `team` and, when there is one, the hub at `hub` (absolute paths).
pub fn registration(
    manager: &str,
    ensemble: &str,
    team: &Path,
    hub: Option<&Path>,
    env_file: Option<&Path>,
    global: bool,
) -> Result<Registration, String> {
    let mut serve = vec![
        ensemble.to_string(),
        "mcp".into(),
        "--team".into(),
        team.display().to_string(),
    ];
    if let Some(hub) = hub {
        serve.push("--hub".into());
        serve.push(hub.display().to_string());
    }
    if let Some(env_file) = env_file {
        serve.push("--env-file".into());
        serve.push(env_file.display().to_string());
    }
    with_manager(manager, global, serve)
}

/// How to register `ensemble mcp` with `manager` as a session of the running
/// node `node` names (its file), else the usual node.
pub fn node_registration(
    manager: &str,
    ensemble: &str,
    node: Option<&Path>,
    global: bool,
) -> Result<Registration, String> {
    let mut serve = vec![ensemble.to_string(), "mcp".into()];
    if let Some(node) = node {
        serve.push("--node".into());
        serve.push(node.display().to_string());
    }
    with_manager(manager, global, serve)
}

/// `manager`'s own command that adds the server `serve` starts.
fn with_manager(manager: &str, global: bool, serve: Vec<String>) -> Result<Registration, String> {
    let (program, mut args): (&'static str, Vec<String>) = match manager {
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
                "unknown manager '{other}'. Managers: {}.",
                MANAGERS.join(", ")
            ));
        }
    };
    args.extend(serve);
    Ok(Registration { program, args })
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
                "`ensemble stats` shows `-` for an agent whose runs report none; its budget never fills",
            ));
        }
    }
    out
}

/// Checks that need no running agent: the structure of the helper team.
pub fn structure(config: &Config, team: &Path, workdir: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    if config.agents.is_empty() {
        out.push(Finding::bad(
            "the helper team has no agents",
            format!(
                "ensemble add-agent <name> --adapter opencode --team {}",
                team.display()
            ),
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
    team: &Path,
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
                .map(|u| format!("ensemble remove-agent {u} --team {}", team.display()))
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

    fn both() -> Detected {
        Detected {
            claude: true,
            opencode: true,
            codex: false,
            servers: Vec::new(),
        }
    }

    fn loads(text: &str) -> Config {
        parse(text, Path::new("config.yaml"), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    #[test]
    fn every_starter_is_a_valid_helper_team_for_every_set_of_clis() {
        for s in &STARTERS {
            for (claude, opencode, codex) in [
                (true, true, false),
                (true, false, false),
                (false, true, false),
                (false, false, true),
                (true, false, true),
            ] {
                let found = Detected {
                    claude,
                    opencode,
                    codex,
                    servers: Vec::new(),
                };
                let c = loads(&starter_team(s, &found).unwrap());
                assert_eq!(c.agents.len(), s.agents.len());
                for a in c.agents.values() {
                    assert!(
                        (a.kind() == CLAUDE_CODE_AGENT && claude)
                            || (a.kind() == OPENCODE_AGENT && opencode)
                            || (a.kind() == CODEX_AGENT && codex),
                        "{} runs on a CLI that is not there",
                        a.name
                    );
                }
            }
        }
    }

    #[test]
    fn writers_run_on_opencode_and_reviewers_on_claude_code_when_both_are_there() {
        let c = loads(&starter_team(starter("software").unwrap(), &both()).unwrap());
        assert_eq!(c.agents["coder"].kind(), OPENCODE_AGENT);
        assert_eq!(c.agents["reviewer"].kind(), CLAUDE_CODE_AGENT);
        assert!(c.agents["coder"].can_change_files());
        assert!(!c.agents["reviewer"].can_change_files());
    }

    #[test]
    fn nothing_is_written_without_an_agent_cli_and_a_server_becomes_a_backend() {
        let none = Detected::default();
        let err = starter_team(starter("docs").unwrap(), &none).unwrap_err();
        assert!(err.contains("claude") && err.contains("opencode"), "{err}");
        let with = Detected {
            servers: vec![("ollama".into(), "http://127.0.0.1:11434/v1".into())],
            ..both()
        };
        let c = loads(&starter_team(starter("docs").unwrap(), &with).unwrap());
        assert!(c.llm_backends.contains_key("ollama"));
        assert!(starter("nope").unwrap_err().contains("software"));
    }

    #[test]
    fn registrations_name_the_team_and_the_scope() {
        let org = Path::new("/home/me/org");
        let r = registration("claude-code", "ensemble", org, None, None, false).unwrap();
        assert_eq!(
            r.line(),
            "claude mcp add --scope local ensemble -- ensemble mcp --team /home/me/org"
        );
        let r = registration(
            "claude-code",
            "ensemble",
            org,
            Some(Path::new("/home/me/hub")),
            Some(Path::new("/k.env")),
            true,
        )
        .unwrap();
        assert!(
            r.line()
                .contains("--team /home/me/org --hub /home/me/hub --env-file"),
            "{}",
            r.line()
        );
        assert!(
            r.line()
                .starts_with("claude mcp add --scope user ensemble -- ")
        );
        assert!(r.line().ends_with("--env-file /k.env"), "{}", r.line());
        let r = registration("opencode", "ensemble", org, None, None, true).unwrap();
        assert_eq!(
            r.line(),
            "opencode mcp add --global ensemble -- ensemble mcp --team /home/me/org"
        );
        let r = registration("opencode", "ensemble", Path::new("/a b"), None, None, false).unwrap();
        assert!(r.line().ends_with("--team '/a b'"), "{}", r.line());
        let r = registration("codex", "ensemble", org, None, None, true).unwrap();
        assert_eq!(
            r.line(),
            "codex mcp add ensemble -- ensemble mcp --team /home/me/org"
        );
        assert!(registration("emacs", "ensemble", org, None, None, false).is_err());
        let r = node_registration("claude-code", "ensemble", None, false).unwrap();
        assert_eq!(
            r.line(),
            "claude mcp add --scope local ensemble -- ensemble mcp"
        );
        let r =
            node_registration("codex", "ensemble", Some(Path::new("/n/gpu.yaml")), true).unwrap();
        assert!(
            r.line().ends_with("ensemble mcp --node /n/gpu.yaml"),
            "{}",
            r.line()
        );
    }

    #[test]
    fn the_structure_check_names_what_is_wrong() {
        let c = loads(&starter_team(starter("software").unwrap(), &both()).unwrap());
        let tmp = crate::workspace::testing::TempDir::new();
        let findings = structure(&c, Path::new("/org"), &tmp.0);
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Note && f.what.contains("not a git repository")),
            "{findings:?}"
        );
        std::fs::create_dir(tmp.0.join(".git")).unwrap();
        let findings = structure(&c, Path::new("/org"), &tmp.0);
        assert!(
            findings.iter().all(|f| f.level == Level::Ok),
            "{findings:?}"
        );

        let bare = crate::config::edit::remove_agent(
            &crate::config::edit::remove_agent(
                &starter_team(starter("software").unwrap(), &both()).unwrap(),
                "coder",
            )
            .unwrap(),
            "reviewer",
        )
        .unwrap();
        let findings = structure(&loads(&bare), Path::new("/org"), &tmp.0);
        assert!(
            findings.iter().any(|f| f.level == Level::Problem
                && f.what.contains("no agents")
                && f.fix
                    .as_deref()
                    .is_some_and(|x| x.contains("add-agent <name>"))),
            "{findings:?}"
        );
    }
}
