# ensemble

An MCP server that runs its own agents.

You describe a team in `ensemble.yaml`: roles, each backed by a model. Your own
agent — Claude Code, OpenCode, anything that speaks MCP — is the manager. It
sees the roles as tools and delegates to them by name. A role with `tools:` is
an agent that Ensemble runs itself: it reads, searches and edits your project
in a loop until the task is done. Any OpenAI-compatible model can be the brain
(llama-server, vLLM, Ollama, a hosted API), and nothing else needs installing.

```
   Your agent (Claude Code, OpenCode, ...)      = the manager
        |  MCP over stdio, project as cwd
        v
     Ensemble                                   src/router.rs
      /         \
 agent loop    one completion                   src/agent/
 (tools in      (think-only role)
  the project)
      \         /
       v       v   the Model Port               src/port.rs
   OpenAI-compatible API    Claude Code CLI     src/backend/
```

## Quick start

```console
$ nix profile install github:Khachatur01/ensemble   # or: cargo install --path .
$ cp ensemble.yaml /your/project/                    # then edit the roles
$ cd /your/project && ensemble check
```

`check` validates the config, prints the team and says whether the backends can
be built. Register Ensemble with your agent, from the project directory:

```console
$ claude mcp add --scope local ensemble -- /path/to/ensemble mcp
$ claude mcp list
ensemble: /path/to/ensemble mcp - ✔ Connected
```

For OpenCode, in `opencode.json`:

```json
{ "mcp": { "ensemble": { "type": "local", "command": ["/path/to/ensemble", "mcp"] } } }
```

Then tell the agent when to use the team, in its `CLAUDE.md` / `AGENTS.md`:

> Delegate implementation to the `coder` role and review to `reviewer` with the
> `ensemble_delegate` tool. Check `ensemble_roles` for what each can do.

Ensemble runs per project: the agent spawns it with the project as its working
directory, and that directory is where agent roles work. There is no path
argument on any tool, deliberately.

## Agent roles

A role with `tools:` is an agent: Ensemble runs the tool loop itself.

```yaml
roles:
  coder:
    backend: openai-local
    model: qwen3-coder
    tools: [read, glob, grep, edit, write]
    max_steps: 20
```

| tool | what it does |
| --- | --- |
| `read` | read a text file, with `offset` / `limit` lines |
| `glob` | list files matching a pattern (`*`, `**`, `?`) |
| `grep` | find a literal string in files, `path:line: text` |
| `edit` | replace one exact, unique occurrence in a file |
| `write` | create or replace a file |

The model is offered these tools, Ensemble runs the calls it makes, feeds the
results back, and repeats until the model answers without asking for a tool or
`max_steps` (default 20) model calls have been made. At the limit it gets one
last call with no tools to report what it did and what is unfinished.

- **Confined to the project.** Every path is resolved through the filesystem
  and must land inside the project directory. `..`, absolute paths elsewhere and
  symlinks that lead out are refused, and a refusal goes back to the model as an
  error it can correct. Results are size-capped.
- **Writing is opt-in.** `edit` and `write` change files, so a role gets them
  only by listing them. `ensemble_roles` marks roles as `[agent, read-only]` or
  `[agent, can change files]`.
- **No shell.** There is no `bash` tool. A role can read, search and edit; it
  cannot run commands.
- **Audited.** A delegation's result ends with how many steps and tool calls
  the agent used.
- **Think-only roles.** A role without `tools:` is one model call, text back.
- **Backends.** Tools need `openai-compatible-api` (structured tool calls). A
  `claude-code-cli` role is already an agent with Claude Code's own tools, and
  config validation rejects `tools:` on it.

Small local models are the weak point. A 9B model that handles a read-and-edit
task fine can still ignore tools when its prompt is large, so keep role prompts
short and give a role only the tools it needs.

## Installation

### With Nix

```console
$ nix run github:Khachatur01/ensemble -- check     # one command, nothing kept
$ nix shell github:Khachatur01/ensemble            # a shell with ensemble on PATH
$ nix profile install github:Khachatur01/ensemble  # install it for good
```

