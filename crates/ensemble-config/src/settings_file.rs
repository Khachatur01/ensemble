//! One settings document (a team's config.yaml, or the settings of a hub or
//! a runner, kept in its database through a [`Store`]) changed by its
//! settings, as text, so that comments and order survive
//! ([`settings`](crate::settings)), and the `config.*` operations on it. A
//! change is loaded before it is written, and refused when it would not load.

use crate::settings;
use ensemble_ops::{Access, Caller, Code, Error, Field, Operation, Spec, operation};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

type Check = dyn Fn(&str) -> Result<(), String> + Send + Sync;
type Changed = dyn Fn() -> Option<String> + Send + Sync;

/// Where settings are kept when not in a file of their own, such as a table
/// of a hub's or a runner's database.
pub trait Store: Send + Sync {
    /// The text kept, or none when nothing is yet.
    fn read(&self) -> Result<Option<String>, String>;
    /// Keep `text` in place of what was kept.
    fn write(&self, text: &str) -> Result<(), String>;
}

/// One settings file, or the settings a [`Store`] keeps.
pub struct SettingsFile {
    /// The file; for settings a store keeps, where it keeps them, such as the
    /// database's file.
    pub path: PathBuf,
    /// What it is called in what is said: the file's path, or what the store
    /// keeps.
    name: String,
    store: Option<Box<dyn Store>>,
    /// What makes it when there is none, for the message.
    made_by: String,
    /// What is wrong with a text as this file, if anything.
    check: Box<Check>,
    /// What to do once it changed, and what to say about it.
    changed: Box<Changed>,
}

impl SettingsFile {
    /// The file at `path`, made by `made_by` when there is none, whose text
    /// `check` loads.
    pub fn new(
        path: PathBuf,
        made_by: &str,
        check: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        SettingsFile {
            name: path.display().to_string(),
            path,
            store: None,
            made_by: made_by.to_string(),
            check: Box::new(check),
            changed: Box::new(|| None),
        }
    }

    /// The settings `store` keeps, `name` in what is said, at `path`, made by
    /// `made_by` when there are none, whose text `check` loads.
    pub fn kept_in(
        store: impl Store + 'static,
        name: &str,
        path: PathBuf,
        made_by: &str,
        check: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        SettingsFile {
            name: name.to_string(),
            store: Some(Box::new(store)),
            ..SettingsFile::new(path, made_by, check)
        }
    }

    /// What it is called in what is said.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Do `changed` once a change is written; what it returns is said.
    pub fn on_change(
        mut self,
        changed: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.changed = Box::new(changed);
        self
    }

