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
- **Node**: one person's machine, running that person's helper team.
- **Personal agent**: the agent that works for one person and leads their helper
  team. The word "manager" is dropped.

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

**Node** (`ensemble node`): one person's machine. It runs that person's helper
team and personal agent, wires the local agent CLIs (MCP, memory files, session
hooks), and connects outbound to the hub over one WebSocket, so it needs no open
port.

**The hub is a role, not a place.** Its modules run inside the node process for a
single developer, or in `ensemble hub` on a server for a team. The node calls the
same Rust traits either way (`TaskInbox`, `VaultRead`, `ModelAccess`,
`Identity`); behind each is the local module or a client that forwards the call
over the link. The node never checks whether a hub exists. A developer who wants
access from elsewhere moves the hub's state and vault repository to a server.

A node may connect to several hubs: a home hub with the person's own knowledge,
and one workplace hub per organization they work for. Each has its own identity,
token and policy, and nothing passes between them except what the export rules
below allow.

### Rules that hold everywhere

- Agents stay home. An agent runs on its owner's node, or on the hub on the
  owner's behalf. Nodes share nothing with each other: no agents, tools, files,
  models or subscriptions. (Sharing a node's model with colleagues was considered
  and dropped on 2026-11-10.)
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

## Personal agent

Its identity, definition, memory and inbox belong to the hub role; its execution
runs on the node, where the subscriptions, repositories, files and GPU are.
Standalone, all of it is on disk in the node's state directory.

When the node is offline, the hub may run the personal agent in a limited mode,
only if the user enabled it and the organization allows hub backends to see that
user's content: it answers from memory and the inbox and accepts tasks, but runs
no helper team, tools or repositories. Work that needs the node waits for it.

A personal agent is separate per hub, because its identity, memory and inbox
belong to that hub.

The user's own CLI (Claude Code, Codex, OpenCode) is one interface to the
system, not its brain: it reaches the helper team, the inbox and the knowledge
base through Ensemble's MCP tools.

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

An organization limits helper teams through policy, not structure: which
backends its data may be sent to, gates its repositories require, budgets per
user, forbidden tools and permission modes. (Exactly which limits exist is open.)

## Tasks

Tasks live in a task tracker: Jira, GitLab, Anning, or markdown files. Ensemble
never becomes the source of truth for tasks.

The helper team never calls a tracker itself. The `tasks` module offers one
trait, `TaskInbox`: the tasks assigned to this user that carry the delegation
signal, a lease to claim one, and comments, transitions and attachments written
back to the tracker. Behind it, tracker adapters do the tracker work: webhooks
where the tracker has them, polling where it does not, account mapping, the
signal, deduplication. With a hub, the inbox runs there, with one set of
credentials, a reachable address for webhooks, and on while nodes are off; a
`TaskAssigned` event is pushed to the node. Standalone, the same inbox runs in
the node with the user's own token. Today's `local_markdown` tracker and queue
become one adapter and the embedded inbox.

Tasks are assigned to a **user**, never to an agent: the tracker knows people,
and the person stays accountable. A task carries an explicit signal that agents
may take it (its form is open). The flow:

1. The inbox sees a task assigned to me with the signal and delivers it.
2. My personal agent reads it with a context package from recall.
3. If it is unclear, it asks in a tracker comment and notifies me.
4. It plans, splits the work among my helper team's agents, runs it, checks
   gates.
5. It reports in the tracker; I remain the assignee.
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

Scopes are separate repositories, because Git cannot hide single files: one
organization vault, and one personal vault per user. Recall merges them,
checking visibility.

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
personal setup. CLIs and nodes enroll with the OAuth device flow (`ensemble
login <hub>`), tokens in the OS keyring, stored hashed by the hub. Nodes,
connectors and agents are identities too, and an agent acts on behalf of a
person.

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
| team | the helper team, fallback resolver, personal agent | agents, identity |
| tasks | `TaskInbox`, tracker adapters, leases, hand-off | identity, team |
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
5. Hub server, link, identity, tracker adapters.

## Open questions

1. Which organization limits on helper teams exist: backends, tools, budgets,
   gates.
2. The delegation signal in a tracker: a label or column, or a pseudo-user.
3. Capture routing between hubs when a destination is unclear.
4. Export from workplace to home: who approves.
5. Hub exposure: public internet behind TLS, or a VPN only.
6. Do teams of people exist in the hub, and are they taken from tracker groups?
7. Recording from a browser without sending audio to the hub.
8. Telegram as a team chat source: keep it or drop it.
