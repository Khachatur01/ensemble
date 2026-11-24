//! The runner's command line: what each command takes, and the help it
//! shows. Its commands are for this computer's runner, its teams and their
//! agents.

use clap::{CommandFactory, Parser, Subcommand};
use ensemble_ops::Registry;
use ensemble_ops::cli::mount;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Files, in --config-dir (by default ~/.local/state/ensemble):
  runner/core/      database.db (its settings, the keys of its backends and
                    its hub tokens, sealed), master.key
  runner/teams/     its helper teams, one folder each
  runner/workdirs/  run logs and worktrees, per repository

Its settings are kept in its core's database, and changed with `settings`,
`backend` and `login`; `settings show` shows them, `settings history` who
changed what. A key given to `backend add` or `backend key` is kept there,
sealed by master.key: back up both. A key a secrets manager keeps is read from
the environment, and named with `backend key --key-env`.";

/// This computer's Ensemble runner: it runs the tasks of your hub with your
/// helper teams.
#[derive(Debug, Parser)]
#[command(name = "ensemble-runner", version, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// The folder everything is kept in, the runner in its runner/; by
    /// default ~/.local/state/ensemble.
    #[arg(long, global = true, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// The folder an agent works in for `agent run`, and where `agent health`
    /// and `doctor` start agent CLIs. Default: the current directory.
    #[arg(short, long, global = true, value_name = "PATH")]
    pub dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

/// The whole command line: the commands below, with those made from the
/// runner's operations (`ensemble_ops::cli`).
pub fn command<R>(runner: &Registry<R>) -> clap::Command {
    let groups = [
        (
            "settings",
            "The runner's settings, kept in its database: every one, who changed them, and each of them.",
        ),
        (
            "backend",
            "The LLM backends the runner's teams run on, their keys, and which it shares.",
        ),
    ];
    mount(Cli::command(), runner, &groups)
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set this computer up: find the agent CLIs, write the runner if there is
    /// none, and check the teams.
    Setup {
        /// The runner's name; asked, or this computer's name.
        #[arg(long)]
        name: Option<String>,
        /// The folder it works in, of git repositories; asked, or ~/ensemble.
        #[arg(long, value_name = "PATH")]
        workspace: Option<String>,
        /// Ask nothing: take what is given and found.
        #[arg(long)]
        yes: bool,
    },
    /// Make the runner as `setup` does, asking nothing: named for this
    /// computer, working for you in ~/ensemble, offering nothing.
    Init {
        /// Make it anew when there is one.
        #[arg(long, conflicts_with = "keep")]
        force: bool,
        /// When there is one, keep it and say nothing: what the NixOS module
        /// runs before each start.
        #[arg(long)]
        keep: bool,
    },
    /// Run the runner until Ctrl-C or SIGTERM: it works on the tasks of the git
    /// repositories in its workspace. A computer runs one.
    Serve,
    /// Have the running runner read its teams again, after one was changed by
    /// hand.
    Reload,
    /// The runner: its name, its workspace, its hub, whether it runs, and its
    /// teams.
    Status,
    /// Say what is wrong with the runner's teams and the CLIs and models they
    /// need, and how to fix each thing.
    Doctor,
    /// Your runners connected to your hub, with their agents and models.
    Peers,
    /// The runner's helper teams.
    #[command(subcommand)]
    Team(TeamCommand),
    /// The agents of the runner's teams, named <team>/<agent>.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Print a completion script for a shell, such as `ensemble-runner
    /// completions zsh > ~/.zfunc/_ensemble-runner`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Every team, where it comes from, and its agents.
    List,
    /// Make a team: a sample config.yaml with every setting, to edit.
    New {
        #[arg(required_unless_present = "stdout")]
        name: Option<String>,
        /// Print the sample instead.
        #[arg(long)]
        stdout: bool,
    },
    /// Link a folder with a config.yaml, such as a checkout of your company's
    /// team, as the team NAME.
    Link { name: String, dir: PathBuf },
    /// Pull a team from a git repository, pinned to the commit it is at, once
    /// you trust that commit.
    Pull {
        /// The repository, with @REF for a tag or branch, or #COMMIT.
        source: String,
        /// Its name; the repository's by default.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Move a pulled team to the commit a ref is at now, once you trust it.
    Update {
        name: String,
        /// The tag or branch; the default branch otherwise.
        #[arg(long = "ref", value_name = "REF")]
        git_ref: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Read a team an agent wrote, and let it run.
    Approve {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Take a team out of the runner; a linked folder or a pulled clone is left.
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Load and validate every team, then report them as one helper team.
    Check,
    /// Print the JSON Schema of config.yaml, for an editor.
    Schema,
    /// A team's settings, its config.yaml: one made here or linked.
    Config {
        name: String,
        #[command(subcommand)]
        action: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Every agent, and what runs it.
    List,
    /// How an agent is started: its command line and the config handed to
    /// its CLI, API keys hidden, and its models in order.
    Show {
        agent: String,
        /// Ask the backends which model a run would use now.
        #[arg(long)]
        probe: bool,
    },
    /// Send one task to one agent and print its answer.
    Run {
        agent: String,
        #[arg(required = true, num_args = 1..)]
        task: Vec<String>,
        /// Background the agent needs.
        #[arg(long)]
        context: Option<String>,
    },
    /// Check that every agent CLI starts and every model server answers and
    /// serves its agents' models.
    Health,
    /// What each agent has done: runs, failures, time, tokens.
    Stats {
        /// Only runs this recent, such as 7d or 12h.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add an agent to a team, every setting written: AGENT is <team>/<name>.
    Add {
        agent: String,
        /// claude-code, opencode, codex, builtin or direct.
        #[arg(long)]
        adapter: String,
        #[arg(long)]
        model: Option<String>,
        /// The LLM backend it runs on, for opencode, builtin and direct.
        #[arg(long)]
        backend: Option<String>,
        /// What it is for.
        #[arg(long)]
        prompt: Option<String>,
        /// Let it change files.
        #[arg(long)]
        writes: bool,
    },
    /// Remove an agent from its team: AGENT is <team>/<name>.
    Remove { agent: String },
}

/// A team's settings file, changed by its settings: each named by its keys
/// joined by dots, such as `agents.coder.max_concurrent`. Comments and order
/// are kept, and a change that would not load is refused.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print a setting, or the whole file.
    Get { key: Option<String> },
    /// Set a setting. The value is YAML: `laptop`, `8`, `true`, `[a, b]`,
    /// `{}`; anything else is written as a string.
    Set { key: String, value: String },
    /// Take a setting out.
    Unset { key: String },
    /// Open the file in $VISUAL or $EDITOR, and keep the change only if it
    /// loads.
    Edit,
    /// Print where the file is.
    Path,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole() -> clap::Command {
        command(&crate::cmd::runner::operations())
    }

    #[test]
    fn the_command_line_is_well_formed() {
        whole().debug_assert();
    }

    #[test]
    fn operations_are_commands_with_their_fields_as_arguments() {
        let runner = crate::cmd::runner::operations();
        let matches = whole()
            .try_get_matches_from(["ensemble-runner", "login", "wss://hub", "--user", "ana"])
            .unwrap();
        let (name, leaf) = ensemble_ops::cli::matched(&runner, &matches).unwrap();
        assert_eq!(name, "login");
        assert_eq!(leaf.get_one::<String>("url").unwrap(), "wss://hub");
        // A secret is never an argument.
        assert!(
            whole()
                .try_get_matches_from(["ensemble-runner", "login", "wss://hub", "--password", "x"])
                .is_err()
        );
    }
}
