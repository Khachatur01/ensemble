//! A form for any of the hub's operations, made from what `GET /api/v1/ops`
//! says of it: its fields in order, each with the JSON Schema of its value.
//! So a page needs no form of its own for what an operation takes, and a new
//! operation's form comes with it.
//!
//! A page may say more than the schema can, so the form is hard to fill in
//! wrong: the values a field may take ([`Pick`]), such as the hub's roles;
//! which fields matter only for some value of another ([`When`]); which are
//! seldom needed and folded away; which secrets are typed twice; and which
//! operations ask before they run.

use crate::api::{self, Failure};
use dioxus::prelude::*;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The operations the signed-in account may run, as the hub lists them.
pub type Catalog = Resource<Result<Value, Failure>>;

/// Fields whose text may run to lines.
const LONG: &[&str] = &[
    "task", "text", "context", "goal", "body", "prompt", "settings",
];

/// Fields seldom needed, folded away on every form.
const SELDOM: &[&str] = &["no_check"];

/// The values a field may take, each with what a person reads for it. Only
/// these when `strict`, else suggestions, and what is typed goes.
#[derive(Clone, PartialEq, Debug)]
pub struct Pick {
    pub field: String,
    pub options: Vec<(String, String)>,
    pub strict: bool,
}

impl Pick {
    /// Only one of `values`, or some of them for a list.
    pub fn only(field: &str, values: impl IntoIterator<Item = String>) -> Self {
        Pick {
            field: field.into(),
            options: values.into_iter().map(|v| (v.clone(), v)).collect(),
            strict: true,
        }
    }

    /// `values` suggested, and anything else typed.
    pub fn suggest(field: &str, values: impl IntoIterator<Item = String>) -> Self {
        Pick {
            strict: false,
            ..Pick::only(field, values)
        }
    }

    /// Only these values, each with its words.
    pub fn named(field: &str, options: &[(&str, &str)]) -> Self {
        Pick {
            field: field.into(),
            options: options
                .iter()
                .map(|(v, l)| (v.to_string(), l.to_string()))
                .collect(),
            strict: true,
        }
    }
}

/// The field `field` is asked for only while the field `on` is one of
/// `values` ("" for empty).
#[derive(Clone, PartialEq, Debug)]
pub struct When {
    pub field: String,
    pub on: String,
    pub values: Vec<String>,
}

impl When {
    pub fn new(field: &str, on: &str, values: &[&str]) -> Self {
        When {
            field: field.into(),
            on: on.into(),
            values: values.iter().map(|v| v.to_string()).collect(),
        }
    }
}

/// How a field is asked for.
#[derive(Clone, PartialEq, Debug)]
enum Input {
    Text,
    Secret,
    Long,
    Number(Option<u64>),
    Flag,
    /// Text, one per line.
    Lines,
    /// One of these.
    OneOf(Vec<(String, String)>),
    /// Some of these.
    SomeOf(Vec<(String, String)>),
    /// Text, with these suggested.
    Suggest(Vec<(String, String)>),
    /// Texts in order, each with these suggested.
    SuggestLines(Vec<(String, String)>),
}

/// One field of an operation's form.
#[derive(Clone, PartialEq)]
struct FieldSpec {
    name: String,
    label: String,
    help: String,
    required: bool,
    input: Input,
}

