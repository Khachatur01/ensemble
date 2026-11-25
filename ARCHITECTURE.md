# Architecture

This document has two parts. **Part one, "How it works"**, describes the
system as it is built: the processes, the crates, where things are kept, how
work flows through them and how they run at once. **Part two, "Decisions and
direction"**, records what was decided with the owner and where Ensemble is
going; its build order says what is done and what is not. The questions at the
end are open; everything else is decided unless marked otherwise. Part two's
"Hub, adapters and runners" (decided 2026-11-19) is being built: the runner
is a pure worker, clients reach the hub alone, each is an executable of its
own, a runner signs in and joins over its link alone, it finds or clones the
project a task names in its workspace, asks when it cannot, and gives a task
back when its computer cannot reach the repository, and runners share
models through it. "The hub's core and its
connectors" (decided 2026-11-19) is built: the hub's settings are the core
(accounts, the port, its ANN pool) and the connectors, every one always
on. "Settings in the database" (decided 2026-11-20) is built: the hub keeps
its settings, and its providers' keys sealed by a master key, in its database,
as a GitLab instance does, and a NixOS module only runs it. "Models and
harnesses" (decided 2026-11-25) is being built: nobody makes a team, a runner
offers its ANN pool and its harnesses, and a picker chooses what works on
each task; part one still describes the teams until it is.

`README.md` is the user's guide: every setting, command and tool. This document
is the map behind it.

# Part one: how it works

## The pieces

Ensemble is an agentic development system, not an MCP server: a **hub** at
the center, which everything else reaches through its connectors, and
**runners**, one of those connectors, that provide computing power. It is two
executables, `ensemble-hub` and `ensemble-runner`, in these roles:

- **The hub**: its core is the accounts, their roles and what each may do, the
  port it listens on and its ANN pool: the providers of models it calls and
  the models they serve. Everything else is a
  **connector**, each always on: the REST API, MCP over HTTP for client
  agents, the link for runners, knowledge, tasks (and the trackers whose
  issues become tasks for the right person) and sources. A connector may hold
  **adapters** the admin picks, such as the trackers of `tasks`. The hub has
  no team and runs no agents. It is always its own process, `ensemble-hub
  serve`, on a server for a team or on one person's computer, and each runner
  names it as `hub:` in its settings.
