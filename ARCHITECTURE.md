# Architecture

This document has two parts. **Part one, "How it works"**, describes the
system as it is built: the processes, the crates, where things are kept, how
work flows through them and how they run at once. **Part two, "Decisions and
direction"**, records what was decided with the owner and where Ensemble is
going; its build order says what is done and what is not. The questions at the
end are open; everything else is decided unless marked otherwise.

`README.md` is the user's guide: every setting, command and tool. This document
is the map behind it.

# Part one: how it works

## The pieces

Ensemble is an agentic development system, not an MCP server: a **hub** that
provides services, and **nodes** that provide computing power, with work
spread across them. MCP is how an agent CLI reaches a node. It is one binary,
`ensemble`, in these roles:

- **The node**: a computational resource. It holds a helper team, runs its
  agents (agent CLIs, Ensemble's own loop) and works through tasks, and may
  offer the model servers of its machine. A private node works for its owner;
  a shared one lends its agents and models to others on the hub, so a team
  spreads its load over every machine it has. It serves its owner's agent
  sessions from any folder.
- **The hub**: the services: task tracking (issues from a tracker become tasks
  on the right person's node), knowledge, accounts and tokens, routing runs
  and model calls between nodes, and integrations (Telegram and others, to
  come). It has no team and runs no agents. It is always its own process,
  `ensemble hub serve`, on a server for a team or on one person's computer,
  and each node names it as `hub:` in its `node.yaml`.
- **The helper teams**: each a `config.yaml` in the node's teams directory,
  its agents named `<team>/<agent>`, each run by an
  adapter (an agent CLI started headless, an agent loop of Ensemble's own, one
  chat request to a model, or a run on another node).
- **The ways in**: the agent CLI a person already uses, which reaches its node
  over MCP and sees the helper team as tools; the hub's tracker, whose issues
  become tasks; and, to come, the hub's integrations, chat API and web UI.

On one computer:

```text
  your agent CLI (Claude Code, Codex, OpenCode, ...)
        │  MCP: JSON-RPC 2.0, one object per line, on stdio
        ▼
  ensemble mcp          a bridge
        │  Unix socket: one hello line, then MCP both ways
        ▼
  ensemble node serve   one per computer: sessions from any folder,
        │               a worker per folder it served
        ▼
  the helper team (config.yaml)
        │  router: agent → adapter; model from its `try` list
        ▼
  claude -p · opencode run · codex exec        agent CLIs, as subprocesses
  builtin                                      harness-rs's loop, in-process
  direct                                       one chat request
  node                                         a run on another computer
        │
        ▼
  model servers (llm_backends), or the CLI's own login
```

With a hub server for a team:

```text
  member's node                              ensemble hub serve
  ─────────────                              ──────────────────
  RemoteMemory (a session's knowledge) ══╗   accounts, sessions, tokens (database.db)
  RemoteBoard (a folder's tasks)       ══╬══ the tasks, leased to nodes (database.db)
  Worker (runs and models it offers)   ══╝   the knowledge vault and its distiller
                                             the tracker inbox (database.db)
  `node` adapter (a run elsewhere)     ══    the connected nodes, runs, /nodes/v1/
                                             REST /api/v1/
     ══ one WebSocket each, opened by the node, signed in with a bearer token
```

## Processes

Every process is the same binary. What runs in each:

| command | what it runs | ends |
| --- | --- | --- |
| `ensemble mcp` | a bridge: it connects to the node's socket, sends `{"dir": "<folder>"}`, and carries MCP between stdio and the socket a message a line; when the node goes away while the session goes on, it answers the requests left with an error to try again, waits for the node to come back, says the session's `initialize` again and tells the client its tools may have changed | when the node closes the session |
| `ensemble node serve` | the node, one per computer: a socket for sessions and for the operations `ensemble node …` asks of it, a worker per folder that runs the tasks the hub keeps for its project, and with the hub its `node.yaml` names, a link that offers runs and models (`Worker`) | on SIGINT or SIGTERM; a change to `node.yaml` has it start again in the same process |
| `ensemble hub serve` | the hub, from `~/.config/ensemble/hub/`: one port for the link, the REST API and `/nodes/v1/`; a socket for the operations `ensemble hub …` asks of it; the tasks; the vault's distiller; the tracker inbox | on SIGINT or SIGTERM; a change to `hub.yaml` it cannot use as it runs has it start again in the same process |
| everything else | one command: read the config and the state, or ask the hub, do one thing, print; `task new`, `task assign` and `task answer` also tell the node about the folder, so that its worker runs there | at once |

A folder's tasks are run by one worker of the computer at a time: whoever
holds the lock file (`worker.lock`) in the folder's state directory; across
computers, a lease on the hub keeps one node on a task. A node that finds the lock
taken serves sessions there but starts no tasks. A lock left by a dead process
is taken over. The same lock keeps one node per computer, one distiller per
vault and one inbox per hub.

## Crates

A Cargo workspace. Each crate depends only on crates above it in this table;
`tests/workspace.rs` checks that.

| crate | what it is | files |
| --- | --- | --- |
| `ensemble-core` | what everything shares | the error vocabulary (`port.rs`); the per-user state, cache and config directories (`state.rs`); the working folder, a `repo` inside it and what a run changed (`workspace.rs`); the record of runs behind `ensemble node agent stats` (`records.rs`); the one-worker lock (`lock.rs`); dates in UTC (`time.rs`); the files a run on a node carries there and back (`snapshot.rs`); the folders a node served, by project (`folders.rs`); the async runtime and its bridges to code that blocks (`runtime.rs`) |
| `ensemble-ops` | operations, defined once for every interface | the operation, its spec (name, summary, who may run it, input fields), errors as data, the caller, and the registry that checks both before it runs one (`lib.rs`) |
| `ensemble-config` | the three files a person writes | `config.yaml`: the document as written (`raw.rs`), the environment, `.env` files and `--env-file` (`env.rs`), validation (`validate.rs`), the resolved types (`lib.rs`), adding and removing an agent as text, comments kept (`edit.rs`); one setting read, set or merged as text (`settings.rs`); a settings file, with the Nix-declared settings kept, and its `config.*` operations (`settings_file.rs`); the teams directory and teams combined as `<team>/<agent>` (`teams.rs`); `hub.yaml` (`hub.rs`); `node.yaml` (`node.rs`) |
| `ensemble-backends` | talking to model servers | one chat request in the server's own format (`chat.rs`); which models a server serves (`models.rs`); the first usable model of a `try` list (`resolve.rs`); texts as vectors (`embed.rs`); a stand-in server for tests (`testing.rs`) |
| `ensemble-store` | Turso files | the hub's `database.db`, opened for each read or change, and any other Turso file, such as the vault's cache of vectors (`lib.rs`) |
| `ensemble-knowledge` | the hub's knowledge | the vault, a git repository (`vault.rs`); the secret check (`secrets.rs`); shares into notes (`pipeline.rs`); BM25 search (`search.rs`); notes by meaning, with their vectors in `.ensemble/embeddings.db` (`meaning.rs`); `Memory`, the `share` and `recall` tools, the background distiller, and the `KnowledgeAccess` trait a session uses (`lib.rs`) |
| `ensemble-link` | the hub server and the link to it | the hub's operations, its accounts, tokens, sessions and settings (`accounts.rs`); accounts and hashed tokens (`users.rs`), kept with the tasks and the inbox's deliveries in `database.db` (`ensemble-store`); the REST API, each route one of the operations (`api.rs`), over a small HTTP reader (`http.rs`); the frames of the link (`protocol.rs`); the hub's side, a task per connection (`server.rs`); connected nodes, runs and model calls, `/nodes/v1/` (`nodes.rs`); the node's side, `Connection`, `Link` (one reconnecting) and `RemoteMemory` (`client.rs`); the token file (`tokens.rs`) |
| `ensemble-agents` | running one agent once | the adapters (`external/`): `claude_code.rs`, `opencode.rs`, `codex.rs`, `builtin.rs`, `direct.rs`, `node.rs`, the subprocess runner (`process.rs`) and gates (`gate.rs`); a git worktree per task (`worktree.rs`); the router: agents to adapters, models, records, `health` (`router.rs`) |
| `ensemble-tasks` | work handed to the team | tickets and tasks as the hub has them (`tasks.rs`); where they are kept and leased: the `Board` trait, the hub's `DbBoard` and the `Service` that answers the link, a node's `RemoteBoard` (`board.rs`); what a node keeps of its runs (`runs.rs`); the queue that runs tasks and background delegations within slots and budgets (`queue.rs`); the MCP tools (`delegation.rs`); review and merge of a worktree task (`review.rs`) |
| `ensemble-inbox` | issues from a tracker into tasks | the `IssueTracker` trait (`tracker.rs`) and markdown issues (`local_markdown.rs`); an issue made a task and what the task reports (`work.rs`); the hub's inbox (`relay.rs`) |
| `ensemble-node` | a node's link to a hub | `Worker`: runs and model calls for others (`lib.rs`) |
| `ensemble-mcp` | MCP over stdio | the session loop (`lib.rs`), the tool catalogue (`protocol.rs`), notifications (`notify.rs`) |
| `ensemble` (the root) | the binary | the command line, parsed with clap (`src/cli.rs`), and what each group of commands does (`src/cmd/`: `node`, `team`, `agent`, `task`, `knowledge`, `hub`); the node's sessions and the bridge (`src/daemon.rs`); what `node setup`, `node connect` and `node doctor` find and register (`src/setup.rs`); session hooks (`src/hooks.rs`); the node's teams, loaded, made, linked, pulled and approved (`src/teams.rs`), over git repositories pinned and trusted per commit (`src/remote.rs`) |
| `nix/` | not a crate: the NixOS module (`nixos-module.nix`: a hub as a system service, the node as a user service, each run as the person it is for), the Home Manager module (`home-manager-module.nix`: the node on Linux or macOS), what they share about a node (`node.nix`), and an example configuration the flake's checks build (`example.nix`). Settings declared in Nix are merged into the person's own files at each start, and `ensemble` refuses to change them (`.nix-managed` names them); the rest is made and changed with `ensemble`, as on any platform |

Two rules keep the layers apart. The MCP session knows the tools and the
router, not which CLIs exist. The router knows agents; only the adapters know
how a CLI is started and read.

## Configuration and state

What a person writes is **configuration**, reviewed and never changed by
Ensemble except on request (`node agent add`, `node team new`, an approved
`team_create`):

| file | where | holds |
| --- | --- | --- |
| `config.yaml` | a helper team's folder in the node's teams directory (`~/.config/ensemble/node/teams/<name>/`), a folder linked there, or a pulled repository's clone | `llm_backends`, `limits`, `gates`, `tracker`, `agents` |
| `hub.yaml` | `~/.config/ensemble/hub/`, on the computer that runs the hub | `owner`, the hub's own `llm_backends`, `knowledge`, `tasks`, `server` and `roles` |
| `node.yaml` | `~/.config/ensemble/node/`, one per computer | `name`, `access`, `hub`, `offers` |

No setting has a default: every one is written, and a missing one is refused by
name. Each file is changed by hand or setting by setting from the command line
(`ensemble node config`, `ensemble hub config`, `ensemble node team config`;
`settings.rs` changes one setting as text, keeping comments), loaded before it
is written. A block that is absent means its module is off. No file holds a secret:
keys are named by the variable that holds them, read from the environment or
from a hidden `.env` beside the file that names them, `node/.env` for the
node's teams and `hub/.env` for the hub (`--env-file` reads another file in
their place). The node and the hub keep separate secrets because they are
separate services, often of separate users. Each file has a JSON Schema at the repository root, checked
against the code by the tests.

What Ensemble learns while it runs is **state**, per user, never in a project
or a config directory:

| path | holds |
| --- | --- |
| `~/.local/state/ensemble/workdirs/<folder>-<hash>/` | a working folder's `runs/`, `worktrees/`, `state.json` and `worker.lock` |
| `~/.local/state/ensemble/orgs/teams-<hash>/` | the teams' `usage.json` (budgets) and `records.jsonl` (`ensemble node agent stats`), by `<team>/<agent>`, shared by every folder |
| `~/.local/state/ensemble/node/` | the node's lock, `folders.json` (the folder last used for each project), `runs/` for others |
| `$XDG_RUNTIME_DIR/ensemble/<hash>.sock` | a node's socket, in a folder only its user may enter (`/tmp/ensemble-<user>/` without `XDG_RUNTIME_DIR`) |
| `~/.config/ensemble/node/hubs.json` | the node's hub tokens, one per URL, mode 0600 |
| `~/.config/ensemble/node/.env`, `~/.config/ensemble/hub/.env` | the keys the node's teams and the hub's backends name, written by you, mode 0600 |
| `~/.config/ensemble/node/teams/` | the node's helper teams: a folder per team, its `config.yaml`, a link to a folder elsewhere, or `pulled.json` (repository and commit); `pending` beside a team an agent wrote |
| `~/.cache/ensemble/` | clones of pulled teams, checked against their commit each time |

`<hash>` is a hash of the whole path, so two folders of one name are kept
apart. The knowledge vault is the one thing a hub keeps in its own directory:
it is meant to be read, in Obsidian or anything that reads Markdown.

## How it runs at once

Ensemble is async, on tokio (one shared multi-thread runtime,
`ensemble_core::runtime`).

- **A session is one task.** Its tool calls are futures in one
  `FuturesUnordered`, beside the loop that reads requests, so several calls run
  at once and each can borrow what the session holds. Replies and
  notifications go through a channel to one writer, so messages never
  interleave.
- **A queue is one task.** `Queue::work` scans the folder's tasks every two
  seconds, starts what may start, and keeps the running tasks and background
  delegations as futures in one `FuturesUnordered`. It wakes early on a
  `Notify` when something is submitted, ends or frees a slot.
- **A node** spawns a task per session, and one per folder's queue. Its link
  to a hub is one task whose runs and model calls go on beside it; they hand
  what to send to the link through a channel.
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
  locks the file while it is open, and this lets `ensemble hub user` change
  accounts while the server runs.
- **Loops that are not async** (a vault's distiller, a tracker's inbox) run on
  threads of their own, polling every few seconds.
- **Stopping**: SIGINT and SIGTERM set one flag. Every loop watches it, the
  queues close, running tasks are stopped (and, having no result, run again
  next time), and the node waits up to ten seconds for them.

What a node keeps for a folder, and what a hub server's connections borrow, is
made once and lives as long as the process, so it is leaked on purpose
(`Box::leak`) and borrowed as `'static`.

## Teams

The node runs every team in its teams directory at once. When it starts it
reads each one (`src/teams.rs`): a folder made here, a link to a folder
elsewhere, or a pulled repository, whose clone in the cache is checked against
its pinned commit and must be trusted. A team an agent wrote is skipped, with a
line saying it waits for approval; a team that cannot be loaded stops the node,
naming it. The teams are then combined into one helper team
(`ensemble_config::teams::combine`): each name of a team, its agents, model
servers, limits and gates, becomes `<team>/<name>`, and every reference inside
it (an agent's `limit`, its models' backends) is renamed with it. So the
router, the queue, budgets, records and tasks work as with one team, with
agents named `web/coder`; a task's `gate: { name: test }` means the gate
`test` of its agent's team.

- **Made**: `node team new` writes the sample; `node agent add` and `node agent remove` edit
  it as text, comments kept.
- **Linked**: `team add` makes a symbolic link to a folder with a
  `config.yaml`, such as a checkout a company keeps.
- **Pulled**: `team pull` resolves a tag or branch to a commit, clones it into
  the cache, shows what it would run, and records the trust and
  `pulled.json`; `team update` moves the pin the same way. A branch is never
  followed at run time.
- **Written by an agent**: `team_create` checks the config and writes it with
  a `pending` file; `team approve` shows what it would run and removes the
  file. The agent has no tool to change a team that exists.

The commands that read teams (`node team check`, `node doctor`, the `node agent`
commands, `task new`) load them the same way. A running node reloads them when a `node team` command
changes the directory, or on `ensemble node reload` after a change by hand:
it reads them again, and if one cannot be loaded it keeps the teams it had
and says why. Otherwise each folder's `Ensemble` gets the new teams with
`set_config` (a run going on keeps the snapshot it started with), the node's
offers and its inbox get them too, and every open session sends its client
`notifications/tools/list_changed`, so the client asks for the tools again.

## A delegation

Your agent calls `delegate` with an agent's name, `<team>/<agent>`, and a task:

1. The session hands the call to `delegation::call` (`ensemble-tasks`), as a
   future of the session's task. The client's `progressToken` names it in the
   notifications.
2. The queue takes a **slot** of the agent (`max_concurrent`, or its shared
   `limit`), waiting if they are all busy, and counts the run against its
   **budget**, refusing it when over.
3. If the agent has `recall`, the hub's knowledge is asked about the task, and
   the notes go after the task's context.
4. The **router** (`ensemble-agents`) picks the adapter, and for an agent on
   `llm_backends` resolves the first model of its `try` list whose backend
   answers and serves it (`ensemble-backends`; what a backend serves is
   remembered for a minute).
5. The **adapter** runs it. A CLI adapter starts the CLI in the working folder
   in its own process group, with the agent's prompt, tools and model, and
   reads its JSON events line by line as they come; each step and tool call
   becomes a progress notification. `builtin` runs harness-rs's loop in
   process; `direct` sends one chat request; `node` sends the run to a hub
   (see "Nodes").
6. At `max_seconds` or on a cancel (`notifications/cancelled`), the whole
   process tree is killed and what was done is returned, marked unfinished.
7. The working folder is compared before and after, for agents that can write,
   and the result lists the files changed, the commands run, steps, tool
   calls and tokens. A run that overlapped another in the same folder says so.
8. One line is appended to `records.jsonl`; the slot is given back.

A failed delegation comes back as a tool result with `isError: true`, never a
JSON-RPC error, so the model learns why. With `background: true` the call
answers at once with an id (`adhoc-N`) and steps 2 to 8 go on in the queue's
task; `status`, `wait` and `cancel` follow it.

## A task

A task is kept on the hub, in its database, for one account and one project
(a working folder's name): its ticket, its assignee, its questions and
answers, what agents handed on, its result, and its **lease**. It comes from
the `submit` tool, `ensemble task new`, or the hub's inbox. The hub never runs one; a
node leases it. Each folder's worker, every two seconds and whenever
something changes, asks the hub for its project's tasks (`RemoteBoard`, over
one link that opens again when it breaks), and:

1. skips tasks that ended, with no assignee (backlog), waiting for an answer,
   with `after` tasks not yet `done`, or running on another node;
2. waits while the agent is over budget, its slots are busy, or (with
   `when_none: wait`) no model of its `try` list can be used;
3. **claims** the task for a minute (`task_claim`); another node that was first
   wins, and the task is left;
4. for `isolation: worktree`, makes or reuses a git worktree on the branch
   `ensemble/<id>`, from where its work was last pushed when it has been;
5. recalls notes once, if the agent has `recall`;
6. runs the agent as a delegation, then the task's **gate** if it names one,
   running the agent again with the gate's output until it passes or
   `attempts` are used, all the while renewing the claim every 15 seconds
   (`task_renew`), which also says when someone cancelled the task;
7. says how the run ended (`task_finish`): `NEEDS INPUT: <question>` keeps the
   question and the task waits for `answer`; `HAND OFF: <agent> <what
   remains>` reassigns it with a note; otherwise its result (`done`, `failed`
   or `cancelled`), after committing everything on the task's branch in a
   worktree. Nothing is merged: that is `ensemble task merge`, which only a person
   runs.

A node that stops puts its running tasks back (`task_release`); one that dies
stops renewing, and the hub treats its claim as gone once it runs out. Either
way the task is `queued (interrupted on <node>)`, and the next node to look
runs it again. A node whose claim ran out, and whose task another took, is
refused when it says how its run ended, so a task never ends twice. The hub's
clock alone decides when a claim runs out.

Where the work is decides who goes on with it. A claim says whether the run
is **movable**: its agent works in a worktree, and the repository has a remote
`origin`. Whenever a movable run stops, the node commits what it did on the
task's branch, pushes the branch, and tells the hub where it is (`progress`:
node, branch, commit, with `task_release` or `task_finish`); the next run, on
any node, fetches it and makes or moves its worktree there, and its agent is
told to go on from it. A run that is not movable changes one computer's
folder, so the hub **pins** the task to that node from its first claim, and
no other node may claim it. `review` and `merge` fetch a branch pushed from
another computer, and move a local one that is behind it up to it.