`nix run` is for a single invocation, `nix shell` for a session of them.
Neither leaves anything behind, so neither gives you a path you can hand to a
harness — use `nix profile install` for that.

### With cargo

Needs Rust 1.85 or newer (edition 2024) and a C linker for the TLS stack.

```console
$ cargo install --path .
```

The `claude` CLI is needed only if you configure a `claude-code-cli` backend,
and nothing else is: there is no runtime service, no daemon and no port.

## Configuration

Ensemble reads `ensemble.yaml`, searching upward from the working directory. The
shipped [`ensemble.yaml`](ensemble.yaml) is a working example.

```yaml
version: 1

backends:
  claude:
    type: claude-code-cli

  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:8000/v1"
    api_key_env: OPENAI_LOCAL_API_KEY

roles:
  planner:
    backend: openai-local
    model: qwen3-coder
    prompt: You turn a goal into an ordered plan of small, verifiable steps.

  coder:
    backend: openai-local
    model: qwen3-coder
    tools: [read, glob, grep, edit, write]

  reviewer:
    backend: claude
    model: claude-sonnet
```

A **backend** is how a model is reached. A **role** is a member of the team,
and assigning one to a different model is two words with nothing else to
change. No model is hardcoded as planner, coder or reviewer, and the role names
are yours. A role takes `backend`, `model`, and optionally `prompt`,
`temperature`, `tools` and `max_steps`.

There is no `manager:` block. The agent that calls Ensemble is the manager; an
old config that still has one is rejected with a message saying so.

### Backend types

| type | what it is | tool calling |
| --- | --- | --- |
| `openai-compatible-api` | any `/chat/completions` endpoint — llama-server, vLLM, Ollama, a hosted API | yes |
| `claude-code-cli` | the official `claude` CLI as a subprocess, on your own subscription | **no** — it returns prose |

Both accept `timeout_seconds` (default 600).
`openai-compatible-api` requires `base_url_env`.
`claude-code-cli` accepts `command`, `allowed_tools` (default `Read,Grep,Glob`
— read-only unless you say otherwise), `permission_mode` and `max_turns`.

Settings from the wrong type are **rejected**, not ignored: a `base_url_env`
on a `claude-code-cli` backend means somebody believes a URL is being used.

### Secrets

API keys are named by the **variable** that holds them and read from the
environment at load time. There is no `api_key` field, and a config that tries
one is rejected by name — before it reaches a commit.

Environment variables override `base_url_default`. A variable that is exported
but empty falls through to the default.

With Ollama, raise the context window before starting it. The default is 4096
tokens, which a tool-using role can exceed after a few file reads:

```console
$ OLLAMA_CONTEXT_LENGTH=32768 ollama serve
```

### Validation

Every independent problem is reported in one pass:

```console
$ ensemble check
ensemble: ensemble.yaml is invalid:
  - version: 4 is not supported; this build understands version 1
  - backends.x: type 'ollama' is unknown. Known types: claude-code-cli, openai-compatible-api.
  - roles.coder: backend 'opanai-local' is not defined. Defined backends: openai-local, x.
  - roles.reviewer: tool 'bash' is unknown. Known tools: read, glob, grep, edit, write.
```

A typo'd key is an error, never a silent default.

## Usage

Day to day you do not run Ensemble — your agent does. These are for setting it
up and for debugging it.

```console
$ ensemble check               # validate the config, print the team
$ ensemble roles               # the routing table
$ ensemble health              # is every backend reachable? exits non-zero if not
$ ensemble run <role> <task>   # send one task to one role (agent roles work in --dir)
$ ensemble mcp                 # serve an agent over stdio
```

`run` prints the answer on stdout and the provenance on stderr, so it pipes
cleanly. Options: `--config <path>`, `--dir <path>` (the project, default the
current directory), `--context <text>` (for `run`).

## How it fits together

