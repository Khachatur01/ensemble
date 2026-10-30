# ensemble

An MCP server that gives your agent a team of agents.

You describe the team in `ensemble.yaml`: agents, each with an adapter that
says what runs it. Your own agent — Claude Code, OpenCode, anything that speaks
MCP — is the manager. It sees the team as tools and delegates to an agent by
name.

Two adapters ship:

- **`ensemble`**, Ensemble's own agent harness. It runs on any
  OpenAI-compatible model (llama-server, vLLM, Ollama, a hosted API) and reads,
  searches and edits your project in a loop until the task is done, so a team
  needs nothing installed but a model server.
- **`claude-code`**, the Claude Code CLI, run headless in the project on your
  own subscription, with the tools you allow it.

They are at the same level: a team can mix them, and your agent delegates to
either the same way. OpenCode and Codex are meant to become further adapter
types.

```
   Your agent (Claude Code, OpenCode, ...)      = the manager
        |  MCP over stdio, project as cwd
        v
     Ensemble                                   src/router.rs
        |  agents, by adapter type
        +--------------------------------+
        v                                v
     ensemble: agent loop            claude-code: `claude -p`
     (src/agent/)                    (src/claude_code.rs)
        |  the Model Port (src/port.rs)
        v
     LLM backend: OpenAI-compatible API (src/backend/)
```

## Quick start