    pub fn read(&self) -> Result<String, Error> {
        let missing = || {
            Error::not_found(format!("there is no {}", self.name))
                .hint(format!("{} writes it", self.made_by))
        };
        if let Some(store) = &self.store {
            return store.read().map_err(Error::failed)?.ok_or_else(missing);
        }
        std::fs::read_to_string(&self.path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => missing(),
            _ => Error::failed(format!("cannot read {}: {e}", self.name)),
        })
    }

    /// Whether there is any yet.
    pub fn exists(&self) -> Result<bool, Error> {
        match &self.store {
            Some(store) => Ok(store.read().map_err(Error::failed)?.is_some()),
            None => Ok(self.path.exists()),
        }
    }

    /// Keep `text` as it is, loaded or not.
    fn keep(&self, text: &str) -> Result<(), Error> {
        match &self.store {
            Some(store) => store.write(text),
            None => write_atomically(&self.path, text),
        }
        .map_err(Error::failed)
    }

    /// What is wrong with `text` as this file, if anything.
    pub fn check(&self, text: &str) -> Result<(), String> {
        (self.check)(text)
    }

    /// Write `text`, if it loads.
    fn write(&self, text: &str, field: &str) -> Result<(), Error> {
        self.check(text)
            .map_err(|e| Error::invalid(format!("nothing was changed: {e}")).on(field))?;
        self.keep(text)
    }

    /// Set `key` to `value`, and say what changed.
    pub fn set(&self, key: &str, value: &str) -> Result<Option<String>, Error> {
        let text =
            settings::set(&self.read()?, key, value).map_err(|e| Error::invalid(e).on("key"))?;
        self.write(&text, "value")?;
        Ok((self.changed)())
    }

    /// Take `key` out, and say what changed.
    pub fn unset(&self, key: &str) -> Result<Option<String>, Error> {
        let text = settings::unset(&self.read()?, key).map_err(|e| Error::invalid(e).on("key"))?;
        self.write(&text, "key")?;
        Ok((self.changed)())
    }

    /// The file's text with `edits` made in order, each a setting set to a
    /// value (YAML) or, with none, taken out: refused when the text would not
    /// load. Nothing is written yet: [`SettingsFile::commit`] writes it, so
    /// what goes with it, such as a sealed key, can be written in between.
    pub fn plan(&self, edits: &[(String, Option<String>)]) -> Result<String, Error> {
        let mut text = self.read()?;
        for (key, value) in edits {
            text = match value {
                Some(value) => settings::set(&text, key, value),
                None => settings::unset(&text, key),
            }
            .map_err(Error::invalid)?;
        }
        self.check(&text)
            .map_err(|e| Error::invalid(format!("nothing was changed: {e}")))?;
        Ok(text)
    }

    /// Write `text`, as [`SettingsFile::plan`] made it, and say what the running
    /// process did with it.
    pub fn commit(&self, text: &str) -> Result<Option<String>, Error> {
        self.keep(text)?;
        Ok((self.changed)())
    }

    /// Tell the running process its settings changed though the file did not,
    /// such as when a key one of them names did, and say what it did.
    pub fn changed(&self) -> Option<String> {
        (self.changed)()
    }

    /// Write `text` in place of the file, as an edit of it: refused when it
    /// would not load and, given `before`, the text it was made from, when the
    /// file changed since. What to say of it.
    pub fn replace(&self, text: &str, before: Option<&str>) -> Result<Option<String>, Error> {
        let now = self.read()?;
        if before.is_some_and(|b| b != now) {
            return Err(Error::new(
                Code::Conflict,
                format!("{} changed since it was read", self.name),
            )
            .hint("read it again, and make the change on what it is now"));
        }
        self.write(text, "text")?;
        Ok((self.changed)())
    }
}

/// Write through a temporary file beside it, made with the folder if it is not
/// there, so a reader never sees half of it.
pub fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// A context with a settings file: a hub's or a runner's.
pub trait WithSettings {
    /// Its settings file.
    fn settings_file(&self) -> Result<&SettingsFile, Error>;
}

#[derive(Deserialize)]
struct Get {
    key: Option<String>,
}

