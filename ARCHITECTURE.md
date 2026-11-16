# Architecture: where Ensemble is going

This document records the direction decided with the owner in November 2026. It
is a map of the whole system, not a build order: nothing here is built until a
phase of `PLAN.md` says so. Where this document and `PLAN.md` disagree, this
document states the intent and `PLAN.md` states what exists. The questions at
the end are open; everything else is decided unless marked otherwise.

## Names

- **Helper team**: the agents a user works with, described in one `config.yaml`
  (config version 3). It is flat: a list of professional agents, such as a
  coder, a reviewer, a PCB designer or an RF engineer, with no teams and no
  hierarchy. A user connects exactly one helper team, from a local directory or
  a git repository; a company may share one, or each user may keep their own.
  The description is strict YAML, never natural language. This replaced the
  version 2 "organization of teams".
- **Hub**: the people layer: identities, the task inbox, knowledge vaults, the
  knowledge pipeline, model access. The word "organization" is not used for it,
  to keep it apart from the old agent config.
- **Node**: a worker: a process that gives computing resources to the hub's
  users. A computer may run any number of nodes. A node is **private**, working
  only for its owner, or **shared**, lending its machine's power to other users
  of the hub. (Decided 2026-11-16; a node was one person's machine before.)
- **Interface**: what a person talks to the system through: their own agent CLI
  (Claude Code, Codex, OpenCode) with Ensemble's MCP tools, or later the hub's
  chat API and web UI. There is no personal agent and no manager. (Decided
  2026-11-16; see "No personal agent".)

## What it is

One tool for two kinds of user: a single developer who wants agents to work on
tasks and save tokens by sending cheap work to cheap models, and an organization
whose people and agents share knowledge. The same binary serves both. Features
beyond the core are modules switched on by their own config block; a block that
is absent means the module is not used, which is the one kind of optional
setting the config rules allow.

| Level | What runs | Who it is for |
|---|---|---|
| 0. Node alone | Helper team, agent runs, worktrees, gates, tasks from a tracker reached with the user's own token | Anyone who wants agents to work on tasks. This is today's Ensemble. |
| 1. Node + memory | The hub role embedded in the node: a knowledge vault, fed by explicit shares, recalled in later runs | Users who want their agents to remember |
| 2. Node + hub | Sharing, people, always-on work, a tracker shared by a team, connectors, a central model server | Teams and organizations |

Each level works without the next. Knowledge always belongs to a hub: a node
with no hub role has none, and a single user who wants it turns the hub role on
in their own node (level 1). (Decided 2026-11-10.)

## Hub and node

**Hub** (`ensemble hub`): one per group of people, always on. It owns identities
and tokens, the front door for clients (an OpenAI-compatible chat API, an MCP
endpoint, a web UI), the task inbox and tracker adapters, the knowledge pipeline
and vaults, and model access (hub API keys, a central LLM server, quotas per
user, a usage log).

**Node** (`ensemble node`): a worker. It runs its owner's helper team, wires the
local agent CLIs (MCP, memory files, session hooks), and connects outbound to
the hub over one WebSocket, so it needs no open port. A computer may run several
nodes, such as one per hub, or a private one beside a shared one. (The WebSocket was confirmed on 2026-11-11, over plain HTTP with
long-polling.) Tracker adapters beyond local markdown wait until after the hub
server.

**The hub is a role, not a place.** Its modules run inside the node process for a
single developer, or in `ensemble hub` on a server for a team. The node calls the
same Rust traits either way (an inbox, `VaultRead`, `ModelAccess`,
`Identity`); behind each is the local module or a client that forwards the call
over the link. The node never checks whether a hub exists. A developer who wants
access from elsewhere moves the hub's state and vault repository to a server.

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
- **runs**: whole agent runs for other users' tasks, with the agent CLI and
  gates on the shared machine.

Who may use shared nodes is decided by hub roles, through a permission, like
every other access in the hub. The helper team stays defined on its owner's
node; a shared node lends power, not its owner's team.

Limits that follow from the rules below: a shared node runs other users' work
only on API keys or on models its machine serves, never on its owner's CLI
subscription. A run on a shared node needs the code it works on and runs
commands there, so the node's owner can see that code, and the user's gates run
on someone else's machine; both sides must allow it.

### Rules that hold everywhere

- An agent runs on its owner's node, on a shared node its owner may use, or on
  the hub on the owner's behalf. Nodes share only what a shared node offers
  through the hub (models, runs); private nodes share nothing, and no node
  shares tools, files or subscriptions. (Sharing a node's model was dropped on
  2026-11-10 and brought back, through shared nodes, on 2026-11-16.)
- An organization may run a central LLM server on its own GPUs. It is an
  OpenAI-compatible backend like any other: a standalone node lists it in
  `llm_backends`; through the hub it also gets quotas per user and a usage log.
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
signal, and statuses and comments written back. The `Inbox` makes issues into
tasks and reports the tasks back on the issues. Behind it, tracker adapters do the tracker work: webhooks
where the tracker has them, polling where it does not, account mapping, the
signal, deduplication. With a hub, the inbox runs there, with one set of
credentials, a reachable address for webhooks, and on while nodes are off; a
`TaskAssigned` event is pushed to the node. Standalone, the same inbox runs in
the node with the user's own token. Today's `local_markdown` tracker and queue
become one adapter and the embedded inbox.

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
6. The outcome goes to the knowledge pipeline.

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
search index is built from the notes. (Decided 2026-11-10.) A recording
transcript is kept briefly; audio is never kept.

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
  hub's own chat API.
- Recordings, started by a person, transcribed on the node.

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
check, off-topic filter, exclusions, a pause switch.

The pipeline runs in the hub role and distills with a model the hub's config
names; a single user's embedded hub uses that user's own backends or API key.
(Decided 2026-11-10.)

### Prompt injection

Shared text may carry instructions meant for a model. The first guards
(decided 2026-11-10):

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

A note inherits its source's visibility; merged sources take the strictest.
Recall never returns what the asking identity may not see, and a prompt goes
only to a backend whose operator may see it.

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
2026-11-10.) Built so far: `share`, `recall` and `admin`, which manages users.

