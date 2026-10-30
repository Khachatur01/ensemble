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

1. [Install](#installation) Ensemble so that `ensemble` is on your `PATH`.
2. Give the project a team, then check it:

   ```console
   $ cp ensemble.yaml /your/project/      # the shipped example; edit the roles
   $ cd /your/project && ensemble check
   ```

   `check` validates the config, prints the team and says whether the backends
   can be built.
3. [Connect your agent](#connect-your-agent) from the project directory.
4. Tell the agent when to use the team, in `AGENTS.md` (OpenCode) or
   `CLAUDE.md` (Claude Code):

   > Delegate implementation to the `coder` role and review to `reviewer` with
   > the `delegate` tool. Check `roles` for what each can do.

Ensemble runs per project: the agent spawns it with the project as its working
directory, and that directory is where agent roles work. There is no path
argument on any tool, deliberately.

## Connect your agent

Ensemble is a standard MCP server on stdio. Any agent that can spawn one can
use it; nothing in it is specific to one harness. Register it from the project
directory, the one with `ensemble.yaml`.

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
only projects that have an `ensemble.yaml` get a team: elsewhere the server
exits at once with "no ensemble.yaml in the project directory", and the agent
lists it as failed. Ensemble reads no user-level config.

## Agent roles

A role with `tools:` is an agent: Ensemble runs the tool loop itself.

```yaml
roles:
  coder:
    backend: openai-local
    model: qwen3-coder
    tools: [read, glob, grep, edit, write, bash, fetch]
    max_steps: 20
    max_seconds: 1800     # wall-clock limit per delegation (the default)
    context_window: 16384 # tokens your server runs the model with (required)
    network: false        # bash may not open outbound TCP (the default)
    readable: [~/.m2]     # more paths bash may read (optional)
```

| tool | what it does |
| --- | --- |
| `read` | read a text file, with `offset` / `limit` lines |
| `glob` | list files matching a pattern (`*`, `**`, `?`) |
| `grep` | find a literal string in files, `path:line: text` |
| `edit` | replace one exact, unique occurrence in a file |
| `write` | create or replace a file |
| `bash` | run a shell command in the project, sandboxed (below) |
| `fetch` | read a public web page or text file as text (`web_fetch`) |

The model is offered these tools, Ensemble runs the calls it makes, feeds the
results back, and repeats until the model answers without asking for a tool or
`max_steps` (default 20) model calls have been made. At the limit it gets one
last call with no tools to report what it did and what is unfinished.

A run also stops after `max_seconds` (default 1800), or when the caller cancels
it (`notifications/cancelled`). It then returns what the model last said and
what it changed, marked unfinished. The deadline also bounds each HTTP call and
kills a running `bash` command. A refused connection, a 429 or a 5xx from the
model server is retried twice (after 2 s, then 8 s) before the delegation
fails, so a server that restarts mid-run does not lose the work.

`context_window` is required for a role with tools: the tokens of context your
server really runs the model with (Ollama: `OLLAMA_CONTEXT_LENGTH`, 4096 unless
raised). A server that overflows does not complain; Ollama silently drops the
oldest messages, keeping the system prompt and the newest one, so the task
itself can vanish. Ensemble keeps the conversation inside the window instead:
over it, the oldest tool results are replaced by a one-line note (never the
system prompt, the task or the latest step) and the delegation result says how
many were dropped. A single tool result may take at most 1/8 of the window;
longer output is cut and the model is told to ask for less (`offset`/`limit`,
a narrower pattern). Tokens are estimated at 4 bytes each, with no tokenizer.

- **Confined to the project.** Every path is resolved through the filesystem
  and must land inside the project directory. `..`, absolute paths elsewhere and
  symlinks that lead out are refused, and a refusal goes back to the model as an
  error it can correct. Results are size-capped.
- **Writing is opt-in.** `edit` and `write` change files, so a role gets them
  only by listing them. `roles` marks roles as `[agent, read-only]` or
  `[agent, can change files]`.
- **`bash` is sandboxed, not safe.** It runs `sh -c` in the project root with
  a timeout (default 120 s, at most 600), size-capped output and an environment
  rebuilt from an allowlist, so your API keys never reach a command. On Linux
  the command runs under Landlock: it can **write** only inside the project,
  `/tmp` and `/dev`, and it cannot open **outbound TCP** connections unless the
  role sets `network: true` (kernel 6.7+; older kernels cannot restrict the
  network). It can **read** only the project, the system directories (`/nix`,
  `/usr`, `/etc`, `/proc`, ...), the toolchain directories `~/.cargo`,
  `~/.rustup`, `~/.npm` and `~/.nix-profile`, git's config, and paths the role
  lists in `readable` (absolute or `~/`). The rest of your home, `~/.ssh`
  included, is denied, and so is another process's environment under `/proc`.
  A toolchain that keeps its cache elsewhere in the home needs that directory
  in `readable`. Without Landlock (other OSes, old kernels) the tool refuses to
  run; there is no unconfined fallback.
- **`fetch` reaches only the public internet.** A host that resolves to
  loopback, a private range or link-local (cloud metadata) is refused, and so
  is a redirect to one, so it cannot reach your model server or router. HTML
  is turned into plain text; binary content is refused. The URL itself can
  still carry what the role has read to a public server: give `fetch` only as
  far as you would let the role talk to the internet.
- **Audited.** A delegation's result ends with how many steps and tool calls
  the agent used, every file it added, modified or deleted, and the `bash`
  commands it ran:

  ```text
  [coder ran as an agent: 6 step(s), 5 tool call(s)]
  [files changed: src/lib.rs (modified), tests/new.rs (added)]
  [commands run: cargo test]
  ```

  Edits and writes are recorded by name. What a `bash` command changed is found
  by comparing the project before and after it (`.git`, `target` and
  `node_modules` are not compared).
- **Parallel.** Each delegation runs on its own thread, so the caller can send
  several at once. They share the project: two agents editing the same file
  will clash. When an agent with `bash` overlapped another delegation, its
  result says some listed files may be the other run's changes.
- **Think-only roles.** A role without `tools:` is one model call, text back.
- **Backends.** Tools need `openai-compatible-api` (structured tool calls). A
  `claude-code-cli` role is already an agent with Claude Code's own tools, and
  config validation rejects `tools:` on it.

Small local models are the weak point. A 9B model that handles a read-and-edit
task fine can still ignore tools when its prompt is large, so keep role prompts
short and give a role only the tools it needs.

### Progress

An agent role can run for minutes on a local model. While it does, `ensemble mcp`
sends two notifications per step and tool call, before the final reply:
`notifications/progress` (the client's `progressToken`, a rising counter and a
`message` such as `coder · step 2 · edit_file {"path":"a.txt",…}`) and the same
line as a `notifications/message` log at `info` level. Raise the level with
`logging/setLevel` to silence the logs; progress is not filtered. A request
without a `progressToken` gets its request id as the token. Claude Code sends a
token on tool calls. Neither notification reaches the model: they go to the
client program, so they cost the manager no tokens.

## Installation

Linux is the supported platform: the `bash` tool's sandbox is Linux Landlock
(kernel 5.13+; 6.7+ to block network access). The rest builds on macOS too, but
there agent roles cannot have `bash`.

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

The `claude` CLI is needed only if you configure a `claude-code-cli` backend,
and nothing else is: there is no runtime service, no daemon and no port.

## Configuration

Ensemble reads `ensemble.yaml` from the project directory (the working
directory your agent spawned it in, or `--dir`) and nowhere else: parent
directories are not searched and there is no user-level config. Each project
defines its own team; to share one, copy the file. The
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
`temperature`, `reasoning_effort` (`none` turns thinking off), `tools`, `max_steps`,
`max_seconds`, `context_window`, `network` and `readable`.

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
  - roles.reviewer: tool 'shell' is unknown. Known tools: read, glob, grep, edit, write, bash, fetch.
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
- **`bash` is Linux-only.** See above. `grep` is a literal-string search, not
  a regex.
- **Context is trimmed, not summarised.** Old tool output is replaced by a note
  to stay within `context_window`; the agent loses that detail.
- **Weak models can ignore tools.** See above.
- **Claude-backed roles return prose, never tool calls.** Right for a planner or
  a reviewer; they cannot run Ensemble's agent loop.
- **No concurrency cap.** Nothing stops a calling agent delegating to a
  Claude-backed role in a loop, or many at once, and spending the subscription
  limit.
- **Cancelling cannot interrupt a model call already in flight.** The run stops
  when that call returns; `max_seconds` bounds how long that can be.
- **The project directory is fixed when the server starts.** The working
  directory of the process (or `--dir`). It cannot vary per call.
- **Process-group kill is Unix-only.** Elsewhere a timeout still returns on
  time, but an orphan may survive it.

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
