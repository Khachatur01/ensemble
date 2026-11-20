//! Operations: everything a person can do to a hub or a runner, defined once.
//! An interface, the CLI or the REST API or one to come, is an adapter over
//! them: it shows an operation by its [`Spec`] and reaches it through a
//! [`Registry`], and knows nothing of what it does.
//!
//! An operation has a name (`user.add`, `config.set`), a summary, who may run
//! it ([`Access`]), its input fields ([`Field`]), typed input and output, and
//! errors as data ([`Error`]): a code, the field it is about, a message and a
//! hint, so each interface shows it next to what it is about. The registry
//! checks who asks and what is given before the operation runs.
//!
//! The command line both executables make from operations is `cli`; `local`
//! runs one in the running process, over its socket, or else here.

pub mod cli;
pub mod local;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;

/// What kind of failure an [`Error`] is, for an interface to answer with:
/// a REST status, a CLI exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// What was given is not what the operation takes.
    Invalid,
    /// It needs a signed-in account, and none is.
    Unauthorized,
    /// The one who asks may not.
    Forbidden,
    /// What it names is not there.
    NotFound,
    /// It is there already, or the change would clash with what is.
    Conflict,
    /// Asked too often; try again later.
    TooMany,
    /// Something it needs cannot be reached now.
    Unavailable,
    /// It failed for a reason of its own.
    Failed,
}

/// Why an operation did not do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: Code,
    /// The input field it is about, when it is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
    /// What to do about it, when there is something.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Error {
            code,
            field: None,
            message: message.into(),
            hint: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Error::new(Code::Invalid, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::new(Code::NotFound, message)
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Error::new(Code::Failed, message)
    }

    /// About the input field `field`.
    pub fn on(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "; {hint}")?;
        }
        Ok(())
    }
}

/// A failure the code below has only as words.
impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::failed(message)
    }
}

/// What a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// Text never shown back nor taken on a command line, such as a password:
    /// an interface asks for it.
    Secret,
    /// A whole number, 0 or more.
    Number,
    /// Yes or no.
    Flag,
    /// A list of texts.
    List,
    /// One of these texts.
    Choice(&'static [&'static str]),
}

/// One input field of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// As the input names it, such as `tracker_user`.
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
    /// Given by its place rather than its name, where an interface can: the
    /// CLI's arguments, in the order of the fields.
    pub positional: bool,
    pub help: &'static str,
}

impl Field {
    pub const fn new(name: &'static str, kind: Kind, help: &'static str) -> Self {
        Field {
            name,
            kind,
            required: false,
            positional: false,
            help,
        }
    }

    pub const fn text(name: &'static str, help: &'static str) -> Self {
        Field::new(name, Kind::Text, help)
    }

    pub const fn secret(name: &'static str, help: &'static str) -> Self {
        Field::new(name, Kind::Secret, help)
    }

    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub const fn positional(mut self) -> Self {
        self.positional = true;
        self
    }
}

/// Who may run an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Only who runs the hub or the runner, on its computer: the CLI.
    Local,
    /// An account whose role, and token, have this permission; and who runs
    /// it, on its computer.
    Permission(&'static str),
    /// A signed-in account, for itself, with a session from its password
    /// rather than an access token.
    Session,
    /// Any signed-in account, for itself.
    Signed,
    /// Anyone who reaches it.
    Anyone,
}

/// Who asks for an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// Who runs the hub or the runner, on its computer.
    Local,
    /// A signed-in account, with the permissions of its role and token.
    Account {
        name: String,
        permissions: Vec<String>,
        /// Signed in with a session rather than an access token.
        session: bool,
        /// The id of the token it signed in with, never the token.
        token: String,
    },
    /// No one signed in.
    Anyone,
}

