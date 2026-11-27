//! The hub's command line: what each command takes, and the help it shows.

use clap::{CommandFactory, Parser, Subcommand};
use ensemble_ops::Registry;
use ensemble_ops::cli::mount;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Files, in --config-dir (by default ~/.local/state/ensemble):
  hub/core/        database.db (settings, accounts, sealed keys), master.key
  hub/knowledge/   vault/ (the notes, a git repository), embeddings.db
  hub/tasks/       database.db (the tasks it keeps)
  hub/trackers/    database.db (which issue of an integration became which task)

The hub's settings are kept in its core's database, and changed with its
commands, its REST API or a web UI; `settings show` shows them, `settings
history` who changed what. `provider add` and `provider key` keep a provider's
key sealed there by master.key: back up both, as without master.key no key can
be read. A key a secrets manager keeps is read from the environment. MCP
clients reach the hub over MCP, with an account's token.";

/// An Ensemble hub, run by this computer: accounts, the task tracker, the
/// knowledge base, MCP for MCP clients, and the link runners connect to.
#[derive(Debug, Parser)]
#[command(name = "ensemble-hub", version, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// The folder everything is kept in, the hub in its hub/; by default
    /// ~/.local/state/ensemble.
    #[arg(long, global = true, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

/// The whole command line: the commands below, with those made from the
/// hub's operations (`ensemble_ops::cli`).
pub fn command<H>(hub: &Registry<H>) -> clap::Command {
    let groups = [
        ("user", "The hub's accounts."),
        ("role", "The hub's roles."),
        ("runner", "The runners connected to the hub."),
        (
            "integration",
            "The task tracker's integrations with trackers whose issues become tasks.",
        ),
        (
            "picker",
            "The task picker: the decision models that choose who works on each task, and what they are asked.",
        ),
        ("registration", "How people join the hub."),
        (
            "provider",
            "The providers of the hub's ANN pool, the servers and APIs that serve its models, and their keys.",
        ),
        (
            "model",
            "The models of the hub's ANN pool: which are on, and what each is.",
        ),
        (
            "settings",
            "The hub's settings, kept in its database: every one, who changed them, and those of the hub itself.",
        ),
    ];
    mount(Cli::command(), hub, &groups)
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Make the hub, asking what it is called and who its first admin is;
    /// what is done already is kept.
    Setup {
        /// The hub's name; asked, or your user name.
        #[arg(long)]
        owner: Option<String>,
        /// Its first admin; asked, or your user name.
        #[arg(long)]
        admin: Option<String>,
        /// Ask nothing: take what is given, and your user name.
        #[arg(long)]
        yes: bool,
    },
    /// Make the settings a new hub starts with, and its vault: on this
    /// computer alone, closed to sign-ups, with no provider or model yet.
    Init {
        /// Make them anew in place of those there are.
        #[arg(long, conflicts_with = "keep")]
        force: bool,
        /// Keep the settings there are, and make them only when there are
        /// none: what the NixOS module runs before the hub starts.
        #[arg(long)]
        keep: bool,
        /// The hub's name; your user name when left out.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Load and validate the hub's settings, and say which model would
    /// distill now.
    Check,
    /// Run the hub until Ctrl-C or SIGTERM.
    Serve {
        /// The address every module over HTTP is served on: 0.0.0.0 for
        /// other computers too.
        #[arg(long, value_name = "ADDRESS", default_value = "127.0.0.1")]
        http_host: std::net::IpAddr,
        /// The port of the REST API, MCP and the runners' link.
        #[arg(long, value_name = "PORT", default_value_t = 7700)]
        http_port: u16,
    },
    /// The knowledge base: its vault, the models that distill it, and its
    /// intake filter.
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