What a run does as it goes (its activity lines) is in `runs/<id>/` on the node
that runs it, which is how `status` shows what an agent is doing there.

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
  `ensemble hub knowledge rebuild` makes them again from scratch.
- **The pipeline** distills with the hub's own `knowledge.distill` models, by
  one chat request per step, with no tools. Shared text goes between markers
  with a nonce, an answer must be the JSON asked for, and anything else goes
  to `review/`.
- **A session reaches knowledge through `KnowledgeAccess`**, an async trait
  with two implementations: `RemoteMemory`, the hub's vault over the link,
  opened on first use and opened again once when it breaks, which is what a
  node uses; and `Memory`, the vault on disk (its file and git work through
  `runtime::blocking`), which is what the hub itself uses. The
  session offers the knowledge tools the hub allows, and the queue uses the
  same trait to give notes to agents with `recall`.
- **Session hooks** (`ensemble hook`) put notes into Claude Code and Codex
  sessions on `SessionStart` and `UserPromptSubmit`, each note once per
  session, through the same trait.
- **On the hub** a share is kept under the signed-in account's name, and the
  running hub distills; `ensemble hub knowledge distill` does it by hand on
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
  node that runs it, its question (posted, and the assignee's next comment is
  the answer), its result (posted once). An issue closed cancels its task.
  Nodes know nothing of trackers: they run the task like any other.

