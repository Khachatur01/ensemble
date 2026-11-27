//! The operations of the hub's task picker (`ensemble_config::picker`): the
//! decision models that choose who works on each task, and the questions
//! they are asked first.
//!
//! | Operation | Does |
//! |---|---|
//! | `tasks.picker.show` | the task picker: its decision models, the least confidence of the worker, its questions |
//! | `tasks.picker.set` | the decision models, in order, and the least confidence of the worker; or none |
//! | `tasks.picker.question` | add a question, or change the one of its name |
//! | `tasks.picker.remove-question` | take a question out |
//! | `tasks.picker.template` | replace the questions with a starting template |
//!
//! Each changes the hub's settings through `Kept`, as every setting does.

use crate::accounts::HubContext;
use crate::goals::{edit, model_of};
use ensemble_config::ann::Use;
use ensemble_config::kept::{Change, WithKept};
use ensemble_config::picker::{NO, TEMPLATES, YES, template, valid_word};
use ensemble_config::{
    Effect, ModelSetting, PickerAnswer, PickerQuestion, QuestionKind, TaskPickerSetting,
};
use ensemble_ops::{Access, Caller, Error, Field, Kind, Operation, Spec, operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ADMIN: Access = Access::Permission("admin");

const SET: &[Field] = &[
    Field::new(
        "models",
        Kind::List,
        "The decision models asked, in order, each <provider>:<model>, a decision model of the hub's ANN pool, such as laya:laya-1; the next is asked when one fails. Unchanged when left out.",
    )
    .positional(),
    Field::text(
        "min_confidence",
        "The least probability, from 0 to 1, the worker chosen must have, such as 0.6; less, and a person chooses. Needed the first time.",
    ),
    Field::new(
        "off",
        Kind::Flag,
        "No task picker: the hub gives no task to a runner.",
    ),
    Field::new(
        "no_check",
        Kind::Flag,
        "Do not ask the providers whether they serve the models now.",
    ),
];
const QUESTION_NAME: Field =
    Field::text("name", "The question's name, one word, such as difficulty.")
        .required()
        .positional();
const QUESTION: &[Field] = &[
    QUESTION_NAME,
    Field::new(
        "type",
        Kind::Choice(QuestionKind::NAMES),
        "choice: one of its answers; score: one of its answers, ordered lowest first; noul: yes or no.",
    )
    .required(),
    Field::text(
        "instructions",
        "The question, as the decision model reads it, such as: How hard is this task?",
    )
    .required(),
    Field::text(
        "min_confidence",
        "The least probability, from 0 to 1, its answer must have, such as 0.6; less, and a person chooses the worker.",
    )
    .required(),
    Field::text(
        "answers",
        "For a choice or a score, its answers, one per line, each `name: what it means`, a score's lowest first. A noul's are yes and no.",
    ),
    Field::new(
        "effects",
        Kind::List,
        "What an answer means for the worker, each <answer>:<effect>: min_tier=<small|medium|strong>, min_context=<tokens>, needs_tools, prefer=<harness or model>, avoid=<harness or model>, model_alone (a model alone answers it, with no harness); such as hard:min_tier=strong.",
    ),
];
const NAMED: &[Field] = &[QUESTION_NAME];
const TEMPLATE_NAMES: &[&str] = &["basic"];
const TEMPLATE: &[Field] = &[Field::new(
    "name",
    Kind::Choice(TEMPLATE_NAMES),
    "The template: basic asks whether the task only needs an answer, how hard it is and whether it needs to run tools.",
)
.required()
.positional()];

#[derive(Deserialize, Serialize)]
struct Set {
    #[serde(default)]
    models: Vec<String>,
    min_confidence: Option<String>,
    #[serde(default)]
    off: bool,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize, Serialize)]