- **The runner**: a computational resource and a pure worker. It holds the
  helper teams, runs their agents (agent CLIs, Ensemble's own loop) on the
  tasks the hub keeps for its owner, like a member of a team handed work: in
  the project's git repository in its workspace, cloned there when it has
  none. It keeps an ANN pool, the providers of models its teams run on and
  the models they serve, off until a person turns them on, and may share
  some of its models through the hub. It takes its owner's tasks alone. It serves no client. It is its own process too, `ensemble-runner
  serve`, one per computer, and its executable holds nothing of the hub's.
- **The helper teams**: each a `config.yaml` in the runner's teams directory,
  its agents named `<team>/<agent>`, each run by an adapter (an agent CLI
  started headless, an agent loop of Ensemble's own, or one chat request to a
  model).
- **The ways in**, all connectors of the hub: the agent CLI a person already
  uses, which reaches the hub over MCP with an account's token as the API key
  and installs nothing of Ensemble: it shares, recalls and makes tasks; the
  hub's trackers, whose issues become tasks; the REST API, for scripts and a
  web UI to come; and, to come, sources.

```text
  client agents (Claude Code, Codex, OpenCode, claude.ai)
        │  MCP over HTTP: /mcp on the hub's port, a token as the API key
        ▼
  ensemble-hub serve          accounts, tokens, knowledge, the tasks,
        ▲                     the tracker inbox, the connected runners
        │  the link: a WebSocket at /link each runner opens, signed in with a token
        │
  ensemble-runner serve       one per computer: a worker per git repository
        │                     of its workspace, which leases its tasks; the
        │                     repositories of tasks it has none of cloned
        ▼
  the helper teams (config.yaml)
        │  router: agent → adapter; model from its `try` list
        ▼
  claude -p · opencode run · codex exec        agent CLIs, as subprocesses
  builtin                                      harness-rs's loop, in-process
  direct                                       one chat request
        │
        ▼
  model servers (the providers of the runner's ANN pool), or the CLI's own login
```

What a runner and the hub keep open between them:

```text
  ensemble-runner serve                      ensemble-hub serve
  ─────────────────────                      ──────────────────
  RemoteBoard (a repository's tasks)   ══╗   accounts, sessions, tokens (database.db)
  the tasks of projects it has none of ══╣   the tasks, leased to runners (database.db)
  Worker (it is here, with its agents) ══╣   the knowledge vault and its distiller
  Relay (models shared and reached)    ══╝   the tracker inbox (database.db)
                                             the connected runners, the calls between them
                                             REST /api/v1/, MCP /mcp
     ══ one WebSocket each, opened by the runner, signed in with a bearer token
```

## Processes

Each process is one of the two executables. What runs in each:

| command | what it runs | ends |
| --- | --- | --- |
| `ensemble-runner serve` | the runner, one per computer: a worker per git repository directly inside its workspace, each running the tasks the hub keeps for the repository's project; every 30 seconds, the workspace looked at again and the hub asked for the tasks of its agents in projects it has no folder for, whose repositories it clones, asks about or gives back; a socket for the operations `ensemble-runner …` asks of it and for reloading its teams; and with the hub its settings name, a link that says it is connected, with its agents (`Worker`), and, when it shares models or reaches other runners', a relay socket for model calls (`Relay`); and once it starts, its providers asked which models they serve now | on SIGINT or SIGTERM; a change to its settings has it start again in the same process |
| `ensemble-hub serve` | the hub, from `hub/` in `--config-dir` (`~/.local/state/ensemble/hub/`), made the first time when there is none: one port (`--http-host`, `--http-port`) for the runners' link at `/link` and their relay sockets at `/relay`, the REST API under `/api/v1/` and MCP for client agents at `/mcp`; a socket for the operations `ensemble-hub …` asks of it; the tasks; the vault's distiller; the tracker inbox | on SIGINT or SIGTERM; a change to its settings it cannot use as it runs has it start again in the same process |
| everything else | one command: read the config and the state, or ask the running process over its socket, do one thing, print | at once |

A repository's tasks are run by one worker of the computer at a time: whoever
holds the lock file (`worker.lock`) in the repository's state directory;
across computers, a lease on the hub keeps one runner on a task. A runner that
finds the lock taken leaves that repository to the process that holds it. A
lock left by a dead process is taken over. The same lock keeps one runner per
computer, one distiller per vault and one inbox per hub.

## Crates

A Cargo workspace. Each crate depends only on crates above it in this table,
and the runner on none of the hub's alone: `ensemble-knowledge` and
`ensemble-hub`. Both keep their settings and sealed keys in `ensemble-store`.
`tests/workspace.rs` checks both.

| crate | what it is | files |
| --- | --- | --- |
| `ensemble-core` | what everything shares | the error vocabulary (`port.rs`); the per-user state, cache and config directories (`state.rs`); the working folder, a `repo` inside it and what a run changed (`workspace.rs`); the record of runs behind `ensemble-runner agent stats` (`records.rs`); the one-worker lock (`lock.rs`); dates in UTC (`time.rs`); the async runtime and its bridges to code that blocks (`runtime.rs`) |
| `ensemble-ops` | operations, defined once for every interface, and what both command lines share | the operation, its spec (name, summary, who may run it, input fields), errors as data, the caller, and the registry that checks both before it runs one (`lib.rs`); the commands made from the operations, each with `--json`, editing a team's file in $EDITOR, and asking at the terminal (`cli.rs`); an output as plain lines (`lines`, `lib.rs`); operations run by the running process over its socket, or here, and stopping or starting again on a signal or a change (`local.rs`) |
| `ensemble-config` | the settings: a team's file, the hub's and the runner's | `config.yaml`: the document as written (`raw.rs`), the environment (`env.rs`), validation (`validate.rs`), the resolved types (`lib.rs`), adding and removing an agent as text, comments kept (`edit.rs`); one setting of a team's file read or set as text (`settings.rs`), and its `config.*` operations (`settings_file.rs`); the teams directory and teams combined as `<team>/<agent>` (`teams.rs`); the hub's settings as kept, `HubSettings`, and resolved (`hub.rs`); the runner's, `RunnerSettings` (`runner.rs`), with its harnesses (`harness.rs`); their ANN pool, its providers and the models they serve, as kept and resolved, and a model named in a setting or a team checked to be one of it that is on and of the right kind (`ann.rs`); kept settings as operations change them, each change checked and kept with who made it (`kept.rs`) |
| `ensemble-backends` | talking to the providers of the ANN pool | one chat request in the server's own format (`chat.rs`); which models a provider serves, and what each is, as Ollama says or Ensemble knows (`models.rs`), from a catalog of known families and sizes (`catalog.rs`); the operations on the pool, `provider.*` and `model.*`, for the hub and the runner (`ops.rs`); the first usable model of a `try` list (`resolve.rs`); texts as vectors (`embed.rs`); a stand-in server for tests (`testing.rs`) |
| `ensemble-store` | Turso files | the hub's `database.db`, opened for each read or change, with the tables of `ensemble-hub`, and any other Turso file, such as the vault's cache of vectors (`lib.rs`); the settings of a hub or a runner as typed rows (a row of settings; the ANN pool's providers and models, roles, the models used and trackers), the audit log of their changes, and the keys sealed by `master.key`, the runner's in a `database.db` of its own with only its tables (`kept.rs`) |
| `ensemble-knowledge` | the hub's knowledge, the hub's alone | the vault, a git repository (`vault.rs`); the secret check (`secrets.rs`); shares into notes (`pipeline.rs`); BM25 search (`search.rs`); notes by meaning, with their vectors in `.ensemble/embeddings.db` (`meaning.rs`); `Memory`, the `share` and `recall` tools, the background distiller, and the `KnowledgeAccess` trait (`lib.rs`) |
| `ensemble-link` | what the runner and the hub share of the link | the frames of the link (`protocol.rs`); bytes as text, for a model's answer (`base64.rs`); the runner's side, `Connection` and `Link`, one reconnecting, which says who its token is, and signing in without a token, for one (`client.rs`); the token file (`tokens.rs`) |
| `ensemble-agents` | running one agent once | the adapters (`external/`): `claude_code.rs`, `opencode.rs`, `codex.rs`, `builtin.rs`, `direct.rs`, the subprocess runner (`process.rs`) and gates (`gate.rs`); a git worktree per task (`worktree.rs`); the router: agents to adapters, models, records, `health` (`router.rs`) |
| `ensemble-tasks` | the work of the helper teams, as a runner does it | tickets and tasks as the hub has them (`tasks.rs`); how they are leased and given back: the `Board` trait, a runner's `RemoteBoard`, over the link, for its account, and the tasks waiting for its agents in every project (`board.rs`); what a runner keeps of its runs (`runs.rs`); the worker that runs a repository's tasks within slots and budgets (`queue.rs`); one run of an agent and what it came to (`delegation.rs`) |
| `ensemble-runner` | the runner, and its executable | `main` and the global options (`lib.rs`); where it keeps its settings and keys (`kept.rs`); the command line, parsed with clap (`cli.rs`), and what each group of commands does (`cmd/`: `runner`, its `harness`es, `team`, `agent`, a team's `config`); the runner's workers, the repositories it clones for tasks, what it asks and gives back, its socket and reload (`daemon.rs`); `Worker`: the link that says the runner is connected, with its agents (`worker.rs`); `Relay`: its relay socket, serving others' calls to the models it shares and carrying its agents' calls to other runners' (`relay.rs`); what `setup` and `doctor` find and check (`setup.rs`); the runner's teams, loaded, made, linked, pulled and approved (`teams.rs`), over git repositories pinned and trusted per commit (`remote.rs`) |
| `ensemble-hub` | the hub, and its executable | `main` and the global options (`lib.rs`); the command line (`cli.rs`) and what its commands do (`cmd.rs`); MCP over HTTP for client agents (`mcp.rs`); the hub's operations, its accounts, tokens, join codes, sessions and settings (`accounts.rs`); its settings and its providers' keys in its database, sealed by `master.key` (`kept.rs`); accounts, and hashed tokens and join codes (`users.rs`), kept with the tasks and the inbox's deliveries in `database.db` (`ensemble-store`); the REST API, each route one of the operations (`api.rs`), the operations only the running hub answers, an account's tasks, runners and knowledge (`served.rs`), and its OpenAPI document (`openapi.rs`), over a small HTTP reader (`http.rs`); the hub's side of the link, a task per connection (`server.rs`); connected runners (`runners.rs`); runners' relay sockets, the models each shares, and the model calls between them and from the hub, who may make them and how many at once to each provider (`calls.rs`); the tasks it keeps and leases, offered to the account's runners, given back and blocked, with the notes recalled into each, `DbBoard`, and the `Service` that answers the link (`board.rs`), and a runner's worker run against them (`tests/queue.rs`); the tracker inbox (`inbox/`): the `IssueTracker` trait (`tracker.rs`) and markdown issues (`local_markdown.rs`), an issue made a task and what the task reports (`work.rs`), the hub's inbox, one for each tracker (`relay.rs`) |
| `ensemble` (the root) | the two executables, and the tests of the whole | `src/bin/ensemble-hub.rs` and `src/bin/ensemble-runner.rs`, each its crate's `main`; the end-to-end suite, which runs both (`tests/end_to_end.rs`); the checks against the real agent CLIs (`tests/real_cli.rs`); the layering of the crates (`tests/workspace.rs`) |
| `ensemble-ui-app` (`ui/app`, in the UI's own workspace) | the web UI's app, for WebAssembly | the pages and routes (`main.rs`), the REST API as the page calls it (`api.rs`), the form made from an operation (`form.rs`), the pages (`pages.rs`); `ui/build.sh` builds `ui/dist` |
| `ensemble-ui` (`ui/server`, in the UI's own workspace) | the web UI's executable, apart from the hub | the app's files, embedded or from `--dist`, and the calls to `/api/` passed on to the hub as from its own origin, only from a page of the UI (`main.rs`) |
| `nix/` | not a crate: the NixOS module (`nixos-module.nix`: the hub and the runner, each a user service of the person it is for, the hub's lingering so it runs from boot), the Home Manager module (`home-manager-module.nix`: the runner on Linux or macOS), what they share about a runner (`runner.nix`), and an example configuration the flake's checks build (`example.nix`). The flake packages each executable on its own (`ensemble-hub`, `ensemble-runner`) and both together. The hub is only run, on the address and port the module says: its settings are kept in its database, and made at its first start by `serve`. The runner is only run too: its settings, keys and teams are made and changed with `ensemble-runner`, as on any platform, and a runner named for the computer is made at its first start by `serve`. A secrets manager's file is the service's `EnvironmentFile` |

Two rules keep the layers apart. The worker knows the router, not which CLIs
exist. The router knows agents; only the adapters know how a CLI is started
and read.

## Configuration and state

What a person decides is **configuration**, never changed by Ensemble except
on request (`ensemble-runner agent add`, `ensemble-runner team new`, an
operation):

| file | where | holds |
| --- | --- | --- |
| `config.yaml` | a helper team's folder in the runner's teams directory (`runner/teams/<name>/`), a folder linked there, or a pulled repository's clone | `limits`, `tracker`, `agents` |
| the hub's settings | typed rows of its core's database, `hub/core/database.db`, on the computer that runs the hub | the core: `owner`, its `ann_pool` (its providers and their models), how people join and sign in, the `roles`; and its connectors' settings: the `distill` and `embed` models, `recall`, the `trackers` and the `source_filter` |
| the runner's settings | typed rows of its core's database, `runner/core/database.db`, one per computer | `name`, `workspace`, `hub`, and its `ann_pool`: the providers its teams run on, each with the most calls it takes at once, and their models, each on or off, and shared or not; and its `harnesses`, one of each kind (`claude-code`, `codex`, `opencode`, `builtin`), each on or off, found or missing, with its command and how long and how many steps a run may take |

No setting has a default: a new hub or runner starts with every one written
(`HubSettings::starting`, `RunnerSettings::starting`), and a missing one is
refused by name. The settings of a hub and a runner are changed only through
operations, from the command line, the REST API or a UI, each checked before
it is kept and kept in the audit log (`settings.history`); `settings.show`
shows them and `settings.import` takes them whole, as JSON. A team's
`config.yaml` is a file, changed by hand or setting by setting (`ensemble-runner
team config`; `settings.rs` changes one setting as text, keeping comments).
The few settings that may be absent mean "not used" when they are (an
embedding model, a source filter, a runner's `hub`). The command line is
always on and has no settings. A relative path is in the folder of its owner:
a tracker's in the hub's `trackers/`, the runner's workspace in `runner/`;
the runner's `workspace` may start with `~/`. No settings hold a secret: keys
are named by the variable that holds them. The hub and the runner keep the
keys they are given sealed in their core's database, by `master.key` beside it
(AES-256-GCM, the variable's name bound to each); a variable is read from the
environment first, where a secrets manager's keys are (a service's
`EnvironmentFile`), then from the keys kept. The runner and the hub keep
separate secrets because they are separate services, often of separate users.

Everything Ensemble keeps is in one folder, `--config-dir` (by default
`$XDG_STATE_HOME/ensemble`, `~/.local/state/ensemble`), never in a project:
a tree for the hub and one for the runner, each of its own, neither reading
the other's (`ensemble_core::state`'s `HubDir` and `RunnerDir`):

| path | holds |
| --- | --- |
| `hub/core/` | the hub's `database.db` (settings, audit log, the ANN pool and its providers' sealed keys, accounts and tokens) and the `master.key` that seals the keys, mode 0600; without it, no key there can be read |
| `hub/knowledge/` | the knowledge vault, `vault/`, a git repository of Markdown files, and `embeddings.db`, the notes' vectors, a cache |
| `hub/tasks/` | the `database.db` of the tasks the hub keeps |
| `hub/trackers/` | the `database.db` of which issue became which task; a local tracker's folder when it names a relative one |
| `hub/worker.lock` | the running hub's lock |
| `runner/core/` | the runner's `database.db` (settings, audit log, the ANN pool and its providers' sealed keys, the harnesses, hub tokens, sealed) and its `master.key` |
| `runner/teams/` | the runner's helper teams: a folder per team, its `config.yaml`, a link to a folder elsewhere, or `pulled.json` (repository and commit); `pending` beside a team that waits for approval |
| `runner/workdirs/<folder>-<hash>/` | a repository's `runs/`, `worktrees/` and `worker.lock` |
| `runner/orgs/teams-<hash>/` | the teams' `usage.json` (budgets) and `records.jsonl` (`ensemble-runner agent stats`), by `<team>/<agent>`, shared by every repository |
| `runner/worker.lock` | the runner's lock |
| `$XDG_RUNTIME_DIR/ensemble/<hash>.sock` | the runner's and the hub's sockets, in a folder only their user may enter (`/tmp/ensemble-<user>/` without `XDG_RUNTIME_DIR`) |
| `~/.cache/ensemble/` | clones of pulled teams, checked against their commit each time |

`<hash>` is a hash of the whole path, so two folders of one name are kept
apart. The knowledge vault is the one thing a hub keeps in its own directory:
it is meant to be read, in Obsidian or anything that reads Markdown.

## How it runs at once

Ensemble is async, on tokio (one shared multi-thread runtime,
`ensemble_core::runtime`).

- **A worker is one task.** `Queue::work` scans its repository's tasks on the
  hub every two seconds, starts what may start, and keeps the running tasks as
  futures in one `FuturesUnordered`, beside the look at the board, so a run
  renewing its claim and the look share one link. It wakes early on a
  `Notify` when a run ends or frees a slot.
- **A runner** spawns a task per repository's worker. Beside the requests on
  its socket, every 30 seconds, it looks at its workspace and asks the hub
  for the tasks of projects it has no folder for; a clone is a `git` process
  awaited there, so the socket is answered meanwhile. Its link that says it
  is connected is one task whose model calls go on beside it; they hand what
  to send to the link through a channel.
- **A hub server** spawns a task per connection, so one slow request holds up
  no other. Connections reach each other through outboxes: a channel per
  connection that others fill and its own task empties.
- **Code that blocks** (git, the vault's files, `health`, the tracker files)
  is called through `runtime::blocking`, which lets the runtime move other
  tasks off the thread. It still holds up the other futures of its own task
  while it runs. `runtime::block_on` is for the CLI's entry points and sync
  interfaces that wait on async work.
- **The hub's database** (`database.db`) is opened for each read or change and
  closed after, a change in one transaction, with a busy timeout: Turso
  locks the file while it is open, and this lets `ensemble-hub user` change
  accounts while the server runs.
- **Loops that are not async** (a vault's distiller, a tracker's inbox) run on
  threads of their own, polling every few seconds.
- **Stopping**: SIGINT and SIGTERM set one flag. Every loop watches it, the
  workers close, running tasks are stopped and put back on the hub, and the
  runner waits up to ten seconds for them.

What a runner keeps for a repository, and what a hub server's connections
borrow, is made once and lives as long as the process, so it is leaked on
purpose (`Box::leak`) and borrowed as `'static`.

## Teams

The runner runs every team in its teams directory at once. When it starts it
reads each one (`teams.rs` in `ensemble-runner`): a folder made here, a link to
a folder elsewhere, or a pulled repository, whose clone in the cache is checked
against its pinned commit and must be trusted. A team marked `pending` is
skipped, with a line saying it waits for approval; a team that cannot be loaded
stops the runner, naming it. The teams are then combined into one helper team
(`ensemble_config::teams::combine`): each name of a team, its agents and
limits, becomes `<team>/<name>`, and every reference inside it (an agent's
`limit`) is renamed with it; the models its agents name are the runner's, each
an LLM of its ANN pool that is on, or the team is not loaded. So the router,
the workers, budgets, records and tasks work as with one team, with agents named
`web/coder`.

- **Made**: `ensemble-runner team new` writes the sample; `ensemble-runner
  agent add` and `ensemble-runner agent remove` edit it as text, comments
  kept.
- **Linked**: `team link` makes a symbolic link to a folder with a
  `config.yaml`, such as a checkout a company keeps.
- **Pulled**: `team pull` resolves a tag or branch to a commit, clones it into
  the cache, shows what it would run, and records the trust and
  `pulled.json`; `team update` moves the pin the same way. A branch is never
  followed at run time.
- **Waiting for approval**: a team folder with a `pending` file beside its
  `config.yaml` runs nothing until `team approve` shows what it would run and
  removes the file.

The commands that read teams (`ensemble-runner team check`, `ensemble-runner
doctor`, the `ensemble-runner agent` commands) load them the same way. A running
runner reloads them when an `ensemble-runner team` command changes the
directory, or on `ensemble-runner reload` after a change by hand: it reads them
again, and if one cannot be loaded it keeps the teams it had and says why.
Otherwise each repository's `Ensemble` gets the new teams with `set_config` (a
run going on keeps the snapshot it started with), and the runner's link tells
the hub its agents again (`runner`).

## A run

A worker runs an agent on a task (`delegation::run_delegation`, in
`ensemble-tasks`), and so does `ensemble-runner agent run`:

1. The worker has taken a **slot** of the agent (`max_concurrent`, or its
   shared `limit`) and counted the run against its **budget**.
2. The **router** (`ensemble-agents`) picks the adapter, and for an agent on
   the ANN pool resolves the first model of its `try` list whose provider
   answers and serves it (`ensemble-backends`; what a provider serves is
   remembered for a minute).
3. The **adapter** runs it. A CLI adapter starts the CLI in the repository (or
   the task's worktree) in its own process group, with the agent's prompt,
   tools and model, and reads its JSON events line by line as they come; each
   step and tool call becomes a line of the task's activity log. `builtin`
   runs harness-rs's loop in process; `direct` sends one chat request.
4. At `max_seconds`, or when the task is cancelled or its worker closes, the
   whole process tree is killed and what was done is returned, marked
   unfinished.
5. The folder is compared before and after, for agents that can write, and the
   result lists the files changed, the commands run, steps, tool calls and
   tokens. A run that overlapped another in the same folder says so.
6. One line is appended to `records.jsonl`; the slot is given back.

A failed run is a result too, with why it failed and whether another attempt
could work, and it becomes the task's result.

## A task

A task is kept on the hub, in its database, for one account and one project
(a repository's name): its ticket, its assignee, its questions and answers,
what agents handed on, the runners that gave it back, its result, and its
**lease**. It comes from a client agent's `submit` over the hub's MCP, or
from the hub's inbox; either way the hub recalls the notes related to it (its
title and text as the query, `Memory::recall_as` for its account) and keeps
them on the ticket as its `notes`, which its maker cannot set. The ticket may
name the project's `repository`, a git URL or path to clone it from. The hub never
runs one; a runner leases it. The worker of each repository of a runner's
workspace, every two seconds and whenever a run ends, asks the hub for its
project's tasks (`RemoteBoard`, over one link that opens again when it
breaks), and:

1. skips tasks that ended, with no assignee (backlog), waiting for an answer,
   with `after` tasks not yet `done`, running on another runner, or given
   back by this one;
2. waits while the agent is over budget, its slots are busy, or (with
   `when_none: wait`) no model of its `try` list can be used;
3. **claims** the task for a minute (`task_claim`); another runner that was
   first wins, and the task is left;
4. for `isolation: worktree`, makes or reuses a git worktree on the branch
   `ensemble/<id>`, from where its work was last pushed when it has been;
5. runs the agent (see "A run"), asked the ticket's sections and then its
   notes, marked as reference data, then the task's **gate** if it has one
   (its command, run with `sh -c` in the task's working directory, passing
   on exit status 0 within its `timeout_seconds`), running the agent again
   with the gate's output until it passes or `attempts` are used, all the while
   renewing the claim every 15 seconds (`task_renew`), which also says when
   someone cancelled the task;
6. says how the run ended (`task_finish`): `NEEDS INPUT: <question>` keeps the
   question and the task waits for an answer; `HAND OFF: <agent> <what
   remains>` reassigns it with a note; otherwise its result (`done`, `failed`
   or `cancelled`), after committing everything on the task's branch in a
   worktree. Nothing is merged: the person merges the branch.

A runner that stops puts its running tasks back (`task_release`); one that
dies stops renewing, and the hub treats its claim as gone once it runs out.
Either way the hub marks the task interrupted on that runner, and the next
runner to look runs it again. A runner whose claim ran out, and whose task another
took, is refused when it says how its run ended, so a task never ends twice.
The hub's clock alone decides when a claim runs out.

Where the work is decides who goes on with it. A claim says whether the run
is **movable**: its agent works in a worktree, and the repository has a remote
`origin`. Whenever a movable run stops, the runner commits what it did on the
task's branch, pushes the branch, and tells the hub where it is (`progress`:
runner, branch, commit, with `task_release` or `task_finish`); the next run,
on any runner with the repository in its workspace, fetches it and makes or
moves its worktree there, and its agent is told to go on from it. A run that
is not movable changes one computer's repository, so the hub **pins** the task
to that runner from its first claim, and no other runner may claim it.

**Projects a runner has no folder for.** Every 30 seconds, as it looks at
its workspace, a runner asks the hub for the account's tasks, in every
project, that wait for one of its agents and that it has not given back
(`tasks_waiting`). For each project with no folder in its workspace, and no
task of it waiting for an answer:

- a task names a `repository`: the runner clones it into
  `<workspace>/<project>` with `git clone`, asking nobody (`GIT_TERMINAL_PROMPT=0`,
  no askpass, SSH in batch mode, at most ten minutes), and scans again: the
  new repository gets a worker, which takes the task as above. Its `origin`
  is the repository, so a worktree agent's branch is pushed there;
- none does: the runner claims the first task, for asking only, and ends it
  with a question (`End::Ask` with `repository`): "The runner desk has no copy
  of the project shop: which repository should it clone?". The answer, by
  `task_answer` from MCP or a tracker comment, is checked as a repository and
  kept as the ticket's `repository`, for every runner; such a question is not
  shown to the agent among the questions it asked;
- the clone fails (the computer cannot reach or read it, or the folder is
  there and no repository): the runner **gives back** each task naming that
  repository (`task_give_back`, with why). The hub keeps who gave it back and
  why, ends that runner's claim on it, and never lets it claim the task
  again; the other runners that may take it still may. A task given back is
  **blocked** when every runner connected now that may take it (the
  account's own with its agent) gave it back, as the hub works out from
  the runners connected whenever it lists the task (`tasks`, `task` show `blocked` and `given_back`). So it is
  offered again when a runner that may take it and has not given it back
  connects; assigning or answering it clears every give-back, for all
  runners to try again.

What a run does as it goes (its activity lines) is in `runs/<id>/log.txt` in
the repository's state directory on the runner that runs it. A task done is
shared to the hub's knowledge as its account's, for the distiller to learn
from.

## Knowledge

```text
  share ─► secret check ─► vault/shares/  ─► distiller (every 5 s) ─► vault/notes/
                            kept as it came    extract → integrate      one commit per share
                                               └► review/ when unsure
  recall ◄─ BM25 + embeddings, fused, over vault/notes/ ◄─────────────────┘
```

- **The vault** is a git repository: `shares/` as they arrived, never changed;
  `notes/`, one per topic, linked to related notes with `[[...]]`, which only
  the pipeline writes; `review/`, what the pipeline would not write;
  `log.jsonl`. Notes are a product of the shares, so
  `ensemble-hub knowledge rebuild` makes them again from scratch.
- **The pipeline** distills with the hub's own `knowledge.distill` models, by
  one chat request per step, with no tools. Shared text goes between markers
  with a nonce, an answer must be the JSON asked for, and anything else goes
  to `review/`.
- **Client agents reach knowledge over the hub's MCP**: `share` and `recall`,
  as the token's permissions allow, each run as the link's request of the
  same name (`Server::answer`). The MCP server's instructions tell the agent
  when to recall and to share when the person asks. `Memory`, the vault on
  disk (its file and git work through `runtime::blocking`), is what the hub
  uses, through the `KnowledgeAccess` trait.
- **Helper agents get no knowledge tools**: a runner has none. They get the
  notes the hub recalled into their task when it was made, as reference data.
  What a task reports when it is done is shared, under its account, from the
  hub.
- **On the hub** a share is kept under the signed-in account's name, and the
  running hub distills; `ensemble-hub knowledge distill` does it by hand on
  the hub's computer.

## The task inbox

Issues live in a tracker and are assigned to people; the inbox makes the ones
with the hub's label into tasks and writes what happens back on the issue. The
helper team never calls a tracker. A tracker is the `IssueTracker` trait; a
folder of markdown issues is the one implementation so far.

- **The hub** (`ServerInbox`, `relay.rs`) polls the tracker every
  `poll_seconds`. An open issue with the label, assigned to an account's
  tracker name and naming a project, becomes a task of that account in that
  project, kept like any other (for the agent its `<label>:<team>/<agent>`
  label names, or in the backlog), and which issue became which task is a
  **delivery** in the hub's database. Then each poll looks at the task: the
  runner that runs it, its question (posted, and the assignee's next comment
  is the answer), its result (posted once). An issue closed cancels its task.
  Runners know nothing of trackers: they run the task like any other.

## Connected runners

A runner with a hub server keeps a link (`Worker`, in `ensemble-runner`)
that says, as soon as it connects and again when its teams are reloaded,
what it is (`runner`): its name and every agent of its teams. So the hub
knows which runners are connected, with which agents: which may take an
account's tasks, and whether a task given back is blocked, depend on them,
and MCP's `agents` and `ensemble-runner peers` list an account's runners
(`runners`), each with every agent and the models it shares. Model calls go on a socket of their own (see [Sharing models](#sharing-models)).

The hub keeps such runners as presence only: a runner is a connection that
said `runner`, and nothing of it is kept once it is gone. A runner serves
its models to the hub and its owner's other runners alone.

## The link

The runner opens WebSockets, at `/link` on the hub's port, says the version of the link's protocol it
speaks as the `ensemble-protocol` header of the upgrade, and signs in with
`Authorization: Bearer <token>`. The hub refuses a link of another version,
or of none, before anything else, with 426 and words that name both versions
and which of the two to update ("this hub speaks link protocol 7, the runner
8: update ensemble-hub"), which the runner shows as they are; then a token
that is not valid, with 401. A link without a token is signed in to nothing,
and may only ask to be, for a token of its own. Frames are JSON text: requests
`{id, method, params}`, replies `{id, result}` or `{id, error}`, and events
`{event, data}` the hub sends unasked. A runner opens one link for the tasks of
each repository it works in, one for the tasks of
projects it has no folder for, one that says it is connected, one per project whose task it asks about or gives back, a short one
without a token to sign in, and a short one for `hello` when it signs in or
says its status; and, when it shares models or reaches other runners', a
relay socket at `/relay` for model calls, whose frames `ensemble_link::relay`
describes. This is version 10 of the protocol. It makes no HTTP request
to the hub.

| for | the runner asks | the hub sends |
| --- | --- | --- |
| a token, on a link without one | `login` (a name, a password and the client's name, `runner <name>`) or `join` (a join code and the client's name), each answered with a new access token of the client's own, named after it | |
| who it is | `hello` (who am I, which permissions) | |
| tasks | each with its `project`: `tasks`, `task`, `task_claim`, `task_renew`, `task_release`, `task_finish`, `task_give_back` (with why), and `task_gate` (a task's gate, or none, while it neither runs nor has ended); and across projects, `tasks_waiting` (the account's tasks for these agents that this runner has not given back) | |
| runners | `runner` (I am a runner: name, every agent), `runners` (mine, each with the models it shares) | |
| model calls, on the relay socket | the models it shares, by provider, and how many calls each provider takes at once, then calls to other runners' models: `open` (the runner and the provider), the request's body in chunks, `end`, or `cancel` | the answers to its calls, and calls to the models it shares, which it answers: `head`, the body in chunks, `done`, or `error` |

`login` and `join` are the hub's operations `token.login` and `token.join`,
run for anyone and for any client, a runner being one: `login` checks the
password as logging in anywhere does, failures locking the name; `join`
spends a join code, which an admin makes for an account with `user.join-code`
(`ensemble-hub user join-code NAME`, or `POST /api/v1/users/{name}/join-codes`)
or the account for itself with a session, `me.join-code`
(`POST /api/v1/me/join-codes`). A code is shown once, kept as a hash with the
tokens, signs in as no one, and is good once within 24 hours; a client
without the link spends it with `POST /api/v1/join`. The hub's MCP runs its tools as the same requests (`share`, `recall`, and
`task_create`, `task_answer`, `task_assign`, `task_gate`, `task_cancel` with
the reading ones, and `runners` for `agents`), so the link and MCP cannot differ. Every request is checked against the
token's permissions (its role, narrowed by its scopes), read again from the
hub's state, so a revoked token stops at once. The permissions are `share`,
`recall`, `tasks` and `admin`. The same port serves the
REST API under `/api/v1/` (accounts, sessions, access tokens, admin) and
`/mcp`; a WebSocket upgrade is the link at `/link` or a runner's relay socket
at `/relay`, and refused anywhere else (after the version check, so an older
runner is still told to update), and anything else is HTTP. The server speaks plain HTTP; for the internet a
TLS reverse proxy goes in front.

## Errors

One error type, `port::Error`, for every adapter and probe: an agent CLI that
would not start and a model server that answered 503 are the same event to the
task that ran it. Each error says where it came from, whether a retry could
work, and is written to be read by the person or the model that will see it.

## Safety boundaries

- **No sandbox of Ensemble's own.** Agents use their CLIs' own tools with your
  permissions; `tools` and `permission_mode` (or Codex's sandbox) limit them.
  `builtin`'s file tools cannot leave the working folder; its `shell` can.
  `ensemble-runner team check` and `ensemble-runner doctor` warn about agents
  that can change files in place.
- **A task's gate is its account's command.** A task may bring the command
  its work must pass, run with `sh -c` in its working directory, as the
  agent's own shell could; only the task's account gives it one (MCP's
  `submit` and `gate`, `task.create` and `task.gate`), never the tracker
  inbox, whose issues anyone who may write in the tracker writes, and a
  task runs only on its account's runners. A task's `repository` is cloned with `--`
  before it, so it cannot be an option of git's, and with no prompt for a
  password. A remote helper team is pinned to a commit and trusted per commit.
- **Secrets** are never in a file Ensemble reads as config, and are passed to a
  CLI only as the provider key of the provider it runs on.
- **Subscriptions stay with their owner.** Claude Code runs only through its
  official CLI on its owner's login, on its owner's runner.
- **Shared text is data.** The distilling model has no tools; recalled notes
  are marked as reference data, in a recall and in a task.
- **The hub keeps hashes.** Passwords as PBKDF2, tokens and join codes as
  SHA-256. A runner's
  token file and socket folder are its user's alone.

# Part two: decisions and direction

## Names

- **Helper team**: the agents a user works with, described in one `config.yaml`.
  It is flat: a list of professional agents, such as a
  coder, a reviewer, a PCB designer or an RF engineer, with no hierarchy. A
  runner holds any number of them in its teams directory and runs them all, its
  agents named `<team>/<agent>`: teams the person makes, links or pulls from a
  git repository like an image, and teams an agent writes at the person's
  request, which run once the person approves them. (Decided 2026-11-17.)
  The description is strict YAML, never natural language.
- **Hub**: the center of the system. Its **core** is the accounts, their roles
  and what each may do, the port it listens on and its ANN pool;
  everything else reaches it through a connector. The word "organization" is
  not used for it, to keep it apart from a helper team.
- **Runner**: a pure worker, `ensemble-runner`: a machine's capacity to do
  agent work, its helper teams, their agent CLIs and logins, and its models. It
  works on the hub's tasks and serves no client. A computer runs one runner,
  tied to no project. (Decided 2026-11-19.)
- **Connector**: a part of the hub beside its core: the REST API, MCP, the
  link for runners, knowledge, tasks and sources. Every connector is always
  on, and each is written in the hub's `connectors`; none can be turned
  off. The runners' link is one connector among the others, not a special
  one. The hub's command line is always on too, and has no settings.
  (Decided 2026-11-19.)
- **Adapter**: a pluggable part of a connector that the admin picks: the
  trackers of `tasks` (markdown issues; Jira and others later), the
  providers of `sources` (Microsoft 365, Telegram). A connector's adapters
  are a map by name, which may be empty. An agent's adapter in a helper team
  (what runs it) is the same idea on the runner's side. (Decided 2026-11-19.)
- **Source**: knowledge brought in from outside, such as a mailbox or a chat,
  through an adapter of the `sources` connector. (Decided 2026-11-19.)
- **Interface**: what a person talks to the system through: their own agent
  (Claude Code, Codex, OpenCode, claude.ai) with the hub's MCP tools, the hub's
  tracker, or later a web UI over the REST API. There is no personal agent and
  no manager. (Decided 2026-11-16; see "No personal agent".)

## What it is

An agentic development system: a hub of services and runners of computing
power, with work spread across them; MCP is one way in. (Decided 2026-11-17:
Ensemble is described as this, not as an MCP server.) It serves two kinds of
user: a single developer who wants agents to work on
tasks and save tokens by sending cheap work to cheap models, and an organization
whose people and agents share knowledge. The same two executables serve both.
The hub's connectors are always on, whatever the level; what changes is which
adapters the admin picks, and a map of adapters may be empty. A block that
may be left out means "not used", which is the one kind of optional setting
the config rules allow.

| Level | What runs | Who it is for |
|---|---|---|
| 1. A hub and a runner of one's own | Both on the person's computer: tasks, the models and harnesses it has, a knowledge vault fed by shares and task reports | A developer on their own |
| 2. A shared hub and many runners | People, always-on work, a tracker shared by a team, shared models, sources | Teams and organizations |

A runner always needs a hub, and knowledge always belongs to one: one person
on their own runs both on their computer. (Decided 2026-11-19.)

## Hub, connectors and runners

(Decided 2026-11-19; being built: see the build order. Part one describes
what is built, and is rewritten as this is built.)

```text
 runner ─┐                         knowledge (the distiller's models)
 runner ─┴── link ──┐                  │
  (WebSocket)       │                  │                 ┌─ Microsoft 365: mail, Teams
                    └───────── hub ────┼── sources ──────┤
                              │    │   │   (one LLM)     └─ Telegram
                    MCP ──────┘    │   └── task tracker ── markdown issues; Jira, … later
                    REST API ──────┘
                       │
                 client agents, apps, a web UI later
```

### The hub's core and its connectors

The hub's core is its accounts, their roles and what each may do; the one
port every connector over HTTP is served on (`listen`); and its ANN pool,
the providers of models it calls. Everything else is a **connector**, always on, some
holding **adapters** the admin picks:

- **`rest`**, the REST API at `/api/v1/`, for apps, and a web UI later,
  offering what MCP offers, as both are made from the same operations;
- **`mcp`**, MCP over HTTP at `/mcp`, for client agents;
- **`runners`**, the link at `/link` and the relay sockets at `/relay`,
  WebSockets, for runners;
- **`knowledge`**, the vault and the models that distill it;
- **`tasks`**, the tasks the hub keeps, with its `trackers` (adapters:
  markdown issues; Jira and others later);
- **`sources`**, knowledge from outside, with its `adapters` (Microsoft 365,
  Telegram; none built yet).

`rest`, `mcp` and `runners` have no settings. The CLI, over the hub's local
socket, for administration, is always on and has none either.

There are two executables, each its own crate and Nix package, from one
repository: **`ensemble-hub`** and **`ensemble-runner`**. The library crates
stay shared, and `tests/workspace.rs` checks that the runner's crate never
depends on the hub's (knowledge, the store, sources). The link's messages are
in one crate both build against. A runner says its protocol version when it
connects, and a hub that cannot work with it refuses it with what to update,
rather than fail halfway through a task.

### Clients

A client agent connects to the hub's MCP alone, with the hub's URL and an API
key, its account's token: the person asks their agent to add the server, or
adds it themselves (`claude mcp add --transport http …`). Nothing of Ensemble
is installed on the client's computer. The tools are `share`, `recall` and the
tasks. A key comes from an admin (`user.token`) or from the person
(`me.token`), and the answer gives the URL and a line ready to paste, so no
one puts it together by hand.

There are no session hooks: the MCP server's instructions tell the agent to
`recall` before work where earlier decisions may matter, and to `share` when
the person asks, as the knowledge rules say. That is weaker than a hook, as
the agent decides when to follow them, but every MCP client has it. (Built:
the `mcp` connector, always on, `/mcp` on the hub's port, one `POST` per
message and no event stream, in `ensemble-hub`'s `mcp.rs`.)

No client connects to a runner, and there is no delegation: runners work
only on tasks, which a client makes on the hub.

### Runners

A **runner** (`ensemble-runner`, its settings in
`runner/core/database.db`) is a machine's capacity to do agent
work: its harnesses, with their logins, and its ANN pool. It is a pure
worker:

- It offers no tools to clients and the harnesses it runs have no knowledge
  tools.
  The hub puts the notes related to a task, as the task's account may see
  them, into the task.
- It needs a hub; one person on their own runs both on their computer.
- It signs in and joins over the link (a name and a password, or a join code),
  and pulls the tasks it may work on.
- It does not say which projects it has. It works in its own workspace, a
  folder its settings name, where it finds the repository a task names or
  clones it, and works on a branch it pushes.
- It asks on the task when it cannot find the project. When its computer
  cannot reach the repository, it gives the task back and the hub offers it
  to another runner; when none can, the task is **blocked**, and offered again
  when a runner that has not given it back connects, or when it is assigned
  or answered again. (Built: "none can" is every runner of the account with
  the task's agent connected at the time; the answer to its question becomes
  the task's `repository`.)
- A task's report becomes a share of the task's account on the hub, and the
  distiller decides what of it becomes notes.

The hub offers a task to the account's runners, and to no other account's.
A run on another computer is a task too: the `node` adapter
and its snapshots are gone. (Built with step 19.)

### Sharing models

A hub may run on a small machine and need a strong model, and so may a
runner. A runner may share models of its own providers; the hub relays calls
to them, so the hub and the owner's other runners use them. Runners never
reach each other.

The runner's owner shares a model; the hub's admin adds that runner as a
provider of `type: runner`, turns the model on, and names it where it is used
(the distiller, the embedding model, the source filter), as any model:

```console
$ ensemble-runner model share local:qwen3.5:72b                # on ana's workstation
$ ensemble-hub provider add gpu-box --type runner --runner ana/workstation
$ ensemble-hub model on gpu-box:local:qwen3.5:72b
$ ensemble-hub knowledge models gpu-box:local:qwen3.5:72b
```

- A provider's name is unique in a pool, and a model of another runner is
  named as that runner names it, `<its provider>:<id>`, so a model is named
  `<provider>:<id>` everywhere: `gpu-box:local:qwen3.5:72b`.
- Choosing a runner's model is the admin's trust decision: whoever runs the
  serving machine sees what is sent to it. `runner.models` (`ensemble-hub
  runner models`) lists what the connected runners share, each model with
  what it is, as the runner's settings say, so an admin picks from a list.
- The hub never learns the address or key of a runner's provider; the runner
  calls its own.
- Only models of a runner's own OpenAI-compatible providers are shared, never
  an agent CLI, and a shared model is asked only for chats, completions and
  embeddings; the runner refuses a call to a model of it that is not shared.
  Which models a runner shares the hub answers itself. The hub may use any;
  a runner's owner's other runners may.
- When the serving runner is offline or busy, the `try` list's next entry is
  used. Each provider says how many calls it takes at once (`max_concurrent`),
  the runner's own and others' together; the hub keeps a call to a shared
  one waiting for a free place for up to 10 seconds, then answers that it is
  busy.
- Model calls go on a second WebSocket of the runner's (`/relay`), with the
  same sign-in, so a long prompt or a busy stream never holds up tasks and
  heartbeats on the link (`ensemble_link::relay`). Prompts go in chunks,
  answers stream back. Each call is numbered by whoever starts it, and the hub
  renumbers it between the two sockets. Measured before it was built, a
  streamed call through three hops (a loopback proxy, the hub, the serving
  runner) took about 2 ms longer to its first token and 3 ms longer in all
  than one made directly, for a short prompt and a 100 KB one alike.
- The hub and a runner that reaches other runners serve them on loopback,
  `http://127.0.0.1:<port>/<provider>`, with a key made when the process starts
  (`ensemble_link::loopback`), so the distiller and the agents call them like
  any OpenAI-compatible server; a request there names the model as the
  serving runner does, and goes to that provider of it. A command that runs
  outside `ensemble-runner serve` has no such endpoint: a team's model reached
  through the hub is named there, and a run tries its next model.

### Sources

A source brings knowledge in from outside. It never replies: it is not chat.
An admin connects one for the whole organization, with a connector per
provider: **Microsoft 365** (Outlook mail and Teams, through one app
registration on Microsoft Graph) and **Telegram** (a bot added to the
organization's groups) first, others later. The admin chooses what each reads:
mailboxes, folders, chats, channels. Credentials are kept sealed in the
hub's database, like every key it is given.

```text
 messages ─► collect ─► filter ─► clean shares ─► the distiller ─► notes (hub)
             the LLM keeps   confidential information removed,
             what is worth   by the admin's prompt (one for all
             knowing         sources, and one a source may add)
```

- One LLM serves every source, chosen from the hub's ANN pool like the
  distiller's.
- Raw messages are dropped once processed: only the cleaned knowledge is kept.
- What passes the filter is `hub`, for every account that may recall.
- What a person or a task report shares was shared on purpose: it goes to the
  distiller as today, without the filter.

### Task tracker

Issues come from a tracker with backends behind one interface: the markdown
issues of today, Jira and others later.

### Where the hub runs

The hub is a service, not a computational resource: it has no harnesses
and runs no agents. (Decided 2026-11-17.) It is always `ensemble-hub`, with
its files in `hub/` of `--config-dir`; one person runs it on their own
computer, a team on a server, and a developer who wants it from elsewhere
moves the folder to a server. A runner names one hub. A person may belong to
several hubs, a home hub with their own knowledge and one workplace hub per
organization, each with its own identity, token and policy, and nothing passes
between them except what the export rules below allow. (Decided 2026-11-10.)

A hub may face the public internet behind TLS, or run on a local network or
behind a VPN; nothing in Ensemble assumes either. (Decided 2026-11-10.)

### Rules that hold everywhere

- An agent runs on a runner of its owner's; never on the hub. Runners share only what they offer
  through the hub (tasks they work on, models they share); none shares
  tools, files or subscriptions, and runners never reach each other.
- An organization may run a central LLM server on its own GPUs. It is an
  OpenAI-compatible provider like any other, in the pools of the hub and the runners
  that use it; through the hub it will also get quotas per user and a usage
  log.
- Delegated scope: an agent acts with its owner's access or less, never more.
- Static config, dynamic state. Agent definitions, policy and the ANN pool
  live in reviewed config; a task brings its own gate. Membership, presence and the model bound
  to an agent for a run live in state.
- Secrets are never in any repository.
- Subscriptions are used only by the real CLI on its owner's machine. A Claude
  Code Pro or Max login is never routed through a proxy or used for someone
  else's work: Anthropic's terms limit subscription OAuth to Claude Code and
  Claude.ai, and to individual use. API keys go through the hub.

## No personal agent

There is no agent that stands for a person. (Decided 2026-11-16; this replaces
the personal agent of 2026-11-10, which was never built.) Each of its jobs has
a home that exists anyway:

| Job | Where it is |
|---|---|
| Knowing the person: decisions, setup, preferences | The vault. Recall serves the person's agent, and the hub puts related notes into every task. |
| Identity, tokens, being accountable | The person's hub account. Issues are assigned to the person. |
| Giving the runners work | The person's own agent makes tasks through the hub's MCP; later a web UI. |
| Turning an issue into work | The inbox makes the issue a task of the person it is assigned to. |
| Planning and splitting work | The person's own agent, in conversation, which makes the tasks. |
| Being reachable while the person's own runner is off | The task waits for one of the person's runners. |

So the system has three parts: the **hub** (people, knowledge, tasks, and
which runner takes what), **runners** (workers), and the **models and
harnesses** each runner offers. The person's agent is the interface to all
three, not one of them.

## Models and harnesses

Nobody makes a team (decided 2026-11-24): a person who is not technical
would not write one, and one who is should not have to. A runner offers what
its computer has, in two lists, and a task names no agent: Ensemble picks
what works on it. The names say what a thing is: an ANN (artificial neural
network), an LLM, an embedding model, a decision model, a harness. Ensemble
never says "AI".

**The ANN pool** (`ann_pool`) holds the neural networks a runner may call, in
two layers:

- A **provider** is where models come from: a model server (an
  OpenAI-compatible one, Ollama, llama.cpp's server), a vendor's API with its
  key, a decision model's service or file, or the models another runner of
  the same account shares through the hub. A person adds it, or setup finds
  it. It holds what all its models share: the address, the key, how long a
  call may take and how many calls it takes at once.
- A **model** is what a provider serves, found by asking the provider: its
  list of models, what Ollama says each can do, and one test call where the
  provider says nothing. A model is of one type:
  - an **LLM**: messages in, text out, with tool calls when it can make them;
  - an **embedding model**: text in, a vector out;
  - a **decision model**: a state and typed questions in, typed answers with
    their probabilities out, such as Jev (a hosted service) or Laya (open
    weights).

  Each model has a profile (an LLM its tier, small, medium or strong, its
  context window and whether it calls tools; an embedding model its
  dimensions), limits of its own, and a switch that shares it through the hub.
  A model a provider newly lists is off until a person turns it on. Its tier
  comes from a catalog of known models Ensemble ships, or is medium for one
  the catalog does not know, and the person who turns it on may change it.

**Harnesses** are the programs that drive an LLM to work in a repository:
Claude Code, Codex, OpenCode, and `builtin`, which is always there. A harness
is not an ANN, so it is a list of its own. Setup finds the CLIs and whether
each is logged in. OpenCode and `builtin` drive any LLM of the pool; Claude
Code and Codex drive their own vendor's models, which belong to the harness
and are not in the pool. Each harness has a time limit for one run and a most
number of steps. A harness is never shared: a task runs on its account's
runners, and a person who wants it worked on elsewhere reassigns it to another
person.

**A run** works in the task's own git worktree, on the branch `ensemble/<id>`,
where the harness may edit files and run commands. Ensemble never merges it.

**The picker** chooses the harness and the LLM a task runs on. It is a trait,
so a decision model can take the rules' place. Until then it follows rules:
among the pairs it may use now (turned on, with a free place, within their
limits), the strongest tier first, and the least busy among equals. When the
task's gate fails, the next try goes one tier up. Every run records what was
picked.

**Gates** are the task's: the command its work must pass, how long it may run
and how many tries it gets. Only the task's account sets one, through MCP, the
REST API or the web UI. The tracker inbox never does, as anyone who may write
in the tracker writes its issues. A task reassigned to another person runs its
gate only once that person confirms it, since someone else wrote the command.

The hub has an ANN pool too, for its own work: the distiller, the embedding
model and the source filter, on its own providers and on the models runners
share with it. A model alone is called, with no harness, only there: until
tasks say what they need (an open question), every task goes to a harness.

The agent loop of Ensemble's own is the `builtin` harness: harness-rs's
`AgentLoop`, its file and shell tools, and its model clients, on the pool's
LLMs, so a runner needs no agent CLI installed. It is pinned to one exact
version because the library is young. (Decided 2026-11-16.) The chat client
that sends one request straight to an LLM belongs to `ensemble-backends`, so
the knowledge pipeline and the hub's chat API share it.

Ensemble is async, on tokio: agent runs (CLI processes included), the task
queue, the MCP server, the runner, the link on both ends, and the knowledge a
session asks for. harness-rs is async, and so is a runner that runs many agents
and serves models at once. (Decided 2026-11-16; Ensemble was synchronous on
purpose until then.) What stays synchronous is the knowledge vault and its
distilling, and the tracker inboxes: their loops run on threads of their own.
Short HTTP requests use ureq; what streams for long (a runner's model calls)
uses reqwest. The trade-off of running a session's calls in one task is in
part one, under "How it runs at once".

An organization may later limit runs through policy: which models its data
may be sent to, and budgets per person. For now there are no such limits.
(Decided 2026-11-10.)

## Tasks

Two kinds of work (decided 2026-11-17):

- An **issue** is a person's: high-level, such as "a panorama video app with
  rotating cameras", in a tracker (Jira, GitLab, markdown files), assigned to
  the person, who stays accountable. Ensemble is never the source of truth
  for issues.
- A **task** is for a harness: one goal, one check, such as "the camera
  calibration module in `calib/`; `cargo test` passes". Tasks are kept on the
  hub, per account and project, so that a task outlives the runner that works
  on it: runners **lease** them (see "A task" in part one), and when a runner
  stops or dies another runner of the account takes the task up. Runners work
  only on tasks; there is no delegation outside them. (Decided 2026-11-19.) A
  task names no agent: the picker chooses what works on it (see "Models and
  harnesses"). It is reassigned only to another person, whose runners then
  take it. (Decided 2026-11-25.)

A harness never calls a tracker itself. Each tracker implements one
trait, `IssueTracker`: the issues assigned to this user that carry the
signal, and statuses and comments written back. The hub's inbox makes such an
issue a task of the account it is assigned to and reports the task back on
the issue. Behind it, tracker adapters do the tracker work: webhooks where the
tracker has them, polling where it does not, account mapping, the signal,
deduplication. The inbox runs on the hub, with one set of credentials, a
reachable address for webhooks, and on while runners are off. (Built: polling a
folder of markdown issues.)

An issue carries an explicit signal that agents may take it: a label, such as
`ensemble`, which every tracker has and which needs no extra account.
(Decided 2026-11-10.) The flow:

1. The inbox sees an issue assigned to me with the signal and makes it a task
   of mine, or puts it in my backlog.
2. A runner of mine takes it, with the vault's notes about it, which the hub
   puts into the task, and the picker chooses what works on it.
3. If it is unclear, the agent asks; the question goes to a tracker comment and
   my answer comes back the same way.
4. It works, checks the task's gate, and may make further tasks.
5. The result is reported in the tracker; I remain the assignee.
6. What the agent reported goes to the knowledge pipeline, as a share of the
   account the task is for.

Planning is a conversation, not an object (decided 2026-11-18). The person
talks the work through with their own agent, with an issue of theirs or
without one, and the agent then makes tasks for the runners, or none, as
the work needs. Ensemble keeps no plan and offers no planning tool: a task's
`after` is all the order it needs.

Ensemble never merges on its own: a merge happens only when the user asks.

## Knowledge

### What is stored

A vault is a Git repository of Markdown notes: text knowledge only, already
analyzed and broken down into one note per concept, linked with `[[...]]`, with
frontmatter naming the source person, the initiator, the connector, the
pipeline version and the visibility. Provenance is a link to the source (a
message URL, a task id), not a copy.

Each share is also kept as it arrived, in the same scope's repository, and the
notes are a product of the shares. A change to the note format rebuilds the
notes from the shares instead of migrating them, and a wrong note traces back to
the text it came from, so the first format may be simple: Markdown with YAML
frontmatter (id, kind, title, scope, sources, who shared, pipeline version), one
topic per note, linked with `[[...]]`. Git history is the audit trail, and a
search index is built from the notes. (Decided 2026-11-10. Built: the notes,
the shares and the frontmatter; the search reads the notes each time, with no
index, because a vault is small.) A recording transcript is kept briefly; audio
is never kept.

Scopes are separate repositories, because Git cannot hide single files. A hub
server keeps one organization vault, and every member's share goes there;
personal knowledge lives in the person's own hub, which is a different hub.
(Decided 2026-11-11.)

### What feeds it

- **Conversations in a user's own CLI are shared only when the user asks.** The
  user tells their agent, in plain words, to share the conversation; the agent
  decides what part matters and calls Ensemble's `share` MCP tool. Nothing reads
  CLI transcripts, and nothing proxies CLI traffic, so private or throwaway
  conversations stay private and no subscription rule is touched. (Decided
  2026-11-10; the session harvester was dropped.)
- Sources the organization owns are captured automatically: task outcomes,
  connectors to team chats, mail and CRM, and conversations with clients of the
  hub's own chat API. (Built: task outcomes. When a task ends done, the hub
  shares what was asked, the questions answered on the way and the agent's
  report, as the task's account, with the client `task`.)
- Recordings, started by a person, transcribed on their computer. (Not built.)
- Sources an admin connects (mail, Teams, Telegram), through their own
  collect and filter steps before the distiller; see "Sources" under "Hub,
  adapters and runners". (Decided 2026-11-19; not built.)

### The pipeline

```
ingest queue → filter → extract → integrate → vault
                 │          │          └─ unsure items wait in review/
                 │          └─ typed items: decision, fact, how-to, lesson,
                 │             commitment, preference, ownership
                 └─ chatter is dropped and never stored
```

Only the integrate step writes a vault. Agents and people propose: `share` and
"remember this" are inputs to the same pipeline, never writes. Only the system
writes notes, so there are no edit conflicts: people read them (in Obsidian too)
and correct one the way agents do, by sharing the correction. (Decided
2026-11-10.) A novelty check confirms, skips or flags a contradiction instead of
duplicating. Text from a user's machine passes a privacy gate first: secret
check, off-topic filter, exclusions, a pause switch. (Built: the secret check.)

The pipeline runs in the hub role and distills with a model the hub's config
names; a single user's own hub uses that user's own providers or API key.
(Decided 2026-11-10.)

### Prompt injection

Shared text may carry instructions meant for a model. The first guards
(decided 2026-11-10, all built):

- The model that distills has no tools: text in, text out, through the chat
  client, never an agent CLI.
- Shared text is passed as quoted data, apart from the pipeline's instructions.
- The answer must match the note schema, or it is rejected and the share waits
  in `review/`.
- Every note names who shared what it came from, so all notes from one source
  can be found and removed.
- Recall marks the notes it returns as reference data, not instructions.

Stronger checks come with connectors, which bring in text nobody on the team
wrote.

### Visibility and export

A note inherits its source's visibility, and recall never returns what the
asking identity may not see. Built: two audiences, as there are no teams of
people in a hub. A share is `private`, for the account that shared it, unless
it says `hub`, for every account that may recall. Rather than merging sources
of two audiences into the strictest, a note is made only from shares of one
audience: the distiller offers an item only the notes of its share's
audience, to merge into or link to, so private text never flows into a note
others see. (Decided 2026-11-18.) Not built: a prompt goes only to a provider
whose operator may see it.

General know-how gained at a workplace may be exported to the person's home hub
as a generalized copy, under the workplace's policy (off, approval, automatic;
the default is approval), with an audit log. Organization-specific knowledge
stays with the organization.

## Identity

One identity per person, one credential per client or device, each revocable
alone. Sign-in through the organization's OIDC provider, or local accounts for a
personal setup. On a hub server built so far, an account has a name and a
password (PBKDF2-HMAC-SHA256); logging in gives a session, which lasts a set
number of hours and is what a UI holds, and from a session a person makes
access tokens for runners, client agents and scripts, each named, with an
optional expiry and optional scopes that narrow the account's role. Only a
session may manage tokens or the password; failed logins lock a name for a
while. (Decided 2026-11-11.) A client agent uses a token as the API key of the
hub's MCP. Any client may sign in with a name and a password or a join code
(`token.login`, `token.join`), for an access token of its own, named after
it (the OAuth device flow and OIDC later). A runner does so over its link, as
`runner <name>`, and keeps the token in its database, sealed by its
`master.key`; the OS keyring may come later as an option. An admin makes a
join code for an account, or the account for itself with a session; it is
shown once and spent by one client within 24 hours. The hub stores tokens and join codes hashed. Runners, sources and agents are
identities too, and an agent acts on behalf of a person.

There are no teams of people in the hub, only roles. Ensemble defines a fixed
set of permissions (such as managing users, managing the hub's ANN pool,
sharing, recalling, receiving tasks), and each hub's config names its own roles
as sets of them, so an organization decides what roles it has. (Decided
2026-11-10.) Built so far: `share`, `recall`, `tasks`, and `admin`, which
manages users.

People join a hub server by signing up through its REST API (`/api/v1/`, on
the hub's own port, for a web UI and scripts) with a name and a password; the
account is pending, and logging in is refused, until an admin approves it with
a role. A hub may instead be `closed`, where only an admin adds users. The
first admin is made on the hub's computer with the CLI. (Decided 2026-11-11.)

## Clients

| Way | What happens |
|---|---|
| Client agents (Claude Code, Codex, OpenCode, claude.ai) | Their own login and model. The hub's MCP over HTTP, with a token as the API key: `share`, `recall`, tasks. Nothing of Ensemble installed. |
| REST API | Apps, scripts and the web UI: what MCP offers, and administration. |
| Web UI | `ensemble-ui`, optional and apart from the hub, a client of its REST API: tasks, runners, recall and share, the account, and for an admin the users and the settings. (Built; the review queue later.) |
| Obsidian over Git | Direct read of the vaults; a correction is shared like any knowledge. (Works today.) |
| Task tracker | Assign an issue to a person and add the signal. (Built for markdown issues.) |
| `ensemble-hub` CLI | Administration on the hub's computer. |

## Operations and interfaces

(Decided 2026-11-18; partly built: see the build order.) People do not edit
Ensemble's files: Ensemble writes them, and every setting has a way to be set
without opening one. Many people will use the web UI; what is built is the
ground any interface stands on, so that adding one, a CLI, a REST API, a web
UI or a desktop app, is writing an adapter, never re-implementing a feature.
The web UI is one such adapter: its forms are made from the operations as
the REST API lists them. Some administration may stay with the CLI.

### Operations

Everything a person can do to a hub or a runner is an **operation**, defined
once, in the core:

- a name, such as `provider.add`, `knowledge.models`, `user.join-code`,
  `settings.owner`;
- its input and output, as Rust types with JSON Schemas made from them; a
  field that takes some of a fixed list of texts (a role's permissions, a
  token's scopes) lists them in its schema, so every interface offers only
  those and the CLI refuses another;
- who may run it: a permission of the hub's roles, or the runner's owner;
- its secret fields, written and never read back nor kept in the audit log
  (keys are kept sealed in the database of the hub or the runner);
- errors as data, `{code, field, message, hint}`, so an interface shows each
  next to what it is about;
- for long work (a rebuild, testing a model), the events it goes through.

There are two kinds. **Goal operations** do what a person means: add a
provider, which is asked which models it serves first, its key kept
under a variable Ensemble names after it (`ENSEMBLE_PROVIDER_<NAME>_KEY`),
and turn its models on
so nobody needs to know a variable's name; choose the distiller's and the
embedding models, each checked; add a tracker; set a role; invite someone;
let a computer join; add a team from a template; rebuild the knowledge.
(Built: `provider.*` and `model.*` on the hub and the runner, in `ensemble-backends`'
`ops.rs`; `knowledge.models`, `knowledge.embedding`, `knowledge.recall`,
`tracker.*`, `role.set`, `role.remove`, `registration.set` in
`ensemble-hub`'s `goals.rs`.) **Settings operations** reach every other
setting, one each (`settings.owner`, `settings.sessions`,
`settings.lockout` on the hub; `settings.name`,
`settings.workspace`, `settings.shared-calls` on the
runner), with `settings.show`, `settings.history` and `settings.import`: a
change is loaded before it is kept, and kept in the audit log with who made
it.

An operation runs **where the state it changes lives**: a running hub or
runner applies it in memory, at once, then writes its settings, so nothing needs a
restart and no two processes write the same file. When the hub or the runner
is not running, the same operation runs in the process that asked, on the
files or the hub's database. The running process is asked over its local socket, which only its OS
user may open.

Agents do not run operations: no configuration through MCP tools.

### Interfaces

An **interface** is an adapter over the operations. It knows how to show them
and how to reach them, and nothing about what they do; one written once serves
every operation, those to come too.

- **The CLI** is made from the operations: each is a command, its input
  fields its options, with a short alias and positional arguments where the
  operation gives them. It reaches a running hub or runner over its socket.
- **The REST API** is one route per operation, `POST /api/v1/ops/<name>`,
  with an OpenAPI document made from the same schemas, signed in with the
  hub's sessions and tokens. It is RPC-shaped rather than one resource per
  route, so it needs no code per operation and matches the CLI one to one.
  Signing up is part of it: without it, an admin adds people from the CLI.
- A web UI, a desktop app, any other: a client of the REST API, or of the
  socket on the same computer. The web UI (`ui/`) is a component of its own,
  which the hub knows nothing of: `ensemble-ui` serves its app, a Dioxus app
  built for WebAssembly and embedded in the executable, on a port of its own,
  and passes the app's calls to `/api/` on to the hub, so the browser sees
  one origin. A browser signs in with `POST /api/v1/session`, which keeps the
  session in an HttpOnly, SameSite=Strict cookie; the hub refuses a change
  from a page of another origin, or one signed in by the cookie that names no
  origin, and `ensemble-ui` passes a change on, as from the hub's own origin,
  only from a page of the UI. Its forms are
  made from `GET /api/v1/ops`: each operation's fields in order, with the JSON
  Schema of each. An account's tasks, runners and knowledge are operations
  only the running hub answers (`served.rs`), each the link's request of the
  same kind, so the link, MCP, REST and the UI cannot differ.

None is turned off, and none is built into another binary: the CLI is always
on, over the socket `ensemble-hub …` and `ensemble-runner …` use, and has no
settings, so neither file names it; the REST API is the hub's `rest`
connector, always on, on the hub's one port beside MCP and the runners' link
(decided 2026-11-19, in place of an optional REST API on a port of its own).

A runner's own operations reach the hub as one more adapter, over the link,
for the runner's own account only: a person manages all their computers from
one place, and an admin never reconfigures someone else's.

### The link is the only way between runner and hub

A runner and its hub speak over the WebSocket and nothing else:

- **Signing in and joining** happen on it: a connection without a token may
  only ask `login` (a name and a password, with the lockout) or `join` (a code
  an admin made with `user.join-code`, or the account with `me.join-code`),
  and gets a token back. A runner makes no HTTP request to the hub, so a hub
  without its REST API serves every runner.
- **The protocol's version** is said on every link, and a hub refuses a
  runner of another version before anything else, naming both and which to
  update.
- **Tasks** are pulled over it, and questions and reports go back on it.
- **Shared models** go over a second WebSocket of the same runner, as
  "Sharing models" says: the runner that serves dials out, so a machine
  no one can reach may serve, and the one that uses it sends its calls over its
  own.

## Modules

Building blocks, not processes. A module is on when its config block is present;
each exposes a trait and may run in either process. The last column says which
crate holds it today.

| Module | Does | Needs | Built in |
|---|---|---|---|
| core | config, error vocabulary, state directory | — | `ensemble-core`, `ensemble-config` |
| identity | users, tokens, scopes; one implicit user standalone | core | `ensemble-hub` (`users.rs`) |
| backends | endpoints, the chat client, format translation, health, quotas | core | `ensemble-backends` (no quotas yet) |
| agents | CLI adapters, `direct`, `builtin`, workspace, worktrees, gates | backends | `ensemble-agents` |
| team | the helper team, fallback resolver | agents, identity | `ensemble-agents` (`router.rs`), `ensemble-backends` (`resolve.rs`) |
| tasks | tasks kept on the hub and leased, the inbox, `IssueTracker` adapters, hand-off | identity, team | `ensemble-tasks`, and `ensemble-hub` (`board.rs`, `inbox/`) |
| vault | vault repositories, schema, index, visibility | identity | `ensemble-knowledge` (no visibility yet) |
| pipeline | ingest, filter, extract, integrate, review | vault, backends | `ensemble-knowledge` |
| recall | context packages | vault | `ensemble-knowledge`, and `ensemble-hub` (`mcp.rs`) for client agents |
| capture | recorder, quick capture, privacy gate | pipeline | not built |
| connectors | chats, mail, CRM, tracker events, each its own | pipeline or tasks | not built |
| sources | connectors per provider (Microsoft 365, Telegram), collect and filter before the distiller | pipeline, backends | not built (decided 2026-11-19) |
| relay | runners' shared models, used by the hub and other runners over a second WebSocket | link, backends | `ensemble-link` (`relay.rs`, `loopback.rs`), `ensemble-hub` (`calls.rs`), `ensemble-runner` (`relay.rs`) |
| wiring | the hub's MCP over HTTP for client agents | api | `ensemble-hub` (`mcp.rs`) |
| operations | every action on a hub or a runner, once, for every interface | identity, config | `ensemble-ops`; the hub's in `ensemble-hub` (`accounts.rs`), the runner's in `ensemble-runner` (`cmd/runner.rs`) (accounts, tokens, settings, signing in so far) |
| interfaces | the CLI, the REST API, the web UI, and later a desktop app, as adapters over the operations | operations | the CLI made from the operations (`ensemble-ops`, `cli.rs`), the rest by hand in each executable's crate; the REST routes in `ensemble-hub` (`api.rs`) |
| api | chat API, MCP endpoint, REST | identity | `ensemble-hub` (REST, and MCP in `mcp.rs`) |
| web | the UI, apart from the hub | api | `ui/`: the app (`ui/app`, Dioxus, for WebAssembly) and `ensemble-ui` (`ui/server`) |
| link | hub-to-runner connection | identity | `ensemble-link`, the hub's side in `ensemble-hub` (`server.rs`, `runners.rs`), and the runner's link that says it is there in `ensemble-runner` (`worker.rs`) |

## Build order

1. The flat helper team, and `direct`. (Done.)
2. The module split into a Cargo workspace, behavior-preserving. (Done.)
3. Per-agent backend fallback and the resolver. (Done: `try` and `when_none`
   for models on LLM backends.)
4. Memory in the hub role: vault, pipeline, the `share`
   tool, recall. (Done: `hub.yaml`; the vault, the secret check,
   the pipeline with the injection guards and a background worker, BM25
   recall; the `share` and `recall` tools and commands; notes for the helper
   agents whose `recall` asks; session hooks for Claude Code and Codex. Not
   yet: visibility, the privacy gate beyond the secret check.)
5. Hub server, link, identity, tracker adapters. (Done: the inbox with a local
   markdown tracker; `ensemble-hub serve` with accounts, sessions, scoped
   access tokens, sign-up with approval and a REST API; `ensemble node login`; the organization vault over the
   link; nodes, private or shared, running agents and serving models through
   the hub, the `node` adapter and `/nodes/v1/`. Not yet: GitLab, GitHub and
   Jira; OIDC and the device flow; the chat API and the web UI.)
6. Async, the `builtin` adapter, and one node per worker that is also the MCP
   server. (Done: tokio throughout, as part one describes; `builtin` on
   harness-rs; the node as the long-running MCP server on a socket, with
   `ensemble mcp` as a bridge to it and a worker per folder.)
7. One node per computer, and the node as the only way in: every session
   is `ensemble mcp`, a bridge to it; `node setup` and `node connect` register the
   bridge with the client. (Done.)
8. Teams in a directory of the node: made, linked, pulled from git pinned
   to a trusted commit, or written by an agent and approved by the person;
   all run at once as `<team>/<agent>`. (Done.)
9. The hub server's accounts, tokens and deliveries in Turso, the Rust
   rewrite of SQLite, pinned to one exact version. (Done; decided
   2026-11-17.)
10. One fixed layout, `~/.config/ensemble/node/` (node.yaml, teams, .env,
    tokens) and `~/.config/ensemble/hub/` (hub.yaml, .env, database.db,
    vault): a hub is always `ensemble-hub serve`, with no team, and a node
    names it as `hub:`. (Done; decided 2026-11-17.)
11. Reloading the node's teams without a restart: after every `team`
    command and on `ensemble node reload`, with the open sessions told their
    tools changed. (Done.)
12. Installing on NixOS and with Home Manager: a hub as a system service and
    this computer's node as a user service (systemd, or launchd on macOS),
    each run as the person it is for and managed with `ensemble` as on any
    platform, with `node.yaml`, `hub.yaml` and the teams written from Nix or
    made with `ensemble`. (Done; no `ensemble-hub` wrapper, decided
    2026-11-18.)
13. Tasks kept on the hub and leased by nodes, so another node takes up the
    task of one that stopped; the hub's inbox makes issues into tasks there.
    (Done; decided 2026-11-17.)
14. A task's work on a pushed branch, so the node that takes a task over
    carries on from where the first got to; a task whose work cannot move
    is pinned to its node. (Done.)
15. Operations (decided 2026-11-18, as "Operations and interfaces" says): an
    `ensemble-ops` crate with the operation, its registry and errors as data;
    the accounts, tokens, settings and signing in that the CLI and the REST
    routes do today, as operations; the CLI made from the registry. (Done:
    `hub user`, `hub role`, `hub config`, `node config`, `node login` and
    `node logout` are made from the operations, which the REST routes run
    too; a team's settings and the other commands are not operations yet.)
16. `interfaces` in `hub.yaml` and `node.yaml`; the hub's socket; each
    operation run by the process whose state it changes; `hub.yaml` and
    `node.yaml` applied without a restart. (Done: the hub uses a change of
    its roles, sign-up, sessions, lockout, models, knowledge settings and
    owner at once, and starts again in its process for the rest; the node
    starts again in its process, its sessions carrying on through their
    bridges. `interfaces` went with step 21.)
17. The hub serves MCP over HTTP: `share`, `recall` and the tasks, signed in
    with an account's token as the API key; the server's instructions in place
    of session hooks; a token's answer gives the URL and a line to paste.
    Clients install nothing. (Done: `interfaces.mcp.url` turned it on, and
    since step 21 it is always on; the node's MCP and its hooks go with step
    18.)
18. `ensemble-hub` and `ensemble-runner`, each its own crate, executable and
    Nix package, with one crate for the link's messages; the node becomes the
    runner, a pure worker: no MCP for clients, no hooks, no delegations;
    signing in and joining over the link, with join codes, and no HTTP from a
    runner to the hub; the protocol version checked on connecting. (Done: the
    node is the runner, `runner.yaml`; the stdio MCP, session hooks,
    delegations, the `task` and `knowledge` commands and the agents' `recall`
    are gone, so clients use the hub's MCP alone; the runner runs a worker for
    each git repository of its `workspace`, looked at again every 30 seconds;
    the hub and the runner are two crates, two executables and two Nix
    packages, `ensemble-hub` and `ensemble-runner`, and the runner is built of
    none of the hub's crates; a link without a token may only ask `login`, with
    a password and the lockout, or `join`, with a join code made by
    `user.join-code` or `me.join-code`, single-use and good for 24 hours, each
    making an access token named after the runner, so `ensemble-runner login`
    and `ensemble-runner join` make no HTTP request and the runner has no HTTP
    client for the hub; every link says the protocol version it speaks, and
    the hub refuses another, or none, with 426 and both versions named.)
19. The runner's workspace: a task's repository found or cloned, worked on a
    branch and pushed; a question when it is not found; a task given back,
    and blocked when no runner can reach its repository; the related notes in
    every task; a task's report as a share; the `node` adapter removed. (Done. A task
    names its `repository`, which `submit` takes with a `gate`; every 30
    seconds a runner asks the hub for its account's tasks waiting for its
    agents in every project (`tasks_waiting`), clones the
    repository of a project it has no folder for into its workspace with no
    prompt, asks on the task which repository to clone when none is named,
    and keeps the answer as the task's `repository`, and gives a task back
    with why when its computer cannot clone it (`task_give_back`); a task is
    blocked while every connected runner of the account with its agent gave
    it back, and offered again when one that has not
    connects or it is assigned or answered again; the hub keeps the notes it
    recalls for every
    new task, which the agent reads as reference data; a task done is still
    shared; every connected runner registers with all its agents
    (`runner`/`runners`); the `node` adapter, its snapshots and the link's
    runs are gone; the link's protocol 3.)
20. Sharing LLM backends: the `llm` block in `hub.yaml` and `runner.yaml`,
    offered backends listed in the hub's settings operations, calls relayed
    over a second WebSocket. (Done: measured first, a few milliseconds
    through the hub; a team names its backends and the runner defines them,
    `llm.direct` with keys from its `.env`, `shares` and `max_shared_calls`,
    and `llm.hub` for others' it reaches; hub.yaml's `llm.direct` and
    `llm.runners`; a name unique across a file's groups; the team checked
    against the runner's backends as it loads; `offers.models`, `/nodes/v1/`
    and the link's model calls removed; a relay socket per runner at
    `/relay`, calls numbered by whoever starts them, prompts in chunks and
    answers streamed, the hub checking who may call whom and keeping a call
    waiting for up to 10 seconds for a free place, then busy; loopback
    endpoints on the hub and on runners that reach others' backends;
    `runner.backends` with the models each runner listed; the link's
    protocol 4.)
21. The hub's core and its connectors: `hub.yaml` as the core (`owner`,
    `http`, `llm`, `accounts` with the `roles`) and `connectors`, each
    always on and each written; tracker adapters by name under
    `connectors.tasks.trackers`; no `interfaces` in either file, the command
    line always on; no MCP `url`, a token's hints naming the address the
    request reached; the link at its own path. (Done; the link's protocol 5.)
22. The REST API as an interface: one route per operation, the same as MCP
    offers, OpenAPI, sign-up in it, on the hub's port. (Done: every
    operation by its name at `POST /api/v1/ops/<name>`, and those a token may
    run, with the JSON Schema of their input, at `GET /api/v1/ops`; the goal
    operations for backends and their keys, knowledge, trackers, roles and
    sign-up; the OpenAPI 3.1 document at `GET /api/v1/openapi.json`, made
    from the routes and the operations, the same for anyone who asks.)
23. The task tracker with backends behind one interface, the markdown issues
    its one backend; more than one tracker at once. (Done: an inbox for each
    tracker, its deliveries kept by the tracker's name and the issue's id, those
    of a hub that watched one adopted by its tracker; two trackers on one
    folder are refused.)
24. Sources: the `Source` trait, and the collect and filter steps with one
    LLM and the admin's prompt, raw messages dropped; `source.try` to see what
    a text would leave. No provider's adapter yet. (Done:
    `connectors.sources.filter`, `ensemble-knowledge`'s `sources.rs`, and the
    `source filter` and `source try` operations. No adapter takes messages
    yet, so no loop runs.)
25. The hub's settings in its database, as a GitLab instance keeps them, so
    a web UI changes them and a deployment only runs the hub. (Done, first
    as one YAML document in `database.db`; the backends' keys sealed there by
    `master.key`, made the first time one is kept; `init --keep`; the NixOS module without `settings`, with `port` for the
    firewall.)
26. The runner's settings in its database, as the hub keeps its own, so
    nothing that installs a runner configures it by editing a file. (Done:
    `ensemble-store`'s `kept.rs`, shared by both: the runner's
    `database.db` holds only its settings and its keys,
    sealed by its `master.key`; `init` makes the runner `setup` would,
    asking nothing, and `init --keep` is what the modules run before each
    start; the NixOS and Home Manager modules without `settings`,
    `workspace` or `teams`, and no `config merge` or `.nix-managed` left.
    Teams stay folders: a team is added with `team pull`, `link` or `new`.)
27. No YAML for the settings of a hub or a runner, as a GitLab instance
    keeps its application settings: typed rows, one of settings and a table
    for each kind of thing there are several of, changed only through
    operations, each change in an audit log with who made it. (Done:
    `HubSettings` and `RunnerSettings`, kept by `ensemble-store`'s
    `kept.rs` and changed through `ensemble-config`'s `Kept`; an operation
    for every setting, `settings.show`, `settings.history` and
    `settings.import`, and `--json` on every command; a backend a runner
    serves is one more type of backend; `config get|set|unset|edit|path`,
    the samples and the two schemas gone; a new hub starts on this computer
    alone, closed, with no backend or model, and `ensemble-hub setup` makes
    it and its first admin. A team's `config.yaml` stays a file.)
28. A hub and a runner each in a tree of its own, under one folder,
    `--config-dir`, as state, not configuration, neither reading the
    other's: a runner is one more connector to the hub. (Done: `hub/` with
    a folder for its core and one for each connector that keeps anything,
    `runner/` with its core, teams and work; the runner's hub tokens sealed
    in its database; no `.env` or `--env-file`, a secrets manager's keys
    reaching each through its environment; where the hub listens is
    `serve`'s, not a setting, and the vault's folder fixed; `serve` makes a
    new hub or runner the first time, so the modules only run them;
    `ensemble-store` keeps only what both share.)
29. Signing in the same for every client: a runner is not special to the
    hub's accounts. (Done: `runner.login` and `runner.join` are `token.login`
    and `token.join`, each taking the client's name, which its new token is
    named after, a runner's being `runner <name>`; a join code signs one
    client in, over the link or with `POST /api/v1/join`; `ensemble-hub setup`
    makes the first admin and their token, and no join code; link protocol 6.)
30. The web UI, over the REST API, a component apart from the hub. (Done:
    `ui/`, a Dioxus app for WebAssembly, and `ensemble-ui`, which embeds it,
    serves it on a port of its own with `index.html` for each of its pages
    and a strict content security policy, and passes its calls to `/api/` on
    to the hub; the Nix package `ensemble-ui` and the NixOS module's optional
    `services.ensemble.ui`, a system service with no state;
    a browser's session in an HttpOnly, SameSite=Strict cookie, and no change
    from a page of another origin; an account's tasks, runners and knowledge
    as operations the running hub answers, as the link's requests;
    `GET /api/v1/ops` listing each operation's fields in order, which the
    UI's forms are made from; pages for tasks, runners, knowledge, the
    account, users and settings; the settings a section each, filled in
    with each setting as it is now, roles as a box for each permission,
    backends each tested; forms that pick what the hub knows a field may
    be (a role, an agent connected now, a project, a model a backend
    serves), ask only for what the choices so far need, check what they
    can before asking the hub, and ask before they remove, revoke or
    cancel anything. Not yet: sign-up, browsing the vault's notes, the
    review queue.)
31. Models and harnesses in place of teams, as "Models and harnesses"
    describes. (Done: runners that work for the hub removed, so a runner
    takes its own account's tasks alone; the ANN pool of the runner and
    the hub in place of the backends: providers, each with the most calls it
    takes at once, and their models, found by asking the provider when it is
    added, on `provider.refresh` and as the process starts, off until turned
    on, of a tier from a catalog of known families and sizes, or Ollama's,
    else medium, each named `<provider>:<id>` and checked to be on and of the
    kind its use needs; a runner sharing models, not whole providers, the
    hub queueing calls for each provider; `provider.*`, `model.*` and
    `runner.models`, and the web UI's Models section; the pool's sources
    called providers, as a source is knowledge from outside; link protocol
    9; the runner's harnesses, one of each kind, looked for on PATH when the
    runner is made, as it starts and on `harness.refresh`, off until turned
    on, each with its command and a run's most seconds and steps, Claude
    Code's and Codex's own models named from the catalog, and
    `harness.list|on|off|set|refresh`; a gate as a task's own, its command,
    time limit and attempts, given only by the task's account when it makes
    the task (MCP's `submit`, `task.create`, the web UI) or with `task.gate`
    (MCP's `gate`, `task_gate` on the link) while the task neither runs nor
    has ended, never by the tracker inbox, the team's `gates` gone; link
    protocol 10. Not yet: decision models in the pool; teams, their `config.yaml` and agents' names on tasks
    removed, with the picker choosing a harness and an LLM; reassigning a
    task to another person, whose runners run its gate only once that
    person confirms it.)
32. Later: Jira and other trackers; source adapters (Microsoft 365 for mail
    and Teams, Telegram); a runner's operations from the hub, for its own
    account; the remaining goal operations (people, computers); chat.

What remains besides is the "Not yet" of steps 4 and 5, and the modules
marked not built above. Other interfaces, such as a desktop app, are
adapters like the web UI.

## Open questions

1. Export from workplace to home: who approves.
2. Recording from a browser without sending audio to the hub.
3. Chat: whether people talk to a helper agent through the hub at all, and
   how. Left for later on 2026-11-19.
4. Several hubs for one person (a home hub and a workplace hub): a runner
   names one hub; whether it may name several.
5. How a task says what it needs (whether it changes code or only asks for
   an answer, how hard it is) and what decides it: the person's agent as it
   makes the task, an LLM, or a decision model. Until it is decided, every
   task goes to a harness and the picker follows its rules. Left for later on
   2026-11-24.
6. Skills, folders of instructions a harness loads when the work calls for
   them, as the expertise a team's prompts once gave: where they live, and
   whether the vault's how-to notes become some. Left for later on
   2026-11-25.