## Nodes

A node with `offers` and a hub server keeps a link that does what comes
(`ensemble-node`'s `Worker`):

- **A run.** A requester's agent of `type: node` (the `node` adapter) sends the
  hub a `run` request with the task and a snapshot of the files git tracks or
  would track (at most 24 MiB). The hub picks the first of the named nodes that
  is connected, may be used by that account, and offers the agent, and sends
  it a `run` event. The node writes the files into a folder of the run's own,
  waits for a slot of the agent, runs its owner's agent there, and sends
  `run_progress` lines and a `run_done` with the changed files. The requester
  writes them into its folder; gates, worktrees and commits stay there. When
  either side goes away the other is told; a cancel is a `run_cancel`.
- **A model call.** An OpenAI-compatible request to `/nodes/v1/` on the hub's
  port, with an access token as the API key, goes as a `model_call` event to
  the least busy node that serves the model (the caller's own first). The node
  sends it to its model server with reqwest and streams the answer back in
  `model_reply` chunks, which the hub writes to the caller as they come.

The hub keeps nodes as presence only: a node is a connection that said `node`,
and nothing of it is kept once it is gone. A private node works for its owner;
a shared one also for accounts with `compute`, and connecting one needs
`host`. A shared node may offer only agents on `llm_backends`, so a CLI's own
login is never lent.

## The link

The node opens one WebSocket and signs in with `Authorization: Bearer <token>`
during the upgrade (401 without a valid one). Frames are JSON text: requests
`{id, method, params}`, replies `{id, result}` or `{id, error}`, and events
`{event, data}` the hub sends unasked. A node may open several links: one for
the hub's knowledge and one for the tasks in each folder it serves, one for
what it offers, and one per run it sends elsewhere.

| for | the node asks | the hub sends |
| --- | --- | --- |
| knowledge | `hello` (who am I, which permissions), `share`, `recall` | |
| tasks | each with its `project`: `tasks`, `task`, `task_create`, `task_claim`, `task_renew`, `task_release`, `task_finish`, `task_answer`, `task_assign`, `task_cancel` | |
| nodes | `node` (I am a node: name, access, agents, models), `nodes` (which may I use), `run`, `run_cancel`, and as a node `run_progress`, `run_done`, `model_reply` | `run`, `run_cancel` and `model_call` events to a node; `run_progress` and `run_done` events to whoever asked for a run (a `run_done` that failed when the node went away) |

Every request is checked against the token's permissions (its role, narrowed by
its scopes), read again from the hub's state, so a revoked token stops at
once. The permissions are `share`, `recall`, `tasks`, `admin`, `compute` and
`host`. The same port serves the REST API under `/api/v1/` (accounts,
sessions, access tokens, admin) and `/nodes/v1/`; a request that asks for a
WebSocket upgrade is the link, anything else is HTTP. The server speaks plain
HTTP; for the internet a TLS reverse proxy goes in front.

## Errors

One error type, `port::Error`, for every adapter and probe: an agent CLI that
would not start and a model server that answered 503 are the same event to the
calling agent. Each error says where it came from, whether a retry could work,
and is written to be read by the model that will see it.

## Safety boundaries

- **No sandbox of Ensemble's own.** Agents use their CLIs' own tools with your
  permissions; `tools` and `permission_mode` (or Codex's sandbox) limit them.
  `builtin`'s file tools cannot leave the working folder; its `shell` can.
  `node team check` and `node doctor` warn about agents that can change files in place.
- **What runs is what the config says.** A task cannot bring a command; gates
  are defined in `config.yaml`. A remote helper team is pinned to a commit and
  trusted per commit.
- **Secrets** are never in a file Ensemble reads as config, and are passed to a
  CLI only as the provider key of the backend it runs on.
- **Subscriptions stay with their owner.** Claude Code runs only through its
  official CLI on its owner's login; a shared node never lends one.
- **Shared text is data.** The distilling model has no tools; recalled notes
  are marked as reference data.
- **The hub keeps hashes.** Passwords as PBKDF2, tokens as SHA-256. A node's
  token file and socket folder are its user's alone.

# Part two: decisions and direction

## Names

- **Helper team**: the agents a user works with, described in one `config.yaml`.
  It is flat: a list of professional agents, such as a
  coder, a reviewer, a PCB designer or an RF engineer, with no hierarchy. A
  node holds any number of them in its teams directory and runs them all, its
  agents named `<team>/<agent>`: teams the person makes, links or pulls from a
  git repository like an image, and teams an agent writes at the person's
  request, which run once the person approves them. (Decided 2026-11-17.)
  The description is strict YAML, never natural language.
- **Hub**: the services: identities, task tracking and the inbox, knowledge
  vaults and the pipeline, integrations (Telegram and others), routing work
  between nodes, model access. The word "organization" is not used for it,
  to keep it apart from a helper team.
- **Node**: a worker: a process that gives computing resources to its owner
  and the hub's users. A computer runs one node. (Decided 2026-11-17; a
  computer could run several before.) A node is
  **private**, working only for its owner, or **shared**, lending its
  machine's power to other users of the hub. It holds a helper team and is the
  MCP server for its owner's agent sessions from any folder: `ensemble mcp`
  is a bridge to it. It is tied to no project. (Decided 2026-11-16; a node
  was one person's machine before.)
- **Interface**: what a person talks to the system through: their own agent CLI
  (Claude Code, Codex, OpenCode) with Ensemble's MCP tools, the hub's tracker,
  or later the hub's integrations, chat API and web UI. There is no personal
  agent and no manager. (Decided 2026-11-16; see "No personal agent".)

## What it is

An agentic development system: a hub of services and nodes of computing
power, with work spread across them; MCP is one way in. (Decided 2026-11-17:
Ensemble is described as this, not as an MCP server.) It serves two kinds of
user: a single developer who wants agents to work on
tasks and save tokens by sending cheap work to cheap models, and an organization
whose people and agents share knowledge. The same binary serves both. Features
beyond the core are modules switched on by their own config block; a block that
is absent means the module is not used, which is the one kind of optional
setting the config rules allow.

| Level | What runs | Who it is for |
|---|---|---|
| 0. Node alone | Helper teams, agent runs, worktrees, gates, tasks | Anyone who wants agents to work on tasks |
| 1. Node + a hub of one's own | A hub on the person's own computer: a knowledge vault fed by explicit shares and recalled in later runs, a tracker | Users who want their agents to remember |
| 2. Nodes + a shared hub | Sharing, people, always-on work, a tracker shared by a team, shared nodes; later connectors and a central model server | Teams and organizations |

Each level works without the next. Knowledge always belongs to a hub: a node
with no hub has none, and a single user who wants it runs a hub of their own
on their computer (level 1). A hub is always its own process, with no team;
an easy way to run a node and a hub together is to come. (Decided
2026-11-17.)

## Hub and node

**Hub** (`ensemble hub`): one per group of people, always on; it provides the
services. It owns identities and tokens, the front door for clients (built: the
REST API and `/nodes/v1/`; to come: an OpenAI-compatible chat API, an MCP
endpoint, a web UI), task tracking (the inbox and tracker adapters), the
knowledge pipeline and vaults, integrations (Telegram and others, to come),
which node runs what, and model access (to come: hub API keys, a central LLM
server, quotas per user, a usage log).

**Node** (`ensemble node`): a worker. It runs its owner's helper team, wires the
local agent CLIs (MCP, session hooks), and connects outbound to the hub over
WebSockets, so it needs no open port. A computer runs one node, private or
shared. (A WebSocket was chosen on
2026-11-11, over plain HTTP with long-polling.)

**The hub is a service, not a computational resource.** (Decided 2026-11-17;
before, the hub was a role that could run inside a node.) It has no helper
team and runs no agents: it keeps knowledge, hands out tasks, keeps accounts,
and connects nodes, which do the work. It is always `ensemble hub serve`, with
its files in `~/.config/ensemble/hub/`; one person runs it on their own
computer, a team on a server. A node reaches it only over the link: its
sessions' knowledge (`RemoteMemory`), its folders' tasks (`RemoteBoard`), and what it
offers (`Worker`). A developer who wants access from elsewhere moves the hub's
folder to a server.

A node names one hub, as `hub:` in its `node.yaml` (decided 2026-11-17).
A person may still belong to several hubs, a home hub with their own knowledge
and one workplace hub per organization they work for, each with its own
identity, token and policy, and nothing passes between them except what the
export rules below allow (decided 2026-11-10); how one node serves several hubs
is open (see the open questions).

A hub server may face the public internet behind TLS, or run on a local network
or behind a VPN; nothing in Ensemble assumes either. (Decided 2026-11-10.)

### Private and shared nodes

(Decided 2026-11-16; to be replaced by `shares`, decided 2026-11-18: see
"What a node shares" under "Operations and interfaces".) The hub connects nodes; a node is there to provide
computing power. A **private** node works only for its owner. A **shared** node,
typically on a powerful machine, also works for other users of the hub. Its
config says what it offers, one or both:

- **models**: the models its machine serves (its GPUs), reached through the hub
  as a backend, so other people's agents run on their own nodes and use it;
- **runs**: agent runs for other users' tasks, on the shared machine.

Who may use shared nodes is decided by hub roles, through a permission, like
every other access in the hub. The helper team stays defined on its owner's
node; a shared node lends power, not its owner's team.

Limits that follow from the rules below: a shared node runs other users' work
only on API keys or on models its machine serves, never on its owner's CLI
subscription. A run on a shared node needs the code it works on, so the node's
owner can see that code.

How it was built (chosen by the assistant on 2026-11-16, when the owner asked
for it all to be built; open to change), as part one describes under "Nodes":
one `node.yaml` per node; `compute` and `host` as the permissions; a run as a
snapshot of the files git tracks, worked on by the node owner's agent, with
the changes coming back to the requester, who keeps gates, worktrees and
commits; a shared node offering only agents on `llm_backends`; models through
`/nodes/v1/`, from the least busy node, the user's own first.

### Rules that hold everywhere

- An agent runs on its owner's node, or on a shared node its owner may use;
  never on the hub, which runs no agents (decided 2026-11-17). Nodes share
  only what a shared node offers through the hub (models, runs); private
  nodes share nothing, and no node shares tools, files or subscriptions. (Sharing a node's model was dropped on
  2026-11-10 and brought back, through shared nodes, on 2026-11-16.)
- An organization may run a central LLM server on its own GPUs. It is an
  OpenAI-compatible backend like any other: a standalone node lists it in
  `llm_backends`; through the hub it will also get quotas per user and a usage
  log.
- Delegated scope: an agent acts with its owner's access or less, never more.
- Static config, dynamic state. Agent definitions, policy, gates and backend
  definitions live in reviewed config. Membership, presence and the model bound
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
| Knowing the person: decisions, setup, preferences | The vault. Recall serves the person's CLI, and the helper agents whose `recall` asks for notes. |
| Identity, tokens, being accountable | The person's hub account. Issues are assigned to the person. |
| Talking to the helper team | The person's own CLI through Ensemble's MCP tools; later the hub's chat API, whose `model` names a helper agent, and the web UI. |
| Turning an issue into work | The inbox: a `<label>:<agent>` label names the agent; without one the task waits in the backlog until someone assigns it. |
| Planning and splitting work | A helper agent the person puts in their team for it, which hands parts on with `HAND OFF:`. A team design choice, not a concept of the system. |
| Being reachable while the person's own node is off | A shared node the person may use runs the work; otherwise it waits. |

So the system has three parts: the **hub** (people, knowledge, the inbox, and
which node runs what), **nodes** (workers, private or shared), and each
person's **helper team**, defined on their node. The person's CLI is the
interface to all three, not one of them.

## Helper team

The user decides how their helper team is structured. Each agent has a CLI, a
prompt, tools and gates tuned for its profession. What changes from run to run
is the model behind an agent: each agent lists an explicit fallback order of
backends (the user's own model, the central LLM server, a hub API key), and a
resolver binds the agent to the first one that is reachable, compatible with
the CLI and within budget. Nothing in that order is defaulted. An agent says
what happens when nothing fits: fail, or wait in the queue. Every run records
the model that actually ran.

The light adapter that sends one chat request straight to a backend, with no CLI
and no tools, is called `direct`. The chat
client it uses belongs to the backends module, so the knowledge pipeline and the
hub's chat API can share it.

An agent loop of Ensemble's own is back, as the `builtin` adapter: harness-rs's
`AgentLoop`, its file and shell tools, and its model clients, on the user's LLM
backends, so a helper team (or a shared node) needs no agent CLI installed. It
is pinned to one exact version because the library is young. (Decided
2026-11-16.)

Ensemble is async, on tokio: agent runs (CLI processes included), the task
queue, the MCP session, the node, the link on both ends, and the knowledge a
session asks for. harness-rs is async, and so is a node that runs many agents
and serves models at once. (Decided 2026-11-16; Ensemble was synchronous on
purpose until then.) What stays synchronous is the knowledge vault and its
distilling, and the tracker inboxes: their loops run on threads of their own.
Short HTTP requests use ureq; what streams for long (a node's model calls)
uses reqwest. The trade-off of running a session's calls in one task is in
part one, under "How it runs at once".

An organization may later limit helper teams through policy, not structure:
which backends its data may be sent to, gates its repositories require, budgets
per user, forbidden tools and permission modes. For now there are no such
limits. (Decided 2026-11-10.)

## Tasks

Two kinds of work (decided 2026-11-17):

- An **issue** is a person's: high-level, such as "a panorama video app with
  rotating cameras", in a tracker (Jira, GitLab, markdown files), assigned to
  the person, who stays accountable. Ensemble is never the source of truth
  for issues.
- A **task** is an agent's: one agent, one goal, one check, such as "the
  camera calibration module in `calib/`; `cargo test` passes". Tasks are kept
  on the hub, per account and project, so that a task outlives the node that
  runs it: nodes **lease** them (see "A task" in part one), and when a node
  stops or dies another of the person's nodes with the agent takes the task
  up. Without a hub there are no tasks, only background delegations, as there
  is no knowledge without one.

The helper team never calls a tracker itself. Each tracker implements one
trait, `IssueTracker`: the issues assigned to this user that carry the
signal, and statuses and comments written back. The hub's inbox makes such an
issue a task of the account it is assigned to and reports the task back on
the issue. Behind it, tracker adapters do the tracker work: webhooks where the
tracker has them, polling where it does not, account mapping, the signal,
deduplication. The inbox runs on the hub, with one set of credentials, a
reachable address for webhooks, and on while nodes are off. (Built: polling a
folder of markdown issues.)

An issue carries an explicit signal that agents may take it: a label, such as
`ensemble`, which every tracker has and which needs no extra account.
(Decided 2026-11-10.) The flow:

1. The inbox sees an issue assigned to me with the signal and makes it a task
   for the helper agent its label names, or puts it in my backlog.
2. The agent gets it with the vault's notes about it, if its `recall` asks.
3. If it is unclear, the agent asks; the question goes to a tracker comment and
   my answer comes back the same way.
4. It works, checks gates, and may hand parts on to other agents of my team.
5. The result is reported in the tracker; I remain the assignee.
6. What the agent reported goes to the knowledge pipeline, as a share of the
   account the task is for.

Planning is a conversation, not an object (decided 2026-11-18). The person
talks the work through with their own agent, with an issue of theirs or
without one, and the agent then delegates parts directly to the helper team,
makes tasks, both, or neither, as the work needs. Ensemble keeps no plan and
offers no planning tool: a task's `after` is all the order it needs.

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
- Recordings, started by a person, transcribed on the node. (Not built.)

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
names; a single user's own hub uses that user's own backends or API key.
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
others see. (Decided 2026-11-18.) Not built: a prompt goes only to a backend
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
access tokens for nodes and scripts, each named, with an optional expiry and
optional scopes that narrow the account's role. Only a session may manage
tokens or the password; failed logins lock a name for a while. (Decided
2026-11-11.) CLIs and nodes enroll with `ensemble node login <hub> --user NAME`,
which makes an access token for that computer (the OAuth device flow and
OIDC later). A node keeps its token in a file only its user can read,
under `~/.config/ensemble`, like an SSH key; the OS keyring may come later as
an option. The hub stores tokens hashed. (Decided 2026-11-11.) Nodes,
connectors and agents are identities too, and an agent acts on behalf of a
person.

There are no teams of people in the hub, only roles. Ensemble defines a fixed
set of permissions (such as managing users, managing the hub's backends,
sharing, recalling, receiving tasks), and each hub's config names its own roles
as sets of them, so an organization decides what roles it has. (Decided
2026-11-10.) Built so far: `share`, `recall`, `tasks`, `admin`, which manages
users, `compute` and `host`, for nodes.

People join a hub server by signing up through its REST API (`/api/v1/`, on
the hub's own port, for a web UI and scripts) with a name and a password; the
account is pending, and logging in is refused, until an admin approves it with
a role. A hub may instead be `closed`, where only an admin adds users. The
first admin is made on the hub's computer with the CLI. (Decided 2026-11-11.)

## Clients

| Way | What happens |
|---|---|
| Local CLIs (Claude Code, Codex, OpenCode) | Their own login and model. Ensemble's MCP tools: the helper team, tasks, `share`, `recall`. Notes through session hooks. (Built.) |
| OpenAI-compatible API | Any chat UI, IDE plugin or SDK, with an API key. The token is the key; `model` names the agent. (Built for a node's models, at `/nodes/v1/`; not yet for agents.) |
| Remote MCP | Desktop and web agents, with OAuth. |
| Web UI | Chat, knowledge, review queue, tasks, roster, node status, admin. |
| Obsidian over Git | Direct read of the vaults; a correction is shared like any knowledge. (Works today.) |
| Task tracker | Assign a task to a person and add the delegation signal. (Built for markdown issues.) |
| `ensemble` CLI | `share`, `recall`, `tasks`, `nodes`, and later `ask`, `record start`. |
| Integrations (Telegram and others) | Hub services that bring messages in and send results out. (Not built.) |

## Operations and interfaces

(Decided 2026-11-18; not built yet: see the build order.) People do not edit
Ensemble's files: Ensemble writes them, and every setting has a way to be set
without opening one. Many people will use a web UI, but none is built yet; what
is built now is the ground any interface stands on, so that adding one, a CLI,
a REST API, a web UI or a desktop app, is writing an adapter, never
re-implementing a feature. Some administration may stay with the CLI.

### Operations

Everything a person can do to a hub or a node is an **operation**, defined
once, in the core:

- a name, such as `models.add`, `people.invite`, `computers.join-code`,
  `settings.set`;
- its input and output, as Rust types with JSON Schemas made from them;
- who may run it: a permission of the hub's roles, or the node's owner;
- its secret fields, written and never read back (keys go to `.env`);
- errors as data, `{code, field, message, hint}`, so an interface shows each
  next to what it is about;
- for long work (a rebuild, testing a model), the events it goes through.

There are two kinds. **Goal operations** do what a person means: add a model,
which tests that it answers; invite someone; let a computer join; add a team
from a template; rebuild the knowledge. **Settings operations**
(`settings.describe`, `settings.set`) reach every setting, from the JSON
Schemas the files already have and the checks `config set` makes: a change
is loaded before it is written, comments stay, and what Nix declares comes
back read-only with why.

An operation runs **where the state it changes lives**: a running hub or node
applies it in memory, at once, then writes its file, so nothing needs a
restart and no two processes write the same file. When the hub or the node is
not running, the same operation runs in the process that asked, on the files.
The running process is asked over its local socket, which only its OS user may
open: the node's exists, and the hub gets one.

Agents do not run operations: no configuration through MCP tools.

### Interfaces

An **interface** is an adapter over the operations. It knows how to show them
and how to reach them, and nothing about what they do; one written once serves
every operation, those to come too.

- **The CLI** is made from the operations: each is a command, its input
  fields its options, with a short alias and positional arguments where the
  operation gives them. It reaches a running hub or node over its socket.
- **The REST API** is one route per operation, `POST /api/v1/ops/<name>`,
  with an OpenAPI document made from the same schemas, signed in with the
  hub's sessions and tokens. It is RPC-shaped rather than one resource per
  route, so it needs no code per operation and matches the CLI one to one.
  Signing up is part of it: without it, an admin adds people from the CLI.
- A web UI, a desktop app, any other: a client of the REST API, or of the
  socket on the same computer.

Each is turned on in the settings, never by building another binary:

```yaml
# hub.yaml
interfaces:
  cli: {}                                # the socket `ensemble hub …` uses
  rest_api: { listen: 127.0.0.1:7710 }   # its own port; left out: no REST API

# node.yaml
interfaces:
  cli: {}
  rest_api: { listen: 127.0.0.1:7701 }   # left out: no REST API
```

`hub init` and `node setup` write `cli: {}`. A file that turns on no
interface is refused: a hub or node nobody can manage is a mistake. Without
`cli`, the running process opens no socket, and `ensemble` refuses to change
it while it runs rather than change its files behind it. The REST API has a
port of its own on the hub too, apart from the link's, because it is
optional; a node has none of its own to share, as it only dials out.

The node's own operations reach the hub as one more adapter, over the link,
for the node's own account only: a person manages all their computers from
one place, and an admin never reconfigures someone else's.

### The link is the only way between node and hub

A node and its hub speak over the WebSocket and nothing else:

- **Signing in and joining** happen on it: a connection without a token may
  only ask `login` (a name and a password, with the lockout) or `join` (a code
  an admin made with `computers.join-code`), and gets a token back. A node
  makes no HTTP request to the hub, so a hub without its REST API serves
  every node.
- **What nodes share** goes through it both ways: the node that serves dials
  out, so a server no one can reach may share; the node that uses it relays
  its agents' calls over its own link. `/nodes/v1/` stays, in the REST API,
  only for clients that are not nodes.

### What a node shares

(Replaces `access` and `offers`; see "Private and shared nodes".) There is no
kind of node called shared: a node shares what its `shares` names, and a node
that names nothing shares nothing. Each entry says who may use it, in the
words knowledge uses:

```yaml
# node.yaml
shares:
  agents:
    web/coder: hub           # everyone on the hub whose role has `compute`
    web/reviewer: private    # only its owner's other computers
  llm_backends:
    web/local: hub           # <team>/<backend>: the models it serves
```

An agent that runs on an agent CLI's own login may only be `private`: no one's
subscription works for someone else. Another node's team names what is shared
by the node that shares it, an agent as the `node` adapter does now, and a
backend the same way:

```yaml
# a team's config.yaml on another node
llm_backends:
  kims-gpu:
    type: node
    backend: web/local
    nodes: [kim/gpu-box]     # tried in order
```

The node serves that backend to its agent CLIs on a local address and relays
each call over its link; no config names a port or the hub.

## Modules

Building blocks, not processes. A module is on when its config block is present;
each exposes a trait and may run in either process. The last column says which
crate holds it today.

| Module | Does | Needs | Built in |
|---|---|---|---|
| core | config, error vocabulary, state directory | — | `ensemble-core`, `ensemble-config` |
| identity | users, tokens, scopes; one implicit user standalone | core | `ensemble-link` (`users.rs`) |
| backends | endpoints, the chat client, format translation, health, quotas | core | `ensemble-backends` (no quotas yet) |
| agents | CLI adapters, `direct`, `builtin`, workspace, worktrees, gates | backends | `ensemble-agents` |
| team | the helper team, fallback resolver | agents, identity | `ensemble-agents` (`router.rs`), `ensemble-backends` (`resolve.rs`) |
| tasks | tasks kept on the hub and leased, the inbox, `IssueTracker` adapters, hand-off | identity, team | `ensemble-tasks`, `ensemble-inbox` |
| vault | vault repositories, schema, index, visibility | identity | `ensemble-knowledge` (no visibility yet) |
| pipeline | ingest, filter, extract, integrate, review | vault, backends | `ensemble-knowledge` |
| recall | context packages | vault | `ensemble-knowledge`, `src/hooks.rs` |
| capture | recorder, quick capture, privacy gate | pipeline | not built |
| connectors | chats, mail, CRM, tracker events, each its own | pipeline or tasks | not built |
| wiring | MCP, hooks in the user's CLIs | — | `ensemble-mcp`, `src/setup.rs`, `src/hooks.rs` |
| operations | every action on a hub or a node, once, for every interface | identity, config | `ensemble-ops`; the hub's in `ensemble-link` (`accounts.rs`), the node's in `src/cmd/node.rs` (accounts, tokens, settings, signing in so far) |
| interfaces | the CLI, the REST API, and later a web UI or a desktop app, as adapters over the operations | operations | the CLI made from the operations (`src/ops_cli.rs`), the rest by hand in `src/cmd/`; the REST routes in `ensemble-link` (`api.rs`) |
| api | chat API, MCP endpoint, REST | identity | `ensemble-link` (REST only) |
| web | the UI | api | not built |
| link | hub-to-node connection | identity | `ensemble-link`, `ensemble-node` |

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
   markdown tracker; `ensemble hub serve` with accounts, sessions, scoped
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
    vault): a hub is always `ensemble hub serve`, with no team, and a node
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
    bridges.)
17. Signing in and joining over the link, with join codes; no HTTP from a
    node to the hub.
18. The REST API as an interface: one route per operation, OpenAPI, its own
    port, sign-up in it, `/nodes/v1/` in it for clients that are not nodes.
19. `shares`, in place of `access` and `offers`; the `node` backend, relayed
    over the link through a local endpoint.
20. Goal operations: models, people, computers, teams from templates,
    knowledge.
21. A node's operations over the link, for the node's own account.

What remains besides is the "Not yet" of steps 4 and 5, and the modules
marked not built above. A web UI and other interfaces come after step 18, as
adapters.

## Open questions

1. Export from workplace to home: who approves.
2. Recording from a browser without sending audio to the hub.
3. Telegram, as a hub integration (decided 2026-11-17): what it does first:
   talk to the helper team, feed the knowledge vault, or report on tasks.
4. Shared nodes, as built (see "Private and shared nodes"): whether the
   owner and the user should consent to each run, beyond roles; whether the
   hub should pick the least busy node for runs too. (A node takes its
   owner's tasks from the inbox: decided 2026-11-16.)
5. Chat from anywhere: how the hub's chat API picks the node that runs the
   helper agent a chat names, and what a chat gets when no such node is
   connected.
6. Several hubs for one person (a home hub and a workplace hub): a node now
   names one hub; whether it may name several, and which one a session of
   which folder uses.
