# ensemble

An agentic development system with a **hub** at its center. Everything else
reaches the hub through its **connectors**: your agent over MCP, scripts over
the REST API, trackers whose issues become tasks, and the **runners** that do
the work, with your helper teams, on the computers you have.

- **The hub** is the core: accounts, their roles and what each may do, the
  port it listens on, and the LLM backends it uses. It runs no agents and has
  no team. It is its own process, `ensemble-hub serve`, on a server for a team
  or on your own computer when you work alone.
- **Connectors** are everything that reaches the hub, each always on: the
  REST API, MCP for client agents, the link for runners, the knowledge vault,
  the tasks it keeps, and sources of knowledge from outside. Some connectors
  hold **adapters** the admin picks, such as the trackers of `tasks`
  (markdown issues today; Jira, GitHub and others later) or the providers of
  `sources` (Microsoft 365 and Telegram, to come).
- **Runners** are one of those connectors: computational resources, each a
  computer's agent CLIs (Claude Code, OpenCode, Codex, or Ensemble's own loop)
  and model servers. A runner holds **helper teams** of agents and works
  through the tasks the hub keeps for you, like a member of the team handed
  work: in the project's repository in its workspace, cloned there when it
  has none. A runner works for you alone, or for everyone on the hub whose
  role allows it, lending its agents and models, so a team uses the power of
  every machine it has.
- **MCP is how your agent reaches the hub.** The agent CLI you already use adds
  the hub as an MCP server over HTTP, with your account's token as its key,
  and installs nothing of Ensemble: it shares and recalls knowledge, makes
  tasks for the helper agents, and follows them.

```text
   your agent CLI         scripts, a web UI       a tracker     sources
   (Claude Code, …)       to come                 (issues)      (to come)
        │ MCP /mcp             │ REST /api/v1/         │             │
        ▼                      ▼                       ▼             ▼
   ┌──────────────────────────── hub ─────────────────────────────────┐
   │ core: accounts · roles · the port · LLM backends                  │
   │ connectors: rest · mcp · runners · knowledge · tasks · sources    │
   └──────────────────────────────────────────────────────────────────┘
        ║ the runners' link: WebSockets at /link and /relay
        ║
   your runner            a teammate's runner      a shared GPU box
   helper teams, tasks    helper teams, tasks      agents and models
                                                   lent to the team
```

A **helper team** is one `config.yaml`: a flat list of professional agents,
such as a coder, a reviewer or a PCB designer. Each agent has an adapter that
says what runs it, so cheap work goes to cheap models and you keep working on
your own tasks. A runner holds any number of teams in its teams directory:
ones you make, ones you pull from a git repository like an image, and ones
someone else wrote, which run once you approve them. Agents are named
`<team>/<agent>`, such as `web/coder`. Nothing of Ensemble lives in your
projects.

Ensemble mostly runs no agent loop of its own. The runner starts the agent's
CLI headless in the repository with the task, reads what it does as it works,
and keeps the answer with what it changed. When you would rather install no
CLI, the `builtin` adapter is a loop in Ensemble itself. Five adapters ship:

- **`claude-code`**, the Claude Code CLI, on your own subscription, with the
  tools you allow it.
- **`opencode`**, the OpenCode CLI, with the tools you allow it, on any
  provider OpenCode is set up for, or on a model server of yours that you
  define once in Ensemble's config.