struct Question {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    instructions: String,
    min_confidence: String,
    answers: Option<String>,
    #[serde(default)]
    effects: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Named {
    name: String,
}

#[derive(Deserialize, Serialize)]
struct Nothing {}

/// The task picker's operations.
pub fn operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "tasks.picker.show",
                summary: "Show the task picker: the decision models that choose who works on each task, and what they are asked.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| show(c),
        ),
        operation(
            Spec {
                name: "tasks.picker.set",
                summary: "Choose the decision models that choose who works on each task, in order, and how sure of the worker they must be; or none, and no task is given to a runner.",
                access: ADMIN,
                input: SET,
            },
            |c: &HubContext, caller: &Caller, input: Set| {
                let change = changed(caller, "tasks.picker.set", &input);
                set(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "tasks.picker.question",
                summary: "Add a question the decision models are asked of each task before they choose its worker, or change the one of its name.",
                access: ADMIN,
                input: QUESTION,
            },
            |c: &HubContext, caller: &Caller, input: Question| {
                let change = changed(caller, "tasks.picker.question", &input);
                question(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "tasks.picker.remove-question",
                summary: "Take a question of the task picker out.",
                access: ADMIN,
                input: NAMED,
            },
            |c: &HubContext, caller: &Caller, input: Named| {
                let change = changed(caller, "tasks.picker.remove-question", &input);
                let said = format!("the question {} is gone", input.name);
                edit(c, change, said, |s| {
                    let picker = picker_of(&mut s.task_picker)?;
                    let before = picker.questions.len();
                    picker.questions.retain(|q| q.name != input.name);
                    if picker.questions.len() == before {
                        return Err(Error::not_found(format!(
                            "there is no question '{}'",
                            input.name
                        ))
                        .on("name"));
                    }
                    Ok(())
                })
            },
        ),
        operation(
            Spec {
                name: "tasks.picker.template",
                summary: "Replace the task picker's questions with a starting template, to change as you like.",
                access: ADMIN,
                input: TEMPLATE,
            },
            |c: &HubContext, caller: &Caller, input: Named| {
                let change = changed(caller, "tasks.picker.template", &input);
                let questions = template(&input.name).ok_or_else(|| {
                    Error::invalid(format!("there is no template '{}'", input.name)).on("name")
                })?;
                let said = format!(
                    "the questions are the template {}: {}",
                    input.name,
                    TEMPLATES
                        .iter()
                        .find(|(n, _)| *n == input.name)
                        .map_or("", |(_, what)| *what)
                );
                edit(c, change, said, |s| {
                    picker_of(&mut s.task_picker)?.questions = questions.clone();
                    Ok(())
                })
            },
        ),
    ]
}

/// The change `caller` makes with the operation `name` and `input`.
fn changed(caller: &Caller, name: &str, input: &impl Serialize) -> Change {
    let spec = operations()
        .into_iter()
        .find(|o| o.spec.name == name)
        .map(|o| o.spec)
        .expect("one of these operations");
    Change::new(caller, &spec, input)
}

/// The task picker, once its decision models are chosen.
fn picker_of(picker: &mut Option<TaskPickerSetting>) -> Result<&mut TaskPickerSetting, Error> {
    picker.as_mut().ok_or_else(|| {
        Error::invalid("there is no task picker yet")
            .hint("choose its decision models first: `picker set <provider>:<model> --min-confidence 0.6`")
    })
}

fn confidence(given: &str, field: &str) -> Result<f64, Error> {
    given
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|m| (0.0..=1.0).contains(m))
        .ok_or_else(|| {
            Error::invalid(format!("`{field}` must be from 0 to 1, such as 0.6")).on(field)
        })
}

fn set(c: &HubContext, input: Set, change: Change) -> Result<Value, Error> {
    if input.off {
        if !input.models.is_empty() || input.min_confidence.is_some() {
            return Err(Error::invalid("give models, or `off`, not both").on("off"));
        }
        return edit(
            c,
            change,
            "there is no task picker: the hub gives no task to a runner".into(),
            |s| {
                s.task_picker = None;
                Ok(())
            },
        );
    }
    let mut models = Vec::new();
    for given in &input.models {
        let (provider, model) = model_of(c, given, "models", Use::Decision, input.no_check)?;
        models.push(ModelSetting {
            provider: provider.to_string(),
            model: model.to_string(),
        });
    }
    let min = input
        .min_confidence
        .as_deref()
        .map(|m| confidence(m, "min_confidence"))
        .transpose()?;
    let now = c.kept()?.read()?.task_picker;
    let picker = match now {
        Some(mut p) => {
            if models.is_empty() && min.is_none() {
                return Err(Error::invalid("give the models, `min_confidence`, or both"));
            }
            if !models.is_empty() {
                p.decision_models = models;
            }
            if let Some(m) = min {
                p.worker_min_confidence = m;
            }
            p
        }
        None => {
            if models.is_empty() {
                return Err(Error::invalid("name at least one decision model").on("models"));
            }
            let Some(m) = min else {
                return Err(Error::invalid(
                    "say how sure of the worker the decision models must be, from 0 to 1, such as 0.6",
                )
                .on("min_confidence"));
            };
            TaskPickerSetting {
                decision_models: models,
                worker_min_confidence: m,
                questions: Vec::new(),
            }
        }
    };
    let names: Vec<String> = picker
        .decision_models
        .iter()
        .map(|m| format!("{}:{}", m.provider, m.model))
        .collect();
    let said = format!(
        "{} {} who works on each task, sure of the worker at least {}",
        names.join(", then "),
        if names.len() == 1 {
            "chooses"
        } else {
            "choose"
        },
        picker.worker_min_confidence
    );
    edit(c, change, said, |s| {
        s.task_picker = Some(picker.clone());
        Ok(())
    })
}

