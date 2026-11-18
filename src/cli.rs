//! The command line: what each command takes, and the help it shows. Commands
//! are grouped by what they act on: this computer's `node` (its teams and
//! agents too), a `hub` this computer runs, and the `task`s and `knowledge` of
//! the folder you work in.

use clap::{Args, CommandFactory, Parser, Subcommand};
use ensemble::ops::Registry;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Files:
  ~/.config/ensemble/node/   node.yaml, teams/ (one folder per team), .env, hubs.json
  ~/.config/ensemble/hub/    hub.yaml, .env, database.db, vault/ (a hub this computer runs)
  ~/.local/state/ensemble/   run logs and worktrees, per working folder

API keys are never written in a config file: a backend names the variable that
holds its key, read from the environment, or from the node's or the hub's .env
(or --env-file).";

/// An agentic development system: a hub of services and nodes of computing
/// power, reached over MCP.
#[derive(Debug, Parser)]
#[command(name = "ensemble", version, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// A file of NAME=value lines read for variables the environment does not
    /// set, instead of the node's or the hub's .env.
    #[arg(long, global = true, value_name = "PATH")]
    pub env_file: Option<PathBuf>,
    /// The folder you work in: where agents work, and whose name is the
    /// project its tasks and knowledge are kept under. Default: the current
    /// directory.
    #[arg(short, long, global = true, value_name = "PATH")]
    pub dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