- **`codex`**, the Codex CLI, on your own login, limited by Codex's own sandbox.
- **`builtin`**, an agent loop in Ensemble itself
  ([harness-rs](https://github.com/liliang-cn/harness-rs)) on a model server of
  yours: it reads, edits and runs commands with the tools you list, and needs
  nothing installed.
- **`direct`**, a light agent: one chat request straight to a model server of
  yours, with no CLI and no tools, for cheap work that needs no files.

A team can mix them, and a task goes to any of them the same way.

## How it works

```text
  your agent (Claude Code, OpenCode, Codex, ...)
        |  MCP over HTTP: share, recall, make and follow tasks
        v
  ensemble-hub serve   core: accounts, roles, the port, LLM backends
        ^              connectors: rest, mcp, runners, knowledge, tasks, sources
        |
        |  the runners' connector: a WebSocket at /link (tasks leased, how
        |  each run ended) and one at /relay (model calls between runners)
        |
  ensemble-runner serve   (this computer's runner, long-running)
        |  a worker for each git repository of its workspace, cloned there
        |  for a task when it has none
        v
  its helper teams (a config.yaml each): an agent -> its adapter -> a model
        |
        +--> claude -p / opencode run / codex exec   agent CLIs, headless
        +--> builtin                                 a loop in Ensemble
        +--> direct                                  one chat request
```

- **The hub is the center.** `ensemble-hub serve`, on a server for a team or
  on your own computer, keeps the accounts and decides what each may do. One
  port serves every connector over HTTP: the REST API at `/api/v1/`, MCP at
  `/mcp`, and the runners' link at `/link` and `/relay`. Its connectors are
  always on, and each is written in its settings, which it keeps in its
  database and changes through its commands, its REST API or a web UI.
- **Your agent reaches the hub over MCP.** It adds the hub with its address and
  your token, and sees tools: `share` and `recall` for knowledge, and
  `submit`, `tasks`, `task`, `answer`, `assign`, `cancel` and `agents` for
  tasks. It never names a model: the team's config decides what runs each
  agent.
- **The hub keeps the knowledge and the tasks.** What your agent shares
  becomes notes in a git vault that later work recalls. Tasks are kept on the
  hub, made by your agent or by a tracker's issues assigned to you; the hub
  puts the notes of your knowledge related to a task into it, for its agent
  to read.
- **Runners do the work.** A computer runs one runner, `ensemble-runner
  serve`, a process you keep running, which names its hub in its
  settings. It works in its workspace, a folder of git repositories:
  each is a project, and the runner runs the tasks the hub keeps for that
  project, in that repository, with its helper teams. A task for a project it
  has no copy of names the repository to clone, or the runner asks you on the
  task; one it cannot clone it gives back, for another of your runners.
- **Load spreads across runners.** Every runner of yours with a task's agent
  may take the task; a runner of someone else's that works for the hub may
  take it when none of yours can, with an agent on a model server, never on
  their CLI's login; and a runner's model servers can serve the whole team
  through the hub as an OpenAI-compatible endpoint.
- **Tasks outlive the runner that runs them.** A runner leases a task while
  it runs it, and when that runner stops, another of yours takes it up from
  the branch the first pushed.
- **Work is kept outside your repositories**, in a per-user state directory:
  run logs, git worktrees, usage and run records.

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
(`ensemble-hub backend add`, `ensemble-hub knowledge models`; see
[Changing settings](#changing-settings)).

The token's answer gives the line that adds the hub to your agent
([Connect your agent](#connect-your-agent)). Your agent learns the tools from
the hub when it connects, so there is nothing to add to its `CLAUDE.md` or
`AGENTS.md`: it can share and recall knowledge from now on.

For helper agents to work on tasks, connect a runner to the hub, on each
computer that should work:

```console
$ ensemble-runner setup
```

`ensemble-runner setup` finds the agent CLIs you have (`claude`, `opencode`,
`codex`; with none it says so and goes on, as `builtin` and `direct` agents need
none) and any local model server. When this computer has no runner yet, it
asks what to call it (this computer's name, unless you say otherwise) and
which folder it works in (`~/ensemble`, unless you say otherwise), and makes
it: its settings are kept in its database,
`~/.local/state/ensemble/runner/core/database.db`. Then it
checks the runner's helper teams and says what is next. Give the runner a team,
a repository and the hub, and run it:

```console
$ ensemble-runner team new web                               # a sample to edit, or:
$ ensemble-runner team pull github.com/acme/web-team@v1.2    # a team your company keeps
$ git clone git@github.com:acme/shop.git ~/ensemble/shop      # a project: a repository in the workspace
$ ensemble-runner login ws://127.0.0.1:7700 --user ana        # your account on the hub, or: join <url> <code>
$ ensemble-runner serve
```

Keep the runner running (see [Running the runner](#running-the-runner)). Then
ask your agent to have the helper team implement something in `shop`.

Without a terminal, or to script it: `ensemble-runner setup --yes --name desk
--workspace ~/code`.

Afterwards, `ensemble-runner doctor` says what is wrong and how to fix each
thing, and `ensemble-runner agent add web/tester --adapter claude-code` and
`ensemble-runner agent remove web/tester` change a team without editing the file
(the file stays yours: comments and order are kept, and every setting is written
out).

The workspace is where the teams work. Each git repository directly inside it
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
their roles and what each may do; the one port it listens on; and the LLM
backends it uses. Everything else is a connector (see
[Connectors](#connectors)). It has no helper team and runs no agents. It is
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
    database.db    its settings and who changed them, its backends' keys (sealed),
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
$ ensemble-hub backend add central --type openai-compatible-api --url http://127.0.0.1:8000/v1
$ ensemble-hub knowledge models central:qwen3.5-72b   # the models that turn shares into notes
$ ensemble-hub check                      # says which model would distill shares now
```

`ensemble-hub settings show` shows every setting; `--json` prints them as
data, which `ensemble-hub settings import` takes on another hub. A hub that
was just set up, given a model server:

```text
$ ensemble-hub settings show
backends:
  central:
    api_key_env: none
    base_url: http://127.0.0.1:8000/v1
    timeout_seconds: 300
    type: openai-compatible-api
distill:
  max_tokens: 4000
  try:
    -
      backend: central
      model: qwen3.5-72b
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
  admin: admin, share, recall, tasks, compute, host
  member: share, recall, tasks, compute
  reader: recall
session_hours: 12
source_filter: none
trackers: none
```

Its core is its owner, its accounts and roles, and the LLM
backends it uses; the rest reaches it through connectors, each always on: the
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

**The LLM backends** are the models the hub itself calls, such as to distill
knowledge: its own, with the keys it keeps, and those runners share through
it (`type: runner`, see [Sharing LLM backends](#sharing-llm-backends)). A connector names one by its
name alone. `ensemble-hub backend add` adds one and keeps its key for you
(see [Changing settings](#changing-settings)).

**Permissions.** The permissions are `share` (keep knowledge in the vault),
`recall` (read its notes), `tasks` (make and follow tasks, have your runners
run them, and receive the issues assigned to you; see
[Tasks from a tracker](#tasks-from-a-tracker)), `admin` (manage users),
`compute` (your tasks may be worked on by other people's runners that work for
the hub, and you use their models) and `host` (connect a runner that works for
the hub; see [Runners](#runners)). The roles are yours to name, each a list of
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
| `POST /api/v1/ops/{name}` `{…its input}` | as the operation says | runs any operation by its name, such as `backend.add`, `knowledge.models` or `tracker.add` |
| `GET /api/v1/openapi.json` | anyone | this API as an OpenAPI 3.1 document: every path above and every operation, with its body and who may call it |

Point an OpenAPI tool at `/api/v1/openapi.json` to browse the API or make a
client for it. The document is the same for everyone: what a token may
actually run is what `GET /api/v1/ops` lists for it.

An account's own work is operations too, which only the running hub answers,
each as the runners' link and MCP do, the permission it needs checked:
`task.projects`, `task.list`, `task.show`, `task.create`, `task.answer`,
`task.assign` and `task.cancel` (`tasks`), `runner.list`, the runners that
work for you with their agents (`tasks`), `knowledge.share` (`share`) and
`knowledge.find` (`recall`).

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
  made, answered, given to an agent and cancelled.
- **Runners**: those connected now that work for you, and their agents.
- **Knowledge**: recall the notes about a topic, and share what is worth
  keeping, for you alone or for the hub.
- **Account**: your access tokens and join codes, each shown once, your
  password and your name in the tracker.
- For an admin, **Users**: the accounts, sign-ups waiting to be let in, and
  what an admin does to an account; and **Settings**, a section each, filled
  in with each setting as it is now: the hub and its logins and sign-ups;
  the roles, a box for each permission; the model backends, each tested,
  given a key or taken out; the models of the knowledge vault and the source
  filter; the trackers; who changed what; and a page for each operation of
  the settings no section shows yet.

Every form is made from what `GET /api/v1/ops` says of the operation it runs,
so the UI does nothing the API does not, and a new operation's form comes
with it. What a field may be is picked, not typed, where the hub says or
knows it: a role from the hub's roles, permissions and a token's scopes as
boxes, an agent from those connected now, a model from those the backends
serve. A form asks only for what the choices made so far need, folds away
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
| `agents` | the agents of the runners connected now that take your tasks: each of yours with all its agents, then others' that work for the hub with the agents they lend |
| `submit` | make a task for an agent, kept on the hub; its id comes back at once. Its arguments are the ticket's, below in [Tasks](#tasks): `project`, `title`, `task`, and optionally `agent`, `repository`, `goal`, `context`, `constraints`, `files`, `done_when`, `after`, `repo` and `gate` |
| `tasks` | a project's tasks: each one's ticket and state, `blocked` and who gave it back why among it |
| `task` | one task: its ticket, state, questions, hand-offs and result |
| `answer` | answer the question a task stopped to ask, so that it goes on |
| `assign` | give a task to an agent, such as one waiting in the backlog |
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
was asked, what was asked and answered on the way, and what the agent
reported, as yours and private, with the client `task`, so what your helper
agents learn is kept like what you share. A report that looks like it holds a
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
model, by what the notes mean: `ensemble-hub knowledge embedding <backend>:<model>` names one
an `openai-compatible-api` backend serves at `/embeddings` (Ollama's
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
decides when to follow them. Helper agents get no notes and no knowledge
tools: a task carries what its ticket says, so your agent puts in it what the
helper needs, from what it recalled.

### Tasks from a tracker

Issues live in a tracker and are assigned to **people**. When an issue is
assigned to you and carries the hub's label, the hub's inbox makes it a task of
yours, which the hub keeps like any other, and writes what happens back on the
issue. Your helper team never touches the tracker itself. A tracker is an
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
labels: [ensemble, ensemble:web/coder]
project: shop
status: open
---
After logging in, it redirects to /home instead of back to where you were.
```

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
- **Which agent.** A second label `ensemble:<team>/<agent>` names the agent;
  without one the task goes to the backlog, for your agent to assign.

What the hub does, every `poll_seconds` of the tracker:

- **An open issue becomes a task** of that account, and the issue says so:
  "picked up as task 0003-fix-the-login-redirect, for web/coder". When one of
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

A runner is a worker on a computer: the one long-running process that holds
the helper teams there, and a computer runs one. It works in its workspace,
the folder its settings name: each git repository directly inside it is
a project, named for its folder, and the runner keeps a worker for each, which
leases the tasks the hub keeps for that project, for the runner's account, and
runs them in that repository. For a task of a project it has no folder for, it
clones the task's repository there first, or asks on the task where it is. No agent connects to a runner: your agent makes
tasks on the hub, and your runners take them.

With the hub its settings name, a runner keeps a link to it: the tasks
it runs (see [Tasks](#tasks)), and one that says it is connected, with every
agent of its teams and what it lends. `works_for` says whose tasks it works
on: `owner`, yours alone, or `hub`, yours and also those of everyone on the
hub whose role has `compute`, when no runner of their own takes them.
Connecting a runner that works for the hub needs `host`. The hub connects
them, and keeps no runner once it is gone.

`ensemble-runner setup` or `ensemble-runner init` makes the runner;
`ensemble-runner settings show` shows its settings, and an operation changes
each:

```console
$ ensemble-runner settings name gpu-box                       # a hub knows it as <you>/gpu-box
$ ensemble-runner settings works-for hub                      # owner: you alone; hub: also others with `compute`
$ ensemble-runner settings workspace ~/ensemble               # its git repositories; ~/ is your home
$ ensemble-runner backend add local --type openai-compatible-api --url http://127.0.0.1:11434/v1 --share
$ ensemble-runner backend add team-gpu --type runner --runner kim/workstation --runner-backend big
$ ensemble-runner settings shared-calls 1                     # the most calls of others to what it shares, at once
$ ensemble-runner login wss://hub.example.com --user kim     # once: it names this hub
$ git clone git@github.com:acme/shop.git ~/ensemble/shop      # a project of its workspace, or let a task name it
$ ensemble-runner serve                                       # until Ctrl-C or SIGTERM
$ ensemble-runner peers                                       # the runners of the hub that work for you now, yours too
```

`ensemble-runner …` reaches the running runner on a socket only you may open
(in `$XDG_RUNTIME_DIR/ensemble`, else a folder of yours under `/tmp`). A
project's tasks run on a runner with a repository of its name in its
workspace: one you cloned there, or one it cloned for a task that names its
`repository` (see [Tasks](#tasks)).

**Working for others.** A runner with `works_for: hub` works on the tasks of
other accounts too, with the agents it lends. Nobody lists them: it lends
every agent of its teams that runs on models of LLM backends (`opencode`
on `llm_backend`, `builtin`, or `direct`), and never one on an agent CLI's own
login or provider (`claude-code`, `codex`, `opencode` on a `provider`): a
subscription, such as Claude Code's, belongs to its owner and is never used
for someone else's work. `ensemble-runner peers` and the hub's `agents` list
it to everyone it works for, with what it lends (its owner sees every agent
of it).

- **Who may.** Connecting it needs `host` in your role; an account's tasks go
  to others' runners only when its role has `compute`. The hub checks both,
  that the runner is connected and works for the hub, and that it lends the
  task's agent, at every request the runner makes about another account's
  task, and it may only take, run and give back such a task: making,
  answering, assigning and cancelling stay with the account.
- **When.** The hub offers a task to the account's own runners with its agent
  first. Another's runner may take it only when none of them is connected, or
  every one gave it back. A busy runner of the account's own has not given it
  back: the task waits for it. A task given back by every connected runner
  that may take it, the account's own and others', is blocked.
- **Where.** It works on another account's project in a folder of its own in
  the workspace, `.for/<account>/<project>`, cloned from the task's
  `repository` like a project of yours, and gives the task back when this
  computer cannot reach it. A project's name never starts with a dot, so none
  of yours is mistaken for it.
- **What comes of it.** The task stays the account's: its notes are those the
  account may see, its branch is pushed to its repository, and what it
  reports becomes a share of that account, not yours.
- **What you see.** Whoever runs the runner can read the code it works on,
  the task and the notes in it, as on any computer it runs on; and the agents
  it lends run with your computer's permissions (a `builtin` agent's `shell`
  too). Make a runner work for the hub only where both sides accept that.

### Running the runner

`ensemble-runner serve` runs this computer's runner until it gets Ctrl-C or
SIGTERM. It makes its workspace when the folder is not there, and gives each git
repository directly inside it a worker, which runs the tasks the hub keeps for
that repository's project. It looks again every 30 seconds: a repository cloned
in since gets a worker, and one that went away has its worker stopped; and it
clones, asks about or gives back the tasks of projects it has no folder for.
With a hub, it tells the hub it is connected, with every agent of its teams,
and again when its teams are reloaded: `agents` and `ensemble-runner peers`
list it. Without a
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

Tasks of a `shared` agent share the repository, so give two of them that
change the same files to an agent with `max_concurrent: 1`, or `isolation:
worktree`.

### Sharing LLM backends

A hub may run on a small machine and need a strong model, and so may a
runner. A runner may offer some of its own backends through the hub
(`ensemble-runner backend share`), and the hub and other runners use them:
each adds one as a backend of `type: runner`, naming the runner,
`<owner>/<name>`, and its name there. Then it names it like any backend, by
the name it gave it, in a `try` list, the distiller or a team. A backend's
name is unique among all of a hub's or a runner's.

```console
$ ensemble-runner backend share local                      # on ana's gpu box
$ ensemble-hub runner backends                             # what the connected runners share, with their models
ana/gpu-box  local  (works for owner, 1 call(s) at once)  models: qwen3.5:72b
$ ensemble-hub backend add gpu --type runner --runner ana/gpu-box --runner-backend local
```

- **Who may use it.** The hub, and its owner's other runners; when the runner
  works for the hub, also the runners of everyone whose role has `compute`.
- **What they learn.** Nothing of its address or key: the runner calls its own
  server, and only for what an agent needs of it (its models, chats,
  completions and embeddings). Whoever runs it sees what is sent to it:
  choosing a runner's backend is a decision of trust.
- **How it goes.** Calls go on a second WebSocket of each runner, beside its
  link, so a long prompt or a busy stream never holds up its tasks. The hub
  and a runner that reaches others' backends serve them on loopback, where
  each looks like any OpenAI-compatible backend to the agents. Through the hub
  a call takes a few milliseconds longer than one made directly.
- **When it cannot.** A runner takes `max_shared_calls` calls of others at
  once (`ensemble-runner settings shared-calls`); the hub keeps the next waiting for up to 10 seconds, then answers that
  it is busy. Busy, offline or not shared, the backend is unavailable and the
  `try` list's next model is used.

## Helper teams

A helper team is one directory with one file in it: `config.yaml`, the llm
backends and the agents. Commit it; it holds no secrets. The runner runs every
team in its teams directory (`~/.local/state/ensemble/runner/teams/`, see
[Teams](#teams)) as one helper team: each name of a team, its agents, model
servers, limits and gates, is `<team>/<name>`, so two teams may both have a
`coder`. Commands that change one agent name it so, `web/coder`; every other
one reads all the teams. Nothing else is searched for, and there is no
user-level config. `ensemble-runner team new <name>` writes a sample with every
setting.

What the runner learns while it runs is kept in its folder,
`~/.local/state/ensemble/runner/` (`runner/` in `--config-dir`), outside every
repository:

| where | |
| --- | --- |
| `core/` | its `database.db`, with its settings, its backends' keys and its hub tokens, sealed by `master.key` beside it |
| `teams/` | its helper teams, a folder each |
| `workdirs/<folder>-<hash>/runs/` | what tasks did as they ran on this computer, for one repository of the workspace |
| `.../worktrees/` | a git worktree per task of an agent with `isolation: worktree` |
| `.../worker.lock` | the lock of the one worker that runs the repository's tasks |
| `orgs/teams-<hash>/usage.json` | how many runs each agent started, shared by every repository |
| `worker.lock` | the runner's own lock |

```yaml
version: 1

limits: {}

gates:
  test:
    command: cargo test
    timeout_seconds: 900

agents:
  coder:
    prompt: You implement exactly what is asked, in the style of the code around it.
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model:
        type: llm_backend
        try:
          - backend: local
            name: qwen3.5-9b
            context_window: 262144
        when_none: fail
      tools: [read, glob, grep, list, edit, bash]
      system_prompt: append
      max_steps: 30
      command: opencode

  reviewer:
    prompt: You review code for correctness and report findings as file:line — problem.
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: claude-code
      model: sonnet
      tools: [Read, Grep, Glob]
      permission_mode: dontAsk
      max_turns: 30
      command: claude
```

**Nothing has a default value.** Every setting is written in the file, and a
missing one is refused by name (``agents.coder: `max_seconds` is required``).
What differs between cases is a tagged variant (`type:`) carrying exactly its
own settings, so a setting that does not apply cannot be written and one that
does cannot be forgotten. Only an agent's `prompt` may be left out, because
leaving it out means "not used": the CLI's own system prompt alone.

An **agent** is a member of the helper team. Its names are yours; nothing is
hardcoded as planner, coder or reviewer. There is no hierarchy: every agent is
one professional a task can go to. It has:

| key | |
| --- | --- |
| `prompt` | what it is for. The first line is what `agents` shows of it; the whole of it goes into the agent's system prompt, added to the CLI's own (OpenCode can also replace its own; see `system_prompt`). Optional: without it a CLI agent runs on its CLI's own system prompt alone, and a `direct` agent has none |
| `max_seconds` | wall-clock limit for one run. At the limit the CLI and its whole process tree are killed and what it did is kept, marked unfinished |
| `limit` | the name of one of the team's [`limits`](#shared-limits), in place of `max_concurrent` and `budget` |
| `max_concurrent` | most of its tasks that run at once in one repository of the workspace. More wait for a free slot. `1` suits a model server that serves one request at a time |
| `isolation` | where its tasks run: `shared`, in the repository, or `worktree`, each task in a git worktree of its own (see [Tasks](#tasks)) |
| `budget` | how much it may run in an hour: `{ type: unlimited }`, `{ type: per_hour, delegations: 20 }` (runs), or `{ type: per_hour_tokens, tokens: 2000000 }` (the tokens its runs reported). A task and each gate retry count, across every repository (`usage.json` in the helper team's state directory). Over it, a task waits until its agent may run again, and a gate retry is not made |
| `adapter` | what runs it. `type` decides which other fields the adapter takes |

An **LLM backend** is a model server of yours, or one your company runs. Each
runner keeps the backends it has (`ensemble-runner backend add`, see
[Sharing LLM backends](#sharing-llm-backends)), with their addresses and keys,
and a team names them by name alone: an OpenCode agent in its model (`type:
llm_backend`), and a `builtin` or `direct` agent in its adapter. So one team
runs on every runner that defines the backends it names, each with its own
servers and keys, and Ensemble hands the agent's CLI the endpoint and key for
each run, so you never configure the same server again in the CLI's own
settings. The team above names `local`, which a runner adds like this, its
key asked for when the server wants one:

```console
$ ensemble-runner backend add local --type openai-compatible-api --url http://127.0.0.1:8888/v1 --timeout 600
```

A backend has a `type`:

- `openai-compatible-api`: any `/chat/completions` endpoint: llama-server, vLLM,
  Ollama, Unsloth, a hosted API. `opencode`, `builtin` and `direct` agents can
  run on it.
- `anthropic-api`: Anthropic's own Messages API (`base_url:
  https://api.anthropic.com`, the key sent as `x-api-key`). `builtin` and `direct` agents
  can run on it; OpenCode needs an openai-compatible one, and a config that says
  otherwise is refused by name. Claude Code uses your own login, not a backend.

Either requires `base_url` (the URL itself; no variable overrides it) and
`timeout_seconds` (per request), and takes `api_key_env`, the name of the
variable holding the key, when the server wants one. `ensemble-runner agent
health` asks each for the models it serves.

An agent on backends lists its models in **`try`**, in order, and says in
**`when_none`** what happens when none of them can be used:

```yaml
model:
  type: llm_backend
  try:
    - { backend: local, name: qwen3.5-9b, context_window: 32768 }
    - { backend: central, name: qwen3.5-72b, context_window: 131072 }
  when_none: wait
```

Each run uses the first model whose backend answers and serves it, so a shared
helper team keeps working on whatever each person has: your own server when it
is on, the company's otherwise. With one model there is nothing to choose, and
it is simply used. With several, Ensemble asks each backend which models it
serves (`GET /models`), at most once a minute per backend. When none works,
`fail` makes the task fail, saying what each backend answered; `wait` keeps
the task in the queue, noted in its log (`waiting for a backend`), and tries
again later. A run on a later model says so in what it reports (`opencode on
central (fallback 2 of 2)`), and `ensemble-runner agent show <agent> --probe`
says which model a run would use now.
Claude Code and Codex use your own login and have no `try` list.

A **gate** is a check a task's work must pass, so an agent that says "done"
too early gets sent back instead of being believed. You define each gate once,
under `gates`, with a name of your choice:

```yaml
gates:
  test:
    command: cargo test
    timeout_seconds: 900
```

| key | |
| --- | --- |
| `command` | what to run: a shell command (`sh -c`), in the task's working directory (its worktree, if the agent has `isolation: worktree`). Exit status 0 passes; anything else fails, and its output (stdout and stderr together) is what the agent is shown |
| `timeout_seconds` | how long it may run. At the limit it is stopped and counts as failed |

Nothing runs a gate until a task asks for it: a task names one with `gate:
{ name: test, attempts: 3 }` (see [Tasks](#tasks)).
A task cannot bring a command of its own, so what runs on your machine is
what this file says. `gates: {}` says there are none, which is fine: tasks
then simply have no check.

There is no leader in the file: your agent, with you, decides who does what.

### The `opencode` adapter

The OpenCode CLI as a member of the team: a whole agent of its own, with its
own tools, managing its own context.

```yaml
agents:
  coder:
    prompt: You fix exactly what is asked, then say what you changed.
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model:
        type: llm_backend      # models the runner's LLM backends serve
        try:                   # in order; a run uses the first that works
          - backend: local
            name: qwen3.5-9b       # the id the server serves
            context_window: 262144 # what the server runs it with
        when_none: fail        # or wait: a task waits for a backend
      tools: [read, glob, grep, list, edit, "bash(git *)"]
      system_prompt: append
      max_steps: 30
      command: opencode        # `opencode` if it is on PATH

  explorer:
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model:
        type: provider         # a model OpenCode is already set up for
        name: opencode/big-pickle  # provider/model, as `opencode models` lists it
      tools: [read, glob, grep, list]
      system_prompt: append
      max_steps: 30
      command: opencode
```

`opencode run` has no flags for a system prompt or allowed tools, so Ensemble
hands OpenCode an agent of its own through `OPENCODE_CONFIG_CONTENT`:
`max_steps` as its step cap, and a permission set that denies everything
(`"*": "deny"`) except the tools listed. OpenCode hides denied tools from the
model, so it cannot even try them; that includes reaching outside the project
(`external_directory`). `tools` takes OpenCode's permission names (`read`,
`glob`, `grep`, `list`, `edit`, `bash`, `webfetch`, `websearch`, `task`) or a
rule such as `bash(git *)`. It runs with `--standalone`, a private OpenCode
server for the run, not the shared background service.

OpenCode has a system prompt of its own, written for each model family, that
teaches the model its tools; it also adds the working directory, the date and
the project's `AGENTS.md`. `system_prompt` says what the agent's `prompt` does
to it:

- `append`: the prompt follows OpenCode's own. OpenCode has no flag for that,
  so Ensemble writes the prompt to a temporary file for the run and lists it
  under `instructions`. Right for most agents.
- `replace`: the prompt takes the place of OpenCode's own, as the agent's
  `prompt`. For a small model whose context cannot spare the few thousand
  tokens OpenCode's own takes on every step. It needs a `prompt`.

OpenCode also reads your global `~/.config/opencode/AGENTS.md` and
`~/.claude/CLAUDE.md` into every agent. Set `OPENCODE_DISABLE_CLAUDE_CODE=1`
in the environment you start Ensemble from to keep `.claude` files out.

With a model of type `llm_backend`, the same inline config also defines that
backend as an OpenCode provider (`@ai-sdk/openai-compatible`, its base URL, its
key, its `timeout_seconds`) serving the agent's model with `context_window` as
its limit, so OpenCode knows when to compact. Nothing is written to disk. See it
with `ensemble-runner agent show <agent>`:

```console
$ ensemble-runner agent show coder
opencode run --standalone --format json --agent ensemble-coder -m ensemble-local/qwen3.5-9b -- '<task>'

OPENCODE_CONFIG_CONTENT:
{
  "agent": { "ensemble-coder": { "mode": "primary", "steps": 30, "permission": { "*": "deny", ... } } },
  "provider": {
    "ensemble-local": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:8888/v1", "apiKey": "<redacted>", "timeout": 600000 },
      "models": { "qwen3.5-9b": { "limit": { "context": 262144, "output": 32768 } } }
    }
  }
}
```

Your own OpenCode configuration (providers, models, the project's
`opencode.json`) still applies; the agent and provider are added to it.

### The `direct` adapter

A light agent: no CLI, no tools, no loop. The task (after its context) and the
agent's prompt (as the system prompt) go straight to an LLM backend as one
`/chat/completions` request, and the answer comes back. It reads and changes
nothing, so it suits cheap work on what it is given: summarising a log,
classifying findings, drafting a commit message, on a small local model.

```yaml
agents:
  summariser:
    prompt: You summarise what you are given in five bullet points or fewer.
    max_seconds: 600
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: direct
      try:                   # in order; a run uses the first that works
        - backend: local     # one of the runner's LLM backends
          model: qwen3.5:9b  # the id the server serves
      when_none: fail        # or wait: a task waits for a backend
      max_tokens: 1000       # an answer cut off here is marked unfinished
```

The request waits up to the backend's `timeout_seconds`, and the agent's
`max_seconds` or a cancel ends the wait sooner. `ensemble-runner agent show
<agent>` prints the request; `health` checks the backend serves the model.

### The `builtin` adapter

An agent loop in Ensemble itself, so an agent on a model server of yours needs
no CLI installed. The model asks for tools, Ensemble runs them in the working
directory and sends back what they returned, until the model answers or
`max_steps` is reached. The loop is [harness-rs](https://github.com/liliang-cn/harness-rs)
(`AgentLoop`), pinned to one exact version.

```yaml
agents:
  patcher:
    prompt: You fix small, well-described bugs and say what you changed.
    max_seconds: 1800
    max_concurrent: 1
    isolation: worktree
    budget:
      type: per_hour
      delegations: 20
    adapter:
      type: builtin
      try:                       # in order; a run uses the first that works
        - backend: local         # one of the runner's LLM backends, of either type
          model: qwen3-coder:30b
          context_window: 65536  # the loop compacts before it outgrows it
      when_none: fail
      tools: [read, list, grep, glob, edit, write, shell]
      max_steps: 40              # model calls for one run
      max_tokens: 4000           # answer tokens per model call
```

| tool | what it is |
| --- | --- |
| `read`, `list`, `grep`, `glob` | look at files and folders |
| `write`, `edit` | change files |
| `shell` | run any program, with arguments, in the working directory |

The file tools cannot leave the working directory: they open it as a
capability directory, so `..`, absolute paths and symlinks out are refused by
the operating system. `shell` has no such limit, which is why an agent has it
only when it is listed. At `max_steps`, at the agent's `max_seconds`, or on a
cancel, the run stops with what it did so far, marked unfinished. A runner
that works for the hub lends a `builtin` agent: it runs on the runner's own
model servers.

### The `codex` adapter

The Codex CLI as a member of the helper team, run with `codex exec --json`, on your own
login:

```yaml
adapter:
  type: codex
  model: gpt-5-codex
  sandbox: workspace-write   # read-only | workspace-write | danger-full-access
  command: codex
```

Codex has no tool list and no turn limit to set. What limits it is its own
sandbox: `read-only` can only read, `workspace-write` can change files in the
project (Ensemble then lists what changed), `danger-full-access` limits nothing,
which `ensemble-runner team check` and `ensemble-runner doctor` warn about. It
has no flag for a system prompt, so the agent's `prompt` goes in front of the
task. Progress, the commands it ran, its tokens and its answer are read from its
JSON events as it works.

**Not yet checked against a real Codex release.** The event shapes follow Codex's
documented `exec --json` output and are covered by stand-ins and a saved stream;
the first run against the real CLI is `ENSEMBLE_REAL_CLI=1 cargo test --test
real_cli -- --ignored`, and a mismatch there is a fixture to capture and a parser
to fix, as for the other CLIs.

### The `claude-code` adapter

The Claude Code CLI as a member of the team, a whole agent of its own.

```yaml
agents:
  reviewer:
    prompt: You review code for correctness and report findings as file:line — problem.
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: claude-code
      model: sonnet         # an alias (sonnet, opus, haiku) or a full model id
      tools: [Read, Grep, Glob]  # Claude Code's own tools; read-only
      permission_mode: dontAsk
      max_turns: 30
      command: claude       # `claude` if it is on PATH
```

Ensemble starts `claude -p` in the project with the task, the agent's prompt
(`--append-system-prompt`) and `--allowedTools`. With `permission_mode:
dontAsk` Claude Code denies any tool not listed instead of waiting for a person
who is not there, and the result says which it was denied. `tools` takes
Claude Code's names and rules (`Read`, `Edit`, `Write`, `Bash`,
`Bash(git diff:*)`, `WebFetch`), one per entry. `[Read, Grep, Glob]` is
read-only. It has no `model` of type `llm_backend`, no `max_steps`: those
settings belong to the `opencode` adapter and are refused here.

It runs on your own subscription, through the official CLI's supported
headless mode. Ensemble does not read, copy or replay Claude Code's
credentials and never talks to Anthropic's API itself; it never passes
`--bare`, which would bypass the saved login and bill per token. If an answer
comes from a model that is not Claude (a stray `ANTHROPIC_BASE_URL`), it is
refused.

### What a run reports

The CLI adapters read their CLI's JSON events as it works, and `builtin` its
loop's steps. A run's result, which a task keeps,
ends with how many steps and tool calls the agent used, every file it added,
modified or deleted, and the shell commands it ran:

```text
[coder: 6 step(s), 5 tool call(s)]
[files changed: src/lib.rs (modified), tests/new.rs (added)]
[commands run: cargo test]
```

Changed files are found by comparing the repository before and after the run,
when the agent has a tool that writes (`.git`, `target` and `node_modules` are
not compared).

Tasks run side by side. Those of agents with `isolation: shared` share the
repository: two agents editing the same file will clash. When a run overlapped
another, its result says some listed files may be the other run's changes. A
cancel or `max_seconds` kills the CLI and its whole process tree.

The agents' tools are the CLIs' own and run with your permissions; `tools` and
`permission_mode` are what limit them. Ensemble adds no sandbox.

### What the team costs

Every run, whether a task or a gate retry, leaves
one line in `records.jsonl` beside the usage counters (shared by every repository
that uses the helper team): the agent, the model that answered, when it started
and how long it took, the tokens, the steps and tool calls, and whether it
worked. `ensemble-runner agent stats` sums them per agent:

```console
$ ensemble-runner agent stats --since 7d
agent              runs  failures    total s    mean s   tokens in  tokens out
web/coder            12         1     1843.2     153.6      418220       30114
web/reviewer          9         0      402.7      44.7           -           -
```

A `-` means no run of that agent reported tokens: an agent's CLI or server that
says nothing is not the same as one that spent none. `--since` takes `30m`,
`12h`, `7d` or `2w`; `--json` prints the same as data.

### Tasks

The helper teams work on **tasks**: tickets kept on the hub, each for a
project, a repository's name. Your agent makes one with `submit`, and the
hub's inbox makes one from a tracker's issue (see
[Tasks from a tracker](#tasks-from-a-tracker)). Ids are numbered within the
project, so an id names one task. `submit` answers at once with the id; the
work goes on without your agent's conversation.

Each of your runners with a repository of the project's name in its
workspace has a worker for it, and a runner with none gets one for a task of
its agents (see **A runner finds the project** below). While the runner runs, the worker works through
the project's tasks that may start, oldest id first, each agent at most
`max_concurrent` at a time, and the hub keeps each one's result. Your agent
follows them with `tasks` and `task`, the only way it sees what an agent is
doing. `cancel` stops one: a task that has not started gets a `cancelled`
result and never runs, and the runner that runs one kills its agent's CLI and
the CLI's process tree.

**Tasks are kept on the hub, and runners lease them.** That is what lets a
task outlive the runner that runs it:

- A runner **claims** a task that may start (assigned, not ended, not waiting
  for an answer or another task) for a minute, renews the claim every 15
  seconds while its agent works, and tells the hub how the run ended. While it
  holds the claim, the task shows as running on that runner, and no other
  runner runs it.
- A runner that **stops** puts its running tasks back: they show as
  interrupted on it, and the next runner to look runs them again. A runner
  that **dies** stops renewing, and its claim runs out within a minute, with
  the same effect.
- Any of your runners with the task's agent takes a task that may start, in
  the repository of the project's name in its workspace, cloned there first
  when it has none. So a task you make at your desk runs on your laptop if the
  desk is off. When none of your runners with the agent is connected, or each
  gave the task back, a runner of someone else's that works for the hub may
  take it (see [Runners](#runners)). Give
  your runners the teams your tasks name: a runner without a task's agent
  fails the task, and `ensemble-runner team check` lists the tasks on the hub
  a runner cannot run.
- A runner whose claim ran out, and whose task another runner took meanwhile,
  cannot say how it ended: the task is never done twice. What that run did
  stays in its repository on that computer.
- A runner with no hub has no tasks.

**Where the work is decides who goes on with it.** A task of an agent with
`isolation: worktree`, in a repository with a remote `origin`, works on a
branch of its own, and that branch can move: whenever a run of it stops (put
back, asking, handed on, or ended), the runner commits what it did so far on
the branch and pushes it to `origin`. The next run, on any of your runners,
fetches the branch and carries on from there, and its agent is told the task
was started elsewhere and to go on, not start over. Any other task changes one
computer's repository, so once it has run there it stays with that runner:
when the runner stops, the task waits for it. A task never pushes anywhere but
`origin`, and only its own branch, `ensemble/<id>`.

A task's fields; only `project`, `title` and `task` are needed:

| field | |
| --- | --- |
| `project` | required. The repository's name: its folder in a runner's workspace |
| `repository` | where the project lives: a git URL or path, such as `git@github.com:acme/shop.git`, that a runner with no copy clones it from. Without one, such a runner asks |
| `title` | required. A short name; it makes the task's id (`0002-add-retry-to-the-fetcher`) |
| `agent` | which agent does it, `<team>/<agent>`. Without one the task is in the backlog |
| `task` | required. What to do. The agent cannot see your conversation, so it says everything needed |
| `context` | background the agent needs, sent with the task |
| `goal` | the outcome wanted, in a sentence, as opposed to the steps |
| `constraints` | what the agent must not do or must keep |
| `files` | where to look or work |
| `done_when` | conditions that mean it is finished. The agent is told to check its work against them. They are advice to the agent; nothing runs them (that is what `gate` is for) |
| `after` | ids of tasks that must end `done` first; each must exist already |
| `repo` | a folder inside the repository to work in, such as one repository of several |
| `gate` | `name`: a check defined under `gates` in `config.yaml`. `attempts`: how many runs the agent gets in all, the first included, so `1` means no retry. The hub keeps it and the runner checks the name when it runs the task, against the gates of the agent's team; `submit` refuses only a gate of the wrong shape |

The agent is sent the text, then the goal, constraints, files and
`done_when` as sections it can check its work against, then the task's notes. A task with `after`
waits until every task it names has ended `done`. If one of them failed or was
cancelled, it stays blocked until it is submitted again.

**Using a gate.** `done_when` tells the agent what finished means; `gate`
makes Ensemble check it. With `gate`, the named command from `config.yaml`
runs once the agent is done:

1. It passes: the task is `done`.
2. It fails: the same agent runs again, told the command, how it ended, the
   last 60 lines of its output and its own previous reply.
3. This repeats until it passes or the agent has had `attempts` runs in all.
   Then the task is `failed`.

Each retry takes a run from the agent's `budget`; over it, no retry is made. The
result records `gate`, `gate_result` (`passed`, `failed`, or `not run` when the
agent itself failed) and `attempts`. A gate that is not defined in `config.yaml`
fails the task at once, and `ensemble-runner team check` reports it before that.

A result starts with `status` (`done`, `failed` or `cancelled`), `agent`,
`started` and `ended`, then the agent's reply. A task with a result is over;
submit it again to run it again. What a run does as it goes, its activity
log, is in `runs/<id>/` in the repository's state directory on the computer
that ran it. `ensemble-runner team check` reports, for each project of the
workspace, the tasks on the hub this runner cannot run as they are: a gate it
has not got, or an agent that is another runner's.

A task's agent may stop to ask. It is told that, when it cannot finish without
a decision only the person who asked can make, it should end its reply with a
line `NEEDS INPUT: <question>`. The hub keeps the question with the task, and
`tasks` and `task` show it. `answer` adds the answer, and the task runs again,
given the questions and answers so far and its reply when it stopped. A gate
does not run on a reply that asks, and a worktree is not committed until the
task ends.

An agent with `isolation: worktree` runs each task in a git worktree of its
own, `worktrees/<id>` in the repository's state directory, on the branch
`ensemble/<id>` made from the repository's `HEAD`. The repository's own files
are not touched, so such tasks can run side by side. The gate runs in the
worktree too. When the task ends, everything it changed is committed on its
branch (`--no-verify`, your git identity), the branch is pushed to `origin`
when the repository has one, and the result records `branch` and `commit`.
Ensemble never merges: you look at the branch and merge it with git, as any
other branch (`git fetch origin ensemble/<id>`, then `git diff` and `git
merge`); without a remote, the branch is in the repository in the runner's
workspace. A task run again on the same computer, or one the runner's stop
interrupted, carries on in the worktree it had; one carried on elsewhere since
is brought to where that left it. The repository, or the task's `repo`, must
be a git repository with at least one commit.

### A helper team that works on its own

You own the work, not operate it: you talk to your own agent about what you
want, with an issue of yours or without one, and it decides with you how the
work gets done: it makes tasks for the helper team, for work that should go
on without it, or does the work itself. You can see who does what, but you do
not have to.

- **Tasks have an assignee**, an agent. A task with none is in the
  **backlog**: `submit` with no `agent` puts it there, and `assign` gives it
  to an agent.
- **A task runs only once it has an agent.** Nothing pulls from the backlog on
  its own: your agent, with you, decides who gets what.
- **An agent may finish a task or hand it on**: it ends its reply with `HAND OFF:
  <agent> <what remains>`. The task goes to that agent, who is shown the note
  and what was done; a worktree stays as it is. A hand-off to a
  name that is no agent fails the task and says so.

Tasks that ask you something (`NEEDS INPUT:`) wait for your answer, through
your agent's `answer`, or a comment on the issue a task came from.

### Shared limits

Two agents on one model server, or one subscription, should not each assume
they have it to themselves. The team's `limits` name a limit once, and
an agent takes it in place of its own `max_concurrent` and `budget`:

```yaml
limits:
  local-server:
    max_concurrent: 1
    budget: { type: per_hour_tokens, tokens: 2000000 }
agents:
  coder:
    limit: local-server    # no max_concurrent, no budget
```

Every agent that names `local-server` uses one count of slots and one budget, so
their runs together stay within it. An agent writes either `limit`, or both
`max_concurrent` and `budget`; a mix, or neither, is refused by name. A token
budget is counted from the record of runs (`ensemble-runner agent stats`), from
what each run reported: an agent whose runs report none counts none, so its
budget never fills.

There is no sandbox: the agents' tools are their CLIs' own. `ensemble-runner
team check` and `ensemble-runner doctor` warn, without failing, about agents
that can change files directly in the repository (`isolation: shared`), about
Claude Code permission modes that skip the checks, and about token budgets.

### Teams

The runner keeps its helper teams in one directory,
`~/.local/state/ensemble/runner/teams/` (`runner/teams/` in `--config-dir`),
a folder per team named for it, and runs every team that is ready. A team gets
there in one of four ways:

```console
$ ensemble-runner team new web                                # made here: a sample to edit
$ ensemble-runner team link pcb ~/work/pcb-team                # linked: a folder of yours elsewhere
$ ensemble-runner team pull github.com/acme/docs-team@v1.2    # pulled: a git repository, pinned
$ ensemble-runner team list                                    # each team, where it comes from, its agents
```

and the fourth: a team someone else wrote, marked pending (a `pending` file in
its folder), waits for you. `ensemble-runner team approve <name>` shows every
agent command, gate command, tool list and permission mode it would run on your
machine, with a warning for each risk, and lets it run only once you say yes.
`ensemble-runner team remove <name>` takes a team out; a linked folder and a
pulled clone are left where they are. A running runner uses a change at once:
each `ensemble-runner team` command tells it to reload its teams, and so does
`ensemble-runner reload` after you edit a team by hand. A team that cannot be
loaded leaves the runner's teams as they were, and the command says why.

**Pulled teams** work like images pinned to a digest. `team pull` resolves
`@v1.2` (a tag or a branch; without one, the default branch) to the commit it
is now, clones it into your cache (`$XDG_CACHE_HOME/ensemble`), shows what it
would run as `team approve` does, and asks you to trust that commit. Only then
is the team kept, as `pulled.json` naming the repository and the commit. Git
does the talking, with your own SSH agent or credential helper; Ensemble
stores no credentials. A pulled team is code that runs with your permissions,
so:

- **It is pinned.** A branch or tag is never followed at run time, so a moved
  tag cannot swap what you approved.
- **It is trusted per commit.** A commit nobody trusted is refused, with the
  command that trusts it. There is no trusting a whole publisher.
- **The cache is checked.** A cached copy that is not at the commit, or has been
  changed by hand, is refused.
- **Updating is explicit.** `ensemble-runner team update <name> --ref main`
  shows what changed in `config.yaml` and asks you to trust the new commit; the
  pin moves only then.

A pulled team is read-only: clone it, change your copy, and `team add` that.
Its keys are not in it; the runner keeps them, or they come from your
environment, as always.

### Secrets

API keys are named by the **variable** that holds them (`api_key_env`), never
written in the runner's settings or the hub's: there is no `api_key` field,
and a config that tries one is rejected by name, before it reaches a commit.
`backend add` and `backend key` write both for you: the key where it is kept,
and its variable in the settings (see [Changing settings](#changing-settings)).

A key given to `backend add` or `backend key` is kept sealed in the runner's
database, with AES-256-GCM, by its master key,
`~/.local/state/ensemble/runner/core/master.key`, made the first time a key is
kept and readable by you alone; so are the runner's hub tokens, which
`ensemble-runner login` and `join` keep. A hub keeps the keys it is given the
same way, by its own `hub/core/master.key`, as a GitLab instance keeps its
secrets. Back up the whole folder; without `master.key`, give each backend
its key again, and sign the runner in again.

A key a secrets manager keeps is read from the environment, and named with
`--key-env`: the hub and the runner are separate services, a team's hub often
on another computer or of another user, so each reads only its own secrets. A
secret that sops-nix or agenix puts in `/run` reaches it as the service's
environment (`environmentFile` in Nix, systemd's `EnvironmentFile=`). A
variable set in the environment wins over a key the hub or the runner keeps.

The helper team holds no secrets, so nothing is read from beside it, and a
pulled or shared helper team cannot carry keys by accident. A backend's key
reaches an agent's CLI only as that backend's provider key in
`OPENCODE_CONFIG_CONTENT`, in the CLI's environment for that run, so it
cannot change how the CLIs log in. `ensemble-runner agent show` and
`ensemble-runner team check` never print a key.

### Editor support

`config.yaml` has a JSON Schema, [`ensemble.schema.json`](ensemble.schema.json):
completion for every key and value, hover descriptions, and errors for the
mistakes `ensemble-runner team check` would also report (a missing key, a
setting the adapter type does not take). The first line of the file
`ensemble-runner team new` writes points an editor with the YAML language server
at it:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Khachatur01/ensemble/master/ensemble.schema.json
```

That works in VS Code (the YAML extension), Neovim, Zed and JetBrains IDEs.
Offline or pinned to your installed version: `ensemble-runner team schema >
ensemble.schema.json` and put that path after `$schema=`. The schema is checked
against the code by the tests, so it cannot list a key the config rejects.

### Validation

Every independent problem is reported in one pass:

```console
$ ensemble-runner team check
ensemble: team 'web': ~/.local/state/ensemble/runner/teams/web/config.yaml is invalid:
  - version: 2 is not supported; this build understands version 1
  - agents.coder: `max_seconds` must be greater than 0
```

A team that is valid is then checked against the runner's backends:

```console
$ ensemble-runner team check
ensemble: team 'web': ~/.local/state/ensemble/runner/teams/web/config.yaml is invalid:
  - agents.coder.adapter.model.try[0]: LLM backend 'lcoal' is not defined in the runner's `llm`. Defined LLM backends: local.
```

A typo'd key is an error, never a silent default, and a missing key is an
error too: ``agents.coder: `max_seconds` is required at line 12 column 5``.

## Changing settings

There is no settings file to open, nor a variable to name. The settings of a
hub and of a runner are kept in their databases, as typed rows, and what you
mean to change is an operation, the same from the command line, the REST API
(`POST /api/v1/ops/<name>`) and the web UI, each checked as it is made.
Only a helper team's `config.yaml` is a file, as it is shared in git.

### By what you mean

```console
$ ensemble-hub backend add central --type openai-compatible-api --url http://127.0.0.1:8888/v1
The backend's API key (empty for none): ••••••       # asked when it wants one, without echo
added backend central, which serves unsloth/Qwen3.5-9B-GGUF, unsloth/embeddinggemma-2-GGUF
$ ensemble-hub backend key central                   # a new key, or empty to take it out
$ ensemble-hub backend list | test [name] | remove <name>
$ ensemble-hub knowledge models central:unsloth/Qwen3.5-9B-GGUF claude:claude-haiku-4-5
$ ensemble-hub knowledge embedding central:unsloth/embeddinggemma-2-GGUF   # or --off
$ ensemble-hub knowledge recall --max-notes 8 --max-chars 6000
$ ensemble-hub tracker add issues --type local_markdown --dir issues      # tracker list | remove
$ ensemble-hub role set auditor recall,share                              # role remove
$ ensemble-hub registration set approval --max-pending 20                 # or closed
$ ensemble-hub source filter central:qwen3.5-72b --prompt "Salaries; clients' names."  # or --off
$ ensemble-hub source try "Bob: deploys go out on Tuesdays now"
$ ensemble-runner backend add local --type openai-compatible-api --url http://127.0.0.1:11434/v1 --share
$ ensemble-runner backend share local | unshare local
```

- **A backend is asked first.** `backend add` asks it which models it serves
  before anything is written, so a wrong address or key is said at once;
  `--no-check` adds it anyway. One that answers that it wants a key has the
  key asked for. `--type runner` names one a runner shares through the hub
  (`--runner <owner>/<name>`), which the hub or the runner reaches only while
  it runs.
- **Keys are kept for you.** A key you give is kept under a variable Ensemble
  names after the backend (`ENSEMBLE_BACKEND_CENTRAL_KEY`), and the backend
  names that variable; `backend remove` takes it out too. The hub and the
  runner each keep it sealed in their database, by their `master.key` (see
  [Secrets](#secrets)). A key you keep elsewhere already, such as one a
  secrets manager puts in the environment (`environmentFile` in Nix), is
  named instead with `--key-env NAME`.
- **Models are checked too.** `knowledge models` asks each backend whether it
  serves its model, and `knowledge embedding` asks the model to embed once.
- **What something uses stays.** A backend the distiller, the embedding model
  or a runner's team uses is not removed, nor a role an account has.

### Every other setting

```console
$ ensemble-hub settings owner acme
$ ensemble-hub settings sessions 12                  # hours a login lasts
$ ensemble-hub settings lockout --failures 5 --minutes 15
$ ensemble-runner settings name laptop
$ ensemble-runner settings workspace ~/code
$ ensemble-runner settings works-for hub
$ ensemble-runner settings shared-calls 2
$ ensemble-hub settings show [--json]                # every setting; ensemble-runner's alike
$ ensemble-hub settings history                      # who changed what, when, with which operation
$ ensemble-hub settings import "$(cat hub.json)"     # settings another hub's `settings show --json` printed
```

Every operation prints JSON with `--json`, for a script. A change is checked
before it is kept, and one that would not load is refused with what is wrong,
and nothing changes. Each is kept in the audit log with who made it, by which
operation, with what input, its secrets hidden. A team's `config.yaml` (one
made here or linked) is changed setting by setting, its comments kept, with
`ensemble-runner team config <team> get|set|unset|edit|path`, such as
`ensemble-runner team config web set agents.coder.max_concurrent 2`.

While the hub or the runner runs, `ensemble-hub …` and `ensemble-runner …` ask it, over its socket, and it
makes the change itself and uses it at once: a team, the hub's roles, how
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

Besides Ensemble, install the CLIs of the adapters your teams use: `claude` for
`claude-code`, `opencode` for `opencode`, `codex` for `codex`. The `builtin`
and `direct` adapters need none. git is needed for the workspace, the
repositories a runner clones, worktrees and pulled teams. Then run `ensemble-runner setup`
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
team pull`, `ensemble-runner settings …` and the rest, as themselves, with no
`sudo`.

Neither has settings in Nix, as one runs a GitLab instance: the settings of
each, and the keys of its backends, are kept in its `core/database.db`,
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
backend add …`, `ensemble-runner settings …`, and its teams with
`ensemble-runner team pull`, `link` or `new`; the running runner uses each at
once. `ensemble-runner` is put on the system's `PATH`, for managing it;
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
the keys of its backends are kept in its `database.db`, and changed with
`ensemble-hub …` as `user`, its REST API or a web UI, never in Nix; only
where it listens is. The first time it starts, `ensemble-hub serve` makes the
settings a new hub starts with, every one of them, named for `user`, and its
vault. Then, as `user`: `ensemble-hub setup`, which adds the first admin, and
`ensemble-hub backend add …`,
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
$ ensemble-runner setup [--name N] [--workspace PATH] [--yes]   # find the CLIs, write the runner, check the teams
$ ensemble-runner init [--force|--keep]   # the runner setup makes, asking nothing
$ ensemble-runner serve                     # run the runner until stopped; a computer runs one
$ ensemble-runner reload                    # the running runner reads its teams again (an `ensemble-runner team` command does it by itself)
$ ensemble-runner status                    # its name, whether it runs, its workspace and projects, its hub: signed in as whom, its teams
$ ensemble-runner doctor                    # what is wrong, and how to fix each thing
$ ensemble-runner login <wss://url> --user <name>   # sign in to a hub with your account (or --token <token>), and name it in the runner's settings
$ ensemble-runner join <wss://url> <code>   # the same with a join code
$ ensemble-runner logout <wss://url>
$ ensemble-runner peers                     # the other runners of your hub you may use now
$ ensemble-runner settings show|history [--json]   # its settings, and who changed them
$ ensemble-runner settings name|workspace|works-for|shared-calls <value>
$ ensemble-runner settings import <json>   # settings `settings show --json` printed, whole

# Its helper teams
$ ensemble-runner team list                 # each team, where it comes from, its agents
$ ensemble-runner team new <name> [--stdout]          # a sample config.yaml to edit
$ ensemble-runner team link <name> <dir>              # a folder with a config.yaml, as a team
$ ensemble-runner team pull <url>[@ref] [--name N]    # a team from git, pinned to a commit you trust
$ ensemble-runner team update <name> [--ref R]        # a pulled team moved to a new commit, once you trust it
$ ensemble-runner team approve <name>                 # read a team someone else wrote, and let it run
$ ensemble-runner team remove <name>
$ ensemble-runner team check                # validate every team, print the gates and agents, and the tasks on the hub it cannot run
$ ensemble-runner team schema               # the JSON Schema of config.yaml
$ ensemble-runner team config <name> get|set|unset|edit|path   # a team's config.yaml

# Their agents, named <team>/<agent>
$ ensemble-runner agent list                # the routing table
$ ensemble-runner agent show <team>/<agent> [--probe]   # how it starts, keys hidden; --probe: which model a run would use now
$ ensemble-runner agent run <team>/<agent> <task> [--context C]   # one task to one agent, in --dir
$ ensemble-runner agent health              # every CLI starts, every backend serves its models? non-zero if not
$ ensemble-runner agent stats [--since 7d] [--json]   # runs, failures, time, tokens
$ ensemble-runner agent add <team>/<name> --adapter A [--model M] [--backend B] [--prompt P] [--writes]
$ ensemble-runner agent remove <team>/<name>

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

$ ensemble-runner completions zsh|bash|fish|elvish|powershell   # a completion script for your shell
$ ensemble-hub completions zsh|bash|fish|elvish|powershell
```

Every command takes `--config-dir <path>`, the folder everything is kept in
(by default `~/.local/state/ensemble`; the hub in its `hub/`, the runner in
its `runner/`), and every command of `ensemble-runner` `--dir <path>` (the folder
`ensemble-runner agent run` works in, and where `ensemble-runner agent health`
and `ensemble-runner doctor` start agent CLIs; by default the current
directory). `ensemble-runner agent run` prints the answer on stdout and the
provenance on stderr, so it pipes cleanly.

## How it fits together

A Cargo workspace of eleven crates under `crates/`, each depending only on
the ones before it, and the root crate, which builds the two executables and
holds the end-to-end tests: `ensemble-core` (shared types, state, runtime),
`ensemble-ops` (the operations of a hub and a runner, defined once, that the
command lines and the REST API are made from), `ensemble-config`
(`config.yaml`, the hub's settings, the runner's), `ensemble-backends`
(model servers), `ensemble-store` (the databases: the settings and sealed
keys of a hub or a runner, and the hub's accounts and tasks), `ensemble-knowledge` (the
vault and its pipeline), `ensemble-link` (the link between a runner and a hub:
its messages, and the runner's side), `ensemble-agents` (the adapters and the
router), `ensemble-tasks` (tasks as a runner leases them, and the worker that
runs a repository's tasks), `ensemble-runner` (the runner: its commands, its
workers, the projects it clones, its teams, and its models for others) and
`ensemble-hub`
(the hub: accounts, the tasks it keeps, the tracker inbox, MCP, the REST API
and the server runners connect to). The runner is built of none of the hub's
crates (`ensemble-hub`, `ensemble-knowledge`);
`tests/workspace.rs` checks that, and the order.
[ARCHITECTURE.md](ARCHITECTURE.md) says what each file does and how the pieces
work together at run time.

`ensemble.schema.json`, at the root, is the JSON Schema for `config.yaml`. The
settings of a hub and of a runner have no file, nor schema: they are typed
rows of their databases (`ensemble-store`'s `kept.rs`).

A task's worker knows the router; it does not know which CLIs exist. The router
knows agents; only the adapters in `crates/ensemble-agents/src/external/` know
how a CLI is started and read.

### Errors are one vocabulary

An agent CLI that would not start and a model server that returned 503 are the
same event to whoever reads the result: this agent did not answer, and here is
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
| `crates/ensemble-config/src/tests.rs` | every validation rule, environment precedence, secrets, typos, the schema kept in step with the code, which agents may work for other accounts |
| `crates/ensemble-agents/src/external/` | each CLI's command line, generated config and event stream, denials, limits, failures, the time limit killing the process tree |
| `crates/ensemble-agents/src/router.rs`, `crates/ensemble-backends/` | routing, `health` and its model check, the chat request in both formats |
| `crates/ensemble-knowledge/` | shares kept as they arrived, notes and the log, commits by the lock holder, the secret check; the pipeline against a stub model: new notes, merges, chatter skipped, unusable answers to review, waiting for a model, rebuild |
| `ui/server/` | the app's files served with its pages and nothing outside them, a call to the API passed on as from the hub's origin only from a page of the UI, the hub's cookies Secure behind TLS |
| `crates/ensemble-hub/` | sign-up, approval and the REST API's codes, a browser's session in a cookie no page of another origin may use, an account's tasks, runners and knowledge as operations, tokens kept as hashes and revoked at once, join codes spent once and before they expire, a runner signing in over the link with a password (and the lockout) or a code, a link without a token refused all else, a runner of another protocol version refused with what to update, roles checked per request, shares under the signed-in name; the hub's MCP: a token needed, the tools a token allows, share and recall as its account, tasks made and followed, with a `repository` and a `gate`; every connected runner listed to its owner with all its agents, to others it works for with what it lends, model calls streamed from a runner; tasks on the hub: ids, leases taken, renewed, run out and taken over, results said once and only by the runner holding the lease, questions, hand-offs, cancelling, tasks given back and blocked until a runner that has not connects or they are assigned again, the answer to where a project is kept as its repository, the tasks waiting for a runner's agents in every project, the notes recalled into a new task, over the link as the signed-in account; another account's task offered to a runner that works for the hub only when no runner of its own with the agent takes it, and acted on only while the runner is connected, works for the hub, lends the agent, and the accounts have `host` and `compute`; the inbox: markdown issues read and written with only the status line changed, issues to tasks of the account matched by tracker name, questions and answers through comments, results posted once, a closed issue cancelling its task |
| `crates/ensemble-link/` | the token file 0600, the link opened again when it breaks, a hub of another protocol version named with what to update |
| `crates/ensemble-core/` | the state directories and their hashes, the worker lock, run records |
| `crates/ensemble-tasks/` | tickets, questions and hand-offs in a reply, run logs |
| `crates/ensemble-hub/tests/queue.rs` | the worker of a repository: tasks run within `max_concurrent` and shared limits, closing the worker interrupting running tasks, cancel, `after`, gates, worktrees, `repo`, questions and answers, the backlog and `assign`, hand-offs, budgets, waiting for a backend |
| `tests/workspace.rs` | the crates depend on each other in one direction only, and the runner on none of the hub's |
| `tests/end_to_end.rs` | the real executables, `ensemble-hub` and `ensemble-runner`: their commands, `show`, `health` against a stub server, an agent changing a file; setup and doctor; the runner working on the tasks of a repository in its workspace, a task made through the hub's MCP; a task resumed when the runner starts again; a task stopped on one computer and carried on by another from its pushed branch; an issue from the tracker worked on a member's runner; a repository added to the workspace while the runner runs, a runner listed with its agents, and a task's notes in its agent's prompt; a task's repository cloned and its branch pushed there; a runner asking where a project is and cloning the answer; a task whose repository cannot be cloned given back and blocked; another account's task done by a runner that works for the hub, cloned into its own folder, and reported as that account's; an agent on a CLI's login never lent; a runner's models through the hub; signing in and joining with a code, a hub refusing another protocol or a bad token, settings changed from the command line, reloads of teams and settings |
| `crates/ensemble-agents/tests/fixtures/` | saved event streams per CLI version, replayed through each adapter's parser (see its README) |
| `tests/real_cli.rs` | the real `claude`, `opencode` and `codex` CLIs: `health` and one trivial run. Ignored; run with `ENSEMBLE_REAL_CLI=1 cargo test --test real_cli -- --ignored`. |
| `nix/example.nix` | `nix flake check` evaluates the NixOS module with it, and builds the hub's and the runner's units |

## Limitations

- **Five adapters so far.** `claude-code`, `opencode`, `codex`, `builtin` and
  `direct`.
- **Only `opencode`, `builtin` and `direct` agents run on an LLM backend**, and
  an Anthropic one only a `builtin` or `direct` agent. Claude Code and Codex use your own login.
- **Flat teams.** A helper team is a list of agents with no hierarchy. A runner
  runs all its teams at once; a task names its agent by `<team>/<agent>`.
- **Only the end of a run can ask.** An agent stops with a question
  (`NEEDS INPUT:`) at the end of a run, never halfway through one: the CLIs'
  headless modes do not wait for an answer. The task waits for the answer,
  and runs again with it.
- **No sandbox of Ensemble's own.** The agents' tools are the CLIs' own; their
  permission settings are what limit them. A `builtin` agent's file tools stay in
  the working directory, but its `shell` does not.
- **Limits are per agent unless shared.** Agents on one server share a limit only
  when they name the same one. Budgets are shared by every repository that uses the
  helper team. A token budget counts what runs report, which not every CLI does.
- **Tasks run only while one of your runners runs**, in a repository of the
  project's name in its workspace, which it clones only from a `repository` a
  task names or you answer with, using the keys its computer has; it finds
  tasks of a project it has no folder for every 30 seconds. A runner that gave
  a task back tries it again only once it is assigned or answered again. One
  runner per computer, and one worker runs a repository's tasks at
  a time. Only a worktree task in a repository with a remote moves between
  runners, carrying its work on its pushed branch; any other task waits for
  the runner whose repository it changed. What a runner that dies did since
  its run last stopped is not pushed, so the next run starts from the last
  push.
- **A task of an agent the runner has not got** fails when it starts, as does
  one whose agent's team was removed.
- **The hub does not know the teams' gates.** `submit` takes a `gate` by name,
  and the runner refuses one its agent's team has not got when it runs the
  task; a tracker's issue names none.
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
  ask; a helper agent gets the notes the hub recalled when its task was made,
  and recalls nothing more.
- **Runners are simple.** The first runner to claim a task takes it, not the
  least busy, and a task waits for a busy runner of its account's own rather
  than go to another's runner that works for the hub. Neither the runner's
  owner nor the task's account is asked about each task: the roles decide. A
  runner's models are reached at `/chat/completions` and `/completions` only.
  The hub keeps nothing of a runner once it disconnects.
- **Markdown issues only so far.** Each inbox reads a folder of markdown
  issues; GitLab, GitHub and Jira adapters are not built, and an issue cannot name a
  `repository`: a runner without a copy asks on the issue. A member's runner
  runs a hub's tasks only while it runs.
