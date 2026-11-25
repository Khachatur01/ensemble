//! The hub's task picker: the decision models that choose which worker, a
//! runner's harness and the model it drives, or a model of a runner's pool
//! alone, works on each task, tried in order, and what they are asked. One
//! for the hub, not per account.
//!
//! A person writes the questions (`questions`): each a `choice` among named
//! options, a `score` of ordered levels or a `noul`, yes or no; each answer
//! with what it means for the worker, its **effects**, from a fixed
//! vocabulary ([`Effect`]). The worker choice itself is Ensemble's: one
//! `choice` among the workers the hub knows that every answer's effects
//! allow. Each question has the least confidence its answer must have, and
//! the worker choice its own (`worker_min_confidence`); less, and the task
//! waits for a person to choose.
//!
//! Without a task picker, the hub gives no task to a runner.

use super::ann::{Known, Tier};
use super::harness::HarnessKind;
use super::hub::ModelSetting;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The task picker, as it is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPickerSetting {
    /// The decision models asked, in order: the next when one fails or
    /// does not answer in time.
    pub decision_models: Vec<ModelSetting>,
    /// The least probability, from 0 to 1, the worker chosen must have.
    pub worker_min_confidence: f64,
    /// What else they are asked, in order, before the worker is chosen.
    pub questions: Vec<PickerQuestion>,
}

/// A question a person wrote, with its answers and what each means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickerQuestion {
    /// One word, as the decision model is asked it.
    pub name: String,
    /// The question, in a person's words.
    pub instructions: String,
    /// The least probability, from 0 to 1, its answer must have.
    pub min_confidence: f64,
    /// Its answers: a `choice`'s options, a `score`'s levels, lowest first,
    /// or a `noul`'s `yes` and `no`.
    #[serde(rename = "type")]
    pub kind: QuestionKind,
    pub answers: Vec<PickerAnswer>,
}

/// What kind of question it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    Choice,
    Score,
    Noul,
}

impl QuestionKind {
    pub const NAMES: &'static [&'static str] = &["choice", "score", "noul"];

    pub fn as_str(self) -> &'static str {
        match self {
            QuestionKind::Choice => "choice",
            QuestionKind::Score => "score",
            QuestionKind::Noul => "noul",
        }
    }

    pub fn parse(text: &str) -> Option<QuestionKind> {
        match text.trim() {
            "choice" => Some(QuestionKind::Choice),
            "score" => Some(QuestionKind::Score),
            "noul" => Some(QuestionKind::Noul),
            _ => None,
        }
    }
}

/// One answer of a question: its name, what it means, and its effects on
/// the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickerAnswer {
    pub name: String,
    /// What it means, as the decision model reads it; empty for `yes` and
    /// `no`.
    pub description: String,
    pub effects: Vec<Effect>,
}

/// What an answer means for the worker. The vocabulary is fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Effect {
    /// Its model at least this strong.
    MinTier { tier: Tier },
    /// Its model with at least this many tokens of context; one whose
    /// context nobody said is taken as enough.
    MinContext { tokens: u32 },
    /// Its model makes tool calls.
    NeedsTools,
    /// A worker of this harness or model is chosen before the others the
    /// decision model ranks high enough.
    Prefer { name: String },
    /// No worker of this harness or model.
    Avoid { name: String },
    /// The task only needs an answer: a model of a runner's pool answers it
    /// alone, in one reply, with no harness, no worktree and no tools.
    /// Without it, a model alone never works on a task.
    ModelAlone,
}

impl Effect {
    /// It, as it is written: `min_tier=strong`, `needs_tools`,
    /// `prefer=claude-code`.
    pub fn written(&self) -> String {
        match self {
            Effect::MinTier { tier } => format!("min_tier={}", tier.as_str()),
            Effect::MinContext { tokens } => format!("min_context={tokens}"),
            Effect::NeedsTools => "needs_tools".into(),
            Effect::Prefer { name } => format!("prefer={name}"),
            Effect::Avoid { name } => format!("avoid={name}"),
            Effect::ModelAlone => "model_alone".into(),
        }
    }