1. [Install](#installation) Ensemble so that `ensemble` is on your `PATH`.
2. Give the project a team, then check it:

   ```console
   $ cp ensemble.yaml /your/project/      # the shipped example; edit the agents
   $ cd /your/project && ensemble check
   ```

   `check` validates the config, prints the team and says whether the LLM
   backends can be built.
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

Your agent sees three tools: `delegate` (send a task to an agent, get the work
back), `agents` (the team and what each is for) and `health` (is every LLM
backend reachable, and Claude Code installed).

## Configuration

Ensemble reads `ensemble.yaml` from the project directory (the working
directory your agent spawned it in, or `--dir`) and nowhere else: parent
directories are not searched and there is no user-level config. Each project
defines its own team; to share one, copy the file. The
shipped [`ensemble.yaml`](ensemble.yaml) is a working example.

```yaml
version: 1

llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:8000/v1"
    api_key_env: OPENAI_LOCAL_API_KEY

agents:
  planner:
    prompt: You turn a goal into an ordered plan of small, verifiable steps.
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: qwen3-coder

  coder:
    prompt: You implement exactly what is asked, in the style of the code around it.
    context_window: 16384
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: qwen3-coder
      tools: [read, glob, grep, edit, write]
```

An **agent** is a member of the team. Its names are yours; nothing is
hardcoded as planner, coder or reviewer. It has:

| key | |
| --- | --- |
| `prompt` | what it is for. The first line is what your agent sees in `agents`; the whole of it is the agent's system prompt |
| `context_window` | required when the adapter has `tools`; see [below](#the-ensemble-adapter) |
| `max_seconds` | wall-clock limit for one delegation (default 1800) |
| `adapter` | what runs it. `type` decides which other fields the adapter takes |

An **LLM backend** is a model endpoint, for agents with the `ensemble` adapter;
a team of `claude-code` agents needs none.
Its one type today, `openai-compatible-api`, is any `/chat/completions`
endpoint. It requires `base_url_env` and takes `base_url_default`,
`api_key_env` and `timeout_seconds` (default 600, per request).

There is no `manager:` block. The agent that calls Ensemble is the manager. A
config in an older layout (`manager:`, `backends:`, `roles:`) is refused with
a message saying what each became.

### The `ensemble` adapter

Ensemble's own agent: a model on one of your LLM backends, a tool loop, and the
project directory.

```yaml
agents:
  coder:
    context_window: 16384 # tokens your server runs the model with (required with tools)
    max_seconds: 1800     # wall-clock limit per delegation (the default)
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: qwen3-coder
      tools: [read, glob, grep, edit, write, bash, fetch]
      max_steps: 20         # model calls per delegation (the default)
      network: false        # bash may not open outbound TCP (the default)
      readable: [~/.m2]     # more paths bash may read (optional)
      temperature: 0.2      # overrides the model's own default (optional)
      reasoning_effort: none  # none, low, medium or high (optional)
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
`max_steps` model calls have been made. At the limit it gets one last call with
no tools to report what it did and what is unfinished. An agent with no
`tools` is one model call, text back.

A run also stops after `max_seconds`, or when the caller cancels it
(`notifications/cancelled`). It then returns what the model last said and what
it changed, marked unfinished. The deadline also bounds each HTTP call and
kills a running `bash` command. A refused connection, a 429 or a 5xx from the
model server is retried twice (after 2 s, then 8 s) before the delegation
fails, so a server that restarts mid-run does not lose the work.

`context_window` is the tokens of context your server really runs the model
with (Ollama: `OLLAMA_CONTEXT_LENGTH`, 4096 unless raised). A server that
overflows does not complain; Ollama silently drops the oldest messages, keeping
the system prompt and the newest one, so the task itself can vanish. Ensemble
keeps the conversation inside the window instead: over it, the oldest tool
results are replaced by a one-line note (never the system prompt, the task or
the latest step) and the delegation result says how many were dropped. A
single tool result may take at most 1/8 of the window; longer output is cut and
the model is told to ask for less (`offset`/`limit`, a narrower pattern).
Tokens are estimated at 4 bytes each, with no tokenizer.

`temperature` is for a server whose baked-in sampling is too hot for reliable
tool calling (an Ollama Modelfile's `temperature`/`presence_penalty`, for
instance). `reasoning_effort: none` turns a reasoning model's thinking off,
which keeps it from spending a small context's whole reply on thinking.

- **Confined to the project.** Every path is resolved through the filesystem
  and must land inside the project directory. `..`, absolute paths elsewhere and
  symlinks that lead out are refused, and a refusal goes back to the model as an
  error it can correct. Results are size-capped.
- **Writing is opt-in.** `edit`, `write` and `bash` change files, so an agent
  gets them only by listing them. `agents` marks each agent `[read-only]` or
  `[can change files]`.
- **`bash` is sandboxed, not safe.** It runs `sh -c` in the project root with
  a timeout (default 120 s, at most 600), size-capped output and an environment
  rebuilt from an allowlist, so your API keys never reach a command. On Linux
  the command runs under Landlock: it can **write** only inside the project,
  `/tmp` and `/dev`, and it cannot open **outbound TCP** connections unless the
  adapter sets `network: true` (kernel 6.7+; older kernels cannot restrict the
  network). It can **read** only the project, the system directories (`/nix`,
  `/usr`, `/etc`, `/proc`, ...), the toolchain directories `~/.cargo`,
  `~/.rustup`, `~/.npm` and `~/.nix-profile`, git's config, and paths listed
  in `readable` (absolute or `~/`). The rest of your home, `~/.ssh` included,
  is denied, and so is another process's environment under `/proc`. A
  toolchain that keeps its cache elsewhere in the home needs that directory in
  `readable`. Without Landlock (other OSes, old kernels) the tool refuses to
  run; there is no unconfined fallback.
- **`fetch` reaches only the public internet.** A host that resolves to
  loopback, a private range or link-local (cloud metadata) is refused, and so
  is a redirect to one, so it cannot reach your model server or router. HTML
  is turned into plain text; binary content is refused. The URL itself can
  still carry what the agent has read to a public server: give `fetch` only as
  far as you would let the agent talk to the internet.
- **Audited.** A delegation's result ends with how many steps and tool calls
  the agent used, every file it added, modified or deleted, and the `bash`
  commands it ran:

  ```text
  [coder: 6 step(s), 5 tool call(s)]
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

Small local models are the weak point. A 9B model that handles a read-and-edit
task fine can still ignore tools when its prompt is large, so keep prompts
short and give an agent only the tools it needs.

### The `claude-code` adapter

The Claude Code CLI as a member of the team. It is a whole agent of its own:
it runs its own loop with its own tools in the project directory, and manages
its own context.

```yaml
agents:
  reviewer:
    prompt: You review code for correctness and report findings as file:line — problem.
    max_seconds: 1800     # wall-clock limit per delegation (the default)
    adapter:
      type: claude-code
      model: sonnet         # an alias (sonnet, opus, haiku) or a full model id
      tools: [Read, Grep, Glob]  # Claude Code's own tools; this is the default
      permission_mode: dontAsk   # the default
      max_turns: 30         # the default
      command: claude       # if it is not `claude` on PATH
```

Ensemble starts `claude -p` in the project with the task, the agent's prompt
(`--append-system-prompt`) and `--allowedTools`. With `permission_mode:
dontAsk` Claude Code denies any tool not listed instead of waiting for a person
who is not there, and the result says which it was denied. `tools` takes
Claude Code's names and rules — `Read`, `Edit`, `Write`, `Bash`,
`Bash(git diff:*)`, `WebFetch` — one per entry; the default is read-only.
`context_window` does not apply.

Ensemble reads Claude Code's `stream-json` output as it goes, so it reports
the same way as an `ensemble` agent: a progress line per tool call, and a
footer with turns, tool calls, the files it changed (found by comparing the
project before and after, when it has a tool that writes) and the `Bash`
commands that ran. `max_seconds` and cancellation kill the CLI and its whole
process tree.

It runs on your own subscription, through the official CLI's supported
headless mode. Ensemble does not read, copy or replay Claude Code's
credentials and never talks to Anthropic's API itself; it never passes
`--bare`, which would bypass the saved login and bill per token. If an answer
comes from a model that is not Claude (a stray `ANTHROPIC_BASE_URL`), it is
refused. Unlike `ensemble`'s `bash`, Claude Code's own tools are not in a
Landlock sandbox: what they may do is what `tools` and `permission_mode`
allow.

### Progress

An agent can run for minutes on a local model. While it does, `ensemble mcp`
sends two notifications per step and tool call, before the final reply:
`notifications/progress` (the client's `progressToken`, a rising counter and a
`message` such as `coder · step 2 · edit_file {"path":"a.txt",…}`) and the same
line as a `notifications/message` log at `info` level. Raise the level with
`logging/setLevel` to silence the logs; progress is not filtered. A request
without a `progressToken` gets its request id as the token. Claude Code sends a
token on tool calls. Neither notification reaches the model: they go to the
client program, so they cost the manager no tokens.

### Secrets

API keys are named by the **variable** that holds them and read from the
environment at load time. There is no `api_key` field, and a config that tries
one is rejected by name — before it reaches a commit.

Environment variables override `base_url_default`. A variable that is exported
but empty falls through to the default.

With Ollama, raise the context window before starting it. The default is 4096
tokens, which a tool-using agent can exceed after a few file reads:

```console
$ OLLAMA_CONTEXT_LENGTH=32768 ollama serve
```

### Editor support

`ensemble.yaml` has a JSON Schema, [`ensemble.schema.json`](ensemble.schema.json):
completion for every key and value, hover descriptions, defaults, and errors for
the mistakes `ensemble check` would also report (a missing `context_window`, a
field the adapter type does not take, an unknown tool). The first line of the
shipped `ensemble.yaml` points an editor with the YAML language server at it:

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
ensemble: ensemble.yaml is invalid:
  - version: 4 is not supported; this build understands version 1
  - llm_backends.x: type 'ollama' is unknown. Known types: openai-compatible-api.
  - agents.coder.adapter: LLM backend 'opanai-local' is not defined. Defined LLM backends: openai-local, x.
  - agents.reviewer.adapter: tool 'shell' is unknown. Known tools: read, glob, grep, edit, write, bash, fetch.
```

A typo'd key is an error, never a silent default.

## Installation

Linux is the supported platform: the `bash` tool's sandbox is Linux Landlock
(kernel 5.13+; 6.7+ to block network access). The rest builds on macOS too, but
there agents cannot have `bash`.

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

The `claude` CLI is needed only for `claude-code` agents. Nothing else is:
there is no runtime service, no daemon and no port.

## Usage

Day to day you do not run Ensemble — your agent does. These are for setting it
up and for debugging it.

```console
$ ensemble check                # validate the config, print the team
$ ensemble agents               # the routing table
$ ensemble health               # is every LLM backend reachable, and claude installed? non-zero if not
$ ensemble run <agent> <task>   # send one task to one agent (it works in --dir)
$ ensemble schema               # print the JSON Schema for ensemble.yaml
$ ensemble mcp                  # serve the team over stdio
```

`run` prints the answer on stdout and the provenance on stderr, so it pipes
cleanly. Options: `--config <path>`, `--dir <path>` (the project, default the
current directory), `--context <text>` (for `run`).

## How it fits together

| | |
| --- | --- |
| `src/port.rs` | the Model Port — the one interface every LLM adapter implements, and the shared error type |
| `src/backend/openai.rs` | anything speaking `/chat/completions` |
| `src/agent/` | the `ensemble` adapter: the agent loop, its tools and the sandboxes |
| `src/claude_code.rs` | the `claude-code` adapter: `claude -p` as a subprocess, its `stream-json` read as it goes |
| `src/router.rs` | agents → adapters; the ensemble itself |
| `src/harness/delegation.rs` | the three tools your agent calls: delegate, agents, health |
| `src/harness/mcp.rs` | the MCP server, over stdio |
| `src/config.rs` | `ensemble.yaml`: load, resolve the environment, validate |
| `ensemble.schema.json` | the JSON Schema for `ensemble.yaml` |
| `docs/HARNESS.md` | the record of what was verified against which harness |

The layering is load-bearing in one direction: **a backend-specific detail may
never travel upward.** The router knows agents and `ModelPort`; it does not
know how an adapter reaches its model. The MCP server knows the router; it does
not know there are backends at all.

### Errors are one vocabulary

An agent failing because a connection was refused and an agent failing because
an endpoint returned 503 are the same event to your agent: this agent did not
answer, and here is why. So there is one `port::Error` covering timeouts,
transport failures, API statuses, malformed responses and unsupported requests
— each carrying the backend it came from, each knowing whether a retry could
plausibly work, and each written to be read by the model that will see it:

```
backend 'openai-local': endpoint returned 401: invalid key — check the backend's api_key_env variable
backend 'openai-local': no answer within 2s, killed. Raise timeout_seconds on the backend if this is normal.
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

None of the tests need the network or a model server.

| suite | what it covers |
| --- | --- |
| `src/config.rs` | every validation rule, environment precedence, secrets, typos, the schema kept in step with the code |
| `src/agent/` | the tools, path confinement (`..`, absolute, symlinks), the bash sandbox, the loop, its limits |
| `src/backend/openai.rs` | request construction, response parsing, every error path, with the transport injected |
| `src/claude_code.rs` | the command line, the event stream, denials, turn limits, failures, the time limit killing the process tree, against stand-in scripts |
| `src/router.rs` | agent → backend routing, prompts, context, provenance |
| `src/harness/mcp.rs` | the protocol: handshake, tool schemas, notifications, parallel calls, cancellation |
| `tests/end_to_end.rs` | the real binary: CLI, MCP over real pipes, an agent editing a file against a stub model server |
| `tests/real_http.rs` | the real HTTP client against a loopback server |

## Limitations

- **Two adapters so far.** `ensemble` and `claude-code`. OpenCode and Codex
  are planned, not built.
- **Delegation is single-shot.** An agent cannot ask a clarifying question; the
  caller must pass everything in `task` and `context`.
- **`bash` is Linux-only.** See above. `grep` is a literal-string search, not
  a regex.
- **Context is trimmed, not summarised.** Old tool output is replaced by a note
  to stay within `context_window`; the agent loses that detail.
- **Weak models can ignore tools.** See above.
- **No concurrency cap.** Nothing stops a calling agent delegating many tasks
  at once against one model server, or spending a Claude subscription limit in
  a loop.
- **Cancelling cannot interrupt a model call already in flight.** The run stops
  when that call returns; `max_seconds` bounds how long that can be.
- **The project directory is fixed when the server starts.** The working
  directory of the process (or `--dir`). It cannot vary per call.
