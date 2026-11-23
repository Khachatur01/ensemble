//! A form for any of the hub's operations, made from what `GET /api/v1/ops`
//! says of it: its fields in order, each with the JSON Schema of its value.
//! So a page needs no form of its own for what an operation takes, and a new
//! operation's form comes with it.

use crate::api::{self, Failure};
use dioxus::prelude::*;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// The operations the signed-in account may run, as the hub lists them.
pub type Catalog = Resource<Result<Value, Failure>>;

/// Fields whose text may run to lines.
const LONG: &[&str] = &[
    "task", "text", "context", "goal", "body", "prompt", "settings",
];

/// One field of an operation's form.
#[derive(Clone, PartialEq)]
struct FieldSpec {
    name: String,
    schema: Value,
    required: bool,
}

impl FieldSpec {
    fn kind(&self) -> &str {
        if self.schema.get("enum").is_some() {
            return "choice";
        }
        match self.schema["type"].as_str() {
            Some("string") if self.schema["writeOnly"] == true => "secret",
            Some("string") if LONG.contains(&self.name.as_str()) => "long",
            Some(kind) => kind,
            None => "string",
        }
    }

    fn help(&self) -> String {
        self.schema["description"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    /// The field's name as a person reads it: `done_when` as "done when".
    fn label(&self) -> String {
        self.name.replace(['_', '-'], " ")
    }

    /// Its value in the input, from what was typed; none when left empty.
    fn value(&self, typed: &str) -> Result<Option<Value>, String> {
        let typed = typed.trim();
        if typed.is_empty() {
            return Ok(None);
        }
        Ok(Some(match self.kind() {
            "integer" => json!(
                typed
                    .parse::<u64>()
                    .map_err(|_| format!("{} is a whole number", self.label()))?
            ),
            "boolean" => json!(typed == "true"),
            "array" => json!(
                typed
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
            ),
            _ => json!(typed),
        }))
    }
}

/// The fields of the operation `name`, as the catalog lists it.
fn fields_of(catalog: &Value, name: &str) -> Option<(String, Vec<FieldSpec>)> {
    let op = catalog["operations"]
        .as_array()?
        .iter()
        .find(|o| o["name"] == name)?;
    let input = &op["input"];
    let required: Vec<&str> = input["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let fields = op["fields"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(|field| FieldSpec {
            name: field.to_string(),
            schema: input["properties"][field].clone(),
            required: required.contains(&field),
        })
        .collect();
    Some((op["summary"].as_str().unwrap_or("").to_string(), fields))
}

/// A form that runs the operation `op`. `fixed` are values of its input not
/// asked for, such as the project of the page; `values` fills fields at
/// first. Once it ran, `on_done` gets what it answered, and the form shows
/// it unless `quiet`.
#[component]
pub fn OpForm(
    op: String,
    #[props(default)] fixed: Vec<(String, Value)>,
    #[props(default)] values: Vec<(String, String)>,
    #[props(default)] submit: Option<String>,
    #[props(default)] on_done: Option<EventHandler<Value>>,
    #[props(default)] quiet: bool,
    #[props(default)] summary: bool,
) -> Element {
    let catalog = use_context::<Catalog>();
    let mut typed = use_signal(|| values.iter().cloned().collect::<HashMap<_, _>>());
    let mut failure = use_signal(|| None::<Failure>);
    let mut answered = use_signal(|| None::<Value>);
    let mut busy = use_signal(|| false);
    let Some(Ok(listed)) = &*catalog.read() else {
        return rsx! { p { class: "loading", "Loading…" } };
    };
    let Some((about, fields)) = fields_of(listed, &op) else {
        return rsx! { p { class: "muted", "You may not run {op}." } };
    };
    let shown: Vec<FieldSpec> = fields
        .into_iter()
        .filter(|f| !fixed.iter().any(|(name, _)| *name == f.name))
        .collect();
    let run = {
        let (op, shown, fixed) = (op.clone(), shown.clone(), fixed.clone());
        move |e: FormEvent| {
            e.prevent_default();
            let mut input = Map::new();
            for field in &shown {
                let text = typed.read().get(&field.name).cloned().unwrap_or_default();
                match field.value(&text) {
                    Ok(Some(value)) => {
                        input.insert(field.name.clone(), value);
                    }
                    Ok(None) => {}
                    Err(message) => {
                        failure.set(Some(Failure {
                            status: 400,
                            message,
                            field: Some(field.name.clone()),
                        }));
                        return;
                    }
                }
            }
            for (name, value) in &fixed {
                input.insert(name.clone(), value.clone());
            }
            let op = op.clone();
            spawn(async move {
                busy.set(true);
                failure.set(None);
                match api::op(&op, Value::Object(input)).await {
                    Ok(out) => {
                        answered.set(Some(out.clone()));
                        if let Some(done) = on_done {
                            done.call(out);
                        }
                    }
                    Err(e) => failure.set(Some(e)),
                }
                busy.set(false);
            });
        }
    };
    let wrong = failure.read().as_ref().and_then(|f| f.field.clone());
    rsx! {
        form { class: "op", onsubmit: run,
            if summary {
                p { class: "muted", "{about}" }
            }
            for field in shown.iter().cloned() {
                FieldInput {
                    key: "{field.name}",
                    field: field.clone(),
                    wrong: wrong.as_deref() == Some(field.name.as_str()),
                    text: typed.read().get(&field.name).cloned().unwrap_or_default(),
                    on_input: move |text: String| {
                        typed.write().insert(field.name.clone(), text);
                    },
                }
            }
            div { class: "actions",
                button { r#type: "submit", disabled: busy(),
                    {submit.clone().unwrap_or_else(|| "Run".into())}
                }
            }
            if let Some(f) = failure.read().as_ref() {
                p { class: "error", "{f}" }
            }
            if !quiet {
                if let Some(out) = answered.read().as_ref() {
                    Answer { value: out.clone() }
                }
            }
        }
    }
}

#[component]
fn FieldInput(
    field: FieldSpec,
    wrong: bool,
    text: String,
    on_input: EventHandler<String>,
) -> Element {
    let class = if wrong { "field wrong" } else { "field" };
    let label = if field.required {
        format!("{} *", field.label())
    } else {
        field.label()
    };
    let input = match field.kind() {
        "boolean" => rsx! {
            input {
                r#type: "checkbox",
                checked: text == "true",
                onchange: move |e: FormEvent| on_input.call(e.checked().to_string()),
            }
        },
        "choice" => {
            let choices: Vec<String> = field.schema["enum"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            rsx! {
                select { value: "{text}", onchange: move |e: FormEvent| on_input.call(e.value()),
                    option { value: "", "—" }
                    for choice in choices {
                        option { key: "{choice}", value: "{choice}", "{choice}" }
                    }
                }
            }
        }
        "array" | "long" => rsx! {
            textarea {
                rows: if field.kind() == "array" { "3" } else { "6" },
                placeholder: if field.kind() == "array" { "one per line" } else { "" },
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
        },
        kind => rsx! {
            input {
                r#type: match kind {
                    "secret" => "password",
                    "integer" => "number",
                    _ => "text",
                },
                autocomplete: if kind == "secret" { "new-password" } else { "off" },
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
        },
    };
    rsx! {
        label { class: "{class}",
            span { class: "name", "{label}" }
            {input}
            span { class: "help", "{field.help()}" }
        }
    }
}

/// What an operation answered: its note, a token or code shown once, and the
/// rest.
#[component]
pub fn Answer(value: Value) -> Element {
    let note = value["note"].as_str().map(String::from);
    let secret = ["token", "code"]
        .iter()
        .find_map(|k| value[*k].as_str().map(|s| (k.to_string(), s.to_string())));
    let mut rest = value.clone();
    if let Some(fields) = rest.as_object_mut() {
        for key in ["note", "said", "token", "code"] {
            fields.remove(key);
        }
    }
    rsx! {
        div { class: "answer",
            if let Some((kind, secret)) = secret {
                p { class: "secret",
                    "The {kind}, shown once:"
                    code { "{secret}" }
                }
            }
            if let Some(note) = note {
                p { "{note}" }
            }
            if rest.as_object().is_some_and(|f| !f.is_empty()) {
                Json { value: rest }
            }
        }
    }
}

/// A value as JSON, laid out.
#[component]
pub fn Json(value: Value) -> Element {
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    rsx! { pre { class: "json", "{text}" } }
}
