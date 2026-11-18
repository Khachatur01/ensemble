//! The command line: what each command takes, and the help it shows. Commands
//! are grouped by what they act on: this computer's `runner` (its teams and
//! agents too), and a `hub` this computer runs.

use clap::{CommandFactory, Parser, Subcommand};
use ensemble::ops::Registry;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Files:
  ~/.config/ensemble/runner/ runner.yaml, teams/ (one folder per team), .env, hubs.json
  ~/.config/ensemble/hub/    hub.yaml, .env, database.db, vault/ (a hub this computer runs)
  ~/.local/state/ensemble/   run logs and worktrees, per repository

API keys are never written in a config file: a backend names the variable that
holds its key, read from the environment, or from the runner's or the hub's .env
(or --env-file). Client agents reach the hub over MCP, with an account's token.";

/// An agentic development system: a hub of services that client agents reach
/// over MCP, and runners of computing power.
#[derive(Debug, Parser)]
#[command(name = "ensemble", version, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// A file of NAME=value lines read for variables the environment does not
    /// set, instead of the runner's or the hub's .env.
    #[arg(long, global = true, value_name = "PATH")]
    pub env_file: Option<PathBuf>,
    /// The folder an agent works in for `runner agent run`, and where
    /// `runner agent health` and `runner doctor` start agent CLIs. Default:
    /// the current directory.
    #[arg(short, long, global = true, value_name = "PATH")]
    pub dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

/// The whole command line: the commands below, with those made from the
/// hub's and the runner's operations (`ops_cli`) under `hub` and `runner`.
pub fn command<H, R>(hub: &Registry<H>, runner: &Registry<R>) -> clap::Command {
    let hub_groups = [
        ("user", "The hub's accounts."),
        ("role", "The hub's roles."),
        (
            "config",
            "The hub's settings, hub.yaml, by their keys joined by dots, such as server.listen",
        ),
    ];
    let runner_groups = [(
        "config",
        "The runner's settings, runner.yaml, by their keys joined by dots, such as hub",
    )];
    Cli::command()
        .mut_subcommand("hub", |c| {
            crate::cmd::config::extras(crate::ops_cli::mount(c, hub, &hub_groups))
        })
        .mut_subcommand("runner", |c| {
            crate::cmd::config::extras(crate::ops_cli::mount(c, runner, &runner_groups))
        })
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// This computer's runner: it runs the tasks of your hub with your helper
    /// teams.
    #[command(subcommand)]
    Runner(RunnerCommand),
    /// A hub this computer runs: accounts, tasks, knowledge, MCP for client
    /// agents, and the link runners connect to.
    #[command(subcommand)]
    Hub(HubCommand),
    /// Print a completion script for a shell, such as `ensemble completions
    /// zsh > ~/.zfunc/_ensemble`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum RunnerCommand {
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
    /// Write the runner, ~/.config/ensemble/runner/runner.yaml: its name, whom
    /// it works for, its workspace, its hub, and what it offers.
    Init {
        /// Overwrite one that exists.
        #[arg(long)]
        force: bool,
        /// Print the sample instead, with every setting.
        #[arg(long)]
        stdout: bool,
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
    /// The other runners of your hub you may use, with their agents and models.
    Peers,
    /// The runner's helper teams.
    #[command(subcommand)]
    Team(TeamCommand),
    /// The agents of the runner's teams, named <team>/<agent>.
    #[command(subcommand)]
    Agent(AgentCommand),
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

#[derive(Debug, Subcommand)]
pub enum HubCommand {
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
    Knowledge(HubKnowledgeCommand),
}

/// One settings file, changed by its settings: each named by its keys joined
/// by dots, such as `server.listen`. Comments and order are kept, and a change
/// that would not load is refused. Settings declared in Nix are kept by the
/// Nix module, and refused here.
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
    /// Merge the settings of a YAML file into it and keep them as declared in
    /// Nix: what the Nix modules run.
    #[command(hide = true)]
    Merge { file: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum HubKnowledgeCommand {
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
        command(
            &ensemble::link::accounts::operations(),
            &crate::cmd::runner::operations(),
        )
    }

    #[test]
    fn the_command_line_is_well_formed() {
        whole().debug_assert();
    }

    #[test]
    fn operations_are_commands_with_their_fields_as_arguments() {
        let hub = ensemble::link::accounts::operations();
        let matches = whole()
            .try_get_matches_from(["ensemble", "hub", "user", "add", "ana", "--role", "member"])
            .unwrap();
        let (_, under) = matches.subcommand().unwrap();
        let (name, leaf) = crate::ops_cli::matched(&hub, under).unwrap();
        assert_eq!(name, "user.add");
        assert_eq!(leaf.get_one::<String>("name").unwrap(), "ana");
        assert_eq!(leaf.get_one::<String>("role").unwrap(), "member");
        // A secret is never an argument; an operation only for accounts is no
        // command.
        assert!(
            whole()
                .try_get_matches_from([
                    "ensemble",
                    "hub",
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
                .try_get_matches_from(["ensemble", "hub", "me", "show"])
                .is_err()
        );
        let matches = whole()
            .try_get_matches_from(["ensemble", "runner", "login", "wss://hub", "--user", "ana"])
            .unwrap();
        let runner = crate::cmd::runner::operations();
        let (_, under) = matches.subcommand().unwrap();
        assert_eq!(crate::ops_cli::matched(&runner, under).unwrap().0, "login");
    }
}
