# ensemble

An agentic development system with a **hub** at its center. Everything else
reaches the hub through its **connectors**: your agent over MCP, scripts over
the REST API, trackers whose issues become tasks, and the **runners** that do
the work on the computers you have.

- **The hub** is the core: accounts, their roles and what each may do, the
  port it listens on, and its ANN pool, the models it uses. It runs no
  harnesses. It is its own process, `ensemble-hub serve`, on a server for a
  team or on your own computer when you work alone.
- **Connectors** are everything that reaches the hub, each always on: the
  REST API, MCP for client agents, the link for runners, the knowledge vault,
  the tasks it keeps, and sources of knowledge from outside. Some connectors
  hold **adapters** the admin picks, such as the trackers of `tasks`
  (markdown issues today; Jira, GitHub and others later) or the providers of
  `sources` (Microsoft 365 and Telegram, to come).
- **Runners** are one of those connectors: computational resources, each a
  computer's **harnesses** (Claude Code, Codex, OpenCode, or Ensemble's own
  loop) and the models of its **ANN pool**. A runner works through the tasks
  the hub keeps for you, like a person handed work: in the project's
  repository in its workspace, cloned there when it has none. A runner works
  for you alone, and may share its models through the hub.
- **MCP is how your agent reaches the hub.** The agent CLI you already use adds
  the hub as an MCP server over HTTP, with your account's token as its key,
  and installs nothing of Ensemble: it shares and recalls knowledge, makes
  tasks for your runners, and follows them.

```text
   your agent CLI         scripts, a web UI       a tracker     sources
   (Claude Code, …)                               (issues)      (to come)
        │ MCP /mcp             │ REST /api/v1/         │             │
        ▼                      ▼                       ▼             ▼
   ┌──────────────────────────── hub ─────────────────────────────────┐
   │ core: accounts · roles · the port · ANN pool                      │
   │ connectors: rest · mcp · runners · knowledge · tasks · sources    │
   └──────────────────────────────────────────────────────────────────┘
        ║ the runners' link: WebSockets at /link and /relay
        ║
   your laptop's runner   your desktop's runner    a shared GPU box
   harnesses, tasks       harnesses, tasks         models shared
                                                   through the hub
```

Nobody writes a team. A runner offers what its computer has, and a task
names no agent: for each task, the hub's **task picker**, a decision model,
chooses which of your runners works on it, with which harness and LLM, from
what the task says and what each model is good at. Nothing of Ensemble lives
in your projects.

Ensemble mostly runs no agent loop of its own. The runner starts the
harness's CLI headless in the task's own git worktree, reads what it does as
it works, and keeps the answer with what it changed. When you would rather
install no CLI, `builtin` is a loop in Ensemble itself. Four harnesses ship:

- **`claude-code`**, the Claude Code CLI, on your own subscription, on its own
  models.
- **`codex`**, the Codex CLI, on your own login, on its own models.
- **`opencode`**, the OpenCode CLI, on an LLM of your ANN pool's
  OpenAI-compatible providers.