    /// The effect `text` writes, as [`Effect::written`] writes it.
    pub fn parse(text: &str) -> Result<Effect, String> {
        let text = text.trim();
        let (what, value) = match text.split_once('=') {
            Some((w, v)) => (w.trim(), Some(v.trim())),
            None => (text, None),
        };
        let named = |value: Option<&str>| -> Result<String, String> {
            value
                .filter(|v| !v.is_empty() && !v.contains(char::is_whitespace))
                .map(str::to_string)
                .ok_or_else(|| {
                    format!("'{text}': name a harness or a model, such as {what}=claude-code")
                })
        };
        match (what, value) {
            ("min_tier", Some(v)) => Tier::parse(v)
                .map(|tier| Effect::MinTier { tier })
                .ok_or_else(|| format!("'{text}': the tier is small, medium or strong")),
            ("min_context", Some(v)) => v
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .map(|tokens| Effect::MinContext { tokens })
                .ok_or_else(|| format!("'{text}': give the tokens, such as min_context=32768")),
            ("needs_tools", None) => Ok(Effect::NeedsTools),
            ("model_alone", None) => Ok(Effect::ModelAlone),
            ("prefer", v) => Ok(Effect::Prefer { name: named(v)? }),
            ("avoid", v) => Ok(Effect::Avoid { name: named(v)? }),
            _ => Err(format!(
                "'{text}' is not an effect: min_tier=<small|medium|strong>, min_context=<tokens>, needs_tools, prefer=<harness or model>, avoid=<harness or model>, model_alone"
            )),
        }
    }

    /// The harness it names, when it names one rather than a model.
    pub fn harness(&self) -> Option<HarnessKind> {
        match self {
            Effect::Prefer { name } | Effect::Avoid { name } => HarnessKind::parse(name),
            _ => None,
        }
    }
}

/// The answers of a `noul`.
pub const YES: &str = "yes";
pub const NO: &str = "no";

/// The starting templates a person may choose, by name, with what each asks.
pub const TEMPLATES: &[(&str, &str)] = &[(
    "basic",
    "whether the task only needs an answer (then a model alone answers it), how hard it is (easy, medium or hard: the least tier of its model), and whether it needs to run tools",
)];

/// The questions of the template `name`.
pub fn template(name: &str) -> Option<Vec<PickerQuestion>> {
    let answer = |name: &str, description: &str, effects: Vec<Effect>| PickerAnswer {
        name: name.into(),
        description: description.into(),
        effects,
    };
    match name {
        "basic" => Some(vec![
            PickerQuestion {
                name: "answer_only".into(),
                instructions: "Does the task only need an answer, such as an explanation, a review or a plan, with no file to change and no command to run?".into(),
                min_confidence: 0.5,
                kind: QuestionKind::Noul,
                answers: vec![
                    answer(YES, "", vec![Effect::ModelAlone]),
                    answer(NO, "", Vec::new()),
                ],
            },
            PickerQuestion {
                name: "difficulty".into(),
                instructions: "How hard is this task for a programmer who does not know the project?".into(),
                min_confidence: 0.5,
                kind: QuestionKind::Score,
                answers: vec![
                    answer(
                        "easy",
                        "A small, clear change: a few lines in one place.",
                        vec![Effect::MinTier { tier: Tier::Small }],
                    ),
                    answer(
                        "medium",
                        "An ordinary change over a few files, which needs some understanding of the project.",
                        vec![Effect::MinTier { tier: Tier::Medium }],
                    ),
                    answer(
                        "hard",
                        "A change across the project, a design to make, or a problem whose cause is unknown.",
                        vec![Effect::MinTier { tier: Tier::Strong }],
                    ),
                ],
            },
            PickerQuestion {
                name: "tools".into(),
                instructions: "Does the task need commands run, such as building, testing or searching the code?".into(),
                min_confidence: 0.5,
                kind: QuestionKind::Noul,
                answers: vec![
                    answer(YES, "", vec![Effect::NeedsTools]),
                    answer(NO, "", Vec::new()),
                ],
            },
        ]),
        _ => None,
    }
}