/// The whole command line: the commands below, with those made from the
/// hub's and the node's operations (`ops_cli`) under `hub` and `node`.
pub fn command<H, N>(hub: &Registry<H>, node: &Registry<N>) -> clap::Command {
    let hub_groups = [
        ("user", "The hub's accounts."),
        ("role", "The hub's roles."),
        (
            "config",
            "The hub's settings, hub.yaml, by their keys joined by dots, such as server.listen",
        ),
    ];
    let node_groups = [(
        "config",
        "The node's settings, node.yaml, by their keys joined by dots, such as hub",
    )];
    Cli::command()
        .mut_subcommand("hub", |c| {
            crate::cmd::config::extras(crate::ops_cli::mount(c, hub, &hub_groups))
        })
        .mut_subcommand("node", |c| {
            crate::cmd::config::extras(crate::ops_cli::mount(c, node, &node_groups))
        })
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// This computer's node: it runs your helper teams, and reaches its hub.
    #[command(subcommand)]
    Node(NodeCommand),
    /// A hub this computer runs: accounts, tasks, knowledge, and the link
    /// nodes connect to.
    #[command(subcommand)]
    Hub(HubCommand),
    /// The tasks of the folder you work in, kept on your node's hub.
    #[command(subcommand)]
    Task(TaskCommand),
    /// The knowledge of your node's hub, for the folder you work in.
    #[command(subcommand)]
    Knowledge(KnowledgeCommand),
    /// Print a completion script for a shell, such as `ensemble completions
    /// zsh > ~/.zfunc/_ensemble`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// A session of your node over MCP on stdio: what your agent CLI starts,
    /// in the folder you work in.
    #[command(hide = true)]
    Mcp,
    /// What a Claude Code or Codex session hook runs: the notes about the
    /// prompt not given in the session yet.
    #[command(hide = true)]
    Hook,
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Set this computer up: find the agent CLIs, write the node if there is
    /// none, connect the agent you talk to, and check the teams.
    Setup {
        /// The agent CLI you talk to; asked when several are installed.
        #[arg(long, value_parser = ensemble::setup::CLIENTS)]
        client: Option<String>,
        /// The node's name; asked, or this computer's name.
        #[arg(long)]
        name: Option<String>,
        /// Register Ensemble for every project of yours, not only this one.
        #[arg(long)]
        global: bool,
        /// Ask nothing: take what is given and found.
        #[arg(long)]
        yes: bool,
    },
    /// Write the node, ~/.config/ensemble/node/node.yaml: its name, whom it
    /// works for, its hub, and what it offers.
    Init {
        /// Overwrite one that exists.
        #[arg(long)]
        force: bool,
        /// Print the sample instead, with every setting.
        #[arg(long)]
        stdout: bool,
    },
    /// Run the node until Ctrl-C or SIGTERM: sessions from any folder reach
    /// its teams, and the tasks of its folders run. A computer runs one.
    Serve,
    /// Have the running node read its teams again, after one was changed by
    /// hand.
    Reload,
    /// The node: its name, its hub, whether it runs, and its teams.
    Status,
    /// Say what is wrong with the node's teams and the CLIs and models they
    /// need, and how to fix each thing.
    Doctor,
    /// Register Ensemble with the agent CLI you talk to, so its sessions reach
    /// the node.
    Connect {
        #[arg(value_parser = ensemble::setup::CLIENTS)]
        client: String,
        /// Register it for every project of yours, not only this one.
        #[arg(long)]
        global: bool,
        /// Also add session hooks (Claude Code, Codex) that put the hub's
        /// notes about each prompt into the conversation.
        #[arg(long)]
        hooks: bool,
    },
    /// The other nodes of your hub you may use, with their agents and models.
    Peers,
    /// The node's helper teams.
    #[command(subcommand)]
    Team(TeamCommand),
    /// The agents of the node's teams, named <team>/<agent>.
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
    /// Take a team out of the node; a linked folder or a pulled clone is left.
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
pub enum TaskCommand {
    /// The tasks of this folder's project, and where each stands.
    List {
        /// Only those whose status starts with this, such as queued or done.
        #[arg(long)]
        status: Option<String>,
    },
    /// One task: its ticket, where it stands, its question or its result.
    Show { id: String },
    /// Make a task. Without --agent it waits in the backlog.
    New(Box<NewTask>),
    /// Give a task to an agent: from the backlog, or to someone else.
    Assign { id: String, agent: String },
    /// Answer the question a task stopped to ask.
    Answer {
        id: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Cancel a task: at once when it has not started, by its node when it
    /// runs.
    Cancel { id: String },
    /// The branch, the change and the result of a task that worked in a git
    /// worktree.
    Review { id: String },
    /// The whole change on that branch.
    Diff { id: String },
    /// Merge that branch, once the task is done. Only you run this: your agent
    /// has no tool for it.
    Merge {
        id: String,
        /// One commit.
        #[arg(long)]
        squash: bool,
        /// Then remove the task's worktree and branch.
        #[arg(long)]
        cleanup: bool,
    },
}

#[derive(Debug, Args)]
pub struct NewTask {
    /// One line naming it.
    pub title: String,
    /// Who does it, <team>/<agent>.
    #[arg(long)]
    pub agent: Option<String>,
    /// The task; read from stdin when not given.
    #[arg(long)]
    pub text: Option<String>,
    /// The outcome wanted, in a sentence.
    #[arg(long)]
    pub goal: Option<String>,
    /// Background the agent needs.
    #[arg(long)]
    pub context: Option<String>,
    /// A folder of the working folder to work in.
    #[arg(long)]
    pub repo: Option<String>,
    /// A check from `gates` its work must pass: NAME, or NAME:ATTEMPTS.
    #[arg(long)]
    pub gate: Option<String>,
    /// Tasks that must be done first.
    #[arg(long, value_delimiter = ',', value_name = "ID,ID")]
    pub after: Vec<String>,
    /// Where the work is.
    #[arg(long, value_delimiter = ',', value_name = "FILE,FILE")]
    pub files: Vec<String>,
    /// What it must not do or change.
    #[arg(long, value_delimiter = ',', value_name = "TEXT,TEXT")]
    pub constraints: Vec<String>,
    /// How to tell it is done.
    #[arg(long, value_delimiter = ',', value_name = "TEXT,TEXT")]
    pub done_when: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum KnowledgeCommand {
    /// Keep knowledge in the hub's vault, as an agent's `share` tool does.
    Share {
        /// What it is about.
        #[arg(long)]
        title: String,
        /// The text; read from stdin when not given.
        #[arg(long)]
        text: Option<String>,
        /// Who may recall it: private, only you; hub, everyone on the hub.
        #[arg(long, default_value = "private", value_parser = ["private", "hub"])]
        visibility: String,
    },
    /// The hub's notes about a topic, as an agent's `recall` tool gets them.
    Recall {
        #[arg(required = true, num_args = 1..)]
        topic: Vec<String>,
    },
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
            &crate::cmd::node::operations(),
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
            .try_get_matches_from(["ensemble", "node", "login", "wss://hub", "--user", "ana"])
            .unwrap();
        let node = crate::cmd::node::operations();
        let (_, under) = matches.subcommand().unwrap();
        assert_eq!(crate::ops_cli::matched(&node, under).unwrap().0, "login");
    }

    #[test]
    fn a_task_takes_its_lists_comma_separated() {
        let cli = Cli::try_parse_from([
            "ensemble",
            "task",
            "new",
            "Add retry",
            "--agent",
            "web/coder",
            "--after",
            "0001-a,0002-b",
        ])
        .unwrap();
        let Command::Task(TaskCommand::New(t)) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(t.after, ["0001-a", "0002-b"]);
        assert_eq!(t.agent.as_deref(), Some("web/coder"));
    }
}
