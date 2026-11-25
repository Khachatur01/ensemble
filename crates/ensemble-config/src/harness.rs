//! The harnesses of a runner: the programs that drive an LLM to work in a
//! repository. A harness is not an ANN, so it is a list of its own beside the
//! ANN pool. There is at most one of each kind: Claude Code, Codex and
//! OpenCode, each a CLI the runner runs, and `builtin`, Ensemble's own loop.
//! A runner has only the harnesses a person added, and Claude Code and Codex
//! drive only the models of their own vendor a person added to them. None is
//! shared: a task runs on its own account's runners.

use crate::ann::Tier;
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

    /// Whether it drives models of its own vendor's, which a person adds to
    /// it, rather than LLMs of the ANN pool.
    pub fn has_own_models(self) -> bool {
        matches!(self, HarnessKind::ClaudeCode | HarnessKind::Codex)
    }
}

/// A model of its own vendor's that Claude Code or Codex drives, as a person
/// added it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessModel {
    /// Its id, as the CLI takes it: `sonnet`, `gpt-5-codex`.
    pub id: String,
    pub tier: Tier,
    /// What a person wrote it is good and bad at: what the hub chooses a
    /// model for a task by, beside its tier. None when nobody wrote it.
    pub abilities: Option<String>,
}

/// A harness that is a CLI the runner starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliHarness {
    /// Its command was not found when the runner last looked.
    pub missing: bool,
    /// The CLI, by its name on PATH or its path.
    pub command: String,
    /// Seconds one run may take.
    pub max_seconds: u64,
    /// The most steps of one run.
    pub max_steps: u32,
    /// The most runs on it at once.
    pub max_concurrent: u32,
    /// The models of its own vendor's it drives, in the order of their ids:
    /// Claude Code's and Codex's; OpenCode drives the ANN pool's and has
    /// none.
    pub models: Vec<HarnessModel>,
}

/// Ensemble's own loop, which needs nothing installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinHarness {
    pub max_seconds: u64,
    pub max_steps: u32,
    pub max_concurrent: u32,
    /// The most tokens of answer per model call.
    pub max_tokens: u32,
}

/// A runner's harnesses as they are kept: those a person added, at most one
/// of each kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessesSetting {
    pub claude_code: Option<CliHarness>,
    pub codex: Option<CliHarness>,
    pub opencode: Option<CliHarness>,
    pub builtin: Option<BuiltinHarness>,
}

/// A change to a harness: what it names is set, the rest stays as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HarnessChange {
    pub max_seconds: Option<u64>,
    pub max_steps: Option<u32>,
    pub max_concurrent: Option<u32>,
    pub max_tokens: Option<u32>,
}

/// What every harness has, whatever its kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Harness {
    pub kind: HarnessKind,
    pub missing: bool,
    /// None for `builtin`, which runs no program.
    pub command: Option<String>,
    pub max_seconds: u64,
    pub max_steps: u32,
    pub max_concurrent: u32,
    /// The most tokens of answer per model call: `builtin`'s alone.
    pub max_tokens: Option<u32>,
    /// The models of its own vendor's it drives: Claude Code's and Codex's.
    pub models: Vec<HarnessModel>,
}

impl HarnessesSetting {
    /// Where the CLI harness of `kind` is kept, added or not; none for
    /// `builtin`.
    pub fn cli_slot(&mut self, kind: HarnessKind) -> Option<&mut Option<CliHarness>> {
        match kind {
            HarnessKind::ClaudeCode => Some(&mut self.claude_code),
            HarnessKind::Codex => Some(&mut self.codex),
            HarnessKind::OpenCode => Some(&mut self.opencode),
            HarnessKind::Builtin => None,
        }
    }

    /// The CLI harness of `kind`, when it was added; none for `builtin`.
    pub fn cli_mut(&mut self, kind: HarnessKind) -> Option<&mut CliHarness> {
        self.cli_slot(kind).and_then(Option::as_mut)
    }

    /// Each harness added, in the order of [`HarnessKind::ALL`].
    pub fn each(&self) -> Vec<Harness> {
        let cli = |kind, h: &Option<CliHarness>| {
            h.as_ref().map(|h| Harness {
                kind,
                missing: h.missing,
                command: Some(h.command.clone()),
                max_seconds: h.max_seconds,
                max_steps: h.max_steps,
                max_concurrent: h.max_concurrent,
                max_tokens: None,
                models: h.models.clone(),
            })
        };
        let builtin = self.builtin.as_ref().map(|b| Harness {
            kind: HarnessKind::Builtin,
            missing: false,
            command: None,
            max_seconds: b.max_seconds,
            max_steps: b.max_steps,
            max_concurrent: b.max_concurrent,
            max_tokens: Some(b.max_tokens),
            models: Vec::new(),
        });
        [
            cli(HarnessKind::ClaudeCode, &self.claude_code),
            cli(HarnessKind::Codex, &self.codex),
            cli(HarnessKind::OpenCode, &self.opencode),
            builtin,
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The harness of `kind`, when it was added.
    pub fn get(&self, kind: HarnessKind) -> Option<Harness> {
        self.each().into_iter().find(|h| h.kind == kind)
    }

    /// Change the harness of `kind`, when it was added, as `change` says:
    /// what it leaves out stays as it is. `max_tokens` is `builtin`'s alone.
    pub fn change(&mut self, kind: HarnessKind, change: HarnessChange) {
        let (seconds, steps, concurrent) = match kind {
            HarnessKind::Builtin => match self.builtin.as_mut() {
                Some(b) => {
                    if let Some(t) = change.max_tokens {
                        b.max_tokens = t;
                    }
                    (&mut b.max_seconds, &mut b.max_steps, &mut b.max_concurrent)
                }
                None => return,
            },
            kind => match self.cli_mut(kind) {
                Some(h) => (&mut h.max_seconds, &mut h.max_steps, &mut h.max_concurrent),
                None => return,
            },
        };
        if let Some(v) = change.max_seconds {
            *seconds = v;
        }
        if let Some(v) = change.max_steps {
            *steps = v;
        }
        if let Some(v) = change.max_concurrent {
            *concurrent = v;
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
            if h.max_concurrent == 0 {
                problems.push(format!("{at}: `max_concurrent` must be greater than 0"));
            }
            if h.max_tokens == Some(0) {
                problems.push(format!("{at}: `max_tokens` must be greater than 0"));
            }
            if h.command.as_deref().is_some_and(|c| c.trim().is_empty()) {
                problems.push(format!(
                    "{at}: `command` is empty; name the CLI, such as {}",
                    h.kind.program().unwrap_or_default()
                ));
            }
            if !h.kind.has_own_models() && !h.models.is_empty() {
                problems.push(format!(
                    "{at}: drives the LLMs of the ANN pool, and has no models of its own"
                ));
            }
            let mut seen = Vec::new();
            for m in &h.models {
                if m.id.trim().is_empty() {
                    problems.push(format!("{at}: a model has no id"));
                } else if seen.contains(&&m.id) {
                    problems.push(format!("{at}: the model {} is listed twice", m.id));
                }
                seen.push(&m.id);
            }
        }
    }
}