- **`builtin`**, an agent loop in Ensemble itself
  ([harness-rs](https://github.com/liliang-cn/harness-rs)) on an LLM of your
  ANN pool: it reads, edits and runs commands, and needs nothing installed.

A runner has only the harnesses you add, and the models you add: nothing
installed on the computer is used until you add it.

## How it works

```text
  your agent (Claude Code, OpenCode, Codex, ...)
        |  MCP over HTTP: share, recall, make and follow tasks
        v
  ensemble-hub serve   core: accounts, roles, the port, ANN pool
        ^              connectors: rest, mcp, runners, knowledge, tasks, sources
        |
        |  the runners' connector: a WebSocket at /link (tasks leased, how
        |  each run ended) and one at /relay (model calls between runners)
        |
  ensemble-runner serve   (this computer's runner, long-running)
        |  a worker for each git repository of its workspace, cloned there
        |  for a task when it has none
        v
  the pair the hub's task picker chose: a harness and an LLM of the runner's
        |
        +--> claude -p / opencode run / codex exec   agent CLIs, headless
        +--> builtin                                 a loop in Ensemble
```

- **The hub is the center.** `ensemble-hub serve`, on a server for a team or
  on your own computer, keeps the accounts and decides what each may do. One
  port serves every connector over HTTP: the REST API at `/api/v1/`, MCP at
  `/mcp`, and the runners' link at `/link` and `/relay`. Its connectors are
  always on, and each is written in its settings, which it keeps in its
  database and changes through its commands, its REST API or a web UI.
- **Your agent reaches the hub over MCP.** It adds the hub with its address and
  your token, and sees tools: `share` and `recall` for knowledge, and
  `submit`, `tasks`, `task`, `answer`, `gate`, `cancel` and `runners` for
  tasks, and `choose` when the hub asks you to. It never names a harness or a
  model: the hub's task picker chooses them.
- **The hub keeps the knowledge and the tasks.** What your agent shares
  becomes notes in a git vault that later work recalls. Tasks are kept on the
  hub, made by your agent or by a tracker's issues assigned to you; the hub
  puts the notes of your knowledge related to a task into it, for its run to
  read.
- **Runners do the work.** A computer runs one runner, `ensemble-runner
  serve`, a process you keep running, which names its hub in its
  settings. It works in its workspace, a folder of git repositories:
  each is a project, and the runner runs the tasks the hub keeps for that
  project, in a worktree of that repository. A task for a project it has no
  copy of names the repository to clone, or the runner asks you on the task;
  one it cannot clone it gives back, for another of your runners.
- **Load spreads across runners.** Every runner of yours that runs tasks may
  take a task. A runner's models can serve the hub and your other runners
  through the hub as an OpenAI-compatible endpoint.
- **Tasks outlive the runner that runs them.** A runner leases a task while
  it runs it, and when that runner stops, another of yours takes it up from
  the branch the first pushed.
- **Work is kept outside your repositories**, in a per-user state directory:
  run logs, git worktrees and run records.

[ARCHITECTURE.md](ARCHITECTURE.md) is the full map: the processes, the crates,
where everything is kept, and the life of a task, a share and an issue.

## Quick start

Start with the hub. Working alone, run it on your own computer; for a team,
on a server everyone's runners and agents reach. [Install](#installation)
Ensemble first, so that `ensemble-hub` and `ensemble-runner` are on your
`PATH`.

```console
$ ensemble-hub serve                       # the first time, makes a new hub; runs until Ctrl-C or SIGTERM
```

In another terminal (or with the hub run as a service, see
[Installation](#installation)):

```console
$ ensemble-hub setup                       # adds you as its first admin: your token, shown once
$ ensemble-hub user password ana           # typed without echo
```

To use the hub from a browser, run its [web UI](#the-web-ui), a component of
its own, and open `http://127.0.0.1:7701`; sign in with that name and
password:

```console
$ ensemble-ui --hub http://127.0.0.1:7700           # serves the UI on 127.0.0.1:7701
```

A new hub listens on this computer alone (`127.0.0.1:7700`; `serve
--http-host` and `--http-port` say otherwise), closed to sign-ups, with no model
yet: give it one when you want shares distilled into notes
(`ensemble-hub provider add`, `ensemble-hub model add`, `ensemble-hub knowledge models`; see
[Changing settings](#changing-settings)).

The token's answer gives the line that adds the hub to your agent
([Connect your agent](#connect-your-agent)). Your agent learns the tools from
the hub when it connects, so there is nothing to add to its `CLAUDE.md` or
`AGENTS.md`: it can share and recall knowledge from now on.

For tasks to be worked on, connect a runner to the hub, on each
computer that should work:

```console
$ ensemble-runner setup
```

`ensemble-runner setup` finds the agent CLIs you have (`claude`, `opencode`,
`codex`; with none it says so and goes on, as `builtin` needs none) and any
local model server. When this computer has no runner yet, it
asks what to call it (this computer's name, unless you say otherwise) and
which folder it works in (`~/ensemble`, unless you say otherwise), and makes
it: its settings are kept in its database,
`~/.local/state/ensemble/runner/core/database.db`, with no harness and no
model. Then it says which CLIs are installed, what the runner runs tasks on,
and what is next. Add a harness (with a model of its vendor's for
`claude-code` or `codex`, or, for `opencode` or `builtin`, a model of the
pool), give the runner a repository and the hub, and run it:

```console
$ ensemble-runner harness add claude-code --command claude --max-seconds 1800 --max-steps 50 --max-concurrent 1
$ ensemble-runner harness add-model claude-code sonnet       # or: provider add …, model add …, harness add opencode …
$ git clone git@github.com:acme/shop.git ~/ensemble/shop      # a project: a repository in the workspace
$ ensemble-runner login ws://127.0.0.1:7700 --user ana        # your account on the hub, or: join <url> <code>
$ ensemble-runner serve
```

Keep the runner running (see [Running the runner](#running-the-runner)). Then
ask your agent to have something implemented in `shop`.

Without a terminal, or to script it: `ensemble-runner setup --yes --name desk
--workspace ~/code`.

Afterwards, `ensemble-runner doctor` says what is wrong and how to fix each
thing, and `ensemble-runner status` what the runner runs tasks on.

The workspace is where the runner works. Each git repository directly inside it
is a project, named for its folder: `~/ensemble/shop` is the project `shop`.
The runner looks at the workspace again every 30 seconds, so a repository you
clone in later gets a worker, and one you take away has its worker stopped.
You need not clone them yourself: a task for a project the runner has no
folder for names the `repository` to clone it from, and the runner clones it
into the workspace, or asks you on the task which repository it is (see
[Tasks](#tasks)).
Ensemble keeps nothing in the repositories: run logs and worktrees go in the
runner's folder (`~/.local/state/ensemble/runner/workdirs/`), one per
repository. A task's `repo`, a path inside
the repository, has it work in one folder of it, such as one repository of
several; a path that leaves the repository is refused.

## The hub

The **hub** is the center of the system. Its core is small: the accounts,
their roles and what each may do; the one port it listens on; and its ANN
pool, the providers of models it calls. Everything else is a connector (see
[Connectors](#connectors)). It has no harnesses and runs no tasks. It is
always its own process, `ensemble-hub serve`: a team runs one on a server, and
one person runs one on their own computer beside their runner. The hub and
the runner know nothing of each other's files: a runner reaches the hub over
the link, wherever it runs. Everything Ensemble keeps is in one folder,
`--config-dir` (by default `~/.local/state/ensemble`), the hub in its `hub/`,
a folder for its core and one for each connector that keeps anything, each
readable only by you:

```text
~/.local/state/ensemble/hub/
  core/
    database.db    its settings and who changed them, its providers' keys (sealed),
                   accounts and their tokens (Turso)
    master.key     what seals the keys in database.db: back it up
  knowledge/
    vault/         its knowledge vault: Markdown files in a git repository
    embeddings.db  the notes' vectors, a cache made again from the notes
  tasks/
    database.db    the tasks it keeps for its accounts
  trackers/
    database.db    which issue of which tracker became which task
```

Its settings are kept in its database, as a GitLab instance keeps its
application settings: the commands below, the REST API and a web UI change
them, each change checked before it is kept, and kept in their audit log with
who made it.
Where it runs, such as a NixOS module, only runs it.

On the hub's computer:

```console
$ ensemble-hub serve                      # makes its settings and vault the first time; until Ctrl-C or SIGTERM
$ ensemble-hub setup                      # its first admin, whose token it prints once
$ ensemble-hub user password root         # typed without echo
$ ensemble-hub provider add central --type openai-compatible-api --url http://127.0.0.1:8000/v1 --max-concurrent 4
$ ensemble-hub model add central:qwen3.5-72b            # nothing it serves is in the pool until it is added
$ ensemble-hub knowledge models central:qwen3.5-72b   # the models that turn shares into notes
$ ensemble-hub check                      # says which model would distill shares now
```

`ensemble-hub settings show` shows every setting; `--json` prints them as
data, which `ensemble-hub settings import` takes on another hub. A hub that
was just set up, given a model server:

```text
$ ensemble-hub settings show
ann_pool:
  models:
    -
      id: qwen3.5-72b
      kind:
        context_window: none
        tier: strong
        tools: none
        type: llm
      missing: false
      on: true
      shared: false
      provider: central
  providers:
    central:
      api_key_env: none
      base_url: http://127.0.0.1:8000/v1
      max_concurrent: 4
      timeout_seconds: 300
      type: openai-compatible-api
distill:
  max_tokens: 4000
  try:
    -
      model: qwen3.5-72b
      provider: central
  when_none: wait
embed: none
lockout_failures: 5
lockout_minutes: 15
owner: Acme
recall_max_chars: 6000
recall_max_notes: 8
registration:
  type: closed
roles:
  admin: admin, share, recall, tasks
  member: share, recall, tasks
  reader: recall
session_hours: 12
source_filter: none
trackers: none
```

Its core is its owner, its accounts and roles, and its ANN pool, the
providers of models it calls; the rest reaches it through connectors, each always on: the
REST API at `/api/v1/`, MCP at `/mcp`, the runners' link, knowledge, tasks
and sources. `ensemble-hub settings history` says who changed what, when,
and with which operation. The hub's command line, `ensemble-hub …` on its
computer, is always on and has no settings.

**The port.** `serve` listens on one port (`--http-host`, `--http-port`) for
every connector over
HTTP: the REST API at `/api/v1/`, MCP at `/mcp`, and the runners' WebSockets at
`/link` and `/relay`. A WebSocket anywhere else is refused. The hub speaks
plain HTTP and WebSocket. For the internet, put a TLS reverse proxy in front,
so passwords and tokens never cross the network in the clear; with Caddy,
which gets and renews the certificate itself:

```text
hub.example.com {
    reverse_proxy 127.0.0.1:7700
}
```

On a LAN or a VPN, listen on that address instead
(`ensemble-hub serve --http-host 10.0.0.2`) and connect with `ws://`.
For one person alone, the hub listens on `127.0.0.1:7700`, what `serve`
listens on unless `--http-host` and `--http-port` say otherwise, and the
runner names `ws://127.0.0.1:7700`. Where it listens is how it is run, not a
setting. The hub needs no setting for its own address: the hints it gives
with a new token or join code name the address the request reached it at
(through a proxy, from its `X-Forwarded-Proto` and `X-Forwarded-Host`), or,
from the command line, where the running hub listens.

**The ANN pool** holds the models the hub itself calls, such as to distill
knowledge, in two layers: **providers**, its own servers and APIs, with the
keys it keeps, and runners that share models through it (`type: runner`, see
[Sharing models](#sharing-models)); and the **models** each serves, an LLM,
an embedding model or a decision model, that someone added, as the provider
says it is.
Nothing a provider serves is in the pool until someone adds it, and only a
model in the pool, of the kind its use needs, may be named, as
`<provider>:<model>`. `ensemble-hub provider add` adds a provider and keeps
its key for you, and `ensemble-hub model add` adds a model it serves (see
[Changing settings](#changing-settings)).

**Permissions.** The permissions are `share` (keep knowledge in the vault),
`recall` (read its notes), `tasks` (make and follow tasks, have your runners
run them, and receive the issues assigned to you; see
[Tasks from a tracker](#tasks-from-a-tracker)) and `admin` (manage users).
The roles are yours to name, each a list of
them. Accounts are not in its settings: they are kept in their own tables of
`database.db`. A share is kept under the name of the account that signed in, never a name
a client sends.

**Accounts.** An account has a name, a role and a password of at least 12
characters, kept as a PBKDF2-HMAC-SHA256 hash. There are two kinds of token,
both sent as `Authorization: Bearer <token>`, and only their SHA-256 is kept:

- A **session** comes from logging in with the password and lasts
  `sessions.hours`. It is what a UI holds. Only a session may change the
  password or make and revoke access tokens, so a stolen access token cannot
  make more. A password change ends every session.
- An **access token** is made from a session, for a runner, a client agent or
  a script: named (`laptop`), with an optional expiry, and optional **scopes**
  that narrow the account's role, such as `["recall"]`. It is shown once,
  listed by id and name, and revoked one by one. Scopes hold on the link, on
  MCP and on REST.

After `lockout.failures` failed logins, a name is refused for
`lockout.minutes`, even with the right password. A revoked or expired token
stops working at its next request, even on an open connection.

**Signing up.** With `registration: approval`, anyone who reaches the hub signs
up with a name and password; they can log in once an admin approves them:

```console
$ curl -X POST https://hub.example.com/api/v1/register -d '{"name":"ana","password":"..."}'
$ ensemble-hub user approve ana --role member     # on the hub's computer, or the API, below
```

## Connectors

Everything that reaches the hub is a **connector**, and each is always on:

| connector | what it is | adapters |
| --- | --- | --- |
| `rest` | the REST API at `/api/v1/`, for scripts and the web UI | |
| `mcp` | MCP over HTTP at `/mcp`, for client agents | |
| `runners` | the runners' link, WebSockets at `/link` and `/relay` | |
| `knowledge` | the vault, and the models that turn shares into notes | |
| `tasks` | the tasks the hub keeps, and the trackers whose issues become tasks | `trackers`: `local_markdown`; Jira, GitHub and others later |
| `sources` | knowledge from outside, such as a mailbox or a chat | `adapters`: none yet; Microsoft 365 and Telegram later |

A connector with adapters names each one the admin picks, by a name of their
choosing, with its `type` and the settings of that type.

### The REST API

The REST API lives on the hub's port, under `/api/v1/`, for the web UI and
for scripts. JSON in and out.

| Method and path | Who | Does |
| --- | --- | --- |
| `POST /api/v1/register` `{name, password}` | anyone, with `approval` | a pending account |
| `POST /api/v1/login` `{name, password}` | anyone | a session: `{token, expires}` |
| `POST /api/v1/join` `{code, client}` | anyone, with a join code | spends it: a new access token of the client's own, named `client`: `{name, token}` |
| `POST /api/v1/logout` | a session | ends it |
| `POST /api/v1/session` `{name, password}` | anyone, from the hub's own origin | a browser's login: the session is kept in an HttpOnly, SameSite=Strict cookie, never in the answer: `{name, expires}` |
| `DELETE /api/v1/session` | the cookie's session | ends it and clears the cookie |
| `GET /api/v1/me` | any token | your account, the token's permissions and kind |
| `PUT /api/v1/me` `{tracker_user}` | a session | who you are in the tracker; empty for your account name |
| `PUT /api/v1/me/password` `{current, new}` | a session | a new password; every session ends |
| `GET /api/v1/me/tokens` | a session | your access tokens: id, name, scopes, made, expires |
| `POST /api/v1/me/tokens` `{name, expires_days?, scopes?}` | a session | a new access token, shown once |
| `DELETE /api/v1/me/tokens/{id}` | a session | revokes one |
| `POST /api/v1/me/join-codes` | a session | a join code that signs one of your clients in, shown once: `{code, expires}` |
| `GET /api/v1/users` | admin | every account; never a password, token or hash |
| `POST /api/v1/users` `{name, role, password}` | admin | an account, active |
| `GET /api/v1/roles` | admin | the roles and their permissions |
| `POST /api/v1/users/{name}/approve` `{role}` | admin | lets a pending account in |
| `PUT /api/v1/users/{name}` `{role?, tracker_user?}` | admin | another role, or who they are in the tracker |
| `PUT /api/v1/users/{name}/password` `{password}` | admin | resets it; the account's sessions end |
| `POST /api/v1/users/{name}/join-codes` | admin | a join code that signs one of their clients in, shown once |
| `DELETE /api/v1/users/{name}/tokens` | admin | revokes every token |
| `DELETE /api/v1/users/{name}` | admin | removes an account, or turns a sign-up down |

| `GET /api/v1/ops` | any token | the operations it may run, each with the JSON Schema of its input (secrets `writeOnly`) and its fields in order, for a UI to make its forms from |
| `POST /api/v1/ops/{name}` `{…its input}` | as the operation says | runs any operation by its name, such as `provider.add`, `knowledge.models` or `tracker.add` |
| `GET /api/v1/openapi.json` | anyone | this API as an OpenAPI 3.1 document: every path above and every operation, with its body and who may call it |

Point an OpenAPI tool at `/api/v1/openapi.json` to browse the API or make a
client for it. The document is the same for everyone: what a token may
actually run is what `GET /api/v1/ops` lists for it.

An account's own work is operations too, which only the running hub answers,
each as the runners' link and MCP do, the permission it needs checked:
`task.projects`, `task.list`, `task.show`, `task.create`, `task.answer`,
`task.gate`, `task.confirm-gate`, `task.reassign`, `task.choose` and `task.cancel`
(`tasks`), `runner.list`, the runners that work for you with what each runs
tasks on (`tasks`), `knowledge.share` (`share`) and `knowledge.find`
(`recall`).

Errors are `{"error": "..."}` with 400, 401, 403, 404, 405, 409 or 429.

### The web UI

The web UI is optional and apart from the hub: `ensemble-ui`, an executable
of its own, serves it on a port of its own and passes its calls on to the
hub's REST API, so the browser sees one origin. The hub knows nothing of it;
it is a client of the REST API as a script is, and any number of them may
front one hub.

```console
$ ensemble-ui --hub http://127.0.0.1:7700 [--http-host 127.0.0.1] [--http-port 7701]
```

Open its address in a browser and sign in with your name and password on the
hub. It shows what your account may do:

- **Tasks**, by project: each task with its state and the question it asks,
  made, answered, given a gate, given to someone else, its gate confirmed
  when someone gave it to you, and cancelled.
- **Runners**: those connected now that work for you, and what each runs
  tasks on.
- **Knowledge**: recall the notes about a topic, and share what is worth
  keeping, for you alone or for the hub.
- **Account**: your access tokens and join codes, each shown once, your
  password and your name in the tracker.
- For an admin, **Users**: the accounts, sign-ups waiting to be let in, and
  what an admin does to an account; and **Settings**, a section each, filled
  in with each setting as it is now: the hub and its logins and sign-ups;
  the roles, a box for each permission; the models: each provider tested,
  given a model it serves, asked again, given a key or taken out, and each
  model added said how strong it is or taken out; the models of the knowledge vault and the source
  filter; the trackers; who changed what; and a page for each operation of
  the settings no section shows yet.

Every form is made from what `GET /api/v1/ops` says of the operation it runs,
so the UI does nothing the API does not, and a new operation's form comes
with it. What a field may be is picked, not typed, where the hub says or
knows it: a role from the hub's roles, permissions and a token's scopes as
boxes, a project from those with tasks, a model from those of the pool
that are on and of the kind the field needs. A form asks only for what the choices made so far need, folds away
what is seldom needed, has a new password typed twice, and asks before it
removes, revokes or cancels anything. A browser signs in with
`POST /api/v1/session`: the hub keeps the
session in an HttpOnly, SameSite=Strict cookie, never where a script reads
it, for the UI's origin; `ensemble-ui` makes it Secure when a TLS proxy in
front says the browser came over TLS. A request that changes anything must
come from a page of the UI, which the browser names, and `ensemble-ui` passes
it on as from the hub's own origin; it passes on `/api/` alone, not MCP or
the runners' link. The UI's pages load nothing from elsewhere, under a
content security policy that allows only their own files.

`--hub` is the hub's plain HTTP address, as the UI's computer reaches it:
run the UI beside the hub, or on a network the two trust, and put a TLS
proxy in front of the UI for the internet.

The UI is in `ui/`, a workspace of its own: `ui/app`, the app, in Rust with
Dioxus for WebAssembly, and `ui/server`, `ensemble-ui`. The Nix package
`ensemble-ui` builds the app and embeds it in the executable. To work on it,
in the dev shell:

```console
$ ui/build.sh                                                   # builds the app into ui/dist
$ cargo run --manifest-path ui/Cargo.toml -- --dist ui/dist     # serves that folder in place of the embedded app
```

### Connect your agent

Your agent reaches the hub, not your runner. The hub serves MCP over HTTP at
`/mcp` on its port, always, and any agent that speaks MCP (Claude Code, Codex,
OpenCode, claude.ai) adds it with the hub's address and an account's token as
the API key. Nothing of Ensemble is installed on the agent's computer, and one
registration serves every project.

A token's answer gives the line to paste, with the address the request
reached the hub at. An admin makes a token for a person on the hub's computer,
or you make one for yourself (`POST /api/v1/me/tokens`, with a session):

```console
$ ensemble-hub user token ana
ens_…
note: This is ana's token, shown once. … For their agent, as its API key for the hub's MCP:
  claude mcp add --transport http ensemble http://127.0.0.1:7700/mcp --header "Authorization: Bearer ens_…"
```

Made on the hub's computer, the line names where the hub listens; behind a
proxy, put the address people reach it at in its place, such as
`https://hub.example.com/mcp`. Made over the REST API, it names the address
that request came in at.

Paste that line, or ask your agent to add the hub with it; with `--scope user`,
Claude Code has it in every project of yours. Other clients take the same two
things in their own MCP settings: the address, `https://hub.example.com/mcp`,
and the header `Authorization: Bearer <token>`.

The tools are what the token may do:

| tool | |
| --- | --- |
| `share` | keep something in the hub's vault, private or for the hub (needs `share`; see [Knowledge](#knowledge)) |
| `recall` | the notes about a topic (needs `recall`) |
| `runners` | your runners connected now, each with the harnesses and models it runs tasks on |
| `submit` | make a task, kept on the hub; its id comes back at once. Its arguments are the ticket's, below in [Tasks](#tasks): `project`, `title`, `task`, `definition_of_done`, and optionally `repository`, `goal`, `context`, `constraints`, `files`, `after`, `repo` and `gate` |
| `tasks` | a project's tasks: each one's ticket and state, `blocked` and who gave it back why among it |
| `task` | one task: its ticket, state, questions and result |
| `answer` | answer the question a task stopped to ask, so that it goes on |
| `gate` | give a task that is not running the check its work must pass, `{command, timeout_seconds, attempts}`, or, without one, take it away |
| `confirm_gate` | confirm the gate of a task someone gave you (`gate_unconfirmed`), once you have read its command, so your runners take it |
| `reassign` | give a task that is not running to another person (`to`, their account's name): it becomes a task of theirs, with a new id |
| `choose` | choose the worker of a task whose `pick` is `needs_choice`, as the hub's decision model was not sure: a `runner` of yours connected now, its `harness` and the `model` it drives, such as one its `decision` ranks |
| `cancel` | cancel a task; a runner working on it stops |

The task tools need the `tasks` permission, and each names the
`project`, a repository's name. A token with scopes gets only their tools: a
reader gets only `recall`. When an agent connects, the hub's instructions tell
it to `recall` before work where earlier decisions may matter, to `share` when
you ask, and how to make and follow tasks. Each message is one `POST` answered
with one JSON response; the hub sends nothing unasked, so your agent sees how
a task goes when it asks with `tasks` or `task`.

### The runners' link

Runners connect to the hub over WebSockets on its port: the link at `/link`,
for tasks and to say they are connected, and a relay socket at `/relay`, for
model calls (see [Runners](#runners)).

**Each runner names its hub.** On each computer that runs a runner for the
hub, sign in once, with your password or with a join code; either puts the hub
in the runner's file:

```console
$ ensemble-runner login wss://hub.example.com --user ana    # asks the password; makes a token "runner <name>"
$ ensemble-runner join wss://hub.example.com join_…         # or a join code, in place of the password
```

Signing in names the hub in the runner's settings (`hub: wss://hub.example.com`
in `ensemble-runner settings show`), and `logout` takes it out.

A runner is one more client of the hub, and signs in as any client does: the
hub's `token.login` (a password) and `token.join` (a join code) make an access
token of the client's own, named after it. The runner asks them over its link,
the WebSocket it works on, and makes no other request to the hub: a link opened
without a token may only sign in. `ensemble-runner login` checks the password,
minding the lockout above, and the hub makes a token named `runner <name>`,
after the runner (or the computer, before there is a runner).
`ensemble-runner join` does the same with a **join code** in place of the
password: an admin makes one for an account on the hub's computer, or you make
one for yourself with a session (`POST /api/v1/me/join-codes`), and it is shown
once, signs in as no one, and is spent by the first client that signs in with
it, within 24 hours (another client spends it with `POST /api/v1/join`):

```console
$ ensemble-hub user join-code ana
join_…
note: This is ana's join code, shown once and good for one sign-in within 24 hours: a client of the hub at https://hub.example.com spends it for a token of its own, as `ensemble-runner join wss://hub.example.com join_…` does.
```

The runner keeps the token in its database, sealed by its `master.key`;
`--token T` keeps a token made elsewhere instead, and
`ensemble-runner logout <url>` forgets it. Signing in again, with a new token
or as someone else, reaches a running runner at once: its links to the hub
connect again with the new token. `ensemble-runner status` asks the hub whether the token still
works, and as whom. From then on the runner runs the tasks the hub keeps for
that account, and `ensemble-runner peers` asks that hub.

Every link a runner opens says the version of the link's protocol it speaks
(the `ensemble-protocol` header). A hub refuses a runner of another version
before anything else, with both versions and which of the two to update, such
as "this hub speaks link protocol 5, the runner 6: update ensemble-hub", and
the runner says it in `login`, `join`, `status` and as `serve` connects.

### Knowledge

The hub keeps knowledge: what agents share becomes notes in its vault, and
later work recalls them. Its settings are the `knowledge` connector in
the hub's settings (see [The hub](#the-hub)): the vault, the models that distill, and
how much a recall returns.

**Sharing is explicit.** Nothing reads your CLI's transcripts and nothing
proxies its traffic, so a private or throwaway conversation stays private and no
subscription's rules are touched. When you want something kept, tell your agent
in plain words ("share what we decided about the deploys"); it picks what is
worth keeping and calls the hub's `share` tool. A share that looks like it
holds a key or a private key is refused, so the agent shares again without it:
the vault keeps everything it is given.

**Private unless you say.** A share is `private`: only you recall what comes
of it. Say it is for the team ("share this with the hub") and your agent shares
it with `visibility: hub`, so everyone on the hub who may recall finds it. A
note is made only from shares of one audience, yours or the hub's, so what you
shared privately never joins a note others recall, nor a link from one; recall
gives each account its own notes and the hub's. This holds for what the hub
serves: whoever runs the hub's computer can read the vault's files.

**Tasks teach it too.** When a task on the hub ends done, the hub shares what
was asked, what was asked and answered on the way, and what the run
reported, as yours and private, with the client `task`, so what your runs
learn is kept like what you share. A report that looks like it holds a
key is not shared, and the hub's log says so.

**Only Ensemble writes notes.** Each share is kept as it arrived, and the
running hub turns it into notes in the background, or now with
`ensemble-hub knowledge distill` on the hub's computer:

1. One chat request extracts typed items (decision, fact, how-to, lesson,
   commitment, preference, ownership); chatter gives none and is skipped.
2. Each item is compared with the notes most like it. It becomes a new note,
   is merged into one, confirms one, or, when it contradicts one, waits in
   `review/` for you. The model also says which of those notes it relates to.
3. The notes are written and the vault is committed, one commit per share.
   Related notes link to each other, both ways, in a `## Related` list of
   `[[links]]` at the end of each: Obsidian's graph shows them, and `recall`
   names them so an agent can recall them too.

To correct a note, share the correction. To read the vault, open it as a
folder in Obsidian, or anything that reads Markdown:

```text
vault/
  shares/<yyyy>/<mm>/<id>.md   each share as it arrived, never changed
  notes/<slug>.md              one note per topic, its related notes linked at the end;
                               frontmatter: kind, title, tags, visibility (and owner),
                               sources (share ids), shared_by, pipeline, updated
  review/<share-id>.md         what the pipeline would not write, and why
  log.jsonl                    what the pipeline did with each share
  .ensemble/embeddings.db      the notes' vectors, with an embedding model: a cache, not in git
```

Because the notes are made from the shares, a better model or a new note format
needs no migration: `ensemble-hub knowledge rebuild` deletes the notes and makes
them again from every share, in one commit. While the hub runs, it asks the hub
instead, which clears the notes and makes them again one share at a time, as
it distills, and goes on from there if it is stopped meanwhile; until it is
done, `recall` finds only the notes made so far. A new version of Ensemble
whose notes differ does the same by itself: notes another version made are
made again from every share. `ensemble-hub knowledge status`
counts shares, pending shares, notes and what waits in `review/`.

**Shared text is data, not instructions.** It may carry instructions meant for
a model, so the model that distills has no tools (one chat request, never an
agent CLI); shared text goes in between markers with a nonce nobody can guess;
an answer must be the JSON asked for and may only change the notes it was
offered, or it goes to `review/`; and every note names the shares it came from.

**Recall.** Your agent calls `recall` with a topic before work where your
earlier decisions or setup may matter, and gets the matching notes, best first,
within `max_notes` and `max_chars`, marked as reference data.
The search is by words (BM25, a
title's words counting more), with no index to keep, and, with an embedding
model, by what the notes mean: `ensemble-hub knowledge embedding <provider>:<model>` names one,
an embedding model of the hub's ANN pool that is on, which an `openai-compatible-api`
provider serves at `/embeddings` (Ollama's
`nomic-embed-text`, say), and notes at least `min_similarity` alike to the
query are found too, in other words than yours. A note found both ways comes
first. The hub embeds each note once, after it writes it, and keeps the vectors
in the vault, in `.ensemble/embeddings.db`: a Turso file that compares them,
out of the history (the vault's `.gitignore` names it), and a cache like the
notes, made again when a note or the model changes, and safe to delete. When
the model cannot be asked, notes are found by their words alone, and the hub's
log says why, once.
The distiller finds the notes an item may belong to the same way.

**Your agent recalls when it decides to.** The hub's instructions, which
every MCP client is given when it connects, tell it to `recall` before work
where earlier decisions may matter, and to `share` when you ask. The agent
decides when to follow them. A run gets no knowledge tools: a task carries
what its ticket says and the notes the hub put in it, so your agent puts in
it what the run needs, from what it recalled.

### Tasks from a tracker

Issues live in a tracker and are assigned to **people**. When an issue is
assigned to you and carries the hub's label, the hub's inbox makes it a task of
yours, which the hub keeps like any other, and writes what happens back on the
issue. A run never touches the tracker itself. A tracker is an
adapter of the `tasks` connector, added by name with its `type`, its `label`
and how often it is looked at (`poll_seconds`). The first type is a folder of markdown files on the hub's
computer (`local_markdown`, with its `dir`); GitLab, GitHub and Jira come
later. A hub watches every tracker it names, each with an inbox of its own
and a folder of its own; with none, tasks come from your agent alone.

```console
$ ensemble-hub tracker add issues --type local_markdown --dir issues --label ensemble --poll-seconds 30
```

An issue is a markdown file anyone can write:

```markdown
---
title: Fix the login redirect
assignee: ana
labels: [ensemble]
project: shop
status: open
---
After logging in, it redirects to /home instead of back to where you were.

## Definition of done

After logging in, you are back on the page you came from.
```

- **What is done.** The issue's `## Definition of done` section, any heading
  level, is the task's definition of done, and the rest of the text is the
  task. An issue without one, or with an empty one, is blocked, saying so:
  Ensemble does not make one up.
- **Who is who.** An issue goes to the account whose tracker name is its
  assignee: the account's `tracker_user` when set (`PUT /api/v1/me`, an admin's
  `PUT /api/v1/users/{name}`, or `ensemble-hub user tracker ana ana.petrosyan`),
  else the account's own name. Its role needs the `tasks` permission.
- **Which repository.** The issue's `project` is a repository's name: the task
  runs on one of that person's [runners](#runners) with a repository of that
  name directly in its workspace, in that repository. So `project: shop` is
  worked in `~/ensemble/shop`, or wherever that runner's workspace is. A
  runner with no copy asks on the issue which repository to clone, and your
  next comment is the answer. An issue with no project is blocked, saying so.

What the hub does, every `poll_seconds` of the tracker:

- **An open issue becomes a task** of that account, and the issue says so:
  "picked up as task 0003-fix-the-login-redirect". When one of
  their runners runs it: "runs on ana's computer laptop", and `in-progress`.
- **A question** the task stops to ask is posted as a comment, and the issue
  goes to `blocked`. Your next comment on the issue is the answer.
- **The result** is posted as a comment, once. The issue goes to `review` when
  the task is done, or to `blocked` when it failed. You close it: Ensemble
  never sets `done`.
- **An issue you close or take the label off** cancels its task, if it has not
  ended, and is forgotten. An issue whose assignee is nobody's tracker name, or whose account may
  not receive tasks, is blocked with a comment saying why.

Ensemble changes only an issue's `status:` line and appends comments
(`## Comment by ensemble at <time>`); everything else in the file stays as you
wrote it.

### Sources

A source brings knowledge in from outside, such as a mailbox or a chat, so
what a team decides there reaches the vault too. Each is an adapter of the
`sources` connector, which an admin connects for the whole hub: Microsoft 365
(Outlook mail and Teams) and Telegram (a bot in the team's groups) are to
come. None is built yet.

What a source brings goes through one model before the distiller, the
`filter`. It first keeps what is worth knowing, then removes what your
`prompt` calls confidential. Only what is left becomes a share, for everyone
who may recall, and the raw messages are dropped. With no filter, no source
may be connected.

```console
$ ensemble-hub source filter central:qwen3.5-72b --prompt "Salaries, contracts and prices agreed with clients; clients' names."
$ ensemble-hub source filter --when-none fail   # what came in is dropped, saying why, when no model can be used
$ ensemble-hub source try "…"                    # what the filter would leave of a text, shared nowhere
$ ensemble-hub source filter --off               # no filter: no source may be connected
```

## Runners

A runner is a worker on a computer: the one long-running process that runs
tasks there, on its harnesses and its ANN pool, and a computer runs one. It works in its workspace,
the folder its settings name: each git repository directly inside it is
a project, named for its folder, and the runner keeps a worker for each, which
leases the tasks the hub keeps for that project, for the runner's account, and
runs them in worktrees of that repository. For a task of a project it has no folder for, it
clones the task's repository there first, or asks on the task where it is. No agent connects to a runner: your agent makes
tasks on the hub, and your runners take them.

With the hub its settings name, a runner keeps a link to it: the tasks
it runs (see [Tasks](#tasks)), and one that says it is connected, with the
harnesses and models it runs tasks on. It works on your tasks alone. The hub connects them, and
keeps no runner once it is gone.

`ensemble-runner setup` or `ensemble-runner init` makes the runner;
`ensemble-runner settings show` shows its settings, and an operation changes
each:

```console
$ ensemble-runner settings name gpu-box                       # a hub knows it as <you>/gpu-box
$ ensemble-runner settings workspace ~/ensemble               # its git repositories; ~/ is your home
$ ensemble-runner provider add local --type openai-compatible-api --url http://127.0.0.1:11434/v1 --max-concurrent 1
$ ensemble-runner provider models local                       # what it serves; none of it is in the pool yet
$ ensemble-runner model add local:qwen3.5:9b --tier medium    # how strong it is, if Ensemble does not know
$ ensemble-runner model set local:qwen3.5:9b --abilities "quick; weak at UI work"   # what it is good and bad at
$ ensemble-runner model list                                  # the models added
$ ensemble-runner model share local:qwen3.5:9b               # offered to the hub and your other runners
$ ensemble-runner provider add team-gpu --type runner --runner kim/workstation
$ ensemble-runner harness list                                # those added, found or missing, and CLIs installed not added
$ ensemble-runner harness add claude-code --command claude --max-seconds 1800 --max-steps 50 --max-concurrent 1
$ ensemble-runner harness add-model claude-code sonnet        # a model of its vendor's it drives
$ ensemble-runner harness set-model claude-code sonnet --abilities "strong at Rust"
$ ensemble-runner login wss://hub.example.com --user kim     # once: it names this hub
$ git clone git@github.com:acme/shop.git ~/ensemble/shop      # a project of its workspace, or let a task name it
$ ensemble-runner serve                                       # until Ctrl-C or SIGTERM
$ ensemble-runner peers                                       # your runners connected to the hub now
```

`ensemble-runner …` reaches the running runner on a socket only you may open
(in `$XDG_RUNTIME_DIR/ensemble`, else a folder of yours under `/tmp`). A
project's tasks run on a runner with a repository of its name in its
workspace: one you cloned there, or one it cloned for a task that names its
`repository` (see [Tasks](#tasks)).

A runner has the **harnesses** you add, at most one of each kind, the
programs that drive an LLM to work in a repository: Claude Code, Codex and
OpenCode, CLIs each added with its `--command` (a name on PATH or a path),
which the runner must find to add it and looks for again when it starts and
on `harness refresh`, and `builtin`, Ensemble's own loop, which needs nothing
installed. Each says how long one run may take, how
many steps it may make and how many run at once, and `builtin` the most
tokens of answer per model call. Claude Code and Codex drive only the models
of their own vendor's you add to them, each with how strong it is (Ensemble's
catalog says, when you do not; `harness list` suggests the ones it knows and
says whether the CLI is logged in where a file of its own or a key says so);
OpenCode drives the LLMs of the runner's pool on OpenAI-compatible providers,
and `builtin` those that make tool calls. Each LLM of the pool and each model
of Claude Code's or Codex's may say, in your words, what it is good and bad
at (`--abilities`, on `model add|set` and `harness add-model|set-model`;
empty takes it away): with how strong it is, it is what the hub chooses a
model for a task by. The runner tells the hub what each harness can drive,
with all of that, and how many runs go on on each harness and provider,
whenever that changes; the web UI's Runners page shows both:

```console
$ ensemble-runner harness add codex --command ~/bin/codex --max-seconds 1800 --max-steps 50 --max-concurrent 1
$ ensemble-runner harness add-model codex gpt-5-codex
$ ensemble-runner harness set codex --max-seconds 900
$ ensemble-runner harness remove codex
```

A harness is never shared: a task runs on its own account's runners. Each
takes at most `max_concurrent` runs at once (`harness set --max-concurrent`),
and `builtin` at most `max_tokens` of answer per model call
(`--max-tokens`). How a task gets a harness and a model is in
[The task picker](#the-task-picker).

### Running the runner

`ensemble-runner serve` runs this computer's runner until it gets Ctrl-C or
SIGTERM. It makes its workspace when the folder is not there, and gives each git
repository directly inside it a worker, which runs the tasks the hub keeps for
that repository's project. It looks again every 30 seconds: a repository cloned
in since gets a worker, and one that went away has its worker stopped; and it
clones, asks about or gives back the tasks of projects it has no folder for.
With a hub, it tells the hub it is connected, with the harnesses and models
it runs tasks on: `runners` and `ensemble-runner peers` list it. Without a
hub it has no tasks, and says so. Started before it is signed in to the hub
its settings name, as a service at boot may be, it waits, saying so,
until `ensemble-runner login` or `join` keeps a token, and goes on from there. On a stop, running tasks are stopped and put
back on the hub, for this runner or another to run again, and the rest stay
queued. A computer runs one runner, so a second `ensemble-runner serve` refuses.
One worker runs a repository's tasks: its lock is a file in the repository's
state directory, and one left by a process that died is taken over.

On NixOS, and with Home Manager on Linux or macOS, the Nix modules run it for
you ([Installation](#installation)). Otherwise, to keep it running, as a
systemd user unit (`~/.config/systemd/user/ensemble-runner.service`):

```ini
[Service]
ExecStart=ensemble-runner serve
Restart=on-failure

[Install]
WantedBy=default.target
```

and `systemctl --user enable --now ensemble-runner`. On macOS, a launchd agent
(`~/Library/LaunchAgents/dev.ensemble.runner.plist`):

```xml
<plist version="1.0"><dict>
  <key>Label</key><string>dev.ensemble.runner</string>
  <key>ProgramArguments</key><array>
    <string>ensemble-runner</string><string>serve</string>
  </array>
  <key>KeepAlive</key><true/>
</dict></plist>
```

### Sharing models

A hub may run on a small machine and need a strong model, and so may a
runner. A runner may share some models of its own servers through the hub
(`ensemble-runner model share`), and the hub and your other runners use them:
each adds the runner as a provider of `type: runner`, naming it
`<owner>/<name>`, and adds the models it wants. A model of it is named as
the runner names it, after the provider's name: `gpu:local:qwen3.5:72b`.

```console
$ ensemble-runner model share local:qwen3.5:72b            # on ana's gpu box
$ ensemble-hub runner models                               # what the connected runners share
ana/gpu-box  local:qwen3.5:72b  llm, strong  (1 call(s) at once)
$ ensemble-hub provider add gpu --type runner --runner ana/gpu-box
$ ensemble-hub model add gpu:local:qwen3.5:72b
```

- **Who may use it.** The hub, and its owner's other runners.
- **What they learn.** Nothing of its address or key: the runner calls its own
  server, and only for what an agent needs of a shared model (chats,
  completions and embeddings); it refuses a call to a model it does not
  share. Whoever runs it sees what is sent to it: choosing a runner's model
  is a decision of trust.
- **How it goes.** Calls go on a second WebSocket of each runner, beside its
  link, so a long prompt or a busy stream never holds up its tasks. The hub
  and a runner that reaches other runners serve them on loopback, where each
  looks like any OpenAI-compatible server to the agents. Through the hub a
  call takes a few milliseconds longer than one made directly.
- **When it cannot.** Each provider takes `max_concurrent` calls at once, the
  runner's own and others' together; the hub keeps the next waiting for up to
  10 seconds, then answers that it is busy. Busy, offline or not shared, the
  model is unavailable, and a run picks another.

## Harnesses and tasks

Nobody writes a team. A runner offers what its computer has: its
**harnesses**, the programs that drive an LLM to work in a repository, and
the models of its **ANN pool**. For each task, the hub's **task picker**
chooses a runner, a harness and an LLM, or an LLM alone for a task that only
needs an answer, and the task's own text is what the run is asked; Ensemble
adds no prompt of its own.

What the runner keeps while it runs is in its folder,
`~/.local/state/ensemble/runner/` (`runner/` in `--config-dir`), outside every
repository:

| where | |
| --- | --- |
| `core/` | its `database.db`, with its settings, its ANN pool and harnesses, its providers' keys and its hub tokens, sealed by `master.key` beside it |
| `workdirs/<folder>-<hash>/runs/` | what tasks did as they ran on this computer, for one repository of the workspace |
| `.../worktrees/` | a git worktree per task |
| `.../worker.lock` | the lock of the one worker that runs the repository's tasks |
| `answers/<project>/runs/` | what tasks a model answered alone did, for a project the runner has no folder for |
| `records.jsonl` | every run: its harness and model, its time and its tokens |
| `worker.lock` | the runner's own lock |

### The task picker

The **workers** a runner may run a task on are its harnesses that are on, found
and not known to be logged out, each with the LLMs it can drive:

| harness | what it drives |
| --- | --- |
| `claude-code` | its own models, `haiku`, `sonnet` and `opus` |
| `codex` | its own models, `gpt-5-codex`, `gpt-5` and `gpt-5-mini` |
| `opencode` | the pool's LLMs that are on, of an OpenAI-compatible provider, whose context window is known |
| `builtin` | the pool's LLMs that are on, whose context window is known, and that are not known to make no tool calls |

A task that only needs an answer, such as an explanation, a review or a
plan, needs no harness: every LLM of the runner's pool may also answer it
**alone**, a worker whose harness is `alone`. It is one request to the model,
with the task's title, text, goal, constraints, definition of done, the notes
recalled for it, what it asked and was answered and what came of its earlier
runs; no tools, no clone and no worktree, so a runner answers it for a project
it has no folder for. The answer is the task's result, kept on the hub as any
run's and posted to its issue for a task from a tracker, and its front matter
says `harness: alone`; an answer that ends with `NEEDS INPUT:` waits for yours.
It holds one call of its provider, and asks for at most 8192 tokens of answer,
less when the model's context window leaves less. A model answers alone only
when an answer of the task picker's questions says so (`model_alone`), and then
only a model alone works on the task; never one with a gate, which runs in a
worktree.

Each runner tells the hub its workers, with each model's tier, context
window, tool calls and the abilities you wrote (`model set --abilities`,
`harness set-model --abilities`), and how busy each harness and provider is.
With no worker at all the runner runs no task, and `ensemble-runner doctor`
says how to give it one; `ensemble-runner status` lists them.

The hub chooses the worker of each task with a **decision model**, the
task picker an admin sets (see [By what you mean](#by-what-you-mean)): once a
task may start, the decision models are asked in order, the next when one
fails, first the questions the admin wrote, then which worker, among the
workers of your runners connected now that the answers allow (a runner that
gave the task back is left out), at most 100, the busiest and the weakest
left out first. What they are told is the task's project, title, text,
definition of done, goal and constraints, and what came of its earlier runs:
**the hub sends your task's text to the decision model's provider**, a third
party for TypeSafe's Jev, your own computer for Laya's `laya-serve`. The
workers at least as likely as the picker's least confidence are its
candidates, one an answer prefers first; the task is offered to the first
of them whose harness and provider are free, that runner alone claims it,
and it runs exactly that harness and model. With every candidate busy, the
task waits for one.

When the decision model is not sure (an answer, or every worker, less likely
than its least confidence), or no worker fits the answers, the task waits for
**you to choose**: its `pick` is `needs_choice`, with the workers ranked, and
you choose one with `choose` (MCP), `task choose` or the task's page. Your
choice is kept with the decision, to learn from later. With no task picker,
or no decision model answering, a task waits, saying why in its `pick`, and is
tried again later; no task is handed out by rules. Each decision, with every
answer's probabilities, the workers ranked and the decision model that
answered, is kept with the task (`decision`).

A change to the task picker, to its decision models, or to the workers
connected has the tasks decided and not yet offered, or waiting for your
choice, chosen for again; your choice stays while its worker is there. A
runner that goes away before it starts a task offered to it, or gives it
back, has it chosen for again without it. When a task's gate fails and it
has runs left, its work is pushed and its next run is chosen for again, told
how the gate failed.

### How each harness runs

A run works in the task's own git worktree, where its harness may read, edit
and run commands; nothing else limits it, so Ensemble adds no sandbox. Its
time limit and its steps come from the harness entry (`harness set`). A cancel
or the time limit kills the CLI and its whole process tree.

- **`claude-code`**: `claude -p` with `--output-format stream-json`, the
  model, `--max-turns` its steps, `--allowedTools Read,Edit,Write,Glob,Grep,Bash`
  and `--permission-mode dontAsk`, on your own subscription. It never passes
  `--bare`, which bypasses the saved login, and never touches Claude Code's
  credentials.
- **`codex`**: `codex exec --json --sandbox workspace-write`, on your own
  login. **Not yet checked against a real Codex release**: the event shapes
  follow its documented output and are covered by stand-ins and a saved
  stream; `ENSEMBLE_REAL_CLI=1 cargo test --test real_cli -- --ignored` is
  the first run against the real CLI.
- **`opencode`**: `opencode run --standalone --format json`, with an agent of
  its own in `OPENCODE_CONFIG_CONTENT`: its steps, and a permission set that
  denies everything but `read`, `glob`, `grep`, `list`, `edit` and `bash`.
  OpenCode keeps its own system prompt. The model's provider goes in the same
  inline config, its key with it, for that run alone.
- **`builtin`**: an agent loop in Ensemble itself
  ([harness-rs](https://github.com/liliang-cn/harness-rs), pinned to one
  exact version), on a model of the pool: the model asks for tools (`read`,
  `list`, `grep`, `glob`, `write`, `edit`, `shell`), Ensemble runs them in the
  worktree and answers, until the model is done or its steps run out, each
  answer at most `max_tokens`. The file tools cannot leave the worktree;
  `shell` runs any program there.

### What a run reports

A run's result, which a task keeps, ends with the harness and model, how many
steps and tool calls it used, every file it added, modified or deleted, and
the shell commands it ran:

```text
[opencode on local:qwen3.5:9b: 6 step(s), 5 tool call(s)]
[files changed: src/lib.rs (modified), tests/new.rs (added)]
[commands run: cargo test]
```

Changed files are found by comparing the worktree before and after the run
(`.git`, `target` and `node_modules` are not compared).

### What the runs cost

Every run, whether a task or a gate retry, leaves one line in the runner's
`records.jsonl`: the harness and the model, when it started and how long it
took, the tokens, the steps and tool calls, and whether it worked.
`ensemble-runner stats` sums them per harness and model:

```console
$ ensemble-runner stats --since 7d
run                          runs  failures    total s    mean s   tokens in  tokens out
claude-code on sonnet           9         0      402.7      44.7           -           -
opencode on qwen3.5:9b         12         1     1843.2     153.6      418220       30114
```

A `-` means no run reported tokens: a CLI or a server that says nothing is
not the same as one that spent none. `--since` takes `30m`, `12h`, `7d` or
`2w`; `--json` prints the same as data.

### Tasks

Your runners work on **tasks**: tickets kept on the hub, each for a project,
a repository's name. Your agent makes one with `submit`, and the hub's inbox
makes one from a tracker's issue (see
[Tasks from a tracker](#tasks-from-a-tracker)). Ids are numbered within the
project, so an id names one task. `submit` answers at once with the id; the
work goes on without your agent's conversation. A task names no harness and
no model: the hub's task picker chooses them (see
[The task picker](#the-task-picker)).

Each of your runners with a repository of the project's name in its
workspace has a worker for it, and a runner with none gets one for a task
that names its `repository` (see [Runners](#runners)).
While the runner runs, the worker works through the project's tasks that may
start, oldest id first, as pairs are free, and the hub keeps each one's
result. Your agent follows them with `tasks` and `task`. `cancel` stops one: a
task that has not started gets a `cancelled` result and never runs, and the
runner that runs one kills its CLI and the CLI's process tree.

**Tasks are kept on the hub, and runners lease them.** That is what lets a
task outlive the runner that runs it:

- A runner **claims** a task that may start (not ended, not waiting for an
  answer or another task) for a minute, renews the claim every 15 seconds
  while it runs, and tells the hub how the run ended. While it holds the
  claim, the task shows as running on that runner, and no other runner runs
  it.
- A runner that **stops** puts its running tasks back: they show as
  interrupted on it, and the next runner to look runs them again. A runner
  that **dies** stops renewing, and its claim runs out within a minute, with
  the same effect.
- Any of your runners that runs tasks takes a task that may start, in the
  repository of the project's name in its workspace, cloned there first when
  it has none. So a task you make at your desk runs on your laptop if the desk
  is off. When none of your runners that run tasks is connected, or each gave
  the task back, it waits. To have a task worked on by someone else's
  computer, it is reassigned to them (to come).
- A runner whose claim ran out, and whose task another runner took meanwhile,
  cannot say how it ended: the task is never done twice.
- A runner with no hub has no tasks.

**Where the work is decides who goes on with it.** Every task works on a
branch of its own, `ensemble/<id>`, in a git worktree, `worktrees/<id>` in the
repository's state directory, made from the repository's `HEAD`; the
repository's own files are not touched, so tasks run side by side. In a
repository with a remote `origin`, that branch can move: whenever a run of it
stops (put back, asking, or ended), the runner commits what it did so far on
the branch and pushes it to `origin`. The next run, on any of your runners,
fetches the branch and carries on from there, told the task was started
elsewhere and to go on, not start over. Without a remote, the task's work is
in one computer's repository, so once it has run there it stays with that
runner. A task never pushes anywhere but `origin`, and only its own branch.
When it ends, everything it changed is committed on its branch
(`--no-verify`, your git identity) and the result records `branch` and
`commit`. Ensemble never merges: you look at the branch and merge it with git.
The repository, or the task's `repo`, must be a git repository with at least
one commit.

A task's fields; only `project`, `title`, `task` and `definition_of_done` are needed:

| field | |
| --- | --- |
| `project` | required. The repository's name: its folder in a runner's workspace |
| `repository` | where the project lives: a git URL or path, such as `git@github.com:acme/shop.git`, that a runner with no copy clones it from. Without one, such a runner asks |
| `title` | required. A short name; it makes the task's id (`0002-add-retry-to-the-fetcher`) |
| `task` | required. What to do. The run cannot see your conversation, so it says everything needed |
| `definition_of_done` | required. What must be true when it is done, in your words. The run is told to check its work against it; nothing runs it (that is what `gate` is for) |
| `context` | background the run needs, sent with the task |
| `goal` | the outcome wanted, in a sentence, as opposed to the steps |
| `constraints` | what the run must not do or must keep |
| `files` | where to look or work |
| `after` | ids of tasks that must end `done` first; each must exist already |
| `repo` | a folder inside the repository to work in, such as one repository of several |
| `gate` | a check its work must pass: `command`, a shell command (`sh -c`) run in the task's worktree, passing on exit status 0, its output (stdout and stderr together) what the next run is shown; `timeout_seconds`, how long it may run before it is stopped and counts as failed; `attempts`, how many runs the task gets in all, the first included, so `1` means no retry. `gate` (or `task.gate`) changes it, or takes it away, while the task neither runs nor has ended |

The run is sent the text, then the goal, constraints and files, then the
definition of done it checks its work against, then the task's notes. A task with
`after` waits until every task it names has ended `done`. If one of them
failed or was cancelled, it stays blocked until it is submitted again.

**Using a gate.** A gate is a check a task's work must pass, so a run that
says "done" too early is sent back instead of being believed. With `gate`,
its command runs once the run is done:

1. It passes: the task is `done`.
2. It fails: its work is kept on its branch, and the task runs again, on a
   worker the hub's task picker chooses again, told of the failed run, and
   the run told the command, how it ended, the last 60 lines of its output
   and the previous reply.
3. This repeats until it passes or the task has had `attempts` runs in all.
   Then it is `failed`.

The result records `gate`, `gate_result` (`passed`, `failed`, or `not run`
when the run itself failed) and `attempts`. A gate is a command that runs on
your runner, as the harness's own shell could, so only you give your task
one: when you make it (`submit`, `task.create`, the web UI's form) or with
`gate` (`task.gate`, the task's page). A task made from a tracker's issue has
none, since anyone who may write in the tracker writes its issues; give it one
yourself. Your tasks run only on your own runners.

**Giving a task to someone.** A task runs on its account's runners. To have
someone else's take it, give it to them with `reassign` (`task.reassign`, or
"Give it to someone" on the task's page), naming their account, which must
have `tasks`. It becomes a task of theirs in the same project, with an id
among their tasks; the notes the hub put into it are recalled again as they
may see them, so none of yours goes with it; and the task keeps who gave it to
whom (`moves`). Not while it runs or after it has ended, nor when it waits for
another task or another waits for it. A gate goes with it, but it is your
command running on their computers, so the task waits (`gate_unconfirmed`,
"confirm its gate" in the web UI) until they confirm it (`confirm_gate`,
`task.confirm-gate`) or change it; a task without a gate is theirs to run at
once.

A result starts with `status` (`done`, `failed` or `cancelled`), the
`harness` and `model` of its last run, `started` and `ended`, then the reply.
A task with a result is over; submit it again to run it again. What a run does
as it goes, its activity log, is in `runs/<id>/` in the repository's state
directory on the computer that ran it.

A run may stop to ask. It is told that, when it cannot finish without a
decision only the person who asked can make, it should end its reply with a
line `NEEDS INPUT: <question>`. The hub keeps the question with the task, and
`tasks` and `task` show it. `answer` adds the answer, and the task runs again,
given the questions and answers so far and its reply when it stopped. A gate
does not run on a reply that asks, and its worktree is not committed until the
task ends. Your agent answers through `answer`, or you with a comment on the
issue a task came from.

### Secrets

API keys are named by the **variable** that holds them (`api_key_env`), never
written in the runner's settings or the hub's: there is no `api_key` field,
and settings that try one are refused by name. `provider add` and `provider
key` write both for you: the key where it is kept, and its variable in the
settings (see [Changing settings](#changing-settings)).

A key given to `provider add` or `provider key` is kept sealed in the runner's
database, with AES-256-GCM, by its master key,
`~/.local/state/ensemble/runner/core/master.key`, made the first time a key is
kept and readable by you alone; so are the runner's hub tokens, which
`ensemble-runner login` and `join` keep. A hub keeps the keys it is given the
same way, by its own `hub/core/master.key`, as a GitLab instance keeps its
secrets. Back up the whole folder; without `master.key`, give each provider
its key again, and sign the runner in again.

A key a secrets manager keeps is read from the environment, and named with
`--key-env`: the hub and the runner are separate services, a team's hub often
on another computer or of another user, so each reads only its own secrets. A
secret that sops-nix or agenix puts in `/run` reaches it as the service's
environment (`environmentFile` in Nix, systemd's `EnvironmentFile=`). A
variable set in the environment wins over a key the hub or the runner keeps.

A provider's key reaches a harness's CLI only as that provider's key in
`OPENCODE_CONFIG_CONTENT`, in the CLI's environment for that run, so it cannot
change how the CLIs log in.

## Changing settings

There is no settings file to open, nor a variable to name. The settings of a
hub and of a runner are kept in their databases, as typed rows, and what you
mean to change is an operation, the same from the command line, the REST API
(`POST /api/v1/ops/<name>`) and the web UI, each checked as it is made.
No setting is a file.

### By what you mean

```console
$ ensemble-hub provider add central --type openai-compatible-api --url http://127.0.0.1:11434/v1 --max-concurrent 2
The provider's API key (empty for none): ••••••       # asked when it wants one, without echo
added provider central, which serves central:qwen3.5:9b, central:embeddinggemma; add the ones to use, as `model add central:qwen3.5:9b`
$ ensemble-hub provider key central                   # a new key, or empty to take it out
$ ensemble-hub provider list | test [name] | models <name> | refresh [name] | remove <name>
$ ensemble-hub model add central:qwen3.5:9b --tier medium   # model remove | set
$ ensemble-hub model add central:embeddinggemma
$ ensemble-hub provider add laya --type decision-api --url http://127.0.0.1:8000 --max-concurrent 4
$ ensemble-hub model add laya:laya                    # a decision model
$ ensemble-hub model list [provider]                  # each model added: what it is, missing
$ ensemble-hub knowledge models central:qwen3.5:9b claude:claude-haiku-4-5
$ ensemble-hub knowledge embedding central:embeddinggemma   # or --off
$ ensemble-hub knowledge recall --max-notes 8 --max-chars 6000
$ ensemble-hub tracker add issues --type local_markdown --dir issues      # tracker list | remove
$ ensemble-hub role set auditor recall,share                              # role remove
$ ensemble-hub registration set approval --max-pending 20                 # or closed
$ ensemble-hub source filter central:qwen3.5-72b --prompt "Salaries; clients' names."  # or --off
$ ensemble-hub source try "Bob: deploys go out on Tuesdays now"
$ ensemble-hub picker set laya:laya --min-confidence 0.6      # the task picker; picker show, or --off
$ ensemble-hub picker template basic                         # how hard, and whether it runs tools
$ ensemble-hub picker question kind --type choice --instructions "What does the task change?" \
    --min-confidence 0.5 --answers $'code: It changes the code.\ndocs: The documentation alone.' \
    --effects code:min_tier=medium,docs:prefer=opencode       # picker remove-question
$ ensemble-runner provider add local --type openai-compatible-api --url http://127.0.0.1:11434/v1 --max-concurrent 1
$ ensemble-runner model share local:qwen3.5:9b      # model unshare
$ ensemble-runner harness add builtin --max-seconds 1800 --max-steps 50 --max-concurrent 1 --max-tokens 4000   # harness remove | set | refresh | add-model | remove-model
```

- **A provider is asked first.** `provider add` asks it which models it serves
  before anything is written, so a wrong address or key is said at once;
  `--no-check` adds it anyway. One that answers that it wants a key has the
  key asked for. `--type runner` names another runner of yours, whose shared
  models the hub or the runner reaches through the hub (`--runner
  <owner>/<name>`), only while it runs.
- **A decision model's API is the hub's.** `--type decision-api` is a
  decision model's API, `POST /v1/systemone`: a state and typed questions
  in, typed answers with their probabilities out. Laya's `laya-serve`
  (`pip install "laya[serve]"`, Apache 2.0) answers it on a CPU, on a
  computer of yours; TypeSafe's Jev (`--url https://api.typesafe.ai`, a key)
  answers it as a service, and so sees what it is asked. What such a provider
  serves are decision models, and `provider test` asks one a yes or no. A
  runner keeps none: the hub decides.
- **The task picker is the hub's, one for the hub.** `picker set` names the
  decision models of the hub's pool asked, in order, the next when one fails,
  and the least probability the worker chosen must have. `picker question`
  adds a question they are asked of each task first: a `choice`, a `score`
  (its answers lowest first) or a `noul` (yes or no), each answer `name: what
  it means`, its least confidence, and what each answer means for the worker,
  from a fixed list: `min_tier=<small|medium|strong>`, `min_context=<tokens>`,
  `needs_tools`, `prefer=<harness or model>`, `avoid=<harness or model>`,
  and `model_alone`: the task only needs an answer, and a model alone
  answers it. `picker template basic` asks whether the task only needs an
  answer, how hard it is and whether it needs to run tools, and replaces the
  questions with that start to change; no
  template is ever used unless chosen. A decision model the picker uses is not
  removed.
- **Nothing is in the pool until it is added.** `provider models` lists what
  a provider serves now, and keeps none of it; `model add` adds one, once the
  provider lists it. What it is, an LLM or an embedding model, is what Ollama
  says of it, what Ensemble knows of its family, or, when nothing says, asked
  once as it is added. An LLM's tier, small, medium or strong, comes from a
  catalog of known families and sizes, else from Ollama's, else medium;
  `model add --tier` or `model set` says it. The providers are asked again on
  `provider refresh` and whenever the hub or the runner starts, only to mark
  a model added that one no longer lists missing, never forgotten.
- **Keys are kept for you.** A key you give is kept under a variable Ensemble
  names after the provider (`ENSEMBLE_PROVIDER_CENTRAL_KEY`), and the provider
  names that variable; `provider remove` takes it out too. The hub and the
  runner each keep it sealed in their database, by their `master.key` (see
  [Secrets](#secrets)). A key you keep elsewhere already, such as one a
  secrets manager puts in the environment (`environmentFile` in Nix), is
  named instead with `--key-env NAME`.
- **Models are checked too.** `knowledge models` takes only LLMs of the pool,
  and asks each provider whether it serves its model now; `knowledge
  embedding` takes only an embedding model of the pool, and asks it to embed
  once.
- **What something uses stays.** A provider or a model the distiller, the
  embedding model or the source filter uses is not removed, nor a role an
  account has.

### Every other setting

```console
$ ensemble-hub settings owner acme
$ ensemble-hub settings sessions 12                  # hours a login lasts
$ ensemble-hub settings lockout --failures 5 --minutes 15
$ ensemble-runner settings name laptop
$ ensemble-runner settings workspace ~/code
$ ensemble-hub settings show [--json]                # every setting; ensemble-runner's alike
$ ensemble-hub settings history                      # who changed what, when, with which operation
$ ensemble-hub settings import "$(cat hub.json)"     # settings another hub's `settings show --json` printed
```

Every operation prints JSON with `--json`, for a script. A change is checked
before it is kept, and one that would not load is refused with what is wrong,
and nothing changes. Each is kept in the audit log with who made it, by which
operation, with what input, its secrets hidden.

While the hub or the runner runs, `ensemble-hub …` and `ensemble-runner …` ask it, over its socket, and it
makes the change itself and uses it at once: the hub's roles, how
people sign up and how long a login lasts, its models and knowledge settings,
its owner. A change the hub cannot use as it runs (its trackers) has it start
again, in the same process;
runners connect again by themselves. A change to the runner's settings has it
start again, in the same process: running tasks start again from where their
work was last pushed. When neither runs, the command makes the change itself, in
its database, and the hub or the runner reads it when it starts. `ensemble-runner
login <url>` names the hub in the runner's settings by itself, and `ensemble-runner
logout` takes it out.

The command line is always on, for a hub and for a runner, and has no
settings: neither names it.

## Installation

Ensemble is two programs: `ensemble-runner`, this computer's runner, and
`ensemble-hub`, a hub. A computer has the one it runs, or both. They are built
and tested on Linux, and they are Unix-only: the runner runs agent CLIs as
process groups and gates with `sh -c`, and each reaches its running process on
a Unix socket.

| platform | how | keeps the runner running |
| --- | --- | --- |
| NixOS | [the NixOS module](#nixos) | a systemd user service |
| Linux with Nix | [`nix profile install`](#linux-with-nix), or [Home Manager](#home-manager-linux-and-macos) | a systemd user service |
| Linux | [cargo](#linux-and-macos-with-cargo) | [a systemd user service](#running-the-runner) |
| macOS | [cargo](#linux-and-macos-with-cargo), or Nix with [Home Manager](#home-manager-linux-and-macos) | a launchd agent |
| Windows | [WSL 2](#windows-through-wsl-2), then as on Linux | a systemd user service inside WSL |

The macOS build is made by the same flake and the same cargo, but the tests run
on Linux; tell us what breaks. Native Windows is not supported: each
executable says so and exits.

On every platform Ensemble keeps its files in the same place
(`~/.local/state/ensemble/`, `hub/` and `runner/`, each its own; another
with `--config-dir`) and is managed with the same commands. The Nix modules
only run the hub and the runner: their settings are kept in their databases,
and made and changed with `ensemble-hub` and `ensemble-runner`.

Besides Ensemble, install the CLIs of the harnesses you add: `claude` for
`claude-code`, `opencode` for `opencode`, `codex` for `codex`. `builtin` needs
none. git is needed for the workspace, the repositories a runner clones and
worktrees. Then run `ensemble-runner setup`
([Quick start](#quick-start)). The computer of your agent CLI needs nothing of
Ensemble: the agent reaches the hub over MCP.

### NixOS

The repository is a flake with a NixOS module. Add it as an input of your
system flake, and import the module:

```nix
# flake.nix
{
  inputs.ensemble = {
    url = "github:Khachatur01/ensemble";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, ensemble, ... }: {
    nixosConfigurations.laptop = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [ ensemble.nixosModules.default ./configuration.nix ];
    };
  };
}
```

The module has three parts, each enabled on its own: a runner, a hub and the
hub's web UI. The runner and the hub each run as the person it is
for, from their `~/.local/state/ensemble/`, so they manage it with `ensemble-hub` and
`ensemble-runner` as on any platform: `ensemble-hub user add`, `ensemble-runner
harness add`, `ensemble-runner settings …` and the rest, as themselves, with no
`sudo`.

Neither has settings in Nix, as one runs a GitLab instance: the settings of
each, and the keys of its providers, are kept in its `core/database.db`,
sealed by its `master.key`, and changed with its commands (a hub's also with
its REST API or a web UI). Back up their folders.

**This computer's runner**, a systemd user service of one person:

```nix
# configuration.nix
{ pkgs, ... }:
{
  services.ensemble.runner = {
    enable = true;
    user = "ana";                          # it runs for her alone
    extraPackages = [ pkgs.claude-code ];  # the agent CLIs on its PATH; git is always there
    environmentFile = "/run/secrets/ensemble-runner.env";  # keys a secrets manager keeps, put in its environment
  };

  # A user service runs only while she is logged in, unless she lingers.
  users.users.ana.linger = true;
}
```

The first time it starts, `ensemble-runner serve` makes a runner named for
this computer, working for her in `~/ensemble`, offering nothing.
Then, as `ana`: `ensemble-runner login <url> --user ana`, `ensemble-runner
provider add …`, `ensemble-runner model add …`, `ensemble-runner harness add …`
and `ensemble-runner settings …`; the running runner uses each at once. `ensemble-runner` is put on the system's `PATH`, for managing it;
`services.ensemble.runner.package` is the package it comes from.

**A hub**, a systemd user service of `user`, from their
`~/.local/state/ensemble/hub/`. The module makes `user` linger, so the hub starts
at boot without a login, and, as a user service, it has their
`XDG_RUNTIME_DIR`, where `ensemble-hub …` finds its socket:

```nix
{
  services.ensemble.hub = {
    enable = true;
    user = "ana";                          # their hub: they run `ensemble-hub …` as themselves
    listenAddress = "127.0.0.1";           # `serve --http-host`: 0.0.0.0 for other computers too
    port = 7700;                           # `serve --http-port`
    openFirewall = false;                  # opens `port`
    environmentFile = "/run/secrets/ensemble-hub.env";  # keys a secrets manager keeps, put in its environment
  };
}
```

The module only runs the hub, as one runs a GitLab instance: its settings and
the keys of its providers are kept in its `database.db`, and changed with
`ensemble-hub …` as `user`, its REST API or a web UI, never in Nix; only
where it listens is. The first time it starts, `ensemble-hub serve` makes the
settings a new hub starts with, every one of them, named for `user`, and its
vault. Then, as `user`: `ensemble-hub setup`, which adds the first admin, and
`ensemble-hub provider add …`, `ensemble-hub model add …`,
`ensemble-hub knowledge models …` and the rest. To reach it from other
computers, give the module `listenAddress = "0.0.0.0";` and
`openFirewall = true;`. Back up the hub's folder: without `core/master.key`,
no key the hub keeps can be read.
`ensemble-hub` is put on the system's `PATH`, from
`services.ensemble.hub.package`. For a hub of an account of its own, make that
user and run `ensemble-hub …` as it.

**The web UI**, optional, a system service with no state of its own:

```nix
{
  services.ensemble.ui = {
    enable = true;
    hub = "http://127.0.0.1:7700";         # `--hub`: this computer's hub, by default
    listenAddress = "127.0.0.1";           # `--http-host`
    port = 7701;                           # `--http-port`
    openFirewall = false;                  # opens `port`
  };
}
```

For the internet, put a TLS proxy in front (`services.caddy`, see
[The hub](#the-hub)) and leave `openFirewall` off. A whole configuration that
runs a hub for one person, its web UI and a runner that names it is in
[`nix/example.nix`](nix/example.nix); the flake's checks build it.

### Home Manager (Linux and macOS)

On any Linux with Nix, and on macOS with Nix (with or without nix-darwin), the
Home Manager module runs this computer's runner: a systemd user service,
`ensemble-runner`, on Linux, and a launchd agent, `ensemble-runner`, on macOS
(its log in `~/Library/Logs/ensemble-runner.log`). Its
`services.ensemble.runner` takes the same `environmentFile` and
`extraPackages` as the NixOS module's runner, without `user`:

```nix
# home.nix, with ensemble.homeManagerModules.default in the modules
{ pkgs, ... }:
{
  services.ensemble.runner = {
    enable = true;
    extraPackages = [ pkgs.opencode ];
  };
}
```

`ensemble-runner` is put in `home.packages`. As with NixOS, the runner makes
itself, named for this computer, the first time it starts, and is set up
with `ensemble-runner` (or run `ensemble-runner setup` first, to name it and
its workspace yourself). A hub runs on
NixOS through the NixOS module, or anywhere with `ensemble-hub serve`.

### Linux with Nix

Without NixOS or Home Manager:

```console
$ nix profile install github:Khachatur01/ensemble  # both executables, on PATH
$ nix profile install github:Khachatur01/ensemble#ensemble-runner   # the runner alone
$ nix shell github:Khachatur01/ensemble            # on PATH for this shell only
$ nix run github:Khachatur01/ensemble -- doctor    # one command of ensemble-runner
```

`nix profile upgrade ensemble` updates it. Keep the runner running as in
[Running the runner](#running-the-runner).

### Linux and macOS with cargo

Needs Rust 1.92 or newer (edition 2024; [rustup](https://rustup.rs) installs
it) and a C toolchain: `build-essential` on Debian and Ubuntu, `base-devel` on
Arch, the Xcode command line tools on macOS (`xcode-select --install`).

```console
$ cargo install --git https://github.com/Khachatur01/ensemble
```

This puts `ensemble-runner` and `ensemble-hub` in `~/.cargo/bin`, which must be
on `PATH`; `--bin ensemble-runner` installs the runner alone. Run the same
command again to update. Ensemble is not published on crates.io. The web UI
is not among them: from a checkout, in the dev shell, `ui/build.sh` builds its
app, and `cargo run --manifest-path ui/Cargo.toml --release -- --dist ui/dist`
serves it ([The web UI](#the-web-ui)). Keep the runner
running with a systemd user unit on Linux, or a launchd agent on macOS, both in
[Running the runner](#running-the-runner).

### Windows, through WSL 2

Ensemble runs inside WSL 2, Linux on Windows, with the agent CLIs it starts.
Claude Code, OpenCode and Codex all run in WSL.

1. In PowerShell, as administrator: `wsl --install -d Ubuntu`, then restart and
   make your Linux user when Ubuntu first opens.
2. Let WSL run systemd, so the runner can be a user service: in Ubuntu, put
   this in `/etc/wsl.conf`, then run `wsl --shutdown` in PowerShell and open
   Ubuntu again:

   ```ini
   [boot]
   systemd=true
   ```

3. In Ubuntu, install Ensemble with [cargo](#linux-and-macos-with-cargo) (after
   `sudo apt install build-essential git` and rustup) or
   [Nix](#linux-with-nix), and the agent CLIs you use, inside WSL too.
4. Run `ensemble-runner setup`, and keep the runner running with the systemd
   user unit in [Running the runner](#running-the-runner) (`loginctl
   enable-linger $USER` keeps it running while no Ubuntu window is open).

Keep the workspace inside WSL (`~/ensemble`), not under `/mnt/c`: the agents,
git and Ensemble are much faster there, and file changes are seen at once.
Editors reach it through their WSL support (VS Code's WSL extension, JetBrains
Gateway). A hub runs in WSL the same way; for other computers to reach it, it
must listen on an address Windows forwards, or run on another machine.

## Usage

Day to day you do not run Ensemble; your agent talks to the hub, and your
runner works. The commands are for setting it up, for looking at what it does,
and for what only you decide. They are grouped by what they act on, and each
has its own `--help`:

```console
# This computer's runner
$ ensemble-runner setup [--name N] [--workspace PATH] [--yes]   # find the CLIs, write the runner, say what it runs tasks on
$ ensemble-runner init [--force|--keep]   # the runner setup makes, asking nothing
$ ensemble-runner serve                     # run the runner until stopped; a computer runs one
$ ensemble-runner status                    # its name, whether it runs, its workspace and projects, its hub: signed in as whom, what it runs tasks on
$ ensemble-runner doctor                    # what is wrong, and how to fix each thing
$ ensemble-runner stats [--since 7d] [--json]   # runs, failures, time, tokens, by harness and model
$ ensemble-runner login <wss://url> --user <name>   # sign in to a hub with your account (or --token <token>), and name it in the runner's settings
$ ensemble-runner join <wss://url> <code>   # the same with a join code
$ ensemble-runner logout <wss://url>
$ ensemble-runner peers                     # your runners connected to your hub now, with their harnesses and models
$ ensemble-runner settings show|history [--json]   # its settings, and who changed them
$ ensemble-runner settings name|workspace <value>
$ ensemble-runner settings import <json>   # settings `settings show --json` printed, whole
$ ensemble-runner provider list|add|test|key|refresh|remove   # the providers of its ANN pool, and their keys
$ ensemble-runner model list|add|remove|set|share|unshare   # their models: what each is, its abilities, which it shares
$ ensemble-runner harness list|add|remove|set|refresh|add-model|set-model|remove-model   # its harnesses: which are found, a run's limits, Claude Code's and Codex's models


# A hub this computer runs
$ ensemble-hub setup [--owner N] [--admin A] [--yes]   # make the hub and its first admin, with their token
$ ensemble-hub init [--force|--keep] [--owner N]   # the settings a new hub starts with, and its vault
$ ensemble-hub check                      # validate its settings; which model would distill now
$ ensemble-hub serve [--http-host A] [--http-port P]   # run the hub until stopped

# The web UI, a component of its own
$ ensemble-ui [--hub URL] [--http-host A] [--http-port P] [--dist D]   # serve it, for the hub at URL (http://127.0.0.1:7700)
$ ensemble-hub user list
$ ensemble-hub user show <name>
$ ensemble-hub role list                  # the roles, and what each may do
$ ensemble-hub user add <name> --role <role>        # prints their token once
$ ensemble-hub user approve|role <name> --role <role>
$ ensemble-hub user password|token|revoke|remove <name>   # a password is asked, or read from stdin; a token comes with the line for an agent
$ ensemble-hub user join-code <name>      # a code, shown once, that signs one client, such as a runner, in within 24 hours
$ ensemble-hub user tracker <name> [<tracker name>]
$ ensemble-hub knowledge status|distill|rebuild
$ ensemble-hub settings show|history [--json]    # its settings, and who changed them
$ ensemble-hub settings owner|sessions|lockout …
$ ensemble-hub settings import <json>     # settings `settings show --json` printed, whole
$ ensemble-hub provider list|add|test|key|refresh|remove   # the providers of its ANN pool, and their keys
$ ensemble-hub model list|add|remove|set      # their models: what each is, and its abilities
$ ensemble-hub runner models              # the models the connected runners share

$ ensemble-runner completions zsh|bash|fish|elvish|powershell   # a completion script for your shell
$ ensemble-hub completions zsh|bash|fish|elvish|powershell
```

Every command takes `--config-dir <path>`, the folder everything is kept in
(by default `~/.local/state/ensemble`; the hub in its `hub/`, the runner in
its `runner/`).

## How it fits together

A Cargo workspace of eleven crates under `crates/`, each depending only on
the ones before it, and the root crate, which builds the two executables and
holds the end-to-end tests: `ensemble-core` (shared types, state, runtime),
`ensemble-ops` (the operations of a hub and a runner, defined once, that the
command lines and the REST API are made from), `ensemble-config`
(the hub's settings and the runner's, with their ANN pools and harnesses), `ensemble-backends`
(model servers), `ensemble-store` (the databases: the settings and sealed
keys of a hub or a runner, and the hub's accounts and tasks), `ensemble-knowledge` (the
vault and its pipeline), `ensemble-link` (the link between a runner and a hub:
its messages, and the runner's side), `ensemble-agents` (the harnesses, the
runner's means and the router), `ensemble-tasks` (tasks as a runner leases them, and the worker that
runs a repository's tasks), `ensemble-runner` (the runner: its commands, its
workers, the projects it clones, its teams, and the models it shares) and
`ensemble-hub`
(the hub: accounts, the tasks it keeps, its task picker, the tracker inbox,
MCP, the REST API and the server runners connect to). The runner is built of none of the hub's
crates (`ensemble-hub`, `ensemble-knowledge`);
`tests/workspace.rs` checks that, and the order.
[ARCHITECTURE.md](ARCHITECTURE.md) says what each file does and how the pieces
work together at run time.

The settings of a hub and of a runner have no file, nor schema: they are
typed rows of their databases (`ensemble-store`'s `kept.rs`).

A task's worker knows the runner's means and the router; it does not know which CLIs
exist. Only the adapters in `crates/ensemble-agents/src/external/` know how a
CLI is started and read.

### Errors are one vocabulary

An agent CLI that would not start and a model server that returned 503 are the
same event to whoever reads the result: this run did not answer, and here is
why. So there is one `port::Error`, each value carrying where it came from,
knowing whether a retry could plausibly work, and written to be read by the
model that will see it. A failed run is the task's result, saying why and
whether retrying could work. A tool of the hub's MCP that fails reaches the
model as a tool *result* with `isError: true`, never as a JSON-RPC error: a
transport error is handled by the harness and the model never learns the
reason, which is the one thing it needs in order to do something sensible
next.

## Tests

```console
$ nix develop   # cargo, rustc, clippy, rust-analyzer
$ cargo test
```

None of the tests need the network, a model server or an agent CLI: shell
scripts stand in for the CLIs.

| suite | what it covers |
| --- | --- |
| `crates/ensemble-config/src/tests.rs` | every validation rule of a hub's and a runner's settings, environment precedence, secrets, typos, the harnesses' limits |
| `crates/ensemble-agents/src/external/` | each CLI's command line, generated config and event stream, denials, limits, failures, the time limit killing the process tree |
| `crates/ensemble-agents/src/means.rs`, `router.rs`, `crates/ensemble-backends/` | the pairs each harness drives, the one the hub chose taken while free and held, a run recorded with its harness and model, the chat request in both formats; what a provider lists, as Ollama says or the catalog knows; a model new to a provider kept off, and one gone marked missing |
| `crates/ensemble-knowledge/` | shares kept as they arrived, notes and the log, commits by the lock holder, the secret check; the pipeline against a stub model: new notes, merges, chatter skipped, unusable answers to review, waiting for a model, rebuild |
| `ui/server/` | the app's files served with its pages and nothing outside them, a call to the API passed on as from the hub's origin only from a page of the UI, the hub's cookies Secure behind TLS |
| `crates/ensemble-hub/` | sign-up, approval and the REST API's codes, a browser's session in a cookie no page of another origin may use, an account's tasks, runners and knowledge as operations, tokens kept as hashes and revoked at once, join codes spent once and before they expire, a runner signing in over the link with a password (and the lockout) or a code, a link without a token refused all else, a runner of another protocol version refused with what to update, roles checked per request, shares under the signed-in name; the hub's MCP: a token needed, the tools a token allows, share and recall as its account, tasks made and followed, with a `repository` and a `gate`, a gate changed or taken away; every connected runner listed to its owner alone with what it runs tasks on, model calls streamed from a runner; tasks on the hub: ids, leases taken, renewed, run out and taken over, results said once and only by the runner holding the lease with the harness and model of the run, questions, cancelling, tasks given back and blocked until a runner that has not connects or they are answered again, the answer to where a project is kept as its repository, the tasks waiting for a runner in every project, the notes recalled into a new task, over the link as the signed-in account; the inbox: markdown issues read and written with only the status line changed, issues to tasks of the account matched by tracker name, questions and answers through comments, results posted once, a closed issue cancelling its task |
| `crates/ensemble-link/` | the token file 0600, the link opened again when it breaks, a hub of another protocol version named with what to update |
| `crates/ensemble-core/` | the state directories and their hashes, the worker lock, run records |
| `crates/ensemble-tasks/` | tickets, questions in a reply, run logs |
| `crates/ensemble-hub/src/dispatch.rs` | the task picker against a stub decision model: a task waiting with no picker or no decision model answering, offered to the worker it is sure of, chosen by a person when it is not, its answers narrowing the workers, a busy worker waited for, a change sending what was decided back, a runner that gave it back or went away left out |
| `crates/ensemble-hub/tests/queue.rs` | the worker of a repository: tasks run in worktrees within a harness's `max_concurrent`, closing the worker interrupting running tasks, cancel, `after`, gates, `repo`, questions and answers, a gate that fails sending the task back to be chosen for again, a task offered on a worker the runner does not have given back |
| `tests/workspace.rs` | the crates depend on each other in one direction only, and the runner on none of the hub's |
| `tests/end_to_end.rs` | the real executables, `ensemble-hub` and `ensemble-runner`: their commands; setup and doctor against a stub server; every run in `stats`; the runner working on the tasks of a repository in its workspace, a task made through the hub's MCP; a task resumed when the runner starts again; a task stopped on one computer and carried on by another from its pushed branch; an issue from the tracker worked on a member's runner; a repository added to the workspace while the runner runs, a runner listed with what it runs tasks on, and a task's notes in its prompt; a task's repository cloned and its branch pushed there; a runner asking where a project is and cloning the answer; a task whose repository cannot be cloned given back and blocked; a runner's models through the hub, run by another runner's `builtin`; signing in and joining with a code, a hub refusing another protocol or a bad token, settings and harnesses changed from the command line |
| `crates/ensemble-agents/tests/fixtures/` | saved event streams per CLI version, replayed through each adapter's parser (see its README) |
| `tests/real_cli.rs` | the real `claude`, `opencode` and `codex` CLIs: `health` and one trivial run. Ignored; run with `ENSEMBLE_REAL_CLI=1 cargo test --test real_cli -- --ignored`. |
| `nix/example.nix` | `nix flake check` evaluates the NixOS module with it, and builds the hub's and the runner's units |

## Limitations

- **Four harnesses so far.** `claude-code`, `opencode`, `codex` and
  `builtin`.
- **Only `opencode` and `builtin` run on the ANN pool**, and an Anthropic
  provider only `builtin`. Claude Code and Codex use your own login.
- **A decision model chooses, or nobody.** With no task picker set, or no
  decision model answering, no task is handed out. Laya is near chance until
  it is fit on labels, so expect to choose often at first; what you choose is
  kept, but nothing learns from it yet.
- **A model alone answers in one reply.** It runs no tool and reads no file,
  so it answers from what the task says and the notes recalled for it; its
  answer is at most 8192 tokens. It answers a task only when a question of
  the task picker says the task only needs an answer, and never a task with
  a gate: one the decision model says only needs an answer but has a gate
  waits for you to choose a harness, or to take the gate away. The hub's own
  pool answers no task.
- **Only the end of a run can ask.** A run stops with a question
  (`NEEDS INPUT:`) at the end of a run, never halfway through one: the CLIs'
  headless modes do not wait for an answer. The task waits for the answer,
  and runs again with it.
- **No sandbox of Ensemble's own.** A run may edit and run commands in its
  worktree; the CLIs' own permissions are what limit it. `builtin`'s file
  tools stay in the worktree, but its `shell` does not.
- **Limits are a harness's and a provider's.** How many runs at once and
  how many calls at once; there are no budgets.
- **Tasks run only while one of your runners runs**, in a repository of the
  project's name in its workspace, which it clones only from a `repository` a
  task names or you answer with, using the keys its computer has; it finds
  tasks of a project it has no folder for every 30 seconds. A runner that gave
  a task back tries it again only once it is answered again. One
  runner per computer, and one worker runs a repository's tasks at
  a time. Only a task in a repository with a remote moves between
  runners, carrying its work on its pushed branch; any other task waits for
  the runner whose repository it changed. What a runner that dies did since
  its run last stopped is not pushed, so the next run starts from the last
  push.
- **Your agent sees a task only when it asks.** The hub's MCP sends nothing
  unasked: no progress, and no word when a task ends, until your agent calls
  `tasks` or `task`.
- **A hub server shares knowledge, keeps tasks and connects runners.** A chat
  API and single sign-on are not built. People sign up through the REST API
  and wait for an admin; the web UI has no sign-up page yet, and does not
  browse the vault's notes, only recalls them. There is no password reset by
  mail, and no second factor. The API sends no CORS headers: the web UI
  reaches it through `ensemble-ui`, on the UI's own origin. Connectors to
  chats and mail are not built either.
- **Recall needs an embedding model to find by meaning.** Without one
  (`knowledge embedding`), or while its model cannot be asked, it finds notes
  that use your words, not ones that only mean the same; how alike is alike
  enough (`min_similarity`) depends on the model, and is yours to tune. The
  notes are compared one by one, which suits a vault of thousands of notes, not
  millions. Your agent recalls when it decides to, as the hub's instructions
  ask; a run gets the notes the hub recalled when its task was made,
  and recalls nothing more.
- **Runners are simple.** The first runner to claim a task takes it, not the
  least busy. A runner's models are reached at `/chat/completions` and `/completions` only.
  The hub keeps nothing of a runner once it disconnects.
- **Markdown issues only so far.** Each inbox reads a folder of markdown
  issues; GitLab, GitHub and Jira adapters are not built, and an issue cannot name a
  `repository`: a runner without a copy asks on the issue. A member's runner
  runs a hub's tasks only while it runs.