| | |
| --- | --- |
| `src/port.rs` | the Model Port — the one interface every adapter implements, and the shared error type |
| `src/backend/openai.rs` | anything speaking `/chat/completions` |
| `src/backend/claude_code.rs` | the `claude` CLI as a subprocess |
| `src/agent/` | the agent loop, its tools and the path sandbox |
| `src/router.rs` | roles → backends; the ensemble itself |
| `src/harness/delegation.rs` | the three tools an agent calls: delegate, roles, health |
| `src/harness/mcp.rs` | the MCP server, over stdio |
| `src/config.rs` | `ensemble.yaml`: load, resolve the environment, validate |
| `docs/HARNESS.md` | the record of what was verified against which harness |

The layering is load-bearing in one direction: **a backend-specific detail may
never travel upward.** The router knows roles and `ModelPort`; it does not know
that one adapter spawns a process and the other opens a socket. The MCP server
knows the router; it does not know there are backends at all.

### Errors are one vocabulary

A role failing because a subprocess was killed and a role failing because an
endpoint returned 503 are the same event to the harness: this role did not
answer, and here is why. So there is one `port::Error` covering spawn failures,
non-zero exits, timeouts, transport failures, API statuses, malformed responses
and unsupported requests — each carrying the backend it came from, each knowing
whether a retry could plausibly work, and each written to be read by the model
that will see it:

```
backend 'claude': could not start 'claude': No such file or directory. Is it installed and on PATH?
backend 'openai-local': endpoint returned 401: invalid key — check the backend's api_key_env variable
backend 'claude': no answer within 2s, killed. Raise timeout_seconds on the backend if this is normal.
```

A failed delegation reaches the model as a tool *result* with `isError: true`,
never as a JSON-RPC error — a transport error is handled by the harness and the
model never learns the reason, which is the one thing it needs in order to do
something sensible next.

## Tests

```console
$ nix develop   # cargo, rustc, clippy, rust-analyzer
$ cargo test
```

None of the tests need the network, a model server or a Claude subscription.

| suite | what it covers |
| --- | --- |
| `src/config.rs` | every validation rule, environment precedence, secrets, typos |
| `src/agent/` | the tools, path confinement (`..`, absolute, symlinks), the loop, the step limit |
| `src/backend/*.rs` | request construction, response parsing, every error path, with the subprocess and the transport injected |
| `src/router.rs` | role → backend routing, prompts, context, provenance |
| `src/harness/mcp.rs` | the protocol: handshake, tool schemas, notifications, malformed input |
| `tests/end_to_end.rs` | the real binary: CLI, real subprocesses, MCP over real pipes, an agent role editing a file against a stub model server |
| `tests/real_http.rs` | the real HTTP client against a loopback server |

## Limitations

- **Delegation is single-shot.** A role cannot ask a clarifying question; the
  caller must pass everything in `task` and `context`.
- **No shell, no regex.** Agent roles cannot run commands, and `grep` is a
  literal-string search.
- **No context management.** An agent that reads a lot can outgrow its model's
  context window; there is no compaction yet.
- **Weak models can ignore tools.** See above.
- **Claude-backed roles return prose, never tool calls.** Right for a planner or
  a reviewer; they cannot run Ensemble's agent loop.
- **No retry or concurrency cap.** Nothing stops a calling agent delegating to a
  Claude-backed role in a loop and spending the subscription limit.
- **The project directory is fixed when the server starts.** The working
  directory of the process (or `--dir`). It cannot vary per call.
- **Process-group kill is Unix-only.** Elsewhere a timeout still returns on
  time, but an orphan may survive it.
- **The OpenCode MCP config block is unverified.** The MCP server is verified
  against Claude Code, over real pipes.

## On the Claude Code backend

It drives the official CLI in its supported headless mode (`claude -p`), as a
subprocess, started locally by the person who owns the subscription. It does
**not** read, copy, replay or forge Claude Code's credentials and it never
speaks to Anthropic's API itself. There is no token extraction and none should
be added: a shim that replays subscription credentials to serve a
general-purpose API is what the terms prohibit, and it is the reason this
backend is a subprocess rather than an HTTP client.

It also never passes `--bare`, which would bypass the saved login and bill per
token. That is guarded by a test, because it is a one-word change with a
billing consequence.
