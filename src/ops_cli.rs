//! The CLI as an interface over operations: each operation the one who runs a
//! hub or a node may run is a command, made from its spec. A name's dots are
//! the command's path (`user.add` is `ensemble hub user add`), its fields its
//! arguments: positional ones by their place, the others as `--options`. A
//! secret is never an argument: when an operation needs one, it is asked for,
//! without echo, or read from stdin, and the operation runs again.

use ensemble::ops::{Access, Caller, Code, Field, Kind, Registry};
use serde_json::{Map, Value, json};
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

/// Run the operation `name` as the one who runs it, with what `matches`
/// gives, asking for the secrets it needs; print what it says, its answer on
/// stdout and its note on stderr.
pub fn run<C>(
    registry: &Registry<C>,
    context: &C,
    name: &str,
    matches: &clap::ArgMatches,
) -> Result<ExitCode, String> {
    let op = registry.get(name).ok_or("no such operation")?;
    let mut given = input(op.spec.input, matches);
    let output = loop {
        match registry.run(context, &Caller::Local, name, given.clone()) {
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
                        let answer = crate::read_secret(&format!("{}: ", field.help))?;
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
