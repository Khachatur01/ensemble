//! The settings of a hub or a runner, as its operations reach them: typed,
//! kept in its database by a [`Keep`] (`ensemble_store::kept`), each change
//! checked before it is kept, and kept with who made it, by which operation,
//! with what input: its audit log. Nothing edits them as text.

use ensemble_ops::{Caller, Code, Error, Kind, Spec};
use serde_json::{Value, json};

type Check<S> = dyn Fn(&S) -> Result<(), String> + Send + Sync;
type Changed = dyn Fn() -> Option<String> + Send + Sync;

/// Where settings are kept: the tables of a database.
pub trait Keep<S>: Send + Sync {
    /// The settings kept, or none before any are.
    fn read(&self) -> Result<Option<S>, String>;
    /// Keep `settings` in place of those kept, with `change` in the audit log.
    fn write(&self, settings: &S, change: &Change) -> Result<(), String>;
}

/// A change, as the audit log keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// Who made it: an account's name, or `local` for whoever runs the hub
    /// or the runner on its computer.
    pub who: String,
    /// The operation, such as `backend.add`.
    pub operation: String,
    /// What it was given, its secrets masked.
    pub input: Value,
}

impl Change {
    /// The change `caller` makes by running `spec` with `input`. A secret
    /// field's value is never kept.
    pub fn new(caller: &Caller, spec: &Spec, input: impl serde::Serialize) -> Self {
        let mut input = serde_json::to_value(input).unwrap_or(Value::Null);
        if let Some(fields) = input.as_object_mut() {
            for field in spec.input.iter().filter(|f| f.kind == Kind::Secret) {
                if let Some(value) = fields.get_mut(field.name)
                    && !value.is_null()
                {
                    *value = json!("(hidden)");
                }
            }
        }
        Change {
            who: match caller {
                Caller::Account { name, .. } => name.clone(),
                Caller::Local => "local".into(),
                Caller::Anyone => "anyone".into(),
            },
            operation: spec.name.to_string(),
            input,
        }
    }

    /// A change made by this computer itself, such as its first settings.
    pub fn by_this_computer(operation: &str) -> Self {
        Change {
            who: "local".into(),
            operation: operation.to_string(),
            input: json!({}),
        }
    }
}

/// The settings of a hub or a runner, as its operations read and change them.
pub struct Kept<S> {
    store: Box<dyn Keep<S>>,
    /// What they are called in what is said, such as "the hub's settings".
    name: String,
    /// What makes them when there are none, for the message.
    made_by: String,
    check: Box<Check<S>>,
    changed: Box<Changed>,
}

impl<S: Clone> Kept<S> {
    /// The settings `store` keeps, called `name`, made by `made_by` when there
    /// are none, which `check` loads.
    pub fn new(
        store: impl Keep<S> + 'static,
        name: &str,
        made_by: &str,
        check: impl Fn(&S) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Kept {
            store: Box::new(store),
            name: name.to_string(),
            made_by: made_by.to_string(),
            check: Box::new(check),
            changed: Box::new(|| None),
        }
    }

    /// Do `changed` once a change is kept; what it returns is said.
    pub fn on_change(
        mut self,
        changed: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.changed = Box::new(changed);
        self
    }

    /// What they are called in what is said.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The settings as they are kept now.
    pub fn read(&self) -> Result<S, Error> {
        self.store.read().map_err(Error::failed)?.ok_or_else(|| {
            Error::not_found(format!("{} are not made yet", self.name))
                .hint(format!("{} makes them", self.made_by))
        })
    }

    /// What is wrong with `settings`, if anything.
    pub fn check(&self, settings: &S) -> Result<(), String> {
        (self.check)(settings)
    }

    /// Make `edit` to the settings, and keep the result with `change`, if it
    /// loads; say what the running process did with it. Nothing is kept when
    /// `edit` refuses or the result would not load.
    pub fn change(
        &self,
        change: Change,
        edit: impl FnOnce(&mut S) -> Result<(), Error>,
    ) -> Result<Option<String>, Error> {
        let mut settings = self.read()?;
        edit(&mut settings)?;
        self.keep(&settings, &change)
    }

    /// Keep `settings` in place of those kept, with `change`, if they load.
    pub fn keep(&self, settings: &S, change: &Change) -> Result<Option<String>, Error> {
        self.check(settings)
            .map_err(|e| Error::new(Code::Invalid, format!("nothing was changed: {e}")))?;
        self.store.write(settings, change).map_err(Error::failed)?;
        Ok((self.changed)())
    }

    /// Tell the running process its settings changed though they did not,
    /// such as when a key one of them names did, and say what it did.
    pub fn changed(&self) -> Option<String> {
        (self.changed)()
    }
}

/// A context with kept settings `S`: a hub's or a runner's.
pub trait WithKept<S> {
    fn kept(&self) -> Result<&Kept<S>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_ops::{Access, Field};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Memory(Mutex<(Option<u32>, Vec<Change>)>);

    impl Keep<u32> for std::sync::Arc<Memory> {
        fn read(&self) -> Result<Option<u32>, String> {
            Ok(self.0.lock().unwrap().0)
        }
        fn write(&self, settings: &u32, change: &Change) -> Result<(), String> {
            let mut kept = self.0.lock().unwrap();
            kept.0 = Some(*settings);
            kept.1.push(change.clone());
            Ok(())
        }
    }

    const SET: Spec = Spec {
        name: "count.set",
        summary: "Set it.",
        access: Access::Local,
        input: &[
            Field::new("to", Kind::Number, "To what."),
            Field::secret("key", "A secret."),
        ],
    };

    #[test]
    fn a_change_is_checked_kept_and_logged_without_its_secrets() {
        let memory = std::sync::Arc::new(Memory::default());
        let kept = Kept::new(memory.clone(), "the count", "`init`", |n: &u32| {
            if *n > 10 {
                Err("too big".into())
            } else {
                Ok(())
            }
        })
        .on_change(|| Some("used now".into()));
        let err = kept.read().unwrap_err();
        assert_eq!(err.hint.as_deref(), Some("`init` makes them"));
        kept.keep(&1, &Change::by_this_computer("init")).unwrap();

        let caller = Caller::Account {
            name: "ana".into(),
            permissions: vec![],
            session: true,
            token: "t1".into(),
        };
        let input = json!({ "to": 5, "key": "sk-1" });
        let said = kept
            .change(Change::new(&caller, &SET, &input), |n| {
                *n = 5;
                Ok(())
            })
            .unwrap();
        assert_eq!(said.as_deref(), Some("used now"));
        assert_eq!(kept.read().unwrap(), 5);
        let err = kept
            .change(Change::new(&caller, &SET, &input), |n| {
                *n = 50;
                Ok(())
            })
            .unwrap_err();
        assert!(err.message.contains("nothing was changed: too big"));
        assert_eq!(kept.read().unwrap(), 5);

        let log = &memory.0.lock().unwrap().1;
        assert_eq!(log.len(), 2);
        assert_eq!(
            (log[1].who.as_str(), log[1].operation.as_str()),
            ("ana", "count.set")
        );
        assert_eq!(log[1].input, json!({ "to": 5, "key": "(hidden)" }));
    }
}
