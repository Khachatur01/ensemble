//! `ensemble … config`: a settings file as the command line reaches it. For
//! the hub and the runner, `get`, `set`, `unset`, `replace` and `path` are their
//! `config.*` operations; this adds what only a command line does: `edit`, in
//! $VISUAL or $EDITOR, which reads and writes through the operations, and
//! `merge`, what the Nix modules run before the service starts. A team's
//! settings file is reached through the same operations, here.

use crate::cli::ConfigCommand;
use ensemble::config::settings_file::{self, SettingsFile, WithSettings};
use ensemble::ops::{Access, Caller, Error, Registry};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// How an operation is run, where it runs.
pub type Call<'a> = &'a dyn Fn(&str, Value) -> Result<Value, Error>;

/// `command`'s `config` group with `edit` and `merge`, which only a command
/// line has.
pub fn extras(command: clap::Command) -> clap::Command {
    command.mut_subcommand("config", |config| {
        config
            .subcommand(
                clap::Command::new("edit").about(
                    "Open the file in $VISUAL or $EDITOR, and keep the change only if it loads",
                ),
            )
            .subcommand(
                clap::Command::new("merge")
                    .about("Merge the settings of a YAML file into it and keep them as declared in Nix: what the Nix modules run")
                    .hide(true)
                    .arg(
                        clap::Arg::new("file")
                            .required(true)
                            .value_parser(clap::value_parser!(PathBuf)),
                    ),
            )
    })
}

/// `config edit` or `config merge`, when `matches` (a hub's or a runner's
/// group of commands) names one: `edit` through `call`, `merge` on `file`,
/// before the service starts.
pub fn run_extra(
    matches: &clap::ArgMatches,
    call: Call<'_>,
    file: impl FnOnce() -> Result<SettingsFile, String>,
) -> Option<Result<ExitCode, String>> {
    let ("config", config) = matches.subcommand()? else {
        return None;
    };
    match config.subcommand()? {
        ("edit", _) => Some(edit(call).map(|()| ExitCode::SUCCESS)),
        ("merge", m) => {
            let from = m.get_one::<PathBuf>("file")?.clone();
            Some(file().and_then(|f| merge(&f, &from)))
        }
        _ => None,
    }
}

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
        ConfigCommand::Edit => return edit(&call).map(|()| ExitCode::SUCCESS),
        ConfigCommand::Merge { file: from } => return merge(&team.0, &from),
    };
    let output = call(name, input).map_err(|e| e.to_string())?;
    let said = registry.get(name).ok_or("no such operation")?.show(&output);
    println!("{}", said.out);
    if let Some(note) = said.note {
        eprintln!("{note}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Edit a copy of the file in $VISUAL or $EDITOR, and keep it through
/// `config.replace`, which refuses it when it would not load, changes what
/// Nix declares, or the file changed meanwhile; then the copy stays.
fn edit(call: Call<'_>) -> Result<(), String> {
    let text = |out: &Value, key: &str| out[key].as_str().unwrap_or_default().to_string();
    let path = PathBuf::from(text(
        &call("config.path", json!({})).map_err(|e| e.to_string())?,
        "path",
    ));
    let before = text(
        &call("config.get", json!({})).map_err(|e| e.to_string())?,
        "text",
    );
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let copy = path.with_file_name(format!(".{name}.edit"));
    settings_file::write_atomically(&copy, &before)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&copy)
        .status()
        .map_err(|e| format!("cannot run {editor}: {e}"))?;
    if !status.success() {
        return Err(format!(
            "{editor} failed ({status}); {} is unchanged",
            path.display()
        ));
    }
    let after = std::fs::read_to_string(&copy).map_err(|e| e.to_string())?;
    match call("config.replace", json!({ "text": after, "before": before })) {
        Ok(out) => {
            let _ = std::fs::remove_file(&copy);
            println!("{}", text(&out, "said"));
            if let Some(note) = out["note"].as_str() {
                eprintln!("{note}");
            }
            Ok(())
        }
        Err(e) => Err(format!(
            "{} is unchanged: {e}\nYour edit is kept in {}",
            path.display(),
            copy.display()
        )),
    }
}

/// Merge the settings of the YAML file `from` into `file`, as declared in
/// Nix, saying what is missing still.
fn merge(file: &SettingsFile, from: &Path) -> Result<ExitCode, String> {
    if let Some(note) = file.merge(from).map_err(|e| e.to_string())? {
        eprintln!("ensemble: note: {note}");
    }
    Ok(ExitCode::SUCCESS)
}
