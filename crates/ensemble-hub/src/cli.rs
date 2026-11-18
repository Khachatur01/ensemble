//! The hub's command line: what each command takes, and the help it shows.

use clap::{CommandFactory, Parser, Subcommand};
use ensemble_ops::Registry;
use ensemble_ops::cli::{config_extras, mount};
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Files:
  ~/.config/ensemble/hub/  hub.yaml, .env, database.db, vault/

API keys are never written in a config file: a backend names the variable that
holds its key, read from the environment, or from the hub's .env (or
--env-file). Client agents reach the hub over MCP, with an account's token.";

/// An Ensemble hub, run by this computer: accounts, tasks, knowledge, MCP for
/// client agents, and the link runners connect to.
#[derive(Debug, Parser)]
#[command(name = "ensemble-hub", version, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// A file of NAME=value lines read for variables the environment does not
    /// set, instead of the hub's .env.
    #[arg(long, global = true, value_name = "PATH")]
    pub env_file: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

/// The whole command line: the commands below, with those made from the
/// hub's operations (`ensemble_ops::cli`).
pub fn command<H>(hub: &Registry<H>) -> clap::Command {
    let groups = [
        ("user", "The hub's accounts."),
        ("role", "The hub's roles."),
        (
            "config",
            "The hub's settings, hub.yaml, by their keys joined by dots, such as server.listen",
        ),
    ];
    config_extras(mount(Cli::command(), hub, &groups))
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a sample hub: hub.yaml with every setting, and its vault.
    Init {
        /// Overwrite one that exists.
        #[arg(long)]
        force: bool,
        /// Print the sample instead.
        #[arg(long)]
        stdout: bool,
    },
    /// Load and validate hub.yaml, and say which model would distill now.
    Check,
    /// Run the hub until Ctrl-C or SIGTERM.
    Serve,
    /// The hub's knowledge vault.
    #[command(subcommand)]
    Knowledge(KnowledgeCommand),
    /// Print a completion script for a shell, such as `ensemble-hub
    /// completions zsh > ~/.zfunc/_ensemble-hub`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum KnowledgeCommand {
    /// Shares, those not distilled yet, notes, and what waits in review/.
    Status,
    /// Distill every pending share now.
    Distill,
    /// Delete the notes and make them again from every share.
    Rebuild {
        #[arg(long)]
        yes: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole() -> clap::Command {
        command(&crate::accounts::operations())
    }

    #[test]
    fn the_command_line_is_well_formed() {
        whole().debug_assert();
    }

    #[test]
    fn operations_are_commands_with_their_fields_as_arguments() {
        let hub = crate::accounts::operations();
        let matches = whole()
            .try_get_matches_from(["ensemble-hub", "user", "add", "ana", "--role", "member"])
            .unwrap();
        let (name, leaf) = ensemble_ops::cli::matched(&hub, &matches).unwrap();
        assert_eq!(name, "user.add");
        assert_eq!(leaf.get_one::<String>("name").unwrap(), "ana");
        assert_eq!(leaf.get_one::<String>("role").unwrap(), "member");
        // A secret is never an argument; an operation only for accounts is no
        // command.
        assert!(
            whole()
                .try_get_matches_from([
                    "ensemble-hub",
                    "user",
                    "password",
                    "ana",
                    "--password",
                    "x"
                ])
                .is_err()
        );
        assert!(
            whole()
                .try_get_matches_from(["ensemble-hub", "me", "show"])
                .is_err()
        );
    }
}
