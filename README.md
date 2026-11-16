# ensemble

An MCP server that gives your agent a helper team of agents.

You describe your helper team in one `config.yaml` in a directory of its own
(usually a git repo): a flat list of professional agents, such as a coder, a
reviewer or a PCB designer. The directory can be yours alone or one your company
shares. Each agent is a coding agent CLI you already have, with an adapter that
says how to start it, or one request straight to a model. Your own agent (Claude
Code, OpenCode, anything that speaks MCP) sees the team as tools and delegates to
an agent by name, so cheap work goes to cheap models and you keep working on your
own tasks. Nothing of Ensemble lives in your projects.

Ensemble mostly runs no agent loop of its own. It starts the agent's CLI
headless in the project with the task, reads what it does as it works, and
returns the answer with what it changed. When you would rather install no CLI,
the `builtin` adapter is a loop in Ensemble itself. Five adapters ship:

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

A team can mix them, and your agent delegates to any of them the same way.

With a hub (`--hub`), Ensemble also keeps your knowledge: what your agent
shares when you ask becomes notes in a git vault, and later work recalls them.
See [Knowledge](#knowledge).

```
   Your agent (Claude Code, OpenCode, ...)
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

```console
$ ensemble setup
```

That is all most people need. `setup` finds the agent CLIs you have (`claude`,
`opencode`) and any local model server, asks which agent you talk to and where
the helper team should live, writes a starter team there, registers Ensemble
with your agent and checks that every agent starts. It ends with a sentence to
try: restart your agent and ask it to have the helper team implement something.
Your agent learns the team from Ensemble itself when it connects, so there is
nothing to add to its `CLAUDE.md` or `AGENTS.md`.

Without a terminal, or to script it: `ensemble setup --yes --manager claude-code
--team ~/my-team --starter software`. Starters are `software`, `hardware` and
`docs`. [Install](#installation) Ensemble first, so that `ensemble` is on your
`PATH`.

Afterwards, `ensemble doctor --team ~/my-team` says what is wrong and how to fix
each thing, and `ensemble add-agent tester --adapter claude-code
--team ~/my-team` and `ensemble remove-agent` change the team without editing
the file (the file stays yours: comments and order are kept, and every setting
is written out).

The folder your agent opens is where the team works, and it can be a parent of
several repositories. Ensemble keeps nothing in it: tasks, run logs and
worktrees go in a per-user state directory (`$XDG_STATE_HOME/ensemble`, by
default `~/.local/state/ensemble`), one per folder. A tool takes a `repo`
argument, a path inside the folder, to work in one repository of several; a
path that leaves the folder is refused.

## Connect your agent

Ensemble is a standard MCP server on stdio. `ensemble setup` and
`ensemble connect --manager claude-code|opencode|codex --team <dir>` register
it for you and print what they run; this shows the same by hand. Any agent that
can spawn one can use it; nothing in it is specific to one harness. Register it
with the helper team it serves: `--team` is the directory with `config.yaml`.

**OpenCode**

```console
$ opencode mcp add ensemble -- ensemble mcp --team ~/my-team
$ opencode mcp list
✓ ensemble  connected
```

This writes the server into the project's `opencode.json`. Add `--global` to
register it for every project instead.

**Claude Code**

```console
$ claude mcp add --scope local ensemble -- ensemble mcp --team ~/my-team
$ claude mcp list
ensemble: ensemble mcp - ✔ Connected
```

`--scope local` keeps the registration to this project and to you;
`--scope project` writes it to `.mcp.json` for everyone who clones the repo.

A registration for every project (`--global`, `--scope user`) works too: the
helper team is named in the registration, so the same team serves every folder
you open. Ensemble reads no other config and searches for none. `--org` is the
old name of `--team` and still works for one release, with a notice.

**Through a node.** Registered with no `--team`, `ensemble mcp` is a session
of your running [node](#nodes), which has the helper team: one registration
for every project (`ensemble connect --manager claude-code --global`, or
`claude mcp add --scope user ensemble -- ensemble mcp`), and tasks that go on
after the session ends. Without a node running it says how to start one.

Your agent sees these tools: `delegate` (send a task to an agent, get the work
back), `submit` (write a task for an agent to work through), `status`, `wait`
and `cancel` (follow tasks and background delegations), `answer` (answer a
question a task stopped to ask), `agents` (the team and what each agent is for) and `health` (every agent
CLI starts, and every model server answers and serves its agents' models).

## Configuration

The helper team is one directory, `--team`, with one file in it: `config.yaml`,
the llm backends and the agents. Commit it; it holds no secrets. Ensemble reads
it from where it is named and nowhere else: nothing is searched for, and there
is no user-level config. `ensemble init` writes a sample with every setting.

What Ensemble learns while it runs is state, not configuration, and is kept per
user, outside every project:

| where | |
| --- | --- |
| `$XDG_STATE_HOME/ensemble/workdirs/<folder>-<hash>/tasks/` | the tasks for the agents to work through, one directory each |
| `.../runs/` | what tasks and background delegations did as they ran |
| `.../worktrees/` | a git worktree per task of an agent with `isolation: worktree` |
| `$XDG_STATE_HOME/ensemble/orgs/<team>-<hash>/usage.json` | how many runs each agent started, shared by every project that uses the helper team |

`--state <dir>` puts the first three somewhere else. An older config, version 1
(one team per project) or version 2 (an organization of teams), becomes a
version 3 helper team with `ensemble migrate <file> --team <dir>`: every team's
agents and gates go into one list, and a name two teams both used becomes
`<team>-<name>`. Tasks kept in the old `tasks/<team>/` folders are moved up the
first time they are read.

```yaml
version: 3

llm_backends:
  local:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:8888/v1"
    api_key_env: OPENAI_LOCAL_API_KEY
    timeout_seconds: 600

limits: {}

tracker:
  type: local_markdown

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
does cannot be forgotten. Only two settings may be left out, because leaving
them out means "not used": an agent's `prompt` (the CLI's own system prompt
alone) and a backend's `api_key_env` (no key). `llm_backends: {}` says there
are none.

An **agent** is a member of the helper team. Its names are yours; nothing is
hardcoded as planner, coder or reviewer. There is no hierarchy: every agent is
one professional your agent can hand work to. It has:

| key | |
| --- | --- |
| `prompt` | what it is for. The first line is what your agent sees in `agents`; the whole of it goes into the agent's system prompt, added to the CLI's own (OpenCode can also replace its own; see `system_prompt`). Optional: without it a CLI agent runs on its CLI's own system prompt alone, and a `direct` agent has none |
| `max_seconds` | wall-clock limit for one delegation. At the limit the CLI and its whole process tree are killed and what it did is returned, marked unfinished |
| `limit` | the name of one of the team's [`limits`](#shared-limits), in place of `max_concurrent` and `budget` |
| `max_concurrent` | most delegations to it that run at once in a session: tasks, background and blocking delegations alike. More wait for a free slot. `1` suits a model server that serves one request at a time |
| `isolation` | where its tasks run: `shared`, in the project directory, or `worktree`, each task in a git worktree of its own (see [Tasks](#tasks-and-background-delegations)). Delegations always run in the project |
| `budget` | how much it may run in an hour: `{ type: unlimited }`, `{ type: per_hour, delegations: 20 }` (runs), or `{ type: per_hour_tokens, tokens: 2000000 }` (the tokens its runs reported). Tasks, each gate retry, and delegations, blocking or in the background, all count, across sessions and projects (`usage.json` in the helper team's state directory). Over it, a delegation is refused saying when the next run is allowed, a task waits (`queued, over its agent's budget for 12 more minutes`), and a gate retry is not made |
| `adapter` | what runs it. `type` decides which other fields the adapter takes |

An **LLM backend** is a model server of yours, or one your company runs, defined
once. An OpenCode agent names it in its model (`type: llm_backend`), and a
`builtin` or `direct` agent in its adapter, and Ensemble hands the agent's CLI the endpoint
and key for each run, so you never configure the same server again in the CLI's
own settings. It has a `type`:

- `openai-compatible-api`: any `/chat/completions` endpoint: llama-server, vLLM,
  Ollama, Unsloth, a hosted API. `opencode`, `builtin` and `direct` agents can
  run on it.
- `anthropic-api`: Anthropic's own Messages API (`base_url:
  https://api.anthropic.com`, the key sent as `x-api-key`). `builtin` and `direct` agents
  can run on it; OpenCode needs an openai-compatible one, and a config that says
  otherwise is refused by name. Claude Code uses your own login, not a backend.

Either requires `base_url` (the URL itself; no variable overrides it) and
`timeout_seconds` (per request), and takes `api_key_env`, the name of the
variable holding the key, when the server wants one. `health` asks each for the
models it serves.

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
`fail` makes the delegation or task fail, saying what each backend answered;
`wait` keeps a task in the queue, noted in its log (`waiting for a backend`),
and tries again later, while a delegation, which someone is waiting for, fails.
A run on a later model says so in what it reports (`opencode on central
(fallback 2 of 2)`); `agents` marks an agent whose first model was last found
down, and `ensemble show <agent> --probe` says which model a run would use now.
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
{ name: test, attempts: 3 }` (see [Tasks](#tasks-and-background-delegations)).
A task cannot bring a command of its own, so what runs on your machine is
what this file says. `gates: {}` says there are none, which is fine: tasks
then simply have no check. Blocking `delegate` calls never run gates.

The **tracker** says where the team's tasks come from. Today it is
`{ type: local_markdown }`: markdown files in the state directory (see
[Tasks](#tasks-and-background-delegations)).

There is no leader in the file: the agent that calls Ensemble decides who does
what.

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
        type: llm_backend      # models that llm_backends serve
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

### The `direct` adapter

A light agent: no CLI, no tools, no loop. The task (after its context) and the
agent's prompt (as the system prompt) go straight to an LLM backend as one
`/chat/completions` request, and the answer comes back. Before version 3 it was
called `llm`; `ensemble migrate` renames it. It reads and changes
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
        - backend: local     # one of llm_backends
          model: qwen3.5:9b  # the id the server serves
      when_none: fail        # or wait: a task waits for a backend
      max_tokens: 1000       # an answer cut off here is marked unfinished
```

The request waits up to the backend's `timeout_seconds`, and the agent's
`max_seconds` or a cancel ends the wait sooner. `ensemble show <agent>` prints
the request; `health` checks the backend serves the model.

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
        - backend: local         # one of llm_backends, of either type
          model: qwen3-coder:30b
          context_window: 65536  # the loop compacts before it outgrows it
      when_none: fail
      tools: [read, list, grep, glob, edit, write, shell]
      max_steps: 40              # model calls for one delegation
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
cancel, the run stops with what it did so far, marked unfinished. A shared
node may offer a `builtin` agent: it runs on the node's own model servers.

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
which `check` and `doctor` warn about. It has no flag for a system prompt, so the
agent's `prompt` goes in front of the task. Progress, the commands it ran, its
tokens and its answer are read from its JSON events as it works.

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

### What the team costs

Every run, whether a delegation, a task, a gate retry or a background run, leaves
one line in `records.jsonl` beside the usage counters (shared by every project
that uses the helper team): the agent, the model that answered, when it started
and how long it took, the tokens, the steps and tool calls, and whether it
worked. `ensemble stats` sums them per agent:

```console
$ ensemble stats --team ~/my-team --since 7d
agent              runs  failures    total s    mean s   tokens in  tokens out
coder                12         1     1843.2     153.6      418220       30114
reviewer              9         0      402.7      44.7           -           -
```

A `-` means no run of that agent reported tokens: an agent's CLI or server that
says nothing is not the same as one that spent none. `--since` takes `30m`,
`12h`, `7d` or `2w`; `--json` prints the same as data.

### Progress

An agent can run for minutes. While it does, `ensemble mcp` sends two
notifications per step and tool call, before the final reply:
`notifications/progress` (the client's `progressToken`, a rising counter and a
`message` such as `coder · step 2 · edit(src/lib.rs)`) and the same line as a
`notifications/message` log at `info` level. Raise the level with
`logging/setLevel` to silence the logs; progress is not filtered. A request
without a `progressToken` gets its request id as the token. Neither
notification reaches the model: they go to the client program, so they cost the
calling agent no tokens.

### Tasks and background delegations

A blocking `delegate` holds the caller's tool call open until the agent is done,
and the caller's MCP client may give up on it first (many clients time out a
tool call after about a minute, whatever progress notifications arrive). On a
slow machine every delegation to a local model can hit that. Two ways around
it both answer at once:

- **`submit`** writes a **task**: a ticket in the state directory, `tasks/<id>/task.md`. Ids are numbered across the folder, so an id names one task. A ticket from before version 3 that names its agent as `team/agent` is read as `agent`. While `ensemble mcp` runs, agents work through the
  tasks without a result, oldest id first, each agent at most
  `max_concurrent` at a time, and write `result.md` beside the ticket when one
  ends. You can write a `task.md` by hand too; it is picked up within a few
  seconds.
- **`delegate` with `background: true`** is a one-off: it returns an id such as
  `adhoc-3` and writes no ticket.

Then, for either:

- `status` lists them (agent, queued / waiting for a slot / running / done /
  failed / cancelled, seconds run) with each running one's last few steps and
  tool calls, or shows one with `task`. This is the only way the calling agent
  can see what an agent is doing.
- `wait` with `task` returns the result: for a task, its `result.md`; for a
  background delegation, the reply in the same form as a blocking `delegate`.
  It waits up to `timeout_seconds` (default 30, at most 300); one still queued
  or running by then is reported, not failed, and the calling agent calls `wait`
  again.
- `cancel` with `task` kills the agent's CLI and its process tree. A task that
  has not started gets a `cancelled` result and never runs.

You can do the same from the terminal, with no agent in between: `ensemble
tasks` and `ensemble task <id>` read the state directory and show what
`status` and `wait` would, `ensemble submit` writes a ticket and `ensemble
answer` answers a question. They start no worker: a running `ensemble mcp` for
the same working folder runs the tasks, and the folder is the current directory
or `--dir`.

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

What each field is for:

| field | |
| --- | --- |
| `title` | required. A short name; it makes the task's folder name (`0002-add-retry-to-the-fetcher`) |
| `agent` | required. Which agent of the team does it |
| the text below the `---` | what to do |
| `context` | background the agent needs, sent as the delegation's context |
| `goal` | the outcome wanted, in a sentence, as opposed to the steps |
| `constraints` | what the agent must not do or must keep |
| `files` | where to look or work |
| `done_when` | conditions that mean it is finished. The agent is told to check its work against them. They are advice to the agent; nothing runs them (that is what `gate` is for) |
| `after` | ids of tasks that must end `done` first |
| `gate` | `name`: a check defined under `gates` in `config.yaml`. `attempts`: how many runs the agent gets in all, the first included, so `1` means no retry |

The agent is sent the text, then the goal, constraints, files and
`done_when` as sections it can check its work against. A task with `after`
waits until every task it names has ended `done`; `status` shows it as
`blocked by 0001-add-ureq (not done yet)`. If one of them failed or was
cancelled, it stays blocked until that task is run again. `ensemble check`
reports an `after` that names no task, and tasks whose `after` leads back to
themselves.

**Using a gate.** `done_when` tells the agent what finished means; `gate`
makes Ensemble check it. With `gate`, the named command from `config.yaml`
runs once the agent is done:

1. It passes: the task is `done`.
2. It fails: the same agent runs again, told the command, how it ended, the
   last 60 lines of its output and its own previous reply.
3. This repeats until it passes or the agent has had `attempts` runs in all.
   Then the task is `failed`.

Each retry takes a run from the agent's `budget`; over it, no retry is made.
`result.md` records `gate`, `gate_result` (`passed`, `failed`, or `not run`
when the agent itself failed) and `attempts`. A gate that is not defined in
`config.yaml` fails the task at once, and `ensemble check` reports it before
that.

Its `result.md` starts with `status` (`done`, `failed` or `cancelled`),
`agent`, `started` and `ended`, then the agent's reply. A task with a result is
over; delete `result.md` to run it again. What a run does as it goes (its
activity log, its state) is in `runs/<id>/` beside `tasks/`. `ensemble check` reports a `task.md` that cannot be read or names an agent
that does not exist.

A task's agent may stop to ask. It is told that, when it cannot finish without
a decision only the person who asked can make, it should end its reply with a
line `NEEDS INPUT: <question>`. The question is added to `questions.md` beside
the ticket, the task shows as `needs input:
<question>` in `status`, and `wait` returns the question. `answer` adds the
answer under it, and the task runs again, given the questions and answers so
far and its reply when it stopped. A gate does not run on a reply that asks,
and a worktree is not committed until the task ends.

An agent with `isolation: worktree` runs each task in a git worktree of its
own, `worktrees/<id>` in the state directory, on the branch `ensemble/<id>` made
from the repository's `HEAD`. The repository's own files are not touched, so such tasks can run
side by side. The gate runs in the worktree too. When the task ends, everything
it changed is committed on its branch (`--no-verify`, your git identity), and
`result.md` records `branch` and `commit`. Ensemble never merges on its own, and the calling agent has no tool that does. Look at
the work with `ensemble review <id>` (the branch, what changed on it, the result;
the calling agent has a read-only `review` tool for the same) and `ensemble diff <id>`.
Take it in with `ensemble merge <id>`, which only you run: it refuses a task that
is not done, a repository with uncommitted changes, and a merge that conflicts
(which it undoes, naming the files). `--squash` makes one commit; `--cleanup`
then removes the task's worktree and branch, and nothing is deleted without it. A task run again, or one the
session's end interrupted, carries on in the worktree it had. The working folder, or
the task's `repo`, must be a git repository with at least one commit.

### A helper team that works on its own

You own the work, not operate it: you talk to your own agent about what you
want, and it plans the work into tasks. You can see who does what, but you do
not have to.

- **Tasks have an assignee**, an agent. A task with none is in the
  **backlog**: `submit` with no `agent` (or `ensemble submit TITLE --backlog`)
  puts it there, and `assign` (the tool, or `ensemble assign ID AGENT`) gives it
  to an agent. The assignee is kept in `assignee.txt` beside the ticket, which is
  never rewritten.
- **An agent works only on tasks assigned to it** and stays idle until one
  appears for it. Nothing pulls from the backlog on its own: your agent, with
  you, decides who gets what.
- **An agent may finish a task or hand it on**: it ends its reply with `HAND OFF:
  <agent> <what remains>`. The task goes to that agent, who is shown the note
  and what was done (`handoffs.md`); a worktree stays as it is. A hand-off to a
  name that is no agent fails the task and says so.
- **When nothing is active** (no task is assigned, unfinished and not waiting for
  your answer) the team says so, and what waits: `Idle: no active tasks (2
  in the backlog, to be assigned, 1 waiting for an answer). The helper team waits
  for planning: tell your agent what to do next.` `status` and `ensemble tasks`
  show it, and `ensemble serve` prints it once without stopping;
  `ensemble serve --exit-when-idle` stops there instead.

Tasks that ask you something (`NEEDS INPUT:`) wait for your answer, through
your agent or `ensemble answer`.

### Running tasks without a session

`ensemble serve --team <dir>`, run in the working folder, is the same worker with
no agent around it: it works through the folder's tasks until it gets Ctrl-C or
SIGTERM, then stops the running ones (they run again next time) and leaves the
rest queued. Only one worker runs a folder's tasks, so a second `serve`
refuses, and an `ensemble mcp` that finds one running reads and writes tasks
but starts none of its own; `ensemble tasks` shows a task as `running` while
the other worker has it. The worker's lock is a file in the state directory; one
left by a process that died is taken over.

To keep it running, as a systemd user unit (`~/.config/systemd/user/ensemble.service`):

```ini
[Service]
WorkingDirectory=%h/work/myproject
ExecStart=ensemble serve --team %h/my-team --env-file %h/.config/ensemble.env
Restart=on-failure

[Install]
WantedBy=default.target
```

and `systemctl --user enable --now ensemble`. On macOS, a launchd agent
(`~/Library/LaunchAgents/dev.ensemble.plist`):

```xml
<plist version="1.0"><dict>
  <key>Label</key><string>dev.ensemble</string>
  <key>WorkingDirectory</key><string>/Users/me/work/myproject</string>
  <key>ProgramArguments</key><array>
    <string>ensemble</string><string>serve</string>
    <string>--team</string><string>/Users/me/my-team</string>
  </array>
  <key>KeepAlive</key><true/>
</dict></plist>
```

When the caller closes the pipe of an `ensemble mcp`, running tasks and background delegations are
stopped. A task stopped that way gets no result, so it runs again in the next
session; tasks that had not started stay queued. Tasks of a `shared` agent share the
project like any other parallel delegations, so give two of them that change
the same files to an agent with `max_concurrent: 1`, or `isolation: worktree`. A blocking
`delegate` takes a slot of its agent too, waiting for one if needed; raising
the MCP timeout in your agent's config is the other fix for a client that
times out.

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
budget is counted from the record of runs (`ensemble stats`), from what each run
reported: an agent whose runs report none counts none, so its budget never fills.

There is no sandbox: the agents' tools are their CLIs' own. `ensemble check` and
`ensemble doctor` warn, without failing, about agents that can change files
directly in your working folder (`isolation: shared`), about Claude Code
permission modes that skip the checks, and about token budgets.

### Remote helper teams

A helper team can live in a git repository you do not have checked out, and be
shared by a department or a company:

```console
$ ensemble connect --manager claude-code --team github.com/acme/team@v1.2
```

`connect` resolves `@v1.2` (a tag or a branch) to the commit it is now, clones it
into your cache (`$XDG_CACHE_HOME/ensemble`), prints every agent command, gate
command, tool list and permission mode the config would run on your machine,
with a warning for each risk, and asks you to trust that commit. Only then does
it register `github.com/acme/team#<commit>`. Git does the talking, with your own
SSH agent or credential helper; Ensemble stores no credentials.

A remote helper team is code that runs with your permissions, so:

- **It is pinned.** `ensemble mcp` and every other command accept only
  `<url>#<commit>`; a branch or tag is never followed at run time, so a moved
  tag cannot swap what you approved.
- **It is trusted per commit.** `ensemble team trust <url>#<commit>` shows what
  it would run and records your approval for that exact commit. An untrusted
  commit is refused with that command to run; the MCP server cannot ask,
  because stdio is the protocol. There is no trusting a whole publisher.
- **The cache is checked.** A cached copy that is not at the commit, or has been
  changed by hand, is refused.
- **Updating is explicit.** `ensemble team update <url>#<commit> --ref main` shows
  what changed in `config.yaml` and prints the new pin; nothing is trusted or
  registered until you do both.

A remote helper team is read-only: clone it to change it. Its keys are not in
it; they come from your environment or `--env-file`, as always.

### Secrets

API keys are named by the **variable** that holds them (`api_key_env`), never
written in `config.yaml`: there is no `api_key` field, and a config that tries
one is rejected by name, before it reaches a commit.

The values come from the environment, or from a file you name with `--env-file`
(registered once, in your agent's `ensemble mcp` command):

```sh
# keys.env — NAME=value per line, # comments, optional quotes
OPENAI_LOCAL_API_KEY=sk-...
```

A variable set in the environment wins over the file. The helper team holds no
secrets, so nothing is read from beside it, and a remote or shared helper team
cannot carry keys by accident. The file feeds only
Ensemble's own settings: a backend's key reaches an agent's CLI only as that
backend's provider key in `OPENCODE_CONFIG_CONTENT`, in the CLI's environment
for that run. Nothing else from it is passed on, so it cannot change how the
CLIs log in. `ensemble show` and `ensemble check` never print a key.
Keep the file readable by you alone, and out of any repository.

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
$ ensemble check --team ~/my-team
ensemble: ~/my-team/config.yaml is invalid:
  - version: 4 is not supported; this build understands version 3
  - llm_backends.x: type 'ollama' is unknown. Known types: openai-compatible-api.
  - agents.coder.adapter: LLM backend 'lcoal' is not defined. Defined LLM backends: local.
  - agents.reviewer.adapter: LLM backend 'lcoal' is not defined. Defined LLM backends: local.
```

A typo'd key is an error, never a silent default, and a missing key is an
error too: ``agents.coder: `max_seconds` is required at line 12 column 5``.

## Knowledge

A **hub** keeps your knowledge: what your agents share becomes notes in a
vault, and later work recalls them. It is configured in its own `hub.yaml`, in a
directory you name with `--hub`, apart from the helper team, which a company may
share. Without `--hub` there is no knowledge; everything above works the same.

```console
$ ensemble hub init --hub ~/hub     # writes ~/hub/hub.yaml and makes ~/hub/vault a git repository
$ ensemble hub check --hub ~/hub    # validates it; says which model would distill shares now
$ ensemble connect --manager claude-code --team ~/team --hub ~/hub
```

```yaml
version: 1
owner: Ana                  # named in everything you share
llm_backends:               # the hub's own model servers, the same kinds as config.yaml
  local: { type: openai-compatible-api, base_url: "http://127.0.0.1:11434/v1", timeout_seconds: 300 }
knowledge:                  # leave it out for no knowledge
  vault: vault              # a git repository, relative to this directory
  distill:                  # the models that turn shares into notes; no tools
    try:
      - { backend: local, model: qwen3.5:9b }
    when_none: wait         # or fail: the share goes to review/ with why
    max_tokens: 4000
  recall:
    max_notes: 8
    max_chars: 6000         # all returned notes together
```

**Sharing is explicit.** Nothing reads your CLI's transcripts and nothing
proxies its traffic, so a private or throwaway conversation stays private and no
subscription's rules are touched. When you want something kept, tell your agent
in plain words ("share what we decided about the deploys"); it picks what is
worth keeping and calls the `share` tool. `ensemble share --hub <dir> --title T`
does the same by hand. A share that looks like it holds a key or a private key
is refused, so the agent shares again without it: the vault keeps everything it
is given.

**Only Ensemble writes notes.** Each share is kept as it arrived, and the
pipeline turns it into notes in the background, while `ensemble mcp` or
`ensemble serve` runs, or now with `ensemble knowledge distill`:

1. One chat request extracts typed items (decision, fact, how-to, lesson,
   commitment, preference, ownership); chatter gives none and is skipped.
2. Each item is compared with the notes most like it. It becomes a new note,
   is merged into one, confirms one, or, when it contradicts one, waits in
   `review/` for you.
3. The notes are written and the vault is committed, one commit per share.

To correct a note, share the correction. To read the vault, open it as a
folder in Obsidian, or anything that reads Markdown:

```text
vault/
  shares/<yyyy>/<mm>/<id>.md   each share as it arrived, never changed
  notes/<slug>.md              one note per topic, frontmatter: kind, title, tags,
                               scope, sources (share ids), shared_by, pipeline, updated
  review/<share-id>.md         what the pipeline would not write, and why
  log.jsonl                    what the pipeline did with each share
```

Because the notes are made from the shares, a better model or a new note format
needs no migration: `ensemble knowledge rebuild` deletes the notes and makes
them again from every share, in one commit.

**Shared text is data, not instructions.** It may carry instructions meant for
a model, so the model that distills has no tools (one chat request, never an
agent CLI); shared text goes in between markers with a nonce nobody can guess;
an answer must be the JSON asked for and may only change the notes it was
offered, or it goes to `review/`; and every note names the shares it came from.

**Recall.** Your agent calls `recall` with a topic before work where your
earlier decisions or setup may matter, and gets the matching notes, best first,
within `max_notes` and `max_chars`, marked as reference data.
`ensemble recall --hub <dir> <topic>` prints the same. The search is by words
(BM25, a title's words counting more), with no index to keep.

**Notes in your own sessions.** Your agent recalls when it decides to. With
session hooks, Claude Code and Codex get the notes without asking: before each
prompt you send, the notes about it go into the conversation, and at the start
of a session, those about the project's folder. Each note is given once per
session, and again after the context is cleared or compacted. A prompt of one
or two words recalls nothing. Set them up when you register Ensemble:

```sh
ensemble connect --manager claude-code --team <dir> --hub <hub> --hooks
```

This writes the hook into Claude Code's `.claude/settings.local.json` of the
project you run it in (not committed), or into your own `~/.claude/settings.json`
with `--global`. For Codex, it writes `.codex/hooks.json`, or `~/.codex/hooks.json`
with `--global`, and Codex runs it once you trust it in `/hooks`. Other hooks in
those files are kept, and running it again replaces only Ensemble's. The hook is
`ensemble hook --hub <hub>`: it reads the event on stdin and prints the notes. It
never fails your turn; a hub it cannot reach costs that prompt its notes.
OpenCode has plugins, not command hooks, so it gets knowledge through the MCP
tools only.

**Notes for the helper team.** An agent of the helper team gets notes only if
its block in `config.yaml` asks for them:

```yaml
agents:
  coder:
    recall:
      max_chars: 3000   # most characters of notes given with one task
    # ...
```

Then each task and delegation it gets comes with the vault's notes about it,
found from the task's text, after the task's context, and still under the header
that marks them as reference data. Recall happens once per task, not again for
each gate retry. A question the agent stops to ask keeps the notes for the
answer. Leave `recall` out and an agent gets no notes. That matters because the
notes go to whatever model the agent runs on, so give them only to agents whose
backend may see them. With no hub that keeps knowledge (`--hub`), an agent with
`recall` gets nothing, and the session says so on stderr. A hub that cannot be
reached costs the task its notes, not its run.

## Tasks from a tracker

Tasks live in a tracker and are assigned to **people**. When an issue is
assigned to you and carries the hub's label, the hub's inbox makes it a task for
your helper team and writes what happens back on the issue. Your helper team
never touches the tracker itself. The first tracker is a folder of markdown
files; GitLab, GitHub and Jira come later.

```yaml
# hub.yaml
tasks:                         # leave it out for no inbox
  tracker:
    type: local_markdown
    dir: issues                # a folder of <id>.md issues, often a git repo a team shares
  user: ana                    # who you are in the tracker
  label: ensemble              # the signal
  poll_seconds: 30
  projects:                    # an issue's project -> the folder its work happens in
    shop: /home/ana/code/shop
```

An issue is a markdown file anyone can write:

```markdown
---
title: Fix the login redirect
assignee: ana
labels: [ensemble, ensemble:coder]
project: shop
status: open
---
After logging in, it redirects to /home instead of back to where you were.
```

What the inbox does, every `poll_seconds`, while `ensemble mcp` or
`ensemble serve` runs (`ensemble inbox --once --team <dir>` does one pass by
hand):

- **An open issue becomes a task** in the folder its `project` maps to: for
  the agent a second label `ensemble:<agent>` names, or else in the backlog, for
  your agent to assign. The issue gets a comment and goes to `in-progress`. An
  issue whose project is unknown goes to `blocked`, with a comment saying why.
- **A question** the task stops to ask is posted as a comment, and the issue
  goes to `blocked`. Your next comment on the issue is the answer.
- **The result** is posted as a comment, once. The issue goes to `review` when
  the task is done, or to `blocked` when it failed. You close it: Ensemble
  never sets `done`.
- **An issue you close or take the label off** is forgotten; its task is left
  alone.

Ensemble changes only an issue's `status:` line and appends comments
(`## Comment by ensemble at <time>`); everything else in the file stays as you
wrote it. The task runs when a worker runs for its folder: an `ensemble mcp`
session opened there, or `ensemble serve --dir <folder>`. `ensemble inbox --hub
<dir>` lists your labelled issues, where each stands and the task it became.

**For a team, on the hub server.** A hub server's `tasks` block names only the
tracker, the label and `poll_seconds`; the rest belongs to each person:

- **Who is who.** An issue goes to the account whose tracker name is its
  assignee: the account's `tracker_user` when set (`PUT /api/v1/me`, an admin's
  `PUT /api/v1/users/{name}`, or `ensemble hub user tracker ana ana.petrosyan`),
  else the account's own name. Its role needs the `tasks` permission.
- **Which folder.** A member's [node](#nodes), `ensemble node serve --hub
  wss://…`, takes their tasks: each goes to the folder of its project's name
  where a session of the node last ran. A project is a folder's name, so
  `project: shop` is worked in the `shop` folder you last opened a session in.

What a member sees on an issue: "waiting for ana's computer" until one of her
nodes connects; then "picked up as task 3 on ana's computer" and
`in-progress`; then the task's question or result, as on an embedded hub, and
her next comment as the answer. The hub keeps a task waiting until one of her
nodes takes it; a node that has not served a folder of that name says so on
the issue, once, and another may still take it. An issue whose assignee is
nobody's tracker name, or whose account may not receive tasks, is blocked
with a comment saying why.

## Hub server

A team runs one hub on a server. Each member's node connects to it, and their
agents share to and recall from the **organization vault** there, as their role
allows. Personal knowledge stays in each person's own embedded hub: one agent
session talks to one hub, set per project when you register Ensemble.

On the server:

```console
$ ensemble hub init --server --hub /srv/hub # hub.yaml for a team, and the vault
$ $EDITOR /srv/hub/hub.yaml                 # knowledge, server, roles
$ ensemble hub user add root --role admin --hub /srv/hub   # the first admin
$ ensemble hub user password root --hub /srv/hub           # typed without echo
$ ensemble hub serve --hub /srv/hub         # until Ctrl-C or SIGTERM
```

```yaml
server:
  listen: 127.0.0.1:7700
  registration:
    type: approval         # anyone may sign up; an admin lets them in
    max_pending: 20        # or: registration: { type: closed }, only an admin adds users
  sessions:
    hours: 12              # how long a login lasts
  lockout:
    failures: 5            # failed logins before a name is refused
    minutes: 15            # and for how long
roles:                     # yours to name; each a list of permissions
  admin: [admin, share, recall, tasks, compute, host]
  member: [share, recall, tasks, compute]
  reader: [recall]
```

The permissions are `share` (keep knowledge in the organization vault),
`recall` (read its notes), `tasks` (receive the issues assigned to you; see
[Tasks from a tracker](#tasks-from-a-tracker)), `admin` (manage users),
`compute` (use other people's shared nodes) and `host` (connect a shared node;
see [Nodes](#nodes)). Accounts are not in `hub.yaml`: they are kept in the
hub's state directory, readable only by whoever runs the hub. A share is kept
under the name of the account that signed in, never a name the node sends, and
the hub distills shares itself, with its own `knowledge.distill` models.

**Accounts.** An account has a name, a role and a password of at least 12
characters, kept as a PBKDF2-HMAC-SHA256 hash. There are two kinds of token,
both sent as `Authorization: Bearer <token>`, and only their SHA-256 is kept:

- A **session** comes from logging in with the password and lasts
  `sessions.hours`. It is what a UI holds. Only a session may change the
  password or make and revoke access tokens, so a stolen access token cannot
  make more. A password change ends every session.
- An **access token** is made from a session, for a node or a script: named
  (`laptop`), with an optional expiry, and optional **scopes** that narrow the
  account's role, such as `["recall"]`. It is shown once, listed by id and
  name, and revoked one by one. Scopes hold on the link as on REST.

After `lockout.failures` failed logins, a name is refused for
`lockout.minutes`, even with the right password. A revoked or expired token
stops working at its next request, even on an open connection.

**Signing up.** With `registration: approval`, anyone who reaches the hub signs
up with a name and password; they can log in once an admin approves them:

```console
$ curl -X POST https://hub.example.com/api/v1/register -d '{"name":"ana","password":"..."}'
$ ensemble hub user approve ana --role member --hub /srv/hub     # or the API, below
```

On each member's computer:

```console
$ ensemble login wss://hub.example.com --user ana    # asks the password; makes a token "ensemble on <host>"
$ ensemble connect --manager claude-code --team ~/team --hub wss://hub.example.com
$ ensemble recall --hub wss://hub.example.com deploys
```

`ensemble login` logs in, makes an access token for this computer, logs out,
and keeps the token in `~/.config/ensemble/hubs.json`, readable only by you.
`--token T` keeps a token made elsewhere instead. `--hub <url>` then works like
`--hub <dir>`: the agent is offered the knowledge tools the token allows (a
reader gets only `recall`), and `ensemble share` and `ensemble recall` go to
the hub. `ensemble logout <url>` forgets the token.

**The REST API** lives on the hub's port, under `/api/v1/`, for the web UI to
come and for scripts. JSON in and out.

| Method and path | Who | Does |
| --- | --- | --- |
| `POST /api/v1/register` `{name, password}` | anyone, with `approval` | a pending account |
| `POST /api/v1/login` `{name, password}` | anyone | a session: `{token, expires}` |
| `POST /api/v1/logout` | a session | ends it |
| `GET /api/v1/me` | any token | your account, the token's permissions and kind |
| `PUT /api/v1/me` `{tracker_user}` | a session | who you are in the tracker; empty for your account name |
| `PUT /api/v1/me/password` `{current, new}` | a session | a new password; every session ends |
| `GET /api/v1/me/tokens` | a session | your access tokens: id, name, scopes, made, expires |
| `POST /api/v1/me/tokens` `{name, expires_days?, scopes?}` | a session | a new access token, shown once |
| `DELETE /api/v1/me/tokens/{id}` | a session | revokes one |
| `GET /api/v1/users` | admin | every account; never a password, token or hash |
| `POST /api/v1/users` `{name, role, password}` | admin | an account, active |
| `GET /api/v1/roles` | admin | the roles and their permissions |
| `POST /api/v1/users/{name}/approve` `{role}` | admin | lets a pending account in |
| `PUT /api/v1/users/{name}` `{role?, tracker_user?}` | admin | another role, or who they are in the tracker |
| `PUT /api/v1/users/{name}/password` `{password}` | admin | resets it; the account's sessions end |
| `DELETE /api/v1/users/{name}/tokens` | admin | revokes every token |
| `DELETE /api/v1/users/{name}` | admin | removes an account, or turns a sign-up down |

Errors are `{"error": "..."}` with 400, 401, 403, 404, 405, 409 or 429. An
embedded hub has none of this: it has one user, you, and listens on nothing.

The hub speaks plain HTTP and WebSocket. For the internet, put a TLS reverse
proxy in front, so passwords and tokens never cross the network in the clear;
with Caddy, which gets and renews the certificate itself:

```text
hub.example.com {
    reverse_proxy 127.0.0.1:7700
}
```

On a LAN or a VPN, listen on that address instead and connect with `ws://`.

## Nodes

A node is a worker on a computer: the one long-running process that holds a
helper team there. It is tied to no project. Your agent CLI starts
`ensemble mcp` with no `--team` in whatever folder you work in; that connects
to the node, says which folder it is, and from then on carries MCP both ways.
The node keeps a worker for each folder it has served, so a folder's tasks go
on after the session that submitted them ends, and start again when the node
does. A computer may run any number of nodes, each from its own file.

With a hub server, a node also keeps one link to it: the tasks the hub sends
you (see [Tasks from a tracker](#tasks-from-a-tracker)), and what it lends. A
node is **private**, working for its owner alone, or **shared**, working also
for everyone whose role has `compute`; connecting a shared one needs `host`.
The hub connects them, and keeps no node once it is gone.

```yaml
# node.yaml — `ensemble node init` writes it; `--node <file>` for another one
version: 3
name: gpu-box                    # a hub knows it as <you>/gpu-box
access: shared                   # private: you alone
team: /srv/ensemble/team         # the helper team it runs; left out: none
offers:                          # through a hub; each block may be left out
  runs:                          # agents of the team others may run here
    agents: [coder]
  models:                        # models a server here serves
    base_url: http://127.0.0.1:11434/v1
    timeout_seconds: 600
    names: [qwen3.5:72b]
```

```console
$ ensemble node serve                                          # until Ctrl-C or SIGTERM
$ cd ~/code/shop && claude                                     # its sessions reach the node
$ ensemble login wss://hub.example.com --user kim
$ ensemble node serve --hub wss://hub.example.com              # and lend through a hub
$ ensemble node serve --hub wss://hub.example.com --node ~/box2.yaml   # another node
$ ensemble nodes --hub wss://hub.example.com                   # the nodes you may use now
```

A session reaches the node on a socket only you may open (in
`$XDG_RUNTIME_DIR/ensemble`, else a folder of yours under `/tmp`). A task the hub sends
you goes to the folder of its project's name where a session of the node last
ran; one for a project the node has not served is refused, and the issue says
so. With `--hub <dir>`, an embedded hub, the node also turns shares into notes
and issues into tasks, as `ensemble mcp --team` does.

**Running an agent on a node.** In your helper team, an agent of
`type: node` names an agent a node offers:

```yaml
agents:
  heavy-coder:
    max_seconds: 3600
    max_concurrent: 1
    isolation: worktree
    budget: { type: unlimited }
    adapter:
      type: node
      hub: wss://hub.example.com
      agent: coder               # as the node's team names it
      nodes: [kim/gpu-box]       # tried in order: the first connected that offers it
```

A task for it sends the files git tracks or would track in the folder it works
in (never what `.gitignore` leaves out, such as `.env`; at most 24 MiB) to the
hub, which hands them to the node. The node's own agent works on them in a
folder of its own, and what it changed (files written, added, deleted) comes
back and is written here. So gates, worktrees, branches and commits all stay
on your computer, and to the rest of Ensemble it is an agent that changed some
files. Its progress lines come through as it works; a cancel or its
`max_seconds` stops it there too. `ensemble health` says whether one of its
nodes is connected and offers the agent.

What runs is the node owner's agent, from their helper team: nothing of your
agent's definition but the name goes there, so the owner decides what runs on
their machine. They also see your files. A shared node offers only agents that
run on models from `llm_backends` (`opencode` on `llm_backend`, `builtin`, or `direct`):
an agent CLI's own login, such as a Claude Code subscription, belongs to its
owner and is never lent; `node serve` refuses to start otherwise. A private
node may offer any of its owner's agents, for the owner's own use from another
computer.

**Using a node's models.** A node that offers `models` serves them through the
hub as an OpenAI-compatible endpoint at `https://<hub>/nodes/v1`, with an
access token as the API key: `GET /nodes/v1/models` lists the models you may
use, and `POST /nodes/v1/chat/completions` (and `/completions`) goes to the
least busy node that serves the model, your own first, streamed as it comes.
In a helper team it is a backend like any other:

```yaml
llm_backends:
  gpu:
    type: openai-compatible-api
    base_url: https://hub.example.com/nodes/v1
    api_key_env: HUB_TOKEN       # an access token with `compute`
    timeout_seconds: 600
```

## Installation

Linux is where it is built and tested; the rest of Unix should work. Windows is not supported:
Ensemble stops agent CLIs as process groups and runs gates with `sh -c`, and
`ensemble` says so and exits there. Use WSL.

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

### Other Unix: cargo

Needs Rust 1.92 or newer (edition 2024) and a C linker.

```console
$ cargo install --git https://github.com/Khachatur01/ensemble
```

This puts `ensemble` in `~/.cargo/bin`, which must be on `PATH`. Run the same
command again to update. Ensemble is not published on crates.io.

Besides Ensemble, you need the CLIs of the adapter types your team uses:
`claude` for `claude-code`, `opencode` for `opencode`, `codex` for `codex`. There is no runtime
service, no daemon and no port.

## Usage

Day to day you do not run Ensemble; your agent does. These are for setting it
up and for debugging it.

```console
$ ensemble setup                      # find the CLIs, write a starter, register it, check it
$ ensemble connect --manager M [--team <dir>] [--hub <dir>] [--global]  # register with claude-code, opencode or codex; no --team: with your node
$ ensemble hub init --hub <dir> [--server]   # a sample hub (--server: for a team): <dir>/hub.yaml, and its knowledge vault
$ ensemble node init [--node FILE]  # ~/.config/ensemble/node.yaml: a node, its team and what it offers
$ ensemble node serve [--hub HUB] [--node FILE]   # run that node until stopped
$ ensemble nodes --hub URL         # the nodes of a hub server you may use now
$ ensemble hook --hub <hub>        # what a Claude Code or Codex session hook runs
$ ensemble hub user tracker <name> [<tracker name>] --hub <dir>   # who the account is in the tracker
$ ensemble hub check --hub <dir>   # validate hub.yaml; which model would distill shares now
$ ensemble hub user add <name> --role <role> --hub <dir>   # a hub server's user; prints their token once
$ ensemble hub user list|token <name>|revoke <name>|remove <name> --hub <dir>
$ ensemble hub user password <name> --hub <dir>               # set a password, typed without echo
$ ensemble hub user approve <name> --role <role> --hub <dir>   # let in someone who signed up
$ ensemble hub user role <name> --role <role> --hub <dir>      # give a user another role
$ ensemble hub serve --hub <dir>   # run the hub for a team, until stopped
$ ensemble login <wss://url> --user <name>     # sign in to a hub server with your account; then --hub <url> works like --hub <dir>
$ ensemble login <wss://url> --token <token>   # or with a token made elsewhere
$ ensemble logout <wss://url>
$ ensemble share --hub <dir> --title T [--text T]   # keep knowledge by hand; text from stdin without --text
$ ensemble knowledge status --hub <dir>    # shares, pending, notes, what waits in review/
$ ensemble knowledge distill --hub <dir>   # turn pending shares into notes now
$ ensemble knowledge rebuild --hub <dir> [--yes]   # delete the notes and make them again from every share
$ ensemble recall --hub <dir> <topic>      # the notes about a topic, as an agent's `recall` gets them
$ ensemble inbox --hub <dir> [--once --team <dir>]   # your labelled issues and their tasks; --once: one pass now
$ ensemble doctor --team <dir>     # what is wrong, and how to fix each thing
$ ensemble add-agent <name> --adapter A --team <dir>   # add an agent, every setting written
$ ensemble remove-agent <name> --team <dir>
$ ensemble review <id>            # a worktree task: its branch, what changed, its result
$ ensemble diff <id>              # the whole change
$ ensemble merge <id> [--squash] [--cleanup]   # only you run this; undone on a conflict
$ ensemble tasks [--status S]      # the tasks of this working folder and where each stands
$ ensemble task <id>               # one task: ticket, state, question or result
$ ensemble submit <agent> <title> --text T   # write a task (also --goal --context --repo --gate --after ...)
$ ensemble submit <title> --backlog --text T # write a task with no assignee yet
$ ensemble assign <id> <agent>     # give a task to an agent
$ ensemble answer <id> <text>      # answer the question a task stopped to ask
$ ensemble stats --team <dir> [--since 7d] [--json]   # what each agent has done: runs, failures, time, tokens
$ ensemble serve --team <dir>      # run this folder's tasks with no session, until stopped
$ ensemble init --team <dir>       # write a sample helper team: <dir>/config.yaml
$ ensemble init --stdout           # print it instead (init refuses to overwrite without --force)
$ ensemble migrate <file> --team <dir>  # a version 1 or 2 config becomes a version 3 helper team
$ ensemble check  --team <dir>     # validate the config, print the gates and agents
$ ensemble agents --team <dir>     # the routing table
$ ensemble show <agent> --team <dir> [--probe]  # how an agent is started, keys hidden; --probe: which model a run would use now
$ ensemble health --team <dir>     # every agent CLI starts, every LLM backend serves its models? non-zero if not
$ ensemble run <agent> <task> --team <dir>  # send one task to one agent (it works in --dir)
$ ensemble team trust <url>#<commit>   # read what a remote helper team runs, and trust that commit
$ ensemble team update <url>#<commit>  # what changed since, and the new pin
$ ensemble schema                  # print the JSON Schema for config.yaml
$ ensemble mcp --team <dir>        # serve the helper team over stdio
```

`run` prints the answer on stdout and the provenance on stderr, so it pipes
cleanly. Options: `--team <dir>` (`--org` is its old name), `--hub <dir>` (the hub, which keeps your knowledge), `--env-file <path>`, `--dir <path>` (the working
folder, default the current directory), `--state <dir>` (instead of the per-user
state directory), `--context <text>` (for `run`).

## How it fits together

A Cargo workspace. Each crate depends only on the ones above it in this table
(`tests/workspace.rs` checks it), so the parts still to come, such as the hub
server, are added as crates on top.

| crate | |
| --- | --- |
| `ensemble-core` | the shared error type (`port.rs`), the per-user state and cache directories (`state.rs`), the working folder, a `repo` inside it and what a run changed (`workspace.rs`), and the record of every run with the sums `ensemble stats` prints (`records.rs`), the one-worker-per-directory lock (`lock.rs`), dates in UTC (`time.rs`), the files a run on a node carries there and back (`snapshot.rs`) |
| `ensemble-config` | the helper team's `config.yaml`: the document (`raw.rs`), the environment (`env.rs`), validation (`validate.rs`), the resolved types (`lib.rs`), versions 1 and 2 to 3 (`migrate.rs`), and adding or removing an agent as text, comments kept (`edit.rs`); the hub's `hub.yaml` (`hub.rs`); a node's `node.yaml` (`node.rs`) |
| `ensemble-backends` | one chat request to a model server in its own format (`chat.rs`), asking it which models it serves, for `health` (`models.rs`), choosing the first usable model of a `try` list (`resolve.rs`), and a stand-in model server for tests (`testing.rs`) |
| `ensemble-knowledge` | the hub's knowledge: the vault of shares and notes, a git repository (`vault.rs`), the secret check a share passes first (`secrets.rs`), the pipeline that turns shares into notes (`pipeline.rs`), finding notes by their words (`search.rs`), and the `share` and `recall` tools and the background worker (`lib.rs`) |
| `ensemble-link` | a hub server's users, sign-ups and their tokens, kept as hashes (`users.rs`), the REST API (`api.rs`) over a little HTTP reader (`http.rs`), what a node and the hub say over their WebSocket (`protocol.rs`), the hub's side (`server.rs`), the connected nodes and the runs and model calls between them, with `/nodes/v1/` (`nodes.rs`), and the node's: a hub server's knowledge over the link (`client.rs`) and the token file (`tokens.rs`) |
| `ensemble-agents` | the `claude-code`, `opencode` and `codex` adapters (each CLI as a subprocess on tokio, its JSON events read as it goes), the `builtin` adapter (harness-rs's loop), the `direct` adapter, the `node` adapter that runs an agent a node offers, and gates (`external/`); a git worktree and branch per task (`worktree.rs`); agents to adapters, slots, budgets and `health` (`router.rs`); the async runtime runs are on (`runtime.rs`) |
| `ensemble-tasks` | tasks as files (`tasks.rs`); the `Tracker` trait and `LocalMarkdown`, its one implementation (`tracker.rs`); the worker that runs tasks and background delegations within each agent's slots (`queue.rs`); the tools your agent calls (`delegation.rs`); `review`, `diff` and `merge` of a worktree task's branch (`review.rs`) |
| `ensemble-inbox` | the task inbox: what it needs from a tracker (`tracker.rs`), a folder of markdown issues, the first tracker (`local_markdown.rs`), making an issue into a task and saying what it reports (`work.rs`), the embedded hub's inbox (`bridge.rs`), a hub server's (`relay.rs`), and a member's computer's part in it (`node.rs`) |
| `ensemble-node` | a node: `ensemble node serve`, running the agents and serving the models it offers to whom it works for |
| `ensemble-mcp` | the MCP server over stdio: the loop (`lib.rs`), the tool catalogue (`protocol.rs`), notifications (`notify.rs`); and `ensemble serve` (`serve.rs`) |
| `ensemble` (the root) | the binary (`src/main.rs`); a node, the process that serves sessions from any folder, and the bridge `ensemble mcp` is to it (`src/daemon.rs`); `setup`, `connect` and `doctor` (`src/setup.rs`); session hooks for Claude Code and Codex (`src/hooks.rs`); remote helper teams: parsing, pinning, the cache, trust (`src/remote.rs`) |

`ensemble.schema.json`, at the root, is the JSON Schema for `config.yaml`, and
`hub.schema.json` and `node.schema.json` the ones for `hub.yaml` and
`node.yaml`.

The MCP server knows the router; it does not know which CLIs exist. The router
knows agents; only the adapters in `crates/ensemble-agents/src/external/` know
how a CLI is started and read.

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
| `crates/ensemble-config/src/tests.rs` | every validation rule, environment precedence, secrets, typos, the schema kept in step with the code |
| `crates/ensemble-agents/src/external/` | each CLI's command line, generated config and event stream, denials, limits, failures, the time limit killing the process tree |
| `crates/ensemble-agents/src/router.rs`, `crates/ensemble-backends/` | routing, `health` and its model check, the chat request in both formats |
| `crates/ensemble-knowledge/` | shares kept as they arrived, notes and the log, commits by the lock holder, the secret check; the pipeline against a stub model: new notes, merges, chatter skipped, unusable answers to review, waiting for a model, rebuild |
| `crates/ensemble-link/` | sign-up, approval and the REST API's codes, tokens kept as hashes and revoked at once, roles checked per request, shares under the signed-in name, a reader offered only `recall`, the token file 0600, the link opened again when it breaks; runs sent to a node that may be used and offers the agent, each side told when the other goes away, model calls streamed from a node |
| `crates/ensemble-inbox/` | markdown issues read and written with only the status line changed; a hub server's deliveries: matched by tracker name, accepted once, refused, asked and answered, reported; issues to tasks, questions and answers through comments, results posted once, one inbox per hub |
| `crates/ensemble-tasks/` | ticket files: reading, ids, results written once, run logs; review and merge |
| `crates/ensemble-mcp/src/tests.rs` | the protocol: handshake, tool schemas, notifications, parallel calls, cancellation, tasks run within `max_concurrent`, the end of a session, notes recalled for agents with `recall` |
| `tests/workspace.rs` | the crates depend on each other in one direction only |
| `tests/end_to_end.rs` | the real binary: CLI, `show`, `health` against a stub server, MCP over real pipes, an agent changing a file; session hooks; an agent run on a node and its changes back; a node's models through the hub; a session through a node, its task going on after the session; a hub task worked by a node |
| `crates/ensemble-agents/tests/fixtures/` | saved event streams per CLI version, replayed through each adapter's parser (see its README) |
| `tests/real_cli.rs` | the real `claude`, `opencode` and `codex` CLIs: `health` and one trivial run. Ignored; run with `ENSEMBLE_REAL_CLI=1 cargo test --test real_cli -- --ignored`. |

## Limitations

- **Six adapters so far.** `claude-code`, `opencode`, `codex`, `builtin`,
  `direct`, and `node`, which runs an agent a node of a hub server offers.
- **Only `opencode`, `builtin` and `direct` agents run on an LLM backend**, and
  an Anthropic one only a `builtin` or `direct` agent. Claude Code and Codex use your own login.
- **One flat team.** A helper team is a list of agents with no hierarchy, and a
  user connects one helper team at a time.
- **Only the end of a run can ask.** An agent stops with a question
  (`NEEDS INPUT:`) at the end of a run, never halfway through one: the CLIs'
  headless modes do not wait for an answer. For a delegation, the reply says
  `needs_input: true` with a session; answer it with `delegate` and `continue`,
  and the agent goes on from what it had. A session lasts as long as the MCP
  session.
- **No sandbox of Ensemble's own.** The agents' tools are the CLIs' own; their
  permission settings are what limit them. A `builtin` agent's file tools stay in
  the working directory, but its `shell` does not.
- **Limits are per agent unless shared.** Agents on one server share a limit only
  when they name the same one. Budgets are shared by every project that uses the
  helper team. A token budget counts what runs report, which not every CLI does.
- **Tasks run only while a worker runs.** With `--team`, the worker lives in
  `ensemble mcp`, so tasks stop with the calling agent's session; a node, or
  `ensemble serve`, is a worker that goes on. One worker runs a folder's tasks at
  a time.
- **The working folder is fixed when the server starts.** The working
  directory of the process (or `--dir`). A call can work in a folder inside it
  with `repo`, but not outside.
- **A hub server shares knowledge and hands out tasks.** A chat API, a web UI
  and single sign-on are not built. People sign up through the REST API
  and wait for an admin; there is no password reset by mail, and no second
  factor. The API sends no CORS headers, so a UI is served from the hub's own
  origin. Connectors to chats and mail are not built either.
- **Recall is a word search.** It finds notes that use your words, not ones
  that only mean the same. Your agent recalls when it decides to; a helper
  agent with `recall` gets the notes its task's words find, once, when the task
  starts. Session hooks recall on your prompt's words in Claude Code and
  Codex; OpenCode has none.
- **Nodes are simple.** A run on a node carries at most 24 MiB of files and
  needs a git repository; what comes back is written over the files here, so a
  change made here meanwhile to the same file is lost (a `worktree` agent
  avoids that). The hub picks the first of an agent's nodes that is
  connected, not the least busy; a node's own `max_concurrent` makes a run
  wait there. A node's models are reached at `/chat/completions` and
  `/completions` only. The hub keeps nothing of a node once it disconnects. A
  node keeps what it holds for a folder until it stops, and works within the
  folder a session started in; `--state` does not apply to it.
- **One tracker so far.** The inbox reads a folder of markdown issues; GitLab,
  GitHub and Jira adapters are not built. A member's computer takes a hub
  server's tasks only while their node runs, and only for projects it has
  served a folder of.