/// Whether `name` is one word of letters, digits, '-' or '_', as a
/// question's or an answer's name is.
pub fn valid_word(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl TaskPickerSetting {
    /// What is wrong with it, each problem said with where it is, the
    /// decision models checked against `pool`.
    pub(crate) fn check(&self, pool: &Known<'_>, problems: &mut Vec<String>) {
        if self.decision_models.is_empty() {
            problems.push(
                "task_picker.decision_models: is empty; name a decision model of the hub's ANN pool".into(),
            );
        }
        for (i, m) in self.decision_models.iter().enumerate() {
            super::ann::check_model(
                &format!("task_picker.decision_models[{i}]"),
                &m.provider,
                &m.model,
                super::ann::Use::Decision,
                pool,
                problems,
            );
        }
        confidence(
            "task_picker.worker_min_confidence",
            self.worker_min_confidence,
            problems,
        );
        let mut names = BTreeSet::new();
        for q in &self.questions {
            let at = format!("task_picker.questions.{}", q.name);
            if !valid_word(&q.name) {
                problems.push(format!(
                    "{at}: the name must be one word of letters, digits, '-' or '_'"
                ));
            }
            if q.name == WORKER {
                problems.push(format!(
                    "{at}: `{WORKER}` is the worker choice's own name; give the question another"
                ));
            }
            if !names.insert(q.name.as_str()) {
                problems.push(format!("{at}: there are two questions of this name"));
            }
            if q.instructions.trim().is_empty() {
                problems.push(format!("{at}.instructions: is empty; write the question"));
            }
            confidence(&format!("{at}.min_confidence"), q.min_confidence, problems);
            q.check_answers(&at, problems);
        }
    }
}

/// The worker choice's name, as the decision model is asked it.
pub const WORKER: &str = "worker";

impl PickerQuestion {
    fn check_answers(&self, at: &str, problems: &mut Vec<String>) {
        let names: Vec<&str> = self.answers.iter().map(|a| a.name.as_str()).collect();
        match self.kind {
            QuestionKind::Noul => {
                if names != [YES, NO] {
                    problems.push(format!(
                        "{at}: a noul's answers are `yes` and `no`, in that order"
                    ));
                }
            }
            QuestionKind::Choice | QuestionKind::Score => {
                if self.answers.len() < 2 {
                    problems.push(format!(
                        "{at}: give it two answers or more, each `name: what it means`"
                    ));
                }
                let mut seen = BTreeSet::new();
                for a in &self.answers {
                    if !valid_word(&a.name) {
                        problems.push(format!(
                            "{at}: the answer '{}' must be one word of letters, digits, '-' or '_'",
                            a.name
                        ));
                    }
                    if !seen.insert(a.name.as_str()) {
                        problems.push(format!("{at}: there are two answers '{}'", a.name));
                    }
                    if a.description.trim().is_empty() {
                        problems.push(format!(
                            "{at}.{}: say what it means, for the decision model to read",
                            a.name
                        ));
                    }
                }
            }
        }
        for a in &self.answers {
            for e in &a.effects {
                if let Effect::Prefer { name } | Effect::Avoid { name } = e
                    && (name.trim().is_empty() || name.contains(char::is_whitespace))
                {
                    problems.push(format!(
                        "{at}.{}: '{}' names no harness or model",
                        a.name,
                        e.written()
                    ));
                }
                if let Effect::MinContext { tokens: 0 } = e {
                    problems.push(format!(
                        "{at}.{}: min_context must be greater than 0",
                        a.name
                    ));
                }
            }
        }
    }
}

fn confidence(at: &str, value: f64, problems: &mut Vec<String>) {
    if !(0.0..=1.0).contains(&value) {
        problems.push(format!("{at}: must be from 0 to 1, such as 0.6"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_effect_is_written_and_read_back() {
        for written in [
            "min_tier=strong",
            "min_context=32768",
            "needs_tools",
            "model_alone",
            "prefer=claude-code",
            "avoid=local:qwen3.5:9b",
        ] {
            assert_eq!(Effect::parse(written).unwrap().written(), written);
        }
        assert_eq!(
            Effect::parse("prefer=codex").unwrap().harness(),
            Some(HarnessKind::Codex)
        );
        assert_eq!(Effect::parse("prefer=local:x").unwrap().harness(), None);
        for wrong in [
            "min_tier=huge",
            "min_context=0",
            "prefer=",
            "faster",
            "needs_tools=1",
            "model_alone=yes",
        ] {
            assert!(Effect::parse(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_basic_template_is_valid() {
        let picker = TaskPickerSetting {
            decision_models: Vec::new(),
            worker_min_confidence: 0.5,
            questions: template("basic").unwrap(),
        };
        let mut problems = Vec::new();
        let pool = crate::AnnPoolSetting::default();
        picker.check(&pool.known(), &mut problems);
        assert_eq!(
            problems,
            vec!["task_picker.decision_models: is empty; name a decision model of the hub's ANN pool".to_string()]
        );
        assert!(template("other").is_none());
    }
}
