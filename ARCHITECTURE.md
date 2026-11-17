# Architecture

This document has two parts. **Part one, "How it works"**, describes the
system as it is built: the processes, the crates, where things are kept, how
work flows through them and how they run at once. **Part two, "Decisions and
direction"**, records what was decided with the owner and where Ensemble is
going; its build order says what is done and what is not. The questions at the
end are open; everything else is decided unless marked otherwise.

`README.md` is the user's guide: every setting, command and tool. This document
is the map behind it. `PLAN.md` is the record of how the first version (level 0
below) was built, phase by phase.

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
  come). It runs as `ensemble hub serve` on a server for a team, or embedded
  in one person's own node or session.
- **The helper team**: agents described in one `config.yaml`, each run by an
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
  ensemble mcp ───────────────────────────────┐  no --team: a bridge
        │  --team: the session and the        │  Unix socket: one hello
        │  folder's worker, in this process   │  line, then MCP both ways
        │                                     ▼
        │                         ensemble node serve
        │                         sessions from any folder,
        │                         a worker per folder it served
        ▼                                     │
  the helper team (config.yaml) ◄─────────────┘
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
  RemoteMemory (a session's knowledge) ══╗   accounts, sessions, tokens (users.json)
  NodeInbox (your tasks)               ══╬══ the organization vault and its distiller
  Worker (runs and models it offers)   ══╝   the tracker relay (deliveries.json)
  `node` adapter (a run elsewhere)     ══    the connected nodes, runs, /nodes/v1/
                                             REST /api/v1/
     ══ one WebSocket each, opened by the node, signed in with a bearer token
```

## Processes

Every process is the same binary. What runs in each:

| command | what it runs | ends |
| --- | --- | --- |
| `ensemble mcp --team <dir>` | one MCP session on stdio and the worker of its folder (the task queue), and with an embedded hub (`--hub <dir>`) its distiller and inbox | when the agent CLI closes stdin; the running tasks stop and are queued again |
| `ensemble mcp` (no `--team`) | a bridge: it connects to the node's socket, sends `{"dir": "<folder>"}`, and copies bytes between stdio and the socket | when the node closes the session |
| `ensemble node serve` | the node: a socket for sessions, a worker per folder, with a hub server a link that offers runs and models (`Worker`) and one that takes your tasks (`NodeInbox`), with an embedded hub its distiller and inbox | on SIGINT or SIGTERM |
| `ensemble serve --team <dir>` | a folder's worker with no session | on a signal, or once idle with `--exit-when-idle` |
| `ensemble hub serve --hub <dir>` | the hub server: one port for the link, the REST API and `/nodes/v1/`; the organization vault's distiller; the tracker relay | on SIGINT or SIGTERM |
| everything else | one command: read the config and the state, do one thing, print | at once |

A folder's tasks are run by one worker at a time: whoever holds the lock file
(`worker.lock`) in the folder's state directory. A session that finds the lock
taken reads and writes tasks but starts none. A lock left by a dead process is
taken over. The same lock keeps one node per `node.yaml` name, one distiller
per vault and one inbox per hub.

## Crates

A Cargo workspace. Each crate depends only on crates above it in this table;
`tests/workspace.rs` checks that.

| crate | what it is | files |
| --- | --- | --- |
| `ensemble-core` | what everything shares | the error vocabulary (`port.rs`); the per-user state, cache and config directories (`state.rs`); the working folder, a `repo` inside it and what a run changed (`workspace.rs`); the record of runs behind `ensemble stats` (`records.rs`); the one-worker lock (`lock.rs`); dates in UTC (`time.rs`); the files a run on a node carries there and back (`snapshot.rs`); the folders a node served, by project (`folders.rs`); the async runtime and its bridges to code that blocks (`runtime.rs`) |
| `ensemble-config` | the three files a person writes | `config.yaml`: the document as written (`raw.rs`), the environment and `--env-file` (`env.rs`), validation (`validate.rs`), the resolved types (`lib.rs`), versions 1 and 2 to 3 (`migrate.rs`), adding and removing an agent as text, comments kept (`edit.rs`); `hub.yaml` (`hub.rs`); `node.yaml` (`node.rs`) |
| `ensemble-backends` | talking to model servers | one chat request in the server's own format (`chat.rs`); which models a server serves (`models.rs`); the first usable model of a `try` list (`resolve.rs`); a stand-in server for tests (`testing.rs`) |
| `ensemble-knowledge` | the hub's knowledge | the vault, a git repository (`vault.rs`); the secret check (`secrets.rs`); shares into notes (`pipeline.rs`); BM25 search (`search.rs`); `Memory`, the `share` and `recall` tools, the background distiller, and the `KnowledgeAccess` trait a session uses (`lib.rs`) |
| `ensemble-link` | the hub server and the link to it | accounts and hashed tokens (`users.rs`); the REST API (`api.rs`) over a small HTTP reader (`http.rs`); the frames of the link (`protocol.rs`); the hub's side, a task per connection (`server.rs`); connected nodes, runs and model calls, `/nodes/v1/` (`nodes.rs`); the node's side, `Connection` and `RemoteMemory` (`client.rs`); the token file (`tokens.rs`) |
| `ensemble-agents` | running one agent once | the adapters (`external/`): `claude_code.rs`, `opencode.rs`, `codex.rs`, `builtin.rs`, `direct.rs`, `node.rs`, the subprocess runner (`process.rs`) and gates (`gate.rs`); a git worktree per task (`worktree.rs`); the router: agents to adapters, models, records, `health` (`router.rs`) |
| `ensemble-tasks` | work handed to the team | tasks as files (`tasks.rs`); the `Tracker` trait and `LocalMarkdown` (`tracker.rs`); the queue that runs tasks and background delegations within slots and budgets (`queue.rs`); the MCP tools (`delegation.rs`); review and merge of a worktree task (`review.rs`) |
| `ensemble-inbox` | issues from a tracker into tasks | the `IssueTracker` trait (`tracker.rs`) and markdown issues (`local_markdown.rs`); an issue made a task and what the task reports (`work.rs`); the embedded hub's inbox (`bridge.rs`); a hub server's (`relay.rs`); a member's computer's part in it (`node.rs`) |
| `ensemble-node` | a node's link to a hub | `Worker`: runs and model calls for others (`lib.rs`) |
| `ensemble-mcp` | MCP over stdio | the session loop (`lib.rs`), the tool catalogue (`protocol.rs`), notifications (`notify.rs`), `ensemble serve` (`serve.rs`) |
| `ensemble` (the root) | the binary | the commands (`src/main.rs`); the node's sessions and the bridge (`src/daemon.rs`); `setup`, `connect`, `doctor` (`src/setup.rs`); session hooks (`src/hooks.rs`); remote helper teams (`src/remote.rs`) |

Two rules keep the layers apart. The MCP session knows the tools and the
router, not which CLIs exist. The router knows agents; only the adapters know
how a CLI is started and read.

## Configuration and state

What a person writes is **configuration**, reviewed and never changed by
Ensemble except on request (`add-agent`, `migrate`):

| file | where | holds |
| --- | --- | --- |
| `config.yaml` | the helper team's directory (`--team`), often a git repository; or a remote one, `<url>#<commit>` | `llm_backends`, `limits`, `gates`, `tracker`, `agents` |
| `hub.yaml` | the hub's directory (`--hub`) | `owner`, the hub's own `llm_backends`, `knowledge`, `tasks`, and for a server `server` and `roles` |
| `node.yaml` | `~/.config/ensemble/node.yaml`, or `--node <file>` | `name`, `access`, `team`, `offers` |

No setting has a default: every one is written, and a missing one is refused by
name. A block that is absent means its module is off. No file holds a secret:
keys are named by the variable that holds them, read from the environment or
`--env-file`. Each file has a JSON Schema at the repository root, checked
against the code by the tests.

What Ensemble learns while it runs is **state**, per user, never in a project
or a config directory:

| path | holds |
| --- | --- |
| `~/.local/state/ensemble/workdirs/<folder>-<hash>/` | a working folder's `tasks/`, `runs/`, `worktrees/`, `state.json` and `worker.lock` |
| `~/.local/state/ensemble/orgs/<team>-<hash>/` | a helper team's `usage.json` (budgets) and `records.jsonl` (`ensemble stats`), shared by every folder |
| `~/.local/state/ensemble/hubs/<hub>-<hash>/` | an embedded inbox's `inbox.json`; a hub server's `users.json` and `deliveries.json` |
| `~/.local/state/ensemble/nodes/<name>/` | a node's lock, `folders.json` (the folder last used for each project), `runs/` for others, `inbox/node.json` |
| `$XDG_RUNTIME_DIR/ensemble/<hash>.sock` | a node's socket, in a folder only its user may enter (`/tmp/ensemble-<user>/` without `XDG_RUNTIME_DIR`) |
| `~/.config/ensemble/hubs.json` | hub tokens, one per URL, mode 0600 |
| `~/.cache/ensemble/` | clones of remote helper teams |

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
- **Loops that are not async** (a vault's distiller, a tracker's inbox) run on
  threads of their own, polling every few seconds.
- **Stopping**: SIGINT and SIGTERM set one flag. Every loop watches it, the
  queues close, running tasks are stopped (and, having no result, run again
  next time), and the node waits up to ten seconds for them.

What a node keeps for a folder, and what a hub server's connections borrow, is
made once and lives as long as the process, so it is leaked on purpose
(`Box::leak`) and borrowed as `'static`.

## A delegation

Your agent calls `delegate` with an agent's name and a task:

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

A task is a folder in the state directory, `tasks/<id>/`, with a `task.md`
written once. It comes from `submit`, `ensemble submit`, a person's editor, or
an inbox. On each scan the queue:

1. skips tasks with a `result.md` (over), with no assignee (backlog), waiting
   for an answer, or `after` tasks not yet `done`;
2. waits while the agent is over budget, its slots are busy, or (with
   `when_none: wait`) no model of its `try` list can be used;
3. for `isolation: worktree`, makes or reuses a git worktree on the branch
   `ensemble/<id>`;
4. recalls notes once, if the agent has `recall`;
5. runs the agent as a delegation, then the task's **gate** if it names one,
   running the agent again with the gate's output until it passes or
   `attempts` are used;
6. reads the end of the reply: `NEEDS INPUT: <question>` adds the question to
   `questions.md` and waits for `answer`; `HAND OFF: <agent> <what remains>`
   reassigns the task with a note in `handoffs.md`;
7. otherwise writes `result.md` (`done`, `failed` or `cancelled`), and in a
   worktree commits everything on the task's branch. Nothing is merged: that
   is `ensemble merge`, which only a person runs.

What a run does as it goes (its activity lines, its state) is in `runs/<id>/`,
which is how `status` shows what an agent is doing. A task stopped by the end
of a session or a node has no result, so it runs again next time.

## Knowledge

```text
  share ─► secret check ─► vault/shares/  ─► distiller (every 5 s) ─► vault/notes/
                            kept as it came    extract → integrate      one commit per share
                                               └► review/ when unsure
  recall ◄─ BM25 over vault/notes/ ◄──────────────────────────────────────┘
```

- **The vault** is a git repository: `shares/` as they arrived, never changed;
  `notes/`, one per topic, which only the pipeline writes; `review/`, what the
  pipeline would not write; `log.jsonl`. Notes are a product of the shares, so
  `ensemble knowledge rebuild` makes them again from scratch.
- **The pipeline** distills with the hub's own `knowledge.distill` models, by
  one chat request per step, with no tools. Shared text goes between markers
  with a nonce, an answer must be the JSON asked for, and anything else goes
  to `review/`.
- **A session reaches knowledge through `KnowledgeAccess`**, an async trait
  with two implementations: `Memory`, the embedded vault (its file and git work
  through `runtime::blocking`), and `RemoteMemory`, a hub server's vault over
  the link, opened on first use and opened again once when it breaks. The
  session offers the knowledge tools the hub allows, and the queue uses the
  same trait to give notes to agents with `recall`.
- **Session hooks** (`ensemble hook`) put notes into Claude Code and Codex
  sessions on `SessionStart` and `UserPromptSubmit`, each note once per
  session, through the same trait.
- **On a hub server** a share is kept under the signed-in account's name, and
  the server runs the distiller for the organization vault.

## The task inbox

Issues live in a tracker and are assigned to people; the inbox makes the ones
with the hub's label into tasks and writes what happens back on the issue. The
helper team never calls a tracker. A tracker is the `IssueTracker` trait; a
folder of markdown issues is the one implementation so far.

- **Embedded** (`hub.yaml` with `tasks.user` and `tasks.projects`): `Inbox`
  polls the tracker every `poll_seconds` on a thread of the session or node. An
  open issue assigned to you with the label becomes a task in the folder its
  project maps to, for the agent a `<label>:<agent>` label names or in the
  backlog. A question goes out as a comment and your next comment comes back as
  the answer; the result is posted once and the issue goes to `review` or
  `blocked`. What it did is kept in `inbox.json`.
- **On a hub server** (`ServerInbox`, `relay.rs`): an issue becomes a
  **delivery** for the account whose tracker name is its assignee, kept in
  `deliveries.json`. The server pushes it over the link, as a `task` event, to
  every node of that account that subscribed. On the member's computer,
  `NodeInbox` makes it a task in the folder of the project's name that a
  session of the node last ran in, and answers `task_accept` (or
  `task_refuse`, when it has no such folder). Every two seconds it sends each
  task's new question or result (`task_update`), and passes `answer` events to
  their tasks.

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
the hub's knowledge in each folder it serves, one for its tasks, one for what
it offers, and one per run it sends elsewhere.

| for | the node asks | the hub sends |
| --- | --- | --- |
| knowledge | `hello` (who am I, which permissions), `share`, `recall` | |
| tasks | `subscribe`, `task_accept`, `task_refuse`, `task_update`, `answer_taken` | `task` and `answer` events, each once per connection and again after a reconnect |
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
  `check` and `doctor` warn about agents that can change files in place.
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

- **Helper team**: the agents a user works with, described in one `config.yaml`
  (config version 3). It is flat: a list of professional agents, such as a
  coder, a reviewer, a PCB designer or an RF engineer, with no teams and no
  hierarchy. A user connects exactly one helper team, from a local directory or
  a git repository; a company may share one, or each user may keep their own.
  The description is strict YAML, never natural language. This replaced the
  version 2 "organization of teams".
- **Hub**: the services: identities, task tracking and the inbox, knowledge
  vaults and the pipeline, integrations (Telegram and others), routing work
  between nodes, model access. The word "organization" is not used for it,
  to keep it apart from the old agent config.
- **Node**: a worker: a process that gives computing resources to its owner
  and the hub's users. A computer may run any number of nodes. A node is
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
| 0. Node alone | Helper team, agent runs, worktrees, gates, tasks from a tracker reached with the user's own token | Anyone who wants agents to work on tasks |
| 1. Node + memory | The hub role embedded in the node: a knowledge vault, fed by explicit shares, recalled in later runs | Users who want their agents to remember |
| 2. Node + hub | Sharing, people, always-on work, a tracker shared by a team, shared nodes; later connectors and a central model server | Teams and organizations |

Each level works without the next. Knowledge always belongs to a hub: a node
with no hub role has none, and a single user who wants it turns the hub role on
in their own node (level 1). (Decided 2026-11-10.)

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
WebSockets, so it needs no open port. A computer may run several nodes, such as
one per hub, or a private one beside a shared one. (A WebSocket was chosen on
2026-11-11, over plain HTTP with long-polling.)

**The hub is a role, not a place.** Its modules run inside the node process for a
single developer, or in `ensemble hub serve` on a server for a team. The node
calls the same traits either way, with the local module or a client of the
link behind each: `KnowledgeAccess` (`Memory`, or `RemoteMemory` over the
link), and the inbox (`Inbox` embedded; `ServerInbox` on a server, with
`NodeInbox` on the member's computer). Identity and model access will follow
the same pattern. A developer who wants access from elsewhere moves the hub's
state and vault repository to a server.

A node may connect to several hubs: a home hub with the person's own knowledge,
and one workplace hub per organization they work for. Each has its own identity,
token and policy, and nothing passes between them except what the export rules
below allow. One agent session talks to one hub: the hub is set per project when
Ensemble is registered with the CLI, so work projects use the workplace hub and
personal ones the home hub, and a share always goes to the session's hub.
(Decided 2026-11-10.)

A hub server may face the public internet behind TLS, or run on a local network
or behind a VPN; nothing in Ensemble assumes either. (Decided 2026-11-10.)

### Private and shared nodes

(Decided 2026-11-16.) The hub connects nodes; a node is there to provide
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

- An agent runs on its owner's node, on a shared node its owner may use, or on
  the hub on the owner's behalf. Nodes share only what a shared node offers
  through the hub (models, runs); private nodes share nothing, and no node
  shares tools, files or subscriptions. (Sharing a node's model was dropped on
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
and no tools, is called `direct` (it was `llm` before version 3). The chat
client it uses belongs to the backends module, so the knowledge pipeline and the
hub's chat API can share it.

An agent loop of Ensemble's own is back, as the `builtin` adapter: harness-rs's
`AgentLoop`, its file and shell tools, and its model clients, on the user's LLM
backends, so a helper team (or a shared node) needs no agent CLI installed. It
is pinned to one exact version because the library is young. (Decided
2026-11-16; the old `ensemble` type, removed in version 3, was a loop of its
own written here.)

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

Tasks live in a task tracker: Jira, GitLab, Anning, or markdown files. Ensemble
never becomes the source of truth for tasks.

The helper team never calls a tracker itself. A tracker's item is an **issue**,
assigned to a person; a **task** is an agent's ticket. Each tracker implements
one trait, `IssueTracker`: the issues assigned to this user that carry the
signal, and statuses and comments written back. The inbox makes issues into
tasks and reports the tasks back on the issues. Behind it, tracker adapters do
the tracker work: webhooks where the tracker has them, polling where it does
not, account mapping, the signal, deduplication. With a hub server, the inbox
runs there, with one set of credentials, a reachable address for webhooks, and
on while nodes are off; a `task` event is pushed to the member's node.
Standalone, the same inbox runs in the node with the user's own token. (Built:
both, polling a folder of markdown issues.)

Tasks are assigned to a **user**, never to an agent: the tracker knows people,
and the person stays accountable. A task carries an explicit signal that agents
may take it: a label, such as `ensemble`, which every tracker has and which
needs no extra account. (Decided 2026-11-10.) The flow:

1. The inbox sees an issue assigned to me with the signal and makes it a task
   for the helper agent its label names, or puts it in my backlog.
2. The agent gets it with the vault's notes about it, if its `recall` asks.
3. If it is unclear, the agent asks; the question goes to a tracker comment and
   my answer comes back the same way.
4. It works, checks gates, and may hand parts on to other agents of my team.
5. The result is reported in the tracker; I remain the assignee.
6. The outcome goes to the knowledge pipeline. (Not built yet.)

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
personal knowledge lives in the person's own embedded hub, which is a
different hub and a different session. (Decided 2026-11-11.)

### What feeds it

- **Conversations in a user's own CLI are shared only when the user asks.** The
  user tells their agent, in plain words, to share the conversation; the agent
  decides what part matters and calls Ensemble's `share` MCP tool. Nothing reads
  CLI transcripts, and nothing proxies CLI traffic, so private or throwaway
  conversations stay private and no subscription rule is touched. (Decided
  2026-11-10; the session harvester was dropped.)
- Sources the organization owns are captured automatically: task outcomes,
  connectors to team chats, mail and CRM, and conversations with clients of the
  hub's own chat API. (Not built.)
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
names; a single user's embedded hub uses that user's own backends or API key.
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

(Not built.) A note inherits its source's visibility; merged sources take the
strictest. Recall never returns what the asking identity may not see, and a
prompt goes only to a backend whose operator may see it.

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
2026-11-11.) CLIs and nodes enroll with `ensemble login <hub> --user NAME`,
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
first admin is made on the server with the CLI. An embedded hub has one user
and no registration and no API. (Decided 2026-11-11.)

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
| tasks | the inbox, `IssueTracker` adapters, hand-off | identity, team | `ensemble-tasks`, `ensemble-inbox` |
| vault | vault repositories, schema, index, visibility | identity | `ensemble-knowledge` (no visibility yet) |
| pipeline | ingest, filter, extract, integrate, review | vault, backends | `ensemble-knowledge` |
| recall | context packages | vault | `ensemble-knowledge`, `src/hooks.rs` |
| capture | recorder, quick capture, privacy gate | pipeline | not built |
| connectors | chats, mail, CRM, tracker events, each its own | pipeline or tasks | not built |
| wiring | MCP, hooks in the user's CLIs | — | `ensemble-mcp`, `src/setup.rs`, `src/hooks.rs` |
| api | chat API, MCP endpoint, REST | identity | `ensemble-link` (REST only) |
| web | the UI | api | not built |
| link | hub-to-node connection | identity | `ensemble-link`, `ensemble-node` |

## Build order

1. Config version 3: the flat helper team, `direct`, `--team`. (Done.)
2. The module split into a Cargo workspace, behavior-preserving. (Done.)
3. Per-agent backend fallback and the resolver. (Done: `try` and `when_none`
   for models on LLM backends.)
4. Memory in the hub role, embedded in the node: vault, pipeline, the `share`
   tool, recall. (Done: `hub.yaml` and `--hub`; the vault, the secret check,
   the pipeline with the injection guards and a background worker, BM25
   recall; the `share` and `recall` tools and commands; notes for the helper
   agents whose `recall` asks; session hooks for Claude Code and Codex. Not
   yet: visibility, the privacy gate beyond the secret check.)
5. Hub server, link, identity, tracker adapters. (Done: the inbox with a local
   markdown tracker, embedded and on a hub server; `ensemble hub serve` with
   accounts, sessions, scoped access tokens, sign-up with approval and a REST
   API; `ensemble login` and `--hub wss://`; the organization vault over the
   link; nodes, private or shared, running agents and serving models through
   the hub, the `node` adapter and `/nodes/v1/`. Not yet: GitLab, GitHub and
   Jira; OIDC and the device flow; the chat API and the web UI.)
6. Async, the `builtin` adapter, and one node per worker that is also the MCP
   server. (Done: tokio throughout, as part one describes; `builtin` on
   harness-rs; the node as the long-running MCP server on a socket, with
   `ensemble mcp` without `--team` as a bridge to it and a worker per folder;
   `node.yaml` version 3.)

What remains is the "Not yet" of steps 4 and 5, and the modules marked not
built above.

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