impl FieldSpec {
    fn new(name: &str, schema: &Value, required: bool, pick: Option<&Pick>) -> Self {
        let listed = |values: &Value| -> Vec<(String, String)> {
            values
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_str)
                        .map(|v| (v.to_string(), v.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let list = schema["type"] == "array";
        let input = match pick {
            Some(p) if p.strict && list => Input::SomeOf(p.options.clone()),
            Some(p) if p.strict => Input::OneOf(p.options.clone()),
            Some(p) if list => Input::SuggestLines(p.options.clone()),
            Some(p) => Input::Suggest(p.options.clone()),
            None if schema.get("enum").is_some() => Input::OneOf(listed(&schema["enum"])),
            None if schema["items"].get("enum").is_some() => {
                Input::SomeOf(listed(&schema["items"]["enum"]))
            }
            None => match schema["type"].as_str() {
                Some("string") if schema["writeOnly"] == true => Input::Secret,
                Some("string") if LONG.contains(&name) => Input::Long,
                Some("integer") => Input::Number(schema["minimum"].as_u64()),
                Some("boolean") => Input::Flag,
                Some("array") => Input::Lines,
                _ => Input::Text,
            },
        };
        FieldSpec {
            name: name.to_string(),
            label: name.replace(['_', '-'], " "),
            help: schema["description"].as_str().unwrap_or("").to_string(),
            required,
            input,
        }
    }

    fn is_list(&self) -> bool {
        matches!(
            self.input,
            Input::Lines | Input::SomeOf(_) | Input::SuggestLines(_)
        )
    }

    /// Its value in the input, from what was typed; none when left empty.
    fn value(&self, typed: &str) -> Result<Option<Value>, String> {
        let typed = typed.trim();
        if self.is_list() {
            let items: Vec<&str> = typed
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            // A list it needs may be empty, as a role that may do nothing.
            if items.is_empty() && !self.required {
                return Ok(None);
            }
            return Ok(Some(json!(items)));
        }
        if typed.is_empty() {
            return Ok(None);
        }
        Ok(Some(match self.input {
            Input::Number(least) => {
                let n = typed
                    .parse::<u64>()
                    .map_err(|_| format!("{} is a whole number", self.label))?;
                if least.is_some_and(|least| n < least) {
                    return Err(format!("{} is at least {}", self.label, least.unwrap_or(0)));
                }
                json!(n)
            }
            Input::Flag => json!(typed == "true"),
            Input::OneOf(ref options) if !options.iter().any(|(v, _)| v == typed) => {
                return Err(format!("{} is not one of the choices", self.label));
            }
            _ => json!(typed),
        }))
    }
}

/// The fields of the operation `name`, as the catalog lists it.
fn fields_of(catalog: &Value, name: &str, picks: &[Pick]) -> Option<(String, Vec<FieldSpec>)> {
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
        .map(|field| {
            FieldSpec::new(
                field,
                &input["properties"][field],
                required.contains(&field),
                picks.iter().find(|p| p.field == field),
            )
        })
        .collect();
    Some((op["summary"].as_str().unwrap_or("").to_string(), fields))
}

/// The key under which the second typing of a secret is kept.
fn again(name: &str) -> String {
    format!("{name} again")
}

/// May the operation `op` be run by who is signed in, as the catalog lists?
pub fn may_run(catalog: &Catalog, op: &str) -> bool {
    match &*catalog.read() {
        Some(Ok(listed)) => listed["operations"]
            .as_array()
            .is_some_and(|ops| ops.iter().any(|o| o["name"] == op)),
        _ => false,
    }
}

/// A form that runs the operation `op`.
///
/// `fixed` are values of its input not asked for, such as the project of the
/// page; `values` fills fields at first. `picks`, `when`, `more` (fields
/// folded away), `repeat` (secrets typed twice), `labels` and `confirm` (a
/// question asked before it runs) make it hard to fill in wrong. Once it ran,
/// `on_done` gets what it answered, and the form shows it unless `quiet`, or
/// only its note when `saved`; `clear` empties it for the next.
#[component]
pub fn OpForm(
    op: String,
    #[props(default)] fixed: Vec<(String, Value)>,
    #[props(default)] values: Vec<(String, String)>,
    #[props(default)] picks: Vec<Pick>,
    #[props(default)] when: Vec<When>,
    #[props(default)] more: Vec<String>,
    #[props(default)] repeat: Vec<String>,
    #[props(default)] labels: Vec<(String, String)>,
    #[props(default)] confirm: Option<String>,
    #[props(default)] danger: bool,
    #[props(default)] submit: Option<String>,
    #[props(default)] on_done: Option<EventHandler<Value>>,
    #[props(default)] quiet: bool,
    #[props(default)] saved: bool,
    #[props(default)] clear: bool,
    #[props(default)] summary: bool,
) -> Element {
    let catalog = use_context::<Catalog>();
    let first: HashMap<String, String> = values.iter().cloned().collect();
    let mut typed = use_signal(|| first.clone());
    let mut failure = use_signal(|| None::<Failure>);
    let mut answered = use_signal(|| None::<Value>);
    let mut busy = use_signal(|| false);
    let mut asking = use_signal(|| false);
    let id = use_hook(|| {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    });
    let Some(Ok(listed)) = &*catalog.read() else {
        return rsx! { p { class: "loading", "Loading…" } };
    };
    let Some((about, fields)) = fields_of(listed, &op, &picks) else {
        return rsx! { p { class: "muted", "You may not run {op}." } };
    };
    // A field others are asked for by comes first, so it is chosen before
    // them: a kind before what that kind takes, "none" before the model.
    let mut fields = fields;
    fields.sort_by_key(|f| !when.iter().any(|w| w.on == f.name));
    let shown: Vec<FieldSpec> = fields
        .into_iter()
        .filter(|f| !fixed.iter().any(|(name, _)| *name == f.name))
        .map(|mut f| {
            if let Some((_, label)) = labels.iter().find(|(name, _)| *name == f.name) {
                f.label = label.clone();
            }
            f
        })
        .collect();
    // Is the field asked for, with what is typed now?
    let asked = {
        let when = when.clone();
        move |name: &str, typed: &HashMap<String, String>| {
            when.iter().filter(|w| w.field == name).all(|w| {
                let now = typed.get(&w.on).map(|t| t.trim()).unwrap_or("");
                let now = if now == "false" { "" } else { now };
                w.values.iter().any(|v| v == now)
            })
        }
    };
    let run = {
        let (op, shown, fixed, repeat, asked) = (
            op.clone(),
            shown.clone(),
            fixed.clone(),
            repeat.clone(),
            asked.clone(),
        );
        let confirming = confirm.is_some();
        move |e: FormEvent| {
            e.prevent_default();
            let now = typed.read().clone();
            let wrong = |field: &FieldSpec, message: String| Failure {
                status: 400,
                message,
                field: Some(field.name.clone()),
            };
            let mut input = Map::new();
            for field in shown.iter().filter(|f| asked(&f.name, &now)) {
                let text = now.get(&field.name).cloned().unwrap_or_default();
                match field.value(&text) {
                    Ok(Some(value)) => {
                        input.insert(field.name.clone(), value);
                    }
                    Ok(None) if field.required && field.input != Input::Flag => {
                        failure.set(Some(wrong(field, format!("{} is needed", field.label))));
                        return;
                    }
                    Ok(None) => {}
                    Err(message) => {
                        failure.set(Some(wrong(field, message)));
                        return;
                    }
                }
                if repeat.contains(&field.name)
                    && now
                        .get(&again(&field.name))
                        .map(String::as_str)
                        .unwrap_or("")
                        != text
                {
                    failure.set(Some(wrong(
                        field,
                        format!("the two {}s typed differ", field.label),
                    )));
                    return;
                }
            }
            for (name, value) in &fixed {
                input.insert(name.clone(), value.clone());
            }
            failure.set(None);
            if confirming && !asking() {
                asking.set(true);
                return;
            }
            asking.set(false);
            let op = op.clone();
            let first = first.clone();
            spawn(async move {
                busy.set(true);
                answered.set(None);
                match api::op(&op, Value::Object(input)).await {
                    Ok(out) => {
                        answered.set(Some(out.clone()));
                        if clear {
                            typed.set(first);
                        }
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
    let now = typed.read().clone();
    let field_input = |field: FieldSpec| {
        let name = field.name.clone();
        let error = match (&*failure.read(), wrong.as_deref() == Some(name.as_str())) {
            (Some(f), true) => Some(f.message.clone()),
            _ => None,
        };
        let twice = repeat.contains(&name).then(|| {
            let key = again(&name);
            let text = now.get(&key).cloned().unwrap_or_default();
            (key, text)
        });
        rsx! {
            FieldInput {
                key: "{name}",
                field: field.clone(),
                list: format!("pick-{id}-{name}"),
                error,
                text: now.get(&name).cloned().unwrap_or_default(),
                again: twice.as_ref().map(|(_, t)| t.clone()),
                on_input: {
                    let name = name.clone();
                    move |text: String| {
                        asking.set(false);
                        typed.write().insert(name.clone(), text);
                    }
                },
                on_again: {
                    let name = name.clone();
                    move |text: String| {
                        asking.set(false);
                        typed.write().insert(again(&name), text);
                    }
                },
            }
        }
    };
    let main: Vec<FieldSpec> = shown
        .iter()
        .filter(|f| asked(&f.name, &now))
        .filter(|f| !more.contains(&f.name) && !SELDOM.contains(&f.name.as_str()))
        .cloned()
        .collect();
    let folded: Vec<FieldSpec> = shown
        .iter()
        .filter(|f| asked(&f.name, &now))
        .filter(|f| more.contains(&f.name) || SELDOM.contains(&f.name.as_str()))
        .cloned()
        .collect();
    let label = submit.clone().unwrap_or_else(|| "Run".into());
    let button = if danger { "danger" } else { "" };
    let unfielded = failure
        .read()
        .as_ref()
        .filter(|f| f.field.is_none() || !shown.iter().any(|s| Some(&s.name) == f.field.as_ref()))
        .map(|f| f.message.clone());
    rsx! {
        form { class: "op", onsubmit: run, novalidate: true,
            if summary {
                p { class: "muted", "{about}" }
            }
            for field in main {
                {field_input(field)}
            }
            if !folded.is_empty() {
                details { class: "more",
                    summary { "More" }
                    for field in folded {
                        {field_input(field)}
                    }
                }
            }
            if asking() {
                div { class: "confirm",
                    p { {confirm.clone().unwrap_or_default()} }
                    div { class: "actions",
                        button { r#type: "submit", class: "danger", disabled: busy(), "Yes, {label.to_lowercase()}" }
                        button {
                            r#type: "button",
                            class: "plain",
                            onclick: move |_| asking.set(false),
                            "No"
                        }
                    }
                }
            } else {
                div { class: "actions",
                    button { r#type: "submit", class: "{button}", disabled: busy(),
                        if busy() { "Working…" } else { "{label}" }
                    }
                }
            }
            if let Some(message) = unfielded {
                p { class: "error", "{message}" }
            }
            if !quiet {
                if let Some(out) = answered.read().as_ref() {
                    if saved {
                        p { class: "ok",
                            "Saved. "
                            {out["note"].as_str().unwrap_or("").to_string()}
                        }
                    } else {
                        Answer { value: out.clone() }
                    }
                }
            }
        }
    }
}

#[component]
fn FieldInput(
    field: FieldSpec,
    list: String,
    error: Option<String>,
    text: String,
    again: Option<String>,
    on_input: EventHandler<String>,
    on_again: EventHandler<String>,
) -> Element {
    let class = if error.is_some() {
        "field wrong"
    } else {
        "field"
    };
    let input = match field.input.clone() {
        Input::Flag => {
            return rsx! {
                label { class: "{class} flag",
                    input {
                        r#type: "checkbox",
                        checked: text == "true",
                        onchange: move |e: FormEvent| on_input.call(e.checked().to_string()),
                    }
                    span {
                        span { class: "name", "{field.label}" }
                        span { class: "help", "{field.help}" }
                    }
                }
            };
        }
        Input::OneOf(options) if field.required && options.len() <= 4 => {
            let name = list.clone();
            rsx! {
                div { class: "choices", role: "radiogroup",
                    for (value, shown) in options {
                        label { key: "{value}", class: "choice",
                            input {
                                r#type: "radio",
                                name: "{name}",
                                checked: text.trim() == value,
                                onchange: {
                                    let value = value.clone();
                                    move |_| on_input.call(value.clone())
                                },
                            }
                            span { "{shown}" }
                        }
                    }
                }
            }
        }
        Input::OneOf(options) => rsx! {
            select { value: "{text}", onchange: move |e: FormEvent| on_input.call(e.value()),
                option { value: "", if field.required { "Choose…" } else { "—" } }
                for (value, shown) in options {
                    option { key: "{value}", value: "{value}", selected: text.trim() == value, "{shown}" }
                }
            }
        },
        Input::SomeOf(options) => {
            let chosen: Vec<String> = text.lines().map(|l| l.trim().to_string()).collect();
            rsx! {
                div { class: "choices",
                    for (value, shown) in options.clone() {
                        label { key: "{value}", class: "choice",
                            input {
                                r#type: "checkbox",
                                checked: chosen.contains(&value),
                                onchange: {
                                    let (options, chosen, value) = (options.clone(), chosen.clone(), value.clone());
                                    move |e: FormEvent| {
                                        // Kept in the order of the choices.
                                        let on = e.checked();
                                        let now: Vec<String> = options
                                            .iter()
                                            .map(|(v, _)| v.clone())
                                            .filter(|v| if *v == value { on } else { chosen.contains(v) })
                                            .collect();
                                        on_input.call(now.join("\n"));
                                    }
                                },
                            }
                            span { "{shown}" }
                        }
                    }
                }
            }
        }
        Input::Suggest(options) => rsx! {
            input {
                list: "{list}",
                autocomplete: "off",
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
            datalist { id: "{list}",
                for (value, shown) in options {
                    option { key: "{value}", value: "{value}", "{shown}" }
                }
            }
        },
        Input::SuggestLines(options) => rsx! {
            Rows { list: list.clone(), options, text: text.clone(), on_input }
        },
        Input::Lines | Input::Long => rsx! {
            textarea {
                rows: if field.input == Input::Lines { "3" } else { "6" },
                placeholder: if field.input == Input::Lines { "one per line" } else { "" },
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
        },
        Input::Secret => rsx! {
            input {
                r#type: "password",
                autocomplete: "new-password",
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
            if let Some(again) = again {
                input {
                    r#type: "password",
                    autocomplete: "new-password",
                    placeholder: "the same again",
                    value: "{again}",
                    oninput: move |e: FormEvent| on_again.call(e.value()),
                }
            }
        },
        Input::Number(least) => rsx! {
            input {
                r#type: "number",
                inputmode: "numeric",
                min: least.map(|l| l.to_string()).unwrap_or_default(),
                step: "1",
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
        },
        Input::Text => rsx! {
            input {
                autocomplete: "off",
                value: "{text}",
                oninput: move |e: FormEvent| on_input.call(e.value()),
            }
        },
    };
    rsx! {
        div { class: "{class}",
            span { class: "name",
                "{field.label}"
                if field.required { span { class: "needed", title: "needed", " *" } }
            }
            {input}
            if let Some(error) = error {
                span { class: "error", "{error}" }
            }
            span { class: "help", "{field.help}" }
        }
    }
}

/// Texts in order, one row each, each with `options` suggested: what is
/// typed is the rows, one per line.
#[component]
fn Rows(
    list: String,
    options: Vec<(String, String)>,
    text: String,
    on_input: EventHandler<String>,
) -> Element {
    let mut rows: Vec<String> = text.split('\n').map(String::from).collect();
    if rows.is_empty() {
        rows.push(String::new());
    }
    let count = rows.len();
    let changed = move |rows: Vec<String>| on_input.call(rows.join("\n"));
    rsx! {
        div { class: "rows",
            for (i, row) in rows.clone().into_iter().enumerate() {
                div { key: "{i}", class: "row",
                    span { class: "muted order", "{i + 1}." }
                    input {
                        list: "{list}",
                        autocomplete: "off",
                        value: "{row}",
                        oninput: {
                            let rows = rows.clone();
                            move |e: FormEvent| {
                                let mut rows = rows.clone();
                                rows[i] = e.value();
                                changed(rows);
                            }
                        },
                    }
                    button {
                        r#type: "button",
                        class: "plain",
                        title: "Move up",
                        disabled: i == 0,
                        onclick: {
                            let rows = rows.clone();
                            move |_| {
                                let mut rows = rows.clone();
                                rows.swap(i, i - 1);
                                changed(rows);
                            }
                        },
                        "↑"
                    }
                    button {
                        r#type: "button",
                        class: "plain",
                        title: "Move down",
                        disabled: i + 1 == count,
                        onclick: {
                            let rows = rows.clone();
                            move |_| {
                                let mut rows = rows.clone();
                                rows.swap(i, i + 1);
                                changed(rows);
                            }
                        },
                        "↓"
                    }
                    button {
                        r#type: "button",
                        class: "plain",
                        title: "Take out",
                        onclick: {
                            let rows = rows.clone();
                            move |_| {
                                let mut rows = rows.clone();
                                rows.remove(i);
                                changed(rows);
                            }
                        },
                        "×"
                    }
                }
            }
            button {
                r#type: "button",
                class: "plain add",
                onclick: {
                    let rows = rows.clone();
                    move |_| {
                        let mut rows = rows.clone();
                        rows.push(String::new());
                        changed(rows);
                    }
                },
                "+ Add"
            }
            datalist { id: "{list}",
                for (value, shown) in options {
                    option { key: "{value}", value: "{value}", "{shown}" }
                }
            }
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
        fields.retain(|_, v| !v.is_null());
    }
    let said = value["said"].as_str().map(String::from);
    rsx! {
        div { class: "answer",
            if let Some((kind, secret)) = secret {
                p { class: "secret",
                    "The {kind}, shown once: copy it now."
                    code { "{secret}" }
                }
            }
            if let Some(note) = note {
                p { "{note}" }
            } else if let Some(said) = said {
                p { "{said}" }
            }
            if rest.as_object().is_some_and(|f| !f.is_empty()) {
                details {
                    summary { "Everything it answered" }
                    Json { value: rest }
                }
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
