# ensemble

An MCP server that gives your agent a team of agents.

You describe the team in `.ensemble/config.yaml`. Each agent is a coding agent
CLI you already have, with an adapter that says how to start it. Your own agent
(Claude Code, OpenCode, anything that speaks MCP) is the manager. It sees the
team as tools and delegates to an agent by name.

Ensemble runs no model and no agent loop of its own. It starts the agent's CLI
headless in the project with the task, reads what it does as it works, and
returns the answer with what it changed. Two adapters ship:

- **`claude-code`**, the Claude Code CLI, on your own subscription, with the
  tools you allow it.
- **`opencode`**, the OpenCode CLI, with the tools you allow it, on any
  provider OpenCode is set up for, or on a model server of yours that you
  define once in Ensemble's config.

A team can mix them, and your agent delegates to any of them the same way.
Codex is meant to become a further adapter type.

```
   Your agent (Claude Code, OpenCode, ...)      = the manager
        |  MCP over stdio, project as cwd
        v
     Ensemble                                   src/router.rs
        |  agents, by adapter type
        +-----------------------+
        v                       v
     claude-code: claude -p   opencode: opencode run      src/external/
                                |  provider made from an LLM backend
                                v
                              your model server (llm_backends)
```

## Quick start

1. [Install](#installation) Ensemble so that `ensemble` is on your `PATH`, and
   the agent CLIs your team uses (`claude`, `opencode`).
2. Give the project a team, then check it:

   ```console
   $ cd /your/project
   $ ensemble init     # writes .ensemble/: config.yaml, .env, .gitignore
   $ ensemble check
   $ ensemble health   # every agent CLI starts, every model server answers
   ```

   Edit the agents first: the sample has one of each kind, with every setting
   and what it does. Put API keys in `.ensemble/.env`.
3. [Connect your agent](#connect-your-agent) from the project directory.
4. Tell your agent when to use the team, in `AGENTS.md` (OpenCode) or
   `CLAUDE.md` (Claude Code):

   > Delegate implementation to the `coder` agent and review to `reviewer` with
   > the `delegate` tool. Check `agents` for what each can do.

Ensemble runs per project: your agent spawns it with the project as its working
directory, and that directory is where the team works. There is no path
argument on any tool, deliberately.

## Connect your agent

Ensemble is a standard MCP server on stdio. Any agent that can spawn one can
use it; nothing in it is specific to one harness. Register it from the project
directory, the one with `.ensemble/`.

**OpenCode**

```console
$ opencode mcp add ensemble -- ensemble mcp
$ opencode mcp list
✓ ensemble  connected
```

This writes the server into the project's `opencode.json`. Add `--global` to
register it for every project instead.

**Claude Code**

```console
$ claude mcp add --scope local ensemble -- ensemble mcp
$ claude mcp list
ensemble: ensemble mcp - ✔ Connected
```

`--scope local` keeps the registration to this project and to you;
`--scope project` writes it to `.mcp.json` for everyone who clones the repo.

A registration for every project (`--global`, `--scope user`) works too, but
only projects that have a `.ensemble/config.yaml` get a team: elsewhere the
server exits at once with "no .ensemble/config.yaml in the project directory",
and the agent lists it as failed. Ensemble reads no user-level config.

Your agent sees these tools: `delegate` (send a task to an agent, get the work
back), `submit` (write a task for an agent to work through), `status`, `wait`
and `cancel` (follow tasks and background delegations), `agents` (the team and what each is for) and `health` (every agent
CLI starts, and every model server answers and serves its agents' models).

## Configuration

Everything Ensemble keeps in a project lives in `.ensemble/`, in the project
directory (the working directory your agent spawned it in, or `--dir`):

| file | |
| --- | --- |
| `config.yaml` | the team. Commit it |
| `.env` | API keys and other variables the config names. Not committed |
| `.gitignore` | keeps `.env` out of git |
| `tasks/` | tasks for agents to work through, one directory each. Commit them |
| `runs/` | what tasks and background delegations did as they ran. Ignores itself |

`ensemble init` writes the first three and `tasks/README.md`; `config.yaml` is
a sample with every setting. Ensemble reads them from the project directory
and nowhere else: parent directories are not searched and there is no
user-level config. Each project defines its own team; to share one, copy the
folder.

```yaml
version: 1

llm_backends:
  local:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:8888/v1"
    api_key_env: OPENAI_LOCAL_API_KEY
    timeout_seconds: 600

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
    adapter:
      type: opencode
      model:
        type: llm_backend
        backend: local
        name: qwen3.5-9b
        context_window: 262144
      tools: [read, glob, grep, list, edit, bash]
      max_steps: 30
      command: opencode

  reviewer:
    prompt: You review code for correctness and report findings as file:line — problem.
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
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
does cannot be forgotten. Only two settings may be left out, because leaving
them out means "not used": an agent's `prompt` (no system prompt) and a
backend's `api_key_env` (no key). `llm_backends: {}` says there are none.

An **agent** is a member of the team. Its names are yours; nothing is
hardcoded as planner, coder or reviewer. It has:

| key | |
| --- | --- |
| `prompt` | what it is for. The first line is what your agent sees in `agents`; the whole of it is the agent's system prompt. Optional: without it the agent has none |
| `max_seconds` | wall-clock limit for one delegation. At the limit the CLI and its whole process tree are killed and what it did is returned, marked unfinished |
| `max_concurrent` | most delegations to it that run at once in a session: tasks, background and blocking delegations alike. More wait for a free slot. `1` suits a model server that serves one request at a time |
| `isolation` | where its tasks run: `shared`, in the project directory, or `worktree`, each task in a git worktree of its own (see [Tasks](#tasks-and-background-delegations)). Delegations always run in the project |
| `adapter` | what runs it. `type` decides which other fields the adapter takes |

An **LLM backend** is a model server of yours, defined once. An OpenCode agent
names it in its model (`type: llm_backend`), and Ensemble hands the agent's CLI the endpoint and key for
each run, so you never configure the same server again in the CLI's own
settings. Its one type today, `openai-compatible-api`, is any
`/chat/completions` endpoint: llama-server, vLLM, Ollama, Unsloth, a hosted
API. It requires `base_url` (the URL itself; no variable overrides it) and
`timeout_seconds` (per request), and takes `api_key_env`, the name of the
variable holding the key, when the server wants one. Only the `opencode` adapter
can run on one today; Claude Code speaks the Anthropic API, not this one.

A **gate** is a check a task's work must pass: a shell command (`sh -c`, run
in the task's working directory, exit status 0 passes) and the `timeout_seconds`
it may take. A task names one; it cannot bring a command of its own, so what
runs on your machine is what this file says. `gates: {}` says there are none.

There is no manager in the file: the agent that calls Ensemble is the manager.

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
    adapter:
      type: opencode
      model:
        type: llm_backend      # one of llm_backends serves the model
        backend: local
        name: qwen3.5-9b       # the id the server serves
        context_window: 262144 # what the server runs it with
      tools: [read, glob, grep, list, edit, "bash(git *)"]
      max_steps: 30
      command: opencode        # `opencode` if it is on PATH

  explorer:
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
    adapter:
      type: opencode
      model:
        type: provider         # a model OpenCode is already set up for
        name: opencode/big-pickle  # provider/model, as `opencode models` lists it
      tools: [read, glob, grep, list]
      max_steps: 30
      command: opencode
```

`opencode run` has no flags for a system prompt or allowed tools, so Ensemble
hands OpenCode an agent of its own through `OPENCODE_CONFIG_CONTENT`: the
prompt, `max_steps` as its step cap, and a permission set that denies
everything (`"*": "deny"`) except the tools listed. OpenCode hides denied tools
from the model, so it cannot even try them; that includes reaching outside the
project (`external_directory`). `tools` takes OpenCode's permission names
(`read`, `glob`, `grep`, `list`, `edit`, `bash`, `webfetch`, `websearch`,
`task`) or a rule such as `bash(git *)`. It runs with `--standalone`, a private
OpenCode server for the run, not the shared background service.

With a model of type `llm_backend`, the same inline config also defines that
backend as an OpenCode provider (`@ai-sdk/openai-compatible`, its base URL, its
key, its `timeout_seconds`) serving the agent's model with `context_window` as
its limit, so OpenCode knows when to compact. Nothing is written to disk. See it
with `ensemble show <agent>`:

```console
$ ensemble show coder
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

### The `claude-code` adapter

The Claude Code CLI as a member of the team, a whole agent of its own.

```yaml
agents:
  reviewer:
    prompt: You review code for correctness and report findings as file:line — problem.
    max_seconds: 1800
    max_concurrent: 1
    isolation: shared
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

### What a delegation reports

Both adapters read their CLI's JSON events as it works. A delegation's result
ends with how many steps and tool calls the agent used, every file it added,
modified or deleted, and the shell commands it ran:

```text
[coder: 6 step(s), 5 tool call(s)]
[files changed: src/lib.rs (modified), tests/new.rs (added)]
[commands run: cargo test]
```

Changed files are found by comparing the project before and after the run,
when the agent has a tool that writes (`.git`, `target` and `node_modules` are
not compared). `agents` marks each agent `[read-only]` or `[can change files]`.

Each delegation runs on its own thread, so the caller can send several at
once. They share the project: two agents editing the same file will clash.
When a delegation overlapped another, its result says some listed files may be
the other run's changes. A cancel (`notifications/cancelled`) or `max_seconds`
kills the CLI and its whole process tree.

The agents' tools are the CLIs' own and run with your permissions; `tools` and
`permission_mode` are what limit them. Ensemble adds no sandbox.

### Progress

An agent can run for minutes. While it does, `ensemble mcp` sends two
notifications per step and tool call, before the final reply:
`notifications/progress` (the client's `progressToken`, a rising counter and a
`message` such as `coder · step 2 · edit(src/lib.rs)`) and the same line as a
`notifications/message` log at `info` level. Raise the level with
`logging/setLevel` to silence the logs; progress is not filtered. A request
without a `progressToken` gets its request id as the token. Neither
notification reaches the model: they go to the client program, so they cost the
manager no tokens.

### Tasks and background delegations

A blocking `delegate` holds the caller's tool call open until the agent is done,
and the caller's MCP client may give up on it first (many clients time out a
tool call after about a minute, whatever progress notifications arrive). On a
slow machine every delegation to a local model can hit that. Two ways around
it both answer at once:

- **`submit`** writes a **task**: a ticket in `.ensemble/tasks/<id>/task.md`,
  meant to be committed. While `ensemble mcp` runs, agents work through the
  tasks without a result, oldest id first, each agent at most
  `max_concurrent` at a time, and write `result.md` beside the ticket when one
  ends. You can write a `task.md` by hand too; it is picked up within a few
  seconds.
- **`delegate` with `background: true`** is a one-off: it returns an id such as
  `adhoc-3` and writes no ticket.

Then, for either:

- `status` lists them (agent, queued / waiting for a slot / running / done /
  failed / cancelled, seconds run) with each running one's last few steps and
  tool calls, or shows one with `task`. This is the only way the manager itself
  can see what an agent is doing.
- `wait` with `task` returns the result: for a task, its `result.md`; for a
  background delegation, the reply in the same form as a blocking `delegate`.
  It waits up to `timeout_seconds` (default 30, at most 300); one still queued
  or running by then is reported, not failed, and the manager calls `wait`
  again.
- `cancel` with `task` kills the agent's CLI and its process tree. A task that
  has not started gets a `cancelled` result and never runs.

A task looks like this. Only `title` and `agent` are required; `submit` takes
the same fields:

```markdown
---
title: Add retry to the fetcher
agent: coder
context: The fetcher is src/fetch.rs; it uses ureq.
goal: A request survives one dropped connection.
constraints: [No new dependency]
files: [src/fetch.rs]
done_when: [cargo test passes]
after: [0001-add-ureq]
gate: { name: test, attempts: 3 }
---
Retry a failed request three times, waiting 1s, 2s, 4s.
```

The agent is sent the text, then the goal, constraints, files and
`done_when` as sections it can check its work against; `context` goes as the
delegation's context. A task with `after` waits until every task it names has
ended `done`; `status` shows it as `blocked by 0001-add-ureq (not done yet)`.
If one of them failed or was cancelled, it stays blocked until that task is run
again.

With `gate`, the named check from `config.yaml` runs after the agent is done.
If it fails, the same agent runs again, told the command, how it ended, the
last 60 lines of its output and its own previous reply, up to `attempts` runs
in all. The task is `done` once the gate passes and `failed` when the attempts
run out; `result.md` records `gate`, `gate_result` (`passed`, `failed`, or `not
run` when the agent itself failed) and `attempts`. `ensemble check` reports an `after` that names no task, and tasks whose
`after` leads back to themselves.

Its `result.md` starts with `status` (`done`, `failed` or `cancelled`),
`agent`, `started` and `ended`, then the agent's reply. A task with a result is
over; delete `result.md` to run it again. What a run does as it goes (its
activity log, its state) is in `.ensemble/runs/<id>/`, which ignores itself in
git. `ensemble init` writes `.ensemble/tasks/README.md` with the format, and
`ensemble check` reports a `task.md` that cannot be read or names an agent
that does not exist.

An agent with `isolation: worktree` runs each task in a git worktree of its
own, `.ensemble/worktrees/<id>`, on the branch `ensemble/<id>` made from the
project's `HEAD`. The project directory is not touched, so such tasks can run
side by side. The gate runs in the worktree too. When the task ends, everything
it changed is committed on its branch (`--no-verify`, your git identity), and
`result.md` records `branch` and `commit`. Ensemble never merges: review the
branch and merge it yourself, or have the manager do it, then
`git worktree remove .ensemble/worktrees/<id>`. A task run again, or one the
session's end interrupted, carries on in the worktree it had. The project must
be a git repository with at least one commit; `.ensemble/worktrees/` ignores
itself.

When the caller closes the pipe, running tasks and background delegations are
stopped. A task stopped that way gets no result, so it runs again in the next
session; tasks that had not started stay queued. Tasks of a `shared` agent share the
project like any other parallel delegations, so give two of them that change
the same files to an agent with `max_concurrent: 1`, or `isolation: worktree`. A blocking
`delegate` takes a slot of its agent too, waiting for one if needed; raising
the MCP timeout in the manager's config is the other fix for a client that
times out.

### Secrets

API keys are named by the **variable** that holds them (`api_key_env`), never
written in `config.yaml`: there is no `api_key` field, and a config that tries
one is rejected by name, before it reaches a commit.

The values come from the environment, or from `.ensemble/.env`:

```sh
# .ensemble/.env — NAME=value per line, # comments, optional quotes
OPENAI_LOCAL_API_KEY=sk-...
```

A variable set in the environment wins over the file. The file feeds only
Ensemble's own settings: a backend's key reaches an agent's CLI only as that
backend's provider key in `OPENCODE_CONFIG_CONTENT`, in the CLI's environment
for that run. Nothing else from it is passed on, so it cannot change how the
CLIs log in. `ensemble show` and `ensemble check` never print a key.
`ensemble init` creates the file readable by you alone, and the `.gitignore`
beside it keeps it out of git. With `--config <path>`, the `.env` beside that
file is read.

### Editor support

`config.yaml` has a JSON Schema, [`ensemble.schema.json`](ensemble.schema.json):
completion for every key and value, hover descriptions, and errors for the
mistakes `ensemble check` would also report (a missing key, a setting the
adapter type does not take). The first line of the file
`ensemble init` writes points an editor with the YAML language server at it:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Khachatur01/ensemble/master/ensemble.schema.json
```

That works in VS Code (the YAML extension), Neovim, Zed and JetBrains IDEs.
Offline or pinned to your installed version: `ensemble schema > ensemble.schema.json`
and put that path after `$schema=`. The schema is checked against the code by
the tests, so it cannot list a key the config rejects.

### Validation

Every independent problem is reported in one pass:

```console
$ ensemble check
ensemble: .ensemble/config.yaml is invalid:
  - version: 4 is not supported; this build understands version 1
  - llm_backends.x: type 'ollama' is unknown. Known types: openai-compatible-api.
  - agents.coder.adapter: LLM backend 'lcoal' is not defined. Defined LLM backends: local.
  - agents.coder.adapter: LLM backend 'lcoal' is not defined. Defined LLM backends: local.
```

A typo'd key is an error, never a silent default, and a missing key is an
error too: ``agents.coder: `max_seconds` is required at line 12 column 5``.

## Installation

Linux is the supported platform. The rest of Unix should work; it is not
tested.

### NixOS

The repository is a flake, so Ensemble installs like any other package. Add it
as an input of your system flake:

```nix
# flake.nix
inputs.ensemble = {
  url = "github:Khachatur01/ensemble";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

and to the packages of a module that receives `inputs` (through
`specialArgs`):

```nix
{ inputs, pkgs, ... }:
{
  environment.systemPackages = [
    inputs.ensemble.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

After `nixos-rebuild switch`, `ensemble` is on `PATH`. With Home Manager, put
the same expression in `home.packages`. `nix flake update ensemble` picks up
new commits.

### Nix on any Linux

```console
$ nix profile install github:Khachatur01/ensemble  # installed, on PATH
$ nix shell github:Khachatur01/ensemble            # on PATH for this shell only
$ nix run github:Khachatur01/ensemble -- check     # one command
```

An agent spawns `ensemble mcp` by name, so it has to find `ensemble` on the
`PATH` it was started with: install it, or start the agent from inside
`nix shell`.

### Other Linux: cargo

Needs Rust 1.85 or newer (edition 2024) and a C linker.

```console
$ cargo install --git https://github.com/Khachatur01/ensemble
```

This puts `ensemble` in `~/.cargo/bin`, which must be on `PATH`. Run the same
command again to update.

Besides Ensemble, you need the CLIs of the adapter types your team uses:
`claude` for `claude-code`, `opencode` for `opencode`. There is no runtime
service, no daemon and no port.

## Usage

Day to day you do not run Ensemble; your agent does. These are for setting it
up and for debugging it.

```console
$ ensemble init                 # write .ensemble/ with a sample config.yaml, .env, .gitignore
$ ensemble init --stdout        # print it instead (init refuses to overwrite without --force)
$ ensemble check                # validate the config, print the team
$ ensemble agents               # the routing table
$ ensemble show <agent>         # how an agent is started: command line and generated config, keys hidden
$ ensemble health               # every agent CLI starts, every LLM backend serves its models? non-zero if not
$ ensemble run <agent> <task>   # send one task to one agent (it works in --dir)
$ ensemble schema               # print the JSON Schema for config.yaml
$ ensemble mcp                  # serve the team over stdio
```

`run` prints the answer on stdout and the provenance on stderr, so it pipes
cleanly. Options: `--config <path>`, `--dir <path>` (the project, default the
current directory), `--context <text>` (for `run`).

## How it fits together

| | |
| --- | --- |
| `src/config.rs` | `.ensemble/config.yaml` and `.env`: load, resolve the environment, validate |
| `src/router.rs` | agents to adapters; the ensemble itself, and `health` |
| `src/external/` | the `claude-code` and `opencode` adapters: each CLI as a subprocess, its JSON events read as it goes |
| `src/workspace.rs` | the project directory, and what a run changed in it |
| `src/llm_backend.rs` | asking a model server which models it serves, for `health` |
| `src/port.rs` | the shared error type |
| `src/tasks.rs` | tasks as files: `task.md`, `result.md`, and the git-ignored `runs/` |
| `src/worktree.rs` | a git worktree and branch per task, and committing its work there |
| `src/harness/delegation.rs` | the tools your agent calls: delegate, submit, status, wait, cancel, agents, health |
| `src/harness/queue.rs` | the worker that runs tasks, background delegations, agent slots (`max_concurrent`), wait and cancel |
| `src/harness/mcp.rs` | the MCP server, over stdio |
| `ensemble.schema.json` | the JSON Schema for `config.yaml` |

The MCP server knows the router; it does not know which CLIs exist. The router
knows agents; only the adapters in `src/external/` know how a CLI is started
and read.

### Errors are one vocabulary

An agent CLI that would not start and a model server that returned 503 are the
same event to your agent: this agent did not answer, and here is why. So there
is one `port::Error`, each value carrying where it came from, knowing whether a
retry could plausibly work, and written to be read by the model that will see
it. A failed delegation reaches the model as a tool *result* with
`isError: true`, never as a JSON-RPC error: a transport error is handled by the
harness and the model never learns the reason, which is the one thing it needs
in order to do something sensible next.

## Tests

```console
$ nix develop   # cargo, rustc, clippy, rust-analyzer
$ cargo test
```

None of the tests need the network, a model server or an agent CLI: shell
scripts stand in for the CLIs.

| suite | what it covers |
| --- | --- |
| `src/config.rs` | every validation rule, environment precedence, secrets, typos, the schema kept in step with the code |
| `src/external/` | each CLI's command line, generated config and event stream, denials, limits, failures, the time limit killing the process tree |
| `src/router.rs`, `src/llm_backend.rs` | routing, `health` and its model check |
| `src/tasks.rs` | ticket files: reading, ids, results written once, run logs |
| `src/harness/mcp.rs` | the protocol: handshake, tool schemas, notifications, parallel calls, cancellation, tasks run within `max_concurrent`, the end of a session |
| `tests/end_to_end.rs` | the real binary: CLI, `show`, `health` against a stub server, MCP over real pipes, an agent changing a file |

## Limitations

- **Two adapters so far.** `claude-code` and `opencode`. Codex is planned, not
  built.
- **Only OpenCode runs on an LLM backend.** Claude Code needs the Anthropic API.
- **Delegation is single-shot.** An agent cannot ask a clarifying question; the
  caller must pass everything in `task` and `context`.
- **No sandbox of Ensemble's own.** The agents' tools are the CLIs' own; their
  permission settings are what limit them.
- **Concurrency is capped per agent, not per model server.** Two agents on one
  server each get their own `max_concurrent`. Nothing caps spending a Claude
  subscription limit in a loop.
- **Tasks run only while a session runs.** There is no daemon: the worker lives
  in `ensemble mcp`.
- **The project directory is fixed when the server starts.** The working
  directory of the process (or `--dir`). It cannot vary per call.
