//! The runner's command line: what each command takes, and the help it
//! shows. Its commands are for this computer's runner: its settings, its
//! ANN pool and its harnesses.

use clap::{CommandFactory, Parser, Subcommand};
use ensemble_ops::Registry;
use ensemble_ops::cli::mount;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Files, in --config-dir (by default ~/.local/state/ensemble):
  runner/core/      database.db (its settings, the keys of its providers and
                    its hub tokens, sealed), master.key
  runner/workdirs/  run logs and worktrees, per repository
  runner/records.jsonl  every run: its harness, model, time and tokens

Its settings are kept in its core's database, and changed with `settings`,
`provider`, `model`, `harness` and `login`; `settings show` shows them, `settings history`
who changed what. A key given to `provider add` or `provider key` is kept there,
sealed by master.key: back up both. A key a secrets manager keeps is read from
the environment, and named with `provider key --key-env`.";

/// This computer's Ensemble runner: it runs the tasks of your hub on its
/// harnesses and the models of its ANN pool.
#[derive(Debug, Parser)]
#[command(name = "ensemble-runner", version, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// The folder everything is kept in, the runner in its runner/; by
    /// default ~/.local/state/ensemble.
    #[arg(long, global = true, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
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
            "provider",
            "The providers of the runner's ANN pool, which its harnesses run on, and their keys.",
        ),
        (
            "model",
            "The models of the runner's ANN pool: which are on, what each is, and which it shares.",
        ),
        (
            "harness",
            "The runner's harnesses, the programs that drive an LLM to work in a repository: which are found, which are on, and how long a run may take.",
        ),
    ];
    mount(Cli::command(), runner, &groups)
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set this computer up: find the agent CLIs, write the runner if there is
    /// none, and say what it runs tasks on.
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
    /// The runner: its name, its workspace, its hub, whether it runs, and what
    /// it runs tasks on.
    Status,
    /// Say what is wrong with the runner's harnesses and the models they run
    /// on, and how to fix each thing.
    Doctor,
    /// What each harness has done on each model: runs, failures, time, tokens.
    Stats {
        /// Only runs this recent, such as 7d or 12h.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Your runners connected to your hub, with their harnesses and models.
    Peers,
    /// Print a completion script for a shell, such as `ensemble-runner
    /// completions zsh > ~/.zfunc/_ensemble-runner`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
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