People join a hub server by signing up through its REST API (`/api/v1/`, on
the hub's own port, for a web UI and scripts): they get a token at once, and it
signs in to nothing until an admin approves them with a role. A hub may instead
be `closed`, where only an admin adds users. The first admin is made on the
server with the CLI. An embedded hub has one user and no registration and no
API. (Decided 2026-11-11.)

## Clients

| Way | What happens |
|---|---|
| Local CLIs (Claude Code, Codex, OpenCode) | Their own login and model. Ensemble's MCP tools: helper team, inbox, search, `share`. Recall through memory files and session hooks. |
| OpenAI-compatible API | Any chat UI, IDE plugin or SDK, with an API key. The token is the key; `model` names the agent. |
| Remote MCP | Desktop and web agents, with OAuth. |
| Web UI | Chat, knowledge, review queue, tasks, roster, node status, admin. |
| Obsidian over Git | Direct read of the vaults; a correction is shared like any knowledge. |
| Task tracker | Assign a task to a person and add the delegation signal. |
| `ensemble` CLI | `ask`, `share`, `record start`, `node status`. |

No Telegram bot client for now.

## Modules

Building blocks, not processes. A module is on when its config block is present;
dependencies are checked by name at load time; each exposes a trait and may run
in either process.

| Module | Does | Needs |
|---|---|---|
| core | config, error vocabulary, state directory, event bus, registry | — |
| identity | users, tokens, scopes; one implicit user standalone | core |
| backends | endpoints, the chat client, format translation, health, quotas | core |
| agents | CLI adapters, `direct`, workspace, worktrees, gates | backends |
| team | the helper team, fallback resolver | agents, identity |
| tasks | the `Inbox`, `IssueTracker` adapters, leases, hand-off | identity, team |
| vault | vault repositories, schema, index, visibility | identity |
| pipeline | ingest, filter, extract, integrate, review | vault, backends |
| recall | context packages | vault |
| capture | recorder, quick capture, privacy gate | pipeline |
| connectors | chats, mail, CRM, tracker events, each its own | pipeline or tasks |
| wiring | MCP, memory files and hooks in the user's CLIs | — |
| api | chat API, MCP endpoint, REST | identity |
| web | the UI | api |
| link | hub-to-node connection | identity |

## Build order

1. Config version 3: the flat helper team, `direct`, `--team`. (Done.)
2. The module split into a Cargo workspace, behavior-preserving. (Done:
   `ensemble-core`, `-config`, `-backends`, `-agents`, `-tasks`, `-mcp`.)
3. Per-agent backend fallback and the resolver. (Done: `try` and `when_none`
   for models on LLM backends.)
4. Memory in the hub role, embedded in the node: vault, pipeline, the `share`
   tool, recall. (Done: `hub.yaml` and `--hub`; the `ensemble-knowledge`
   crate with the vault, the secret check, the pipeline with the injection
   guards and a background worker, BM25 recall; the `share` and `recall` MCP
   tools; `ensemble share`, `recall`, `knowledge status|distill|rebuild`. Not
   yet: the organization vault and visibility, the privacy gate beyond the
   secret check, recall handed to delegated agents, memory files and hooks.)
5. Hub server, link, identity, tracker adapters. (Started: the inbox, in the
   embedded hub, with a local markdown tracker: issues become tasks in the
   folder their project maps to, for the agent a `<label>:<agent>` label names
   or in the backlog; questions and answers go through comments; a result is
   posted once and the issue goes to `review` or `blocked`. Ensemble never sets
   an issue to `done`. Then the hub server and the link: `ensemble hub serve`
   with `server` and `roles` in `hub.yaml`, users and hashed tokens in the
   hub's state (`ensemble hub user`), `ensemble login` and `--hub wss://`, and
   `share` and `recall` against the organization vault over one WebSocket;
   sign-up with approval and a REST API for users; accounts with passwords,
   sessions and scoped access tokens. Then the inbox on the server: a labelled
   issue goes to the account whose tracker name is its assignee, as a
   delivery the hub keeps until one of that person's computers takes it over
   the link, makes it a task in the folder its `node.yaml` maps the project
   to, and reports back. Not yet: GitLab, GitHub and Jira; OIDC and the device
   flow; the chat API and the web UI.)

The link, as built: the node opens one WebSocket and signs in with
`Authorization: Bearer <token>` during the upgrade (401 without a valid one).
Frames are JSON: requests `{id, method, params}`, replies `{id, result}` or
`{id, error}`, and events `{event, data}` the hub sends unasked. Requests are
`hello`, `share` and `recall`; for tasks, a node `subscribe`s, and then sends
`task_accept`, `task_refuse`, `task_update` and `answer_taken`, while the hub
pushes `task` and `answer` events, each once per connection and again after a
reconnect. The link knows no tracker: a hub server's inbox fills a
`TaskRelay`. Every request is checked against the token's permissions, its
role narrowed by its scopes, read again from the hub's state, so a revoked
token stops at once. The permissions so far are `share`, `recall`, `tasks`
and `admin`.

## Open questions

1. Export from workplace to home: who approves.
2. Recording from a browser without sending audio to the hub.
3. Telegram as a team chat source: keep it or drop it.
4. Shared nodes: how a run gets its code (a clone and a pushed branch, or
   something else); how the hub picks a node for a run; what the node's owner
   and the user each consent to; the permission's name.
5. Chat from anywhere: how the hub's chat API picks the node that runs the
   helper agent a chat names, and what a chat gets when no such node is
   connected.
