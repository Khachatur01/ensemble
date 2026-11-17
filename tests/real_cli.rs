//! Checks against the real agent CLIs, to catch a CLI upgrade that changes its
//! output. Every test is ignored by default and needs `ENSEMBLE_REAL_CLI=1`:
//!
//!     ENSEMBLE_REAL_CLI=1 cargo test --test real_cli -- --ignored
//!
//! They use your own login or key and spend a few tokens. A CLI that is not on
//! `PATH` is skipped with a note. OpenCode needs `ENSEMBLE_REAL_OPENCODE_MODEL`
//! (a provider model such as `opencode/big-pickle`).

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ensemble");

fn enabled() -> bool {
    std::env::var("ENSEMBLE_REAL_CLI").is_ok_and(|v| v == "1")
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A scratch computer, with its own XDG directories, whose node has one team,
/// `probe`, of one agent of a real CLI, and a project to work in.
struct Project(PathBuf);

impl Project {
    fn new(config: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ensemble-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let team = dir.join("config/ensemble/node/teams/probe");
        std::fs::create_dir_all(&team).unwrap();
        std::fs::create_dir_all(dir.join("work")).unwrap();
        std::fs::write(team.join("config.yaml"), config).unwrap();
        Project(dir)
    }

    fn ensemble(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .args(args)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .current_dir(self.0.join("work"))
            .output()
            .expect("run ensemble")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(agent: &str, adapter: &str) -> String {
    format!(
        "version: 1\nllm_backends: {{}}\nlimits: {{}}\ngates: {{}}\nagents:\n  {agent}:\n    prompt: Reply as briefly as you can.\n    max_seconds: 300\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n{adapter}"
    )
}

fn check(agent: &str, adapter: &str) {
    let project = Project::new(&config(agent, adapter));
    let health = project.ensemble(&["node", "agent", "health"]);
    assert!(
        health.status.success(),
        "health failed: {}{}",
        String::from_utf8_lossy(&health.stdout),
        String::from_utf8_lossy(&health.stderr)
    );
    let run = project.ensemble(&[
        "node",
        "agent",
        "run",
        &format!("probe/{agent}"),
        "Reply with the single word ok and do nothing else.",
    ]);
    let answer = String::from_utf8_lossy(&run.stdout).to_lowercase();
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(answer.contains("ok"), "unexpected answer: {answer}");
    let provenance = String::from_utf8_lossy(&run.stderr);
    assert!(
        !provenance.trim().is_empty(),
        "no provenance on stderr: the CLI's events were not read"
    );
}

#[test]
#[ignore = "needs ENSEMBLE_REAL_CLI=1 and the claude CLI, logged in"]
fn claude_code_answers_a_trivial_task() {
    if !enabled() || !on_path("claude") {
        eprintln!("skipped: ENSEMBLE_REAL_CLI=1 and `claude` on PATH are needed");
        return;
    }
    check(
        "probe",
        "      type: claude-code\n      model: haiku\n      tools: [Read]\n      permission_mode: dontAsk\n      max_turns: 3\n      command: claude\n",
    );
}

#[test]
#[ignore = "needs ENSEMBLE_REAL_CLI=1, the opencode CLI and ENSEMBLE_REAL_OPENCODE_MODEL"]
fn opencode_answers_a_trivial_task() {
    let Ok(model) = std::env::var("ENSEMBLE_REAL_OPENCODE_MODEL") else {
        eprintln!("skipped: ENSEMBLE_REAL_OPENCODE_MODEL is not set");
        return;
    };
    if !enabled() || !on_path("opencode") {
        eprintln!("skipped: ENSEMBLE_REAL_CLI=1 and `opencode` on PATH are needed");
        return;
    }
    check(
        "probe",
        &format!(
            "      type: opencode\n      model:\n        type: provider\n        name: {model}\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n"
        ),
    );
}

#[test]
#[ignore = "needs ENSEMBLE_REAL_CLI=1 and the codex CLI, logged in"]
fn codex_answers_a_trivial_task() {
    if !enabled() || !on_path("codex") {
        eprintln!("skipped: ENSEMBLE_REAL_CLI=1 and `codex` on PATH are needed");
        return;
    }
    let model = std::env::var("ENSEMBLE_REAL_CODEX_MODEL").unwrap_or_else(|_| "gpt-5-codex".into());
    check(
        "probe",
        &format!(
            "      type: codex\n      model: {model}\n      sandbox: read-only\n      command: codex\n"
        ),
    );
}
