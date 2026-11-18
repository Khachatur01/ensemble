//! `ensemble … config`: one settings file (node.yaml, hub.yaml, or a team's
//! config.yaml) changed by its settings, as text, so that comments and order
//! survive. A change is loaded before it is written, and refused when it would
//! not load.
//!
//! The settings declared in Nix (`services.ensemble.node.settings`,
//! `services.ensemble.hub.settings`) are merged into the file by the Nix
//! module whenever the service starts, which notes them in `.nix-managed`
//! beside it: a change to one of them here would be undone, so it is refused.

use crate::cli::ConfigCommand;
use ensemble::config::settings;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The file the settings of the Nix configuration are noted in, beside the
/// settings file.
const MANAGED_FILE: &str = ".nix-managed";

/// One settings file.
pub struct File<'a> {
    pub path: PathBuf,
    /// What makes it when there is none, for the message.
    pub made_by: &'a str,
    /// What is wrong with a text as this file, if anything.
    pub check: &'a dyn Fn(&str) -> Result<(), String>,
    /// What to say, or do, once it changed.
    pub changed: &'a dyn Fn(),
}

impl File<'_> {
    fn read(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "there is no {}; {} writes it",
                self.path.display(),
                self.made_by
            ),
            _ => format!("cannot read {}: {e}", self.path.display()),
        })
    }

    /// Write `text`, if it loads.
    fn write(&self, text: &str) -> Result<(), String> {
        (self.check)(text).map_err(|e| format!("nothing was changed: {e}"))?;
        write_atomically(&self.path, text)
    }

    /// The settings declared in Nix.
    fn managed(&self) -> Vec<String> {
        let Some(dir) = self.path.parent() else {
            return Vec::new();
        };
        std::fs::read_to_string(dir.join(MANAGED_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Refuse a change of `key` when Nix declares it, or something in it.
    fn not_managed(&self, key: &str) -> Result<(), String> {
        match self.managed().iter().find(|m| settings::overlaps(key, m)) {
            Some(m) => Err(format!(
                "`{m}` is set in your Nix configuration, which sets it again whenever the service starts: change it there"
            )),
            None => Ok(()),
        }
    }

    /// Set `key` to `value`, unless Nix declares it.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.not_managed(key)?;
        let text = settings::set(&self.read()?, key, value)?;
        self.write(&text)
    }

    /// Take `key` out, unless Nix declares it.
    pub fn unset(&self, key: &str) -> Result<(), String> {
        self.not_managed(key)?;
        let text = settings::unset(&self.read()?, key)?;
        self.write(&text)
    }
}

pub fn run(file: &File<'_>, command: ConfigCommand) -> Result<ExitCode, String> {
    match command {
        ConfigCommand::Path => println!("{}", file.path.display()),
        ConfigCommand::Get { key: None } => print!("{}", file.read()?),
        ConfigCommand::Get { key: Some(key) } => match settings::get(&file.read()?, &key)? {
            Some(value) => println!("{value}"),
            None => return Err(format!("{} sets no `{key}`", file.path.display())),
        },
        ConfigCommand::Set { key, value } => {
            file.set(&key, &value)?;
            println!("{key}: set in {}", file.path.display());
            (file.changed)();
        }
        ConfigCommand::Unset { key } => {
            file.unset(&key)?;
            println!("{key}: taken out of {}", file.path.display());
            (file.changed)();
        }
        ConfigCommand::Edit => {
            if edit(file)? {
                (file.changed)();
            }
        }
        ConfigCommand::Merge { file: from } => merge(file, &from)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// Edit a copy in $VISUAL or $EDITOR, and keep it when it loads and leaves
/// the settings declared in Nix as they are. Whether it changed.
fn edit(file: &File<'_>) -> Result<bool, String> {
    let before = file.read()?;
    let name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let copy = file.path.with_file_name(format!(".{name}.edit"));
    write_atomically(&copy, &before)?;
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
        return Ok(false);
    }
    for m in file.managed() {
        if settings::get(&before, &m)? != settings::get(&after, &m)? {
            return Err(format!(
                "`{m}` is set in your Nix configuration: change it there. Your edit is kept in {}",
                copy.display()
            ));
        }
    }
    if let Err(e) = (file.check)(&after) {
        return Err(format!(
            "{} is unchanged, because the edit would not load: {e}\nYour edit is kept in {}",
            file.path.display(),
            copy.display()
        ));
    }
    write_atomically(&file.path, &after)?;
    let _ = std::fs::remove_file(&copy);
    println!("{} is changed", file.path.display());
    Ok(true)
}

/// Merge the settings of the YAML file `from` into the file, and note them as
/// declared in Nix. The file is written even when the merge leaves it
/// incomplete, saying what is missing: the settings not in Nix are made with
/// `ensemble`, and the service says the same when it starts.
fn merge(file: &File<'_>, from: &Path) -> Result<(), String> {
    let doc = std::fs::read_to_string(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let text = match std::fs::read_to_string(&file.path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", file.path.display())),
    };
    let (merged, paths) = settings::merge(&text, &doc)?;
    if merged != text {
        write_atomically(&file.path, &merged)?;
    }
    let dir = file.path.parent().ok_or("the file has no folder")?;
    write_atomically(
        &dir.join(MANAGED_FILE),
        &serde_json::to_string(&paths).map_err(|e| e.to_string())?,
    )?;
    if let Err(e) = (file.check)(&merged) {
        eprintln!(
            "ensemble: note: {} does not load yet: {e}",
            file.path.display()
        );
    }
    Ok(())
}

/// Write through a temporary file beside it, made with the folder if it is not
/// there, so a reader never sees half of it.
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}