fn question(c: &HubContext, input: Question, change: Change) -> Result<Value, Error> {
    let name = input.name.trim().to_string();
    if !valid_word(&name) {
        return Err(Error::invalid(format!(
            "'{name}' must be one word of letters, digits, '-' or '_'"
        ))
        .on("name"));
    }
    let kind = QuestionKind::parse(&input.kind).ok_or_else(|| {
        Error::invalid(format!("'{}' is not choice, score or noul", input.kind)).on("type")
    })?;
    let instructions = input.instructions.trim();
    if instructions.is_empty() {
        return Err(Error::invalid("write the question").on("instructions"));
    }
    let min_confidence = confidence(&input.min_confidence, "min_confidence")?;
    let given = input.answers.as_deref().unwrap_or("").trim();
    let mut answers: Vec<PickerAnswer> = match kind {
        QuestionKind::Noul => {
            if !given.is_empty() {
                return Err(
                    Error::invalid("a noul's answers are yes and no; give none").on("answers")
                );
            }
            [YES, NO]
                .into_iter()
                .map(|a| PickerAnswer {
                    name: a.into(),
                    description: String::new(),
                    effects: Vec::new(),
                })
                .collect()
        }
        QuestionKind::Choice | QuestionKind::Score => {
            let mut answers = Vec::new();
            for line in given.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let Some((a, what)) = line.split_once(':') else {
                    return Err(
                        Error::invalid(format!("'{line}' is not `name: what it means`"))
                            .on("answers"),
                    );
                };
                answers.push(PickerAnswer {
                    name: a.trim().to_string(),
                    description: what.trim().to_string(),
                    effects: Vec::new(),
                });
            }
            if answers.len() < 2 {
                return Err(Error::invalid(
                    "give two answers or more, one per line, each `name: what it means`",
                )
                .on("answers"));
            }
            answers
        }
    };
    for given in &input.effects {
        let (answer, effect) = given.split_once(':').ok_or_else(|| {
            Error::invalid(format!(
                "'{given}' is not <answer>:<effect>, such as hard:min_tier=strong"
            ))
            .on("effects")
        })?;
        let effect = Effect::parse(effect).map_err(|e| Error::invalid(e).on("effects"))?;
        let answer = answer.trim();
        let Some(found) = answers.iter_mut().find(|a| a.name == answer) else {
            let names: Vec<&str> = answers.iter().map(|a| a.name.as_str()).collect();
            return Err(Error::invalid(format!(
                "'{answer}' is not an answer of the question: {}",
                names.join(", ")
            ))
            .on("effects"));
        };
        found.effects.push(effect);
    }
    let question = PickerQuestion {
        name: name.clone(),
        instructions: instructions.to_string(),
        min_confidence,
        kind,
        answers,
    };
    let said = format!("the decision models are asked {name}");
    edit(c, change, said, |s| {
        let picker = picker_of(&mut s.task_picker)?;
        match picker.questions.iter_mut().find(|q| q.name == name) {
            Some(q) => *q = question.clone(),
            None => picker.questions.push(question.clone()),
        }
        Ok(())
    })
}

/// The task picker as it is, as data and as lines to read.
fn show(c: &HubContext) -> Result<Value, Error> {
    let Some(picker) = c.kept()?.read()?.task_picker else {
        return Ok(json!({
            "picker": null,
            "said": "There is no task picker: the hub gives no task to a runner. `picker set <provider>:<model> --min-confidence 0.6` chooses its decision models.",
        }));
    };
    let mut said = vec![format!(
        "Decision models, in order: {}",
        picker
            .decision_models
            .iter()
            .map(|m| format!("{}:{}", m.provider, m.model))
            .collect::<Vec<_>>()
            .join(", ")
    )];
    said.push(format!(
        "The worker chosen is at least {} likely.",
        picker.worker_min_confidence
    ));
    if picker.questions.is_empty() {
        said.push("No question is asked first: the worker is chosen from the task alone.".into());
    }
    for q in &picker.questions {
        said.push(format!(
            "\n{} ({}, at least {} sure): {}",
            q.name,
            q.kind.as_str(),
            q.min_confidence,
            q.instructions
        ));
        for a in &q.answers {
            let effects: Vec<String> = a.effects.iter().map(Effect::written).collect();
            let mut line = format!("  {}", a.name);
            if !a.description.is_empty() {
                line.push_str(&format!(": {}", a.description));
            }
            if !effects.is_empty() {
                line.push_str(&format!("  => {}", effects.join(", ")));
            }
            said.push(line);
        }
    }
    Ok(json!({ "picker": picker, "said": said.join("\n") }))
}
