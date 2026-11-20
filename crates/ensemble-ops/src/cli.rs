//! The command line as an interface over operations, and what else the
//! command lines of `ensemble-hub` and `ensemble-runner` share.
//!
//! Each operation the one who runs a hub or a runner may run is a command,
//! made from its spec. A name's dots are the command's path (`user.add` is
//! `ensemble-hub user add`), its fields its arguments: positional ones by
//! their place, the others as `--options`. A secret is never an argument: when
//! an operation needs one, it is asked for, without echo, or read from stdin,
//! and the operation runs again.
//!
//! A settings file's `config` group gets what only a command line does with
//! it ([`config_extras`]): `edit`, in $VISUAL or $EDITOR, which reads and
//! writes through the `config.*` operations, and, for a file Nix declares
//! settings of, `merge`, what the Nix modules run before the service starts.

use crate::{Access, Code, Error, Field, Kind, Registry};
use serde_json::{Map, Value, json};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// `command` with a subcommand for each of the operations of `registry` the
/// one who runs it may run, grouped by the first part of their names, each
/// group with the words of `groups`.
pub fn mount<C>(
    mut command: clap::Command,
    registry: &Registry<C>,
    groups: &[(&'static str, &'static str)],
) -> clap::Command {
    for op in registry.operations() {
        // Only what the one who runs it does: not what an account does for itself,
        // nor signing up or in.
        if !matches!(op.spec.access, Access::Local | Access::Permission(_)) {
            continue;
        }
        let path: Vec<&'static str> = op.spec.name.split('.').collect();
        let leaf = leaf(path[path.len() - 1], op.spec.summary, op.spec.input);
        command = match path.as_slice() {
            [_] => command.subcommand(leaf),
            [group, _] => {
                if command.find_subcommand(group).is_none() {
                    let about = groups
                        .iter()
                        .find(|(g, _)| g == group)
                        .map_or("", |(_, about)| about);
                    command = command.subcommand(
                        clap::Command::new(*group)
                            .about(line(about))
                            .subcommand_required(true)
                            .arg_required_else_help(true),
                    );
                }
                command.mut_subcommand(group, |g| g.subcommand(leaf))
            }
            _ => panic!("operation {} is more than a group and a verb", op.spec.name),
        };
    }
    command
}

/// The command of one operation.
fn leaf(verb: &'static str, summary: &'static str, input: &'static [Field]) -> clap::Command {
    let mut command = clap::Command::new(verb).about(line(summary));
    for field in input {
        if field.kind == Kind::Secret {
            continue;
        }
        let mut arg = clap::Arg::new(field.name).help(line(field.help));
        arg = if field.positional {
            arg.value_name(field.name.to_uppercase())
                .required(field.required)
        } else {
            arg.long(field.name.replace('_', "-"))
                .value_name(field.name.to_uppercase())
                .required(field.required)
        };
        arg = match field.kind {
            Kind::Flag => arg
                .action(clap::ArgAction::SetTrue)
                .value_name(None::<&str>),
            Kind::Number => arg.value_parser(clap::value_parser!(u64)),
            Kind::List => arg.value_delimiter(',').action(clap::ArgAction::Append),
            Kind::Choice(choices) => {
                arg.value_parser(clap::builder::PossibleValuesParser::new(choices))
            }
            Kind::Text | Kind::Secret => arg,
        };
        command = command.arg(arg);
    }
    command
}

/// The operation `matches` names, a subcommand of what `mount` made, if it is
/// one of `registry`'s: its name and its matches.
pub fn matched<'m, C>(
    registry: &Registry<C>,
    matches: &'m clap::ArgMatches,
) -> Option<(&'static str, &'m clap::ArgMatches)> {
    let (first, sub) = matches.subcommand()?;
    let (name, leaf) = match sub.subcommand() {
        Some((second, leaf)) => (format!("{first}.{second}"), leaf),
        None => (first.to_string(), sub),
    };
    let op = registry.get(&name)?;
    Some((op.spec.name, leaf))
}

/// The input of `fields`, from what was given on the command line.
fn input(fields: &[Field], matches: &clap::ArgMatches) -> Value {
    let mut out = Map::new();
    for field in fields {
        let value = match field.kind {
            Kind::Secret => continue,
            Kind::Flag => json!(matches.get_flag(field.name)),
            Kind::Number => match matches.get_one::<u64>(field.name) {
                Some(n) => json!(n),
                None => continue,
            },
            Kind::List => match matches.get_many::<String>(field.name) {
                Some(items) => json!(items.collect::<Vec<_>>()),
                None => continue,
            },
            Kind::Text | Kind::Choice(_) => match matches.get_one::<String>(field.name) {
                Some(text) => json!(text),
                None => continue,
            },
        };
        out.insert(field.name.to_string(), value);
    }
    Value::Object(out)
}

/// Run the operation `name` of `registry` through `call`, where it runs, with
/// what `matches` gives, asking for the secrets it needs; print what it says,
/// its answer on stdout and its note on stderr.
pub fn run<C>(
    registry: &Registry<C>,
    call: &dyn Fn(&str, Value) -> Result<Value, Error>,
    name: &str,
    matches: &clap::ArgMatches,
) -> Result<ExitCode, String> {
    let op = registry.get(name).ok_or("no such operation")?;
    let mut given = input(op.spec.input, matches);
    let output = loop {
        match call(name, given.clone()) {
            Ok(output) => break output,
            Err(e) => {
                // A secret it needs, not asked for yet: ask, and run again.
                let secret = op.spec.input.iter().find(|f| {
                    f.kind == Kind::Secret
                        && e.code == Code::Invalid
                        && e.field.as_deref() == Some(f.name)
                        && given.get(f.name).is_none()
                });
                match secret {
                    Some(field) => {
                        let answer = read_secret(&format!("{}: ", field.help))?;
                        given[field.name] = json!(answer);
                    }
                    None => return Err(e.to_string()),
                }
            }
        }
    };
    let said = op.show(&output);
    if !said.out.is_empty() {
        println!("{}", said.out);
    }
    if let Some(note) = said.note {
        eprintln!("{note}");
    }
    Ok(ExitCode::SUCCESS)
}