#[derive(Deserialize)]
struct Set {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct Unset {
    key: String,
}

#[derive(Deserialize)]
struct Nothing {}

#[derive(Deserialize)]
struct Replace {
    text: String,
    before: Option<String>,
}

const KEY: &str = "The setting: its keys joined by dots, such as `http.listen`.";
const GET: &[Field] = &[Field::text("key", KEY).positional()];
const SET: &[Field] = &[
    Field::text("key", KEY).required().positional(),
    Field::text("value", "Its value, as YAML.")
        .required()
        .positional(),
];
const UNSET: &[Field] = &[Field::text("key", KEY).required().positional()];
const REPLACE: &[Field] = &[
    Field::text("text", "The whole file.").required(),
    Field::text(
        "before",
        "The text it was made from: refused when the file changed since.",
    ),
];

/// `config.get`, `config.set`, `config.unset`, `config.replace` and
/// `config.path` on a context's settings file, for those `access` lets.
pub fn operations<C: WithSettings + 'static>(access: Access) -> Vec<Operation<C>> {
    vec![
        operation(
            Spec {
                name: "config.get",
                summary: "Print a setting, or the whole file.",
                access,
                input: GET,
            },
            |c: &C, _: &Caller, input: Get| {
                let file = c.settings_file()?;
                let text = file.read()?;
                Ok(match input.key {
                    None => json!({ "said": text.trim_end(), "text": text }),
                    Some(key) => {
                        let value = settings::get(&text, &key)
                            .map_err(|e| Error::invalid(e).on("key"))?
                            .ok_or_else(|| {
                                Error::not_found(format!("{} sets no `{key}`", file.name()))
                                    .on("key")
                            })?;
                        json!({ "said": value, "key": key, "value": value })
                    }
                })
            },
        ),
        operation(
            Spec {
                name: "config.set",
                summary: "Set a setting. The value is YAML: `laptop`, `8`, `true`, `[a, b]`, `{}`; anything else is written as a string.",
                access,
                input: SET,
            },
            |c: &C, _: &Caller, input: Set| {
                let file = c.settings_file()?;
                let note = file.set(&input.key, &input.value)?;
                Ok(json!({
                    "said": format!("{}: set in {}", input.key, file.name()),
                    "note": note,
                }))
            },
        ),
        operation(
            Spec {
                name: "config.unset",
                summary: "Take a setting out.",
                access,
                input: UNSET,
            },
            |c: &C, _: &Caller, input: Unset| {
                let file = c.settings_file()?;
                let note = file.unset(&input.key)?;
                Ok(json!({
                    "said": format!("{}: taken out of {}", input.key, file.name()),
                    "note": note,
                }))
            },
        ),
        operation(
            Spec {
                name: "config.replace",
                summary: "Write the whole file, as an edit of it: refused when it would not load, or the file changed since `before` was read.",
                access,
                input: REPLACE,
            },
            |c: &C, _: &Caller, input: Replace| {
                let file = c.settings_file()?;
                if input.before.as_deref() == Some(input.text.as_str()) {
                    return Ok(json!({ "said": format!("{} is unchanged", file.name()) }));
                }
                let note = file.replace(&input.text, input.before.as_deref())?;
                Ok(json!({
                    "said": format!("{} is changed", file.name()),
                    "note": note,
                }))
            },
        ),
        operation(
            Spec {
                name: "config.path",
                summary: "Print where the settings are kept.",
                access,
                input: &[],
            },
            |c: &C, _: &Caller, _: Nothing| {
                let file = c.settings_file()?;
                let path = file.path.display().to_string();
                let said = match file.name() == path {
                    true => path.clone(),
                    false => format!("{}, in {path}", file.name()),
                };
                Ok(json!({ "said": said, "path": path }) as Value)
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_ops::Registry;

    struct Context(SettingsFile);

    impl WithSettings for Context {
        fn settings_file(&self) -> Result<&SettingsFile, Error> {
            Ok(&self.0)
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ensemble-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_are_read_set_and_refused_as_data() {
        let dir = scratch("ops");
        let path = dir.join("runner.yaml");
        std::fs::write(&path, "# mine\nname: laptop\nhub: wss://a\n").unwrap();
        let file = SettingsFile::new(path.clone(), "`ensemble-runner setup`", |t| {
            if t.contains("name: \"\"") || t.contains("name: ''") {
                Err("name: is empty".into())
            } else {
                Ok(())
            }
        })
        .on_change(|| Some("restart it".into()));
        let r = Registry::new(operations::<Context>(Access::Local));
        let c = Context(file);
        let run = |name: &str, input: Value| r.run(&c, &Caller::Local, name, input);

        assert_eq!(
            run("config.get", json!({ "key": "name" })).unwrap()["value"],
            "laptop"
        );
        let out = run("config.set", json!({ "key": "hub", "value": "wss://b" })).unwrap();
        assert_eq!(out["note"], "restart it");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nname: laptop\nhub: wss://b\n",
            "comments stay"
        );
        let err = run("config.set", json!({ "key": "name", "value": "''" })).unwrap_err();
        assert_eq!(
            (err.code, err.field.as_deref()),
            (Code::Invalid, Some("value"))
        );
        let err = run("config.get", json!({ "key": "colour" })).unwrap_err();
        assert_eq!(err.code, Code::NotFound);
        assert_eq!(
            run("config.path", json!({})).unwrap()["path"],
            path.display().to_string()
        );
        // A whole file, as an edit of what was read; not of what was before.
        let read = run("config.get", json!({})).unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string();
        let edited = read.replace("laptop", "desk");
        let err = run(
            "config.replace",
            json!({ "text": edited, "before": "name: x\n" }),
        )
        .unwrap_err();
        assert_eq!(err.code, Code::Conflict);
        run("config.replace", json!({ "text": edited, "before": read })).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("name: desk")
        );
        std::fs::remove_file(&path).unwrap();
        let err = run("config.get", json!({})).unwrap_err();
        assert_eq!(
            err.hint.as_deref(),
            Some("`ensemble-runner setup` writes it")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