impl Caller {
    /// Whether it may run what `access` allows, or why not.
    pub fn may(&self, access: Access) -> Result<(), Error> {
        let refused = |message: &str| Err(Error::new(Code::Forbidden, message));
        match (self, access) {
            (_, Access::Anyone) => Ok(()),
            (Caller::Local, Access::Local | Access::Permission(_)) => Ok(()),
            (Caller::Local, Access::Session | Access::Signed) => {
                refused("this is done by a signed-in account, for itself")
            }
            (_, Access::Local) => refused("this is done only on the computer that runs it"),
            (Caller::Anyone, _) => Err(Error::new(
                Code::Unauthorized,
                "a valid token of an approved user is needed",
            )),
            (Caller::Account { permissions, .. }, Access::Permission(p)) => {
                if permissions.iter().any(|have| have == p) {
                    Ok(())
                } else {
                    refused(&format!(
                        "this needs a role, and a token, with the `{p}` permission"
                    ))
                }
            }
            (Caller::Account { session, .. }, Access::Session) => {
                if *session {
                    Ok(())
                } else {
                    refused(
                        "this needs a session from logging in with your password, not an access token",
                    )
                }
            }
            (Caller::Account { .. }, Access::Signed) => Ok(()),
        }
    }

    /// The signed-in account's name.
    pub fn account(&self) -> Option<&str> {
        match self {
            Caller::Account { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// What an operation is, as interfaces show it.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Its group and verb, such as `user.add`; the CLI's command path.
    pub name: &'static str,
    /// One line on what it does.
    pub summary: &'static str,
    pub access: Access,
    pub input: &'static [Field],
}

impl Spec {
    /// The JSON Schema of its input.
    pub fn input_schema(&self) -> Value {
        let mut properties = Map::new();
        for field in self.input {
            let mut schema = match field.kind {
                Kind::Text => json!({ "type": "string" }),
                Kind::Secret => json!({ "type": "string", "writeOnly": true }),
                Kind::Number => json!({ "type": "integer", "minimum": 0 }),
                Kind::Flag => json!({ "type": "boolean" }),
                Kind::List => json!({ "type": "array", "items": { "type": "string" } }),
                Kind::Choice(choices) => json!({ "enum": choices }),
            };
            schema["description"] = json!(field.help);
            properties.insert(field.name.to_string(), schema);
        }
        let required: Vec<&str> = self
            .input
            .iter()
            .filter(|f| f.required)
            .map(|f| f.name)
            .collect();
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }

    /// Refuse an input that is not what the fields say.
    fn check(&self, input: &Value) -> Result<(), Error> {
        let Some(given) = input.as_object() else {
            return Err(Error::invalid("the input must be an object of fields"));
        };
        for name in given.keys() {
            if !self.input.iter().any(|f| f.name == name) {
                return Err(Error::invalid(format!("{} takes no `{name}`", self.name)).on(name));
            }
        }
        for field in self.input {
            let value = given.get(field.name).filter(|v| !v.is_null());
            let Some(value) = value else {
                if field.required {
                    return Err(
                        Error::invalid(format!("`{}` is required", field.name)).on(field.name)
                    );
                }
                continue;
            };
            let fits = match field.kind {
                Kind::Text | Kind::Secret => value
                    .as_str()
                    .is_some_and(|t| !field.required || !t.trim().is_empty()),
                Kind::Number => value.as_u64().is_some(),
                Kind::Flag => value.is_boolean(),
                Kind::List => value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)),
                Kind::Choice(choices) => value.as_str().is_some_and(|t| choices.contains(&t)),
            };
            if !fits {
                let wanted = match field.kind {
                    Kind::Text | Kind::Secret if field.required => "text, not empty".to_string(),
                    Kind::Text | Kind::Secret => "text".to_string(),
                    Kind::Number => "a whole number".to_string(),
                    Kind::Flag => "true or false".to_string(),
                    Kind::List => "a list of texts".to_string(),
                    Kind::Choice(choices) => format!("one of {}", choices.join(", ")),
                };
                return Err(
                    Error::invalid(format!("`{}` must be {wanted}", field.name)).on(field.name)
                );
            }
        }
        Ok(())
    }
}

/// What an interface that shows text says of an operation's output: `out` is
/// the answer, such as a token to keep, and `note` what to know about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Said {
    pub out: String,
    pub note: Option<String>,
}

type Run<C> = dyn Fn(&C, &Caller, Value) -> Result<Value, Error> + Send + Sync;

/// One operation, over a context `C`: what a hub or a runner gives it.
pub struct Operation<C> {
    pub spec: Spec,
    run: Box<Run<C>>,
    show: fn(&Value) -> Said,
}

