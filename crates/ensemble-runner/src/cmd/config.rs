//! `ensemble-runner team config`: a team's settings file, its config.yaml, as
//! the command line reaches it: through the same `config.*` operations as the
//! runner's own settings, and what only a command line does with one
//! (`ensemble_ops::cli`).

use crate::cli::ConfigCommand;
use ensemble_config::settings_file::{self, SettingsFile, WithSettings};
use ensemble_ops::{Access, Caller, Error, Registry};
use serde_json::{Value, json};
use std::process::ExitCode;

/// A team's settings file, as its context.
struct Team(SettingsFile);

impl WithSettings for Team {
    fn settings_file(&self) -> Result<&SettingsFile, Error> {
        Ok(&self.0)
    }
}

/// A team's settings file, by the command given.
pub fn run(file: SettingsFile, command: ConfigCommand) -> Result<ExitCode, String> {
    let registry = Registry::new(settings_file::operations::<Team>(Access::Local));
    let team = Team(file);
    let call = |name: &str, input: Value| registry.run(&team, &Caller::Local, name, input);
    let (name, input) = match command {
        ConfigCommand::Get { key } => ("config.get", json!({ "key": key })),
        ConfigCommand::Set { key, value } => ("config.set", json!({ "key": key, "value": value })),
        ConfigCommand::Unset { key } => ("config.unset", json!({ "key": key })),
        ConfigCommand::Path => ("config.path", json!({})),
        ConfigCommand::Edit => {
            return ensemble_ops::cli::edit(&call).map(|()| ExitCode::SUCCESS);
        }
    };
    let output = call(name, input).map_err(|e| e.to_string())?;
    let said = registry.get(name).ok_or("no such operation")?.show(&output);
    println!("{}", said.out);
    if let Some(note) = said.note {
        eprintln!("{note}");
    }
    Ok(ExitCode::SUCCESS)
}
