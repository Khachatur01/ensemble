//! The harnesses of a runner: the programs that drive an LLM to work in a
//! repository. A harness is not an ANN, so it is a list of its own beside the
//! ANN pool. There is one of each kind: Claude Code, Codex and OpenCode,
//! each a CLI the runner looks for on PATH, and `builtin`, Ensemble's own
//! loop, which is always there. Each is off until a person turns it on, and
//! none is shared: a task runs on its own account's runners.

use serde::{Deserialize, Serialize};

/// A kind of harness, as it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum HarnessKind {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    #[serde(rename = "builtin")]
    Builtin,
}

impl HarnessKind {
    pub const ALL: [HarnessKind; 4] = [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::OpenCode,
        HarnessKind::Builtin,
    ];

    pub const NAMES: &'static [&'static str] = &["claude-code", "codex", "opencode", "builtin"];

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Codex => "codex",
            HarnessKind::OpenCode => "opencode",
            HarnessKind::Builtin => "builtin",
        }
    }

    pub fn parse(text: &str) -> Option<HarnessKind> {
        HarnessKind::ALL
            .into_iter()
            .find(|k| k.as_str() == text.trim())
    }

    /// The program it runs, as it is called on PATH; none for `builtin`.
    pub fn program(self) -> Option<&'static str> {
        match self {
            HarnessKind::ClaudeCode => Some("claude"),
            HarnessKind::Codex => Some("codex"),
            HarnessKind::OpenCode => Some("opencode"),
            HarnessKind::Builtin => None,
        }
    }
}

/// A harness that is a CLI the runner starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliHarness {
    /// Whether tasks may run on it: off until a person turns it on.
    pub on: bool,
    /// Its command was not found when the runner last looked.
    pub missing: bool,
    /// The CLI, by its name on PATH or its path.
    pub command: String,
    /// Seconds one run may take.
    pub max_seconds: u64,
    /// The most steps of one run.
    pub max_steps: u32,
}

/// Ensemble's own loop, which needs nothing installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinHarness {
    pub on: bool,
    pub max_seconds: u64,
    pub max_steps: u32,
}

/// A runner's harnesses as they are kept: one of each kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessesSetting {
    pub claude_code: CliHarness,
    pub codex: CliHarness,
    pub opencode: CliHarness,
    pub builtin: BuiltinHarness,
}

/// What every harness has, whatever its kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Harness {
    pub kind: HarnessKind,
    pub on: bool,
    pub missing: bool,
    /// None for `builtin`, which runs no program.
    pub command: Option<String>,
    pub max_seconds: u64,
    pub max_steps: u32,
}

/// Seconds one run of a new runner's harness may take.
const STARTING_MAX_SECONDS: u64 = 1800;
/// The most steps of one run of a new runner's harness.
const STARTING_MAX_STEPS: u32 = 50;

impl HarnessesSetting {
    /// The harnesses of a new runner: each off, and each CLI missing until
    /// the runner looks for it.
    pub fn starting() -> Self {
        let cli = |kind: HarnessKind| CliHarness {
            on: false,
            missing: true,
            command: kind.program().unwrap_or_default().to_string(),
            max_seconds: STARTING_MAX_SECONDS,
            max_steps: STARTING_MAX_STEPS,
        };
        HarnessesSetting {
            claude_code: cli(HarnessKind::ClaudeCode),
            codex: cli(HarnessKind::Codex),
            opencode: cli(HarnessKind::OpenCode),
            builtin: BuiltinHarness {
                on: false,
                max_seconds: STARTING_MAX_SECONDS,
                max_steps: STARTING_MAX_STEPS,
            },
        }
    }

    /// The CLI harness of `kind`; none for `builtin`.
    pub fn cli_mut(&mut self, kind: HarnessKind) -> Option<&mut CliHarness> {
        match kind {
            HarnessKind::ClaudeCode => Some(&mut self.claude_code),
            HarnessKind::Codex => Some(&mut self.codex),
            HarnessKind::OpenCode => Some(&mut self.opencode),
            HarnessKind::Builtin => None,
        }
    }

    /// Each harness, in the order of [`HarnessKind::ALL`].
    pub fn each(&self) -> Vec<Harness> {
        let cli = |kind, h: &CliHarness| Harness {
            kind,
            on: h.on,
            missing: h.missing,
            command: Some(h.command.clone()),
            max_seconds: h.max_seconds,
            max_steps: h.max_steps,
        };
        vec![
            cli(HarnessKind::ClaudeCode, &self.claude_code),
            cli(HarnessKind::Codex, &self.codex),
            cli(HarnessKind::OpenCode, &self.opencode),
            Harness {
                kind: HarnessKind::Builtin,
                on: self.builtin.on,
                missing: false,
                command: None,
                max_seconds: self.builtin.max_seconds,
                max_steps: self.builtin.max_steps,
            },
        ]
    }

    /// The harness of `kind`.
    pub fn get(&self, kind: HarnessKind) -> Harness {
        self.each()
            .into_iter()
            .find(|h| h.kind == kind)
            .expect("every kind has a harness")
    }

    /// Change the harness of `kind`: whether it is on, and how long and how
    /// many steps a run may take.
    pub fn change(
        &mut self,
        kind: HarnessKind,
        on: Option<bool>,
        max_seconds: Option<u64>,
        max_steps: Option<u32>,
    ) {
        let (now_on, seconds, steps) = match self.cli_mut(kind) {
            Some(h) => (&mut h.on, &mut h.max_seconds, &mut h.max_steps),
            None => (
                &mut self.builtin.on,
                &mut self.builtin.max_seconds,
                &mut self.builtin.max_steps,
            ),
        };
        if let Some(on) = on {
            *now_on = on;
        }
        if let Some(s) = max_seconds {
            *seconds = s;
        }
        if let Some(s) = max_steps {
            *steps = s;
        }
    }

    /// What is wrong with them, each said with where it is.
    pub(crate) fn problems(&self, problems: &mut Vec<String>) {
        for h in self.each() {
            let at = format!("harnesses.{}", h.kind.as_str());
            if h.max_seconds == 0 {
                problems.push(format!("{at}: `max_seconds` must be greater than 0"));
            }
            if h.max_steps == 0 {
                problems.push(format!("{at}: `max_steps` must be greater than 0"));
            }
            if h.command.as_deref().is_some_and(|c| c.trim().is_empty()) {
                problems.push(format!(
                    "{at}: `command` is empty; name the CLI, such as {}",
                    h.kind.program().unwrap_or_default()
                ));
            }
        }
    }
}