/// An operation from its spec and what it does, with typed input and output.
pub fn operation<C, I, O>(
    spec: Spec,
    run: impl Fn(&C, &Caller, I) -> Result<O, Error> + Send + Sync + 'static,
) -> Operation<C>
where
    I: DeserializeOwned,
    O: Serialize,
{
    Operation {
        spec,
        run: Box::new(move |context, caller, input| {
            let input: I = serde_json::from_value(input)
                .map_err(|e| Error::invalid(format!("{}: {e}", spec.name)))?;
            let output = run(context, caller, input)?;
            serde_json::to_value(output).map_err(|e| Error::failed(e.to_string()))
        }),
        show: said,
    }
}

impl<C> Operation<C> {
    /// Say its output as text this way, rather than as YAML or its `said`.
    pub fn shown(mut self, show: fn(&Value) -> Said) -> Self {
        self.show = show;
        self
    }

    /// Its output, as text.
    pub fn show(&self, output: &Value) -> Said {
        (self.show)(output)
    }
}

/// An output as text: its `said` when it has one, and its `note`; else the
/// whole of it, as [`lines`].
pub fn said(output: &Value) -> Said {
    let note = output["note"].as_str().map(String::from);
    if let Some(said) = output["said"].as_str() {
        return Said {
            out: said.to_string(),
            note,
        };
    }
    Said {
        out: lines(output),
        note,
    }
}

/// `value` as lines to read: each field on a line of its own, `name: value`,
/// what is inside it indented below it, a list's items each after `- `.
pub fn lines(value: &Value) -> String {
    fn scalar(value: &Value) -> Option<String> {
        match value {
            Value::Null => Some("none".into()),
            Value::String(s) => Some(s.clone()),
            Value::Bool(b) => Some(if *b { "yes" } else { "no" }.into()),
            Value::Number(n) => Some(n.to_string()),
            Value::Array(a) if a.is_empty() => Some("none".into()),
            Value::Object(o) if o.is_empty() => Some("none".into()),
            Value::Array(a) if a.iter().all(|v| !v.is_array() && !v.is_object()) => {
                Some(a.iter().filter_map(scalar).collect::<Vec<_>>().join(", "))
            }
            _ => None,
        }
    }
    fn walk(value: &Value, indent: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(indent);
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    match scalar(v) {
                        Some(s) => out.push(format!("{pad}{key}: {s}")),
                        None => {
                            out.push(format!("{pad}{key}:"));
                            walk(v, indent + 1, out);
                        }
                    }
                }
            }
            Value::Array(items) => {
                for v in items {
                    match scalar(v) {
                        Some(s) => out.push(format!("{pad}- {s}")),
                        None => {
                            out.push(format!("{pad}-"));
                            walk(v, indent + 1, out);
                        }
                    }
                }
            }
            other => out.push(format!("{pad}{}", scalar(other).unwrap_or_default())),
        }
    }
    let mut out = Vec::new();
    walk(value, 0, &mut out);
    out.join("\n")
}

/// The operations of a hub or a runner, by name.
pub struct Registry<C> {
    operations: Vec<Operation<C>>,
}

impl<C> Registry<C> {
    /// The registry of `operations`, whose names must differ.
    pub fn new(operations: Vec<Operation<C>>) -> Self {
        for (i, op) in operations.iter().enumerate() {
            assert!(
                operations[..i].iter().all(|o| o.spec.name != op.spec.name),
                "two operations are called {}",
                op.spec.name
            );
        }
        Registry { operations }
    }

    pub fn operations(&self) -> &[Operation<C>] {
        &self.operations
    }

    pub fn get(&self, name: &str) -> Option<&Operation<C>> {
        self.operations.iter().find(|o| o.spec.name == name)
    }