/// A summary or a help as clap shows its own: without the closing period.
fn line(text: &'static str) -> &'static str {
    text.strip_suffix('.').unwrap_or(text)
}

/// How an operation is run, where it runs.
pub type Call<'a> = &'a dyn Fn(&str, Value) -> Result<Value, Error>;

/// `command`'s `config` group with `edit` and, when Nix may declare its
/// settings, `merge`, which only a command line has.
pub fn config_extras(command: clap::Command, merge: bool) -> clap::Command {
    command.mut_subcommand("config", |config| {
        let config = config.subcommand(
            clap::Command::new("edit")
                .about("Open the settings in $VISUAL or $EDITOR, and keep the change only if it loads"),
        );
        if !merge {
            return config;
        }
        config.subcommand(
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

/// Whether `matches` names `config edit` or `config merge`, which only the
/// command line has.
pub fn is_config_extra(matches: &clap::ArgMatches) -> bool {
    matches!(
        matches.subcommand(),
        Some(("config", c)) if matches!(c.subcommand_name(), Some("edit" | "merge"))
    )
}

/// `config edit` or `config merge`, when `matches` names one: `edit` through
/// `call`, `merge` of the file it names by `merge`, before the service starts,
/// which says what is missing still.
pub fn run_config_extra(
    matches: &clap::ArgMatches,
    call: Call<'_>,
    merge: impl FnOnce(&Path) -> Result<Option<String>, String>,
) -> Option<Result<ExitCode, String>> {
    let ("config", config) = matches.subcommand()? else {
        return None;
    };
    match config.subcommand()? {
        ("edit", _) => Some(edit(call).map(|()| ExitCode::SUCCESS)),
        ("merge", m) => {
            let from = m.get_one::<PathBuf>("file")?;
            Some(merge(from).map(|missing| {
                if let Some(note) = missing {
                    eprintln!("ensemble: note: {note}");
                }
                ExitCode::SUCCESS
            }))
        }
        _ => None,
    }
}

/// Edit a copy of the settings file in $VISUAL or $EDITOR, and keep it
/// through `config.replace`, which refuses it when it would not load, changes
/// what Nix declares, or the file changed meanwhile; then the copy stays.
pub fn edit(call: Call<'_>) -> Result<(), String> {
    let text = |out: &Value, key: &str| out[key].as_str().unwrap_or_default().to_string();
    let path = PathBuf::from(text(
        &call("config.path", json!({})).map_err(|e| e.to_string())?,
        "path",
    ));
    let before = text(
        &call("config.get", json!({})).map_err(|e| e.to_string())?,
        "text",
    );
    // A YAML file, for the editor, beside the file or the database that keeps
    // the settings.
    let name = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let copy = path.with_file_name(format!(".{name}.edit.yaml"));
    if let Some(dir) = copy.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&copy, &before).map_err(|e| format!("cannot write {}: {e}", copy.display()))?;
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

/// The file of `NAME=value` lines read for the variables the environment does
/// not set, such as API keys: `given` (`--env-file`), or else `.env` in `dir`
/// when there is one.
pub fn env_file(given: Option<&Path>, dir: &Path) -> Option<PathBuf> {
    if let Some(file) = given {
        return Some(file.to_path_buf());
    }
    let file = dir.join(ensemble_core::state::ENV_FILE);
    if !file.is_file() {
        return None;
    }
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(&file) {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "ensemble: note: other users can read {}; `chmod 600` it",
                file.display()
            );
        }
    }
    Some(file)
}

/// A secret, such as a password: typed at the terminal without echo, or read
/// from stdin when there is no terminal, without its line end.
pub fn read_secret(prompt: &str) -> Result<String, String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdin.lock(), &mut text)
            .map_err(|e| format!("cannot read from stdin: {e}"))?;
        return Ok(text.trim_end_matches(['\n', '\r']).to_string());
    }
    eprint!("{prompt}");
    #[cfg(unix)]
    let restore = unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut term) == 0 {
            let before = term;
            term.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Some(before)
        } else {
            None
        }
    };
    let mut line = String::new();
    let read = stdin.read_line(&mut line);
    #[cfg(unix)]
    if let Some(before) = restore {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &before);
        }
    }
    eprintln!();
    read.map_err(|e| format!("cannot read the terminal: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Ask a question on the terminal and read the answer; empty means `default`.
pub fn ask(question: &str, default: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    match default {
        Some(d) => eprint!("{question} [{d}]: "),
        None => eprint!("{question}: "),
    }
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read an answer: {e}"))?;
    let line = line.trim();
    match (line.is_empty(), default) {
        (true, Some(d)) => Ok(d.to_string()),
        (true, None) => Err(format!("{question}: an answer is needed")),
        _ => Ok(line.to_string()),
    }
}

/// Ask yes or no, or take `yes` (`--yes`) for the answer. Without a terminal
/// and without `--yes` there is nobody to ask, which is a refusal.
pub fn confirm(question: &str, yes: bool) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "{question} There is no terminal to ask on: pass --yes to approve."
        ));
    }
    let answer = ask(&format!("{question} [y/N]"), Some("n"))?;
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}
