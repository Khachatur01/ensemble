//! One settings file (node.yaml, hub.yaml, or a team's config.yaml) changed
//! by its settings, as text, so that comments and order survive
//! ([`settings`](crate::settings)), and the `config.*` operations on it. A
//! change is loaded before it is written, and refused when it would not load.
//!
//! The settings declared in Nix (`services.ensemble.node.settings`,
//! `services.ensemble.hub.settings`) are merged into the file by the Nix
//! module whenever the service starts, which notes them in `.nix-managed`
//! beside it: a change to one of them would be undone, so it is refused.

use crate::settings;
use ensemble_ops::{Access, Caller, Code, Error, Field, Operation, Spec, operation};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The file the settings of the Nix configuration are noted in, beside the
/// settings file.
pub const MANAGED_FILE: &str = ".nix-managed";

type Check = dyn Fn(&str) -> Result<(), String> + Send + Sync;
type Changed = dyn Fn() -> Option<String> + Send + Sync;

/// One settings file.
pub struct SettingsFile {
    pub path: PathBuf,
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
            path,
            made_by: made_by.to_string(),
            check: Box::new(check),
            changed: Box::new(|| None),
        }
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
        std::fs::read_to_string(&self.path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                Error::not_found(format!("there is no {}", self.path.display()))
                    .hint(format!("{} writes it", self.made_by))
            }
            _ => Error::failed(format!("cannot read {}: {e}", self.path.display())),
        })
    }

    /// What is wrong with `text` as this file, if anything.
    pub fn check(&self, text: &str) -> Result<(), String> {
        (self.check)(text)
    }

    /// Write `text`, if it loads.
    fn write(&self, text: &str, field: &str) -> Result<(), Error> {
        self.check(text)
            .map_err(|e| Error::invalid(format!("nothing was changed: {e}")).on(field))?;
        write_atomically(&self.path, text).map_err(Error::failed)
    }

    /// The settings declared in Nix.
    pub fn managed(&self) -> Vec<String> {
        let Some(dir) = self.path.parent() else {
            return Vec::new();
        };
        std::fs::read_to_string(dir.join(MANAGED_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Refuse a change of `key` when Nix declares it, or something in it.
    fn not_managed(&self, key: &str) -> Result<(), Error> {
        match self.managed().iter().find(|m| settings::overlaps(key, m)) {
            Some(m) => Err(Error::new(
                Code::Conflict,
                format!(
                    "`{m}` is set in your Nix configuration, which sets it again whenever the service starts"
                ),
            )
            .on("key")
            .hint("change it there")),
            None => Ok(()),
        }
    }

    /// Set `key` to `value`, unless Nix declares it, and say what changed.
    pub fn set(&self, key: &str, value: &str) -> Result<Option<String>, Error> {
        self.not_managed(key)?;
        let text =
            settings::set(&self.read()?, key, value).map_err(|e| Error::invalid(e).on("key"))?;
        self.write(&text, "value")?;
        Ok((self.changed)())
    }

    /// Take `key` out, unless Nix declares it, and say what changed.
    pub fn unset(&self, key: &str) -> Result<Option<String>, Error> {
        self.not_managed(key)?;
        let text = settings::unset(&self.read()?, key).map_err(|e| Error::invalid(e).on("key"))?;
        self.write(&text, "key")?;
        Ok((self.changed)())
    }

    /// Write `text` in place of the file, as an edit of it: refused when it
    /// would not load or changes what Nix declares, and, given `before`, the
    /// text it was made from, when the file changed since. What to say of it.
    pub fn replace(&self, text: &str, before: Option<&str>) -> Result<Option<String>, Error> {
        let now = self.read()?;
        if before.is_some_and(|b| b != now) {
            return Err(Error::new(
                Code::Conflict,
                format!("{} changed since it was read", self.path.display()),
            )
            .hint("read it again, and make the change on what it is now"));
        }
        for m in self.managed() {
            let get = |t: &str| settings::get(t, &m).map_err(Error::invalid);
            if get(&now)? != get(text)? {
                return Err(Error::new(
                    Code::Conflict,
                    format!("`{m}` is set in your Nix configuration"),
                )
                .hint("change it there"));
            }
        }
        self.write(text, "text")?;
        Ok((self.changed)())
    }

    /// Merge the settings of the YAML file `from` into the file, and note them
    /// as declared in Nix. The file is written even when the merge leaves it
    /// incomplete: the settings not in Nix are made with `ensemble`. What is
    /// missing is said.
    pub fn merge(&self, from: &Path) -> Result<Option<String>, Error> {
        let doc = std::fs::read_to_string(from)
            .map_err(|e| Error::failed(format!("{}: {e}", from.display())))?;
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(Error::failed(format!(
                    "cannot read {}: {e}",
                    self.path.display()
                )));
            }
        };
        let (merged, paths) = settings::merge(&text, &doc).map_err(Error::invalid)?;
        if merged != text {
            write_atomically(&self.path, &merged).map_err(Error::failed)?;
        }
        let dir = self
            .path
            .parent()
            .ok_or_else(|| Error::failed("the file has no folder"))?;
        let paths = serde_json::to_string(&paths).map_err(|e| Error::failed(e.to_string()))?;
        write_atomically(&dir.join(MANAGED_FILE), &paths).map_err(Error::failed)?;
        Ok(self
            .check(&merged)
            .err()
            .map(|e| format!("{} does not load yet: {e}", self.path.display())))
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

/// A context with a settings file: a hub's or a node's.
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

const KEY: &str = "The setting: its keys joined by dots, such as `server.listen`.";
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
                                Error::not_found(format!("{} sets no `{key}`", file.path.display()))
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
                    "said": format!("{}: set in {}", input.key, file.path.display()),
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
                    "said": format!("{}: taken out of {}", input.key, file.path.display()),
                    "note": note,
                }))
            },
        ),
        operation(
            Spec {
                name: "config.replace",
                summary: "Write the whole file, as an edit of it: refused when it would not load, changes what Nix declares, or the file changed since `before` was read.",
                access,
                input: REPLACE,
            },
            |c: &C, _: &Caller, input: Replace| {
                let file = c.settings_file()?;
                if input.before.as_deref() == Some(input.text.as_str()) {
                    return Ok(json!({ "said": format!("{} is unchanged", file.path.display()) }));
                }
                let note = file.replace(&input.text, input.before.as_deref())?;
                Ok(json!({
                    "said": format!("{} is changed", file.path.display()),
                    "note": note,
                }))
            },
        ),
        operation(
            Spec {
                name: "config.path",
                summary: "Print where the file is.",
                access,
                input: &[],
            },
            |c: &C, _: &Caller, _: Nothing| {
                let path = c.settings_file()?.path.display().to_string();
                Ok(json!({ "said": path, "path": path }) as Value)
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
        let path = dir.join("node.yaml");
        std::fs::write(&path, "# mine\nname: laptop\nhub: wss://a\n").unwrap();
        let file = SettingsFile::new(path.clone(), "`ensemble node setup`", |t| {
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
        let err = run("config.get", json!({ "key": "access" })).unwrap_err();
        assert_eq!(err.code, Code::NotFound);
        std::fs::write(dir.join(MANAGED_FILE), r#"["hub"]"#).unwrap();
        let err = run("config.unset", json!({ "key": "hub" })).unwrap_err();
        assert_eq!(
            (err.code, err.hint.as_deref()),
            (Code::Conflict, Some("change it there"))
        );
        assert_eq!(
            run("config.path", json!({})).unwrap()["path"],
            path.display().to_string()
        );
        // A whole file, as an edit of what was read; not of what was before.
        std::fs::remove_file(dir.join(MANAGED_FILE)).unwrap();
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
        assert_eq!(err.hint.as_deref(), Some("`ensemble node setup` writes it"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