    /// Run `name` on `context` for `caller`, with `input`, once the caller may
    /// and the input is what the operation takes.
    pub fn run(
        &self,
        context: &C,
        caller: &Caller,
        name: &str,
        input: Value,
    ) -> Result<Value, Error> {
        let op = self
            .get(name)
            .ok_or_else(|| Error::not_found(format!("there is no operation `{name}`")))?;
        caller.may(op.spec.access)?;
        let input = if input.is_null() { json!({}) } else { input };
        op.spec.check(&input)?;
        (op.run)(context, caller, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    struct Counter(std::sync::Mutex<u64>);

    #[derive(Deserialize)]
    struct Add {
        by: u64,
        why: Option<String>,
    }

    const ADD: Spec = Spec {
        name: "count.add",
        summary: "Add to the count.",
        access: Access::Permission("admin"),
        input: &[
            Field::new("by", Kind::Number, "How much.")
                .required()
                .positional(),
            Field::text("why", "What for."),
        ],
    };

    fn registry() -> Registry<Counter> {
        Registry::new(vec![operation(ADD, |c: &Counter, _, input: Add| {
            if input.by == 0 {
                return Err(Error::invalid("adding 0 changes nothing").on("by"));
            }
            let mut n = c.0.lock().unwrap();
            *n += input.by;
            Ok(json!({ "count": *n, "said": format!("now {}", *n), "note": input.why }))
        })])
    }

    fn account(permissions: &[&str], session: bool) -> Caller {
        Caller::Account {
            name: "ana".into(),
            permissions: permissions.iter().map(|p| p.to_string()).collect(),
            session,
            token: "t1".into(),
        }
    }

    #[test]
    fn an_operation_runs_once_the_caller_may_and_the_input_fits() {
        let (r, c) = (registry(), Counter(std::sync::Mutex::new(0)));
        let out = r
            .run(&c, &Caller::Local, "count.add", json!({ "by": 2 }))
            .unwrap();
        assert_eq!(out["count"], 2);
        let out = r
            .run(
                &c,
                &account(&["admin"], false),
                "count.add",
                json!({ "by": 3, "why": "x" }),
            )
            .unwrap();
        assert_eq!(
            r.get("count.add").unwrap().show(&out),
            Said {
                out: "now 5".into(),
                note: Some("x".into())
            }
        );

        let err = r.run(
            &c,
            &account(&["share"], true),
            "count.add",
            json!({ "by": 1 }),
        );
        assert_eq!(err.unwrap_err().code, Code::Forbidden);
        let err = r.run(&c, &Caller::Anyone, "count.add", json!({ "by": 1 }));
        assert_eq!(err.unwrap_err().code, Code::Unauthorized);
        for (input, field) in [
            (json!({}), "by"),
            (json!({ "by": "two" }), "by"),
            (json!({ "by": 1, "size": 3 }), "size"),
            (json!({ "by": 0 }), "by"),
        ] {
            let err = r.run(&c, &Caller::Local, "count.add", input).unwrap_err();
            assert_eq!(
                (err.code, err.field.as_deref()),
                (Code::Invalid, Some(field)),
                "{err}"
            );
        }
        assert_eq!(
            r.run(&c, &Caller::Local, "count.take", json!({}))
                .unwrap_err()
                .code,
            Code::NotFound
        );
        assert_eq!(*c.0.lock().unwrap(), 5, "nothing refused ran");
    }

    #[test]
    fn who_may_run_what() {
        let local = Caller::Local;
        assert!(local.may(Access::Local).is_ok());
        assert!(local.may(Access::Permission("admin")).is_ok());
        assert!(local.may(Access::Session).is_err(), "local is no account");
        let token = account(&["recall"], false);
        assert!(token.may(Access::Signed).is_ok());
        assert!(token.may(Access::Session).is_err());
        assert!(token.may(Access::Local).is_err());
        assert!(account(&[], true).may(Access::Session).is_ok());
        assert!(Caller::Anyone.may(Access::Anyone).is_ok());
    }

    #[test]
    fn the_input_schema_names_every_field() {
        let schema = ADD.input_schema();
        assert_eq!(schema["required"], json!(["by"]));
        assert_eq!(schema["properties"]["by"]["type"], "integer");
        assert_eq!(schema["properties"]["why"]["description"], "What for.");
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    #[should_panic(expected = "two operations are called count.add")]
    fn two_operations_of_one_name_are_refused() {
        let twice = |_: &Counter, _: &Caller, _: Add| Ok(json!({}));
        Registry::new(vec![operation(ADD, twice), operation(ADD, twice)]);
    }
}
