//! `ensemble … config`: a settings file (`SettingsFile`) as the command line
//! reaches it. For the hub and the node, `get`, `set`, `unset` and `path` are
//! their `config.*` operations; this adds what only a command line does:
//! `edit`, in $VISUAL or $EDITOR, and `merge`, what the Nix modules run. A
//! team's settings file is reached here whole.

use crate::cli::ConfigCommand;
use ensemble::config::SettingsFile;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

/// `config edit` or `config merge`, when `matches` (a hub's or a node's
/// group of commands) names one, on `file`.
pub fn run_extra(
    matches: &clap::ArgMatches,
    file: impl FnOnce() -> Result<SettingsFile, String>,
) -> Option<Result<ExitCode, String>> {
    let ("config", config) = matches.subcommand()? else {
        return None;
    };
    match config.subcommand()? {
        ("edit", _) => Some(file().and_then(|f| edit(&f).map(|()| ExitCode::SUCCESS))),
        ("merge", m) => {
            let from = m.get_one::<PathBuf>("file")?.clone();
            Some(file().and_then(|f| merge(&f, &from)))
        }
        _ => None,
    }
}

/// A team's settings file, by the command given.
pub fn run(file: &SettingsFile, command: ConfigCommand) -> Result<ExitCode, String> {
    let say = |note: Option<String>| {
        if let Some(note) = note {
            eprintln!("{note}");
        }
    };
    match command {
        ConfigCommand::Path => println!("{}", file.path.display()),
        ConfigCommand::Get { key: None } => print!("{}", file.read().map_err(|e| e.to_string())?),
        ConfigCommand::Get { key: Some(key) } => {
            let text = file.read().map_err(|e| e.to_string())?;
            match ensemble::config::settings::get(&text, &key)? {
                Some(value) => println!("{value}"),
                None => return Err(format!("{} sets no `{key}`", file.path.display())),
            }
        }
        ConfigCommand::Set { key, value } => {
            let note = file.set(&key, &value).map_err(|e| e.to_string())?;
            println!("{key}: set in {}", file.path.display());
            say(note);
        }
        ConfigCommand::Unset { key } => {
            let note = file.unset(&key).map_err(|e| e.to_string())?;
            println!("{key}: taken out of {}", file.path.display());
            say(note);
        }
        ConfigCommand::Edit => edit(file)?,
        ConfigCommand::Merge { file: from } => return merge(file, &from),
    }
    Ok(ExitCode::SUCCESS)
}

/// Edit a copy in $VISUAL or $EDITOR, and keep it when it loads and leaves
/// the settings declared in Nix as they are.
fn edit(file: &SettingsFile) -> Result<(), String> {
    let before = file.read().map_err(|e| e.to_string())?;
    let name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let copy = file.path.with_file_name(format!(".{name}.edit"));
    ensemble::config::settings_file::write_atomically(&copy, &before)?;
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
            file.path.display()
        ));
    }
    let after = std::fs::read_to_string(&copy).map_err(|e| e.to_string())?;
    if after == before {
        let _ = std::fs::remove_file(&copy);
        println!("{} is unchanged", file.path.display());
        return Ok(());
    }
    match file.replace(&before, &after) {
        Ok(note) => {
            let _ = std::fs::remove_file(&copy);
            println!("{} is changed", file.path.display());
            if let Some(note) = note {
                eprintln!("{note}");
            }
            Ok(())
        }
        Err(e) => Err(format!(
            "{} is unchanged: {e}\nYour edit is kept in {}",
            file.path.display(),
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
