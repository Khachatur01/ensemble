# ensemble

One interface for several models, delegating work between them.

You point your harness at Ensemble once. From then on it has a team — a
manager, a planner, a coder, a reviewer, whatever you configure — and it asks
them for work by name. Which model answers, and whether that model is a local
GPU, a remote server or the Claude Code CLI, is Ensemble's business and nobody
else's.

```
        Harness (OpenCode, Claude Code, …)
            |                       |
   selects  |                       |  or spawns
 ensemble/  v                       v
  manager   HTTP chat API      MCP over stdio    src/harness/
                  \             /
                   v           v
                     Ensemble                   src/router.rs
                         |
                         v  the Model Port      src/port.rs
                   /            \
                  v              v
            Claude Code     OpenAI-compatible   src/backend/
                CLI               API
```

**Ensemble still runs no tools and edits no files.** The harness owns tool
execution and permission; Ensemble owns the team. Over the chat API the
manager may ask the harness to run one of *its* tools, and the harness decides
whether to.

## Quick start

```console
$ nix shell github:you/ensemble-rs   # ensemble on PATH, nothing installed
$ cp ensemble.yaml /your/project/    # then edit the roles
$ cd /your/project && ensemble check
```

`check` validates the config, prints the team, and says whether `serve` can
run. Once it is happy, install ensemble for real — `nix shell` is ephemeral,
and the harness needs a path that outlives the shell and survives a garbage
collection:

```console
$ nix profile install github:you/ensemble-rs
```

### As your model provider (recommended)

Start the server in the project you are working on:

```console
$ ensemble serve --dir /your/project
ensemble: serving ensemble/manager on http://127.0.0.1:8787/v1
ensemble: manager -> openai-local / qwen3-coder
ensemble: delegates to: coder, planner, reviewer
ensemble: authentication: none (loopback only)
```

Then in `opencode.json`:

```json
{
  "provider": {
    "ensemble": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:8787/v1" },
      "models": { "manager": { "name": "Ensemble manager" } }
    }
  }
}
```

Select `ensemble/manager` as the model. That is the whole integration: no MCP
server to register, and no second agent in front of this one. The manager
delegates to your other roles inside Ensemble, and asks OpenCode to run
OpenCode's own tools when it needs a file read or edited.

`serve` needs a manager on an `openai-compatible-api` backend — it has to
drive a tool schema Ensemble supplies, which `claude -p` cannot. It refuses to
start otherwise and says so.

With Ollama, raise the context window before starting it. The default is 4096
tokens, which the manager prompt, harness system prompt and tool schemas can
exceed before the conversation begins, and Ollama then drops the oldest turns:

```console
$ OLLAMA_CONTEXT_LENGTH=32768 ollama serve
```

A Modelfile with `PARAMETER num_ctx 32768` works too. Set the harness's declared
context limit to match.

### As an MCP server (the alternative)

Still supported, and the right choice when you want a `claude-code-cli`
manager, or a harness that is not OpenAI-compatible:

```console
$ claude mcp add --scope local ensemble -- /path/to/ensemble mcp
$ claude mcp list
ensemble: /path/to/ensemble mcp - ✔ Connected
```

For OpenCode, in `opencode.json`:

```json
{ "mcp": { "ensemble": { "type": "local", "command": ["/path/to/ensemble", "mcp"] } } }
```

See [`docs/HARNESS.md`](docs/HARNESS.md) for what was verified against which
harness, how the two surfaces differ, and the limitations of each.

## Installation

### With Nix

```console
$ nix run github:you/ensemble-rs -- check     # one command, nothing kept
$ nix shell github:you/ensemble-rs            # a shell with ensemble on PATH
$ nix profile install github:you/ensemble-rs  # install it for good
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

Ensemble reads `ensemble.yaml`, searching upward from the working directory, so
a delegate invoked deep in a tree finds the same config the harness did. The
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

  openai-remote:
    type: openai-compatible-api
    base_url_env: OPENAI_REMOTE_BASE_URL
    base_url_default: "http://10.10.10.5:8000/v1"
    api_key_env: OPENAI_REMOTE_API_KEY

manager:
  backend: claude
  model: claude-sonnet

roles:
  planner:
    backend: openai-local
    model: qwen3-coder

  coder:
    backend: openai-remote
    model: qwen3-coder

  reviewer:
    backend: openai-local
    model: qwen3-coder
```

A **backend** is how a model is reached. A **role** is a member of the team,
and assigning one to a different model is two words with nothing else to
change:

```yaml
reviewer: { backend: claude,       model: claude-sonnet }
reviewer: { backend: openai-local, model: qwen3-coder   }
```

No model is hardcoded as planner, coder or reviewer, and the role names are
yours — `roles:` is a kit, not a fixed set.

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

### The manager is mandatory

It has its own top-level block, not an entry under `roles:`. There is no key
that removes, renames or disables it, and `roles.manager` is itself an error.
Loading fails if it is missing, names a backend that is not defined, or has an
empty model.

### Secrets

API keys are named by the **variable** that holds them and read from the
environment at load time. There is no `api_key` field, and a config that tries
one is rejected by name — before it reaches a commit:

```
ensemble.yaml is not valid YAML: unknown field `api_key`, expected one of ...
```

Environment variables override `base_url_default`. A variable that is exported
but empty falls through to the default rather than being treated as an
override, because an exported-but-empty variable is the normal shape of "I
meant to set this and did not".

### Validation

Every independent problem is reported in one pass, so fixing a config is one
round trip rather than five:

```console
$ ensemble check
ensemble: ensemble.yaml is invalid:
  - version: 4 is not supported; this build understands version 1
  - backends.x: type 'ollama' is unknown. Known types: claude-code-cli, openai-compatible-api.
  - manager: backend 'opanai-local' is not defined. Defined backends: openai-local, x.
  - roles.coder: `model` is empty
```

A typo'd key is an error, never a silent default: `modle: qwen3-coder` fails
rather than leaving the role with no model.

## Usage

Day to day you do not run Ensemble — the harness does. These are for setting
it up and for debugging it.

```console
$ ensemble serve            # the chat API, as the model id ensemble/manager
$ ensemble check            # validate the config, print the team
$ ensemble roles            # the routing table
$ ensemble health           # is every backend reachable? exits non-zero if not
$ ensemble run <role> <task>   # send one task to one role
$ ensemble mcp              # serve a harness over stdio
```

```console
$ ensemble roles
manager   claude          claude-sonnet
coder     openai-remote   qwen3-coder
planner   openai-local    qwen3-coder
reviewer  openai-local    qwen3-coder

$ ensemble health
claude: ok
openai-local: UNREACHABLE
  backend 'openai-local': could not reach the endpoint: io: Connection refused (os error 111)

$ ensemble run reviewer "Does src/port.rs leak backend details upward?"
ensemble: reviewer via claude/claude-sonnet-5 (2 in, 11 out)
No. The trait and its error type name no transport...
```

`run` prints the answer on stdout and the provenance on stderr, so it pipes
cleanly.

Options: `--config <path>`, `--dir <path>`, `--context <text>` (for `run`).

### `ensemble serve`

```console
$ ensemble serve [--host ADDR] [--port PORT] [--api-key-env VAR] [--max-steps N]
```

| option | default | |
| --- | --- | --- |
| `--host` | `127.0.0.1` | A non-loopback address requires `--api-key-env`. |
| `--port` | `8787` | `0` picks a free one and the banner names it. |
| `--api-key-env` | — | Names the variable holding the bearer token clients must send. The token is read from the environment, never written on the command line and never logged. |
| `--max-steps` | `8` | How many times the manager may be asked within one request before it has to answer. |

Two endpoints, both OpenAI-shaped, with or without the `/v1` prefix:

```console
$ curl -s localhost:8787/v1/models
{"object":"list","data":[{"id":"ensemble/manager","object":"model",…}]}

$ curl -s localhost:8787/v1/chat/completions \
    -H 'content-type: application/json' \
    -d '{"model":"ensemble/manager","messages":[{"role":"user","content":"hello"}]}'
```

`stream: true` returns Server-Sent Events, including `stream_options`'
`include_usage`.

**The project directory comes from `--dir`** (or the working directory), fixed
when the server starts. No request field and nothing a model writes can change
it — a path a model chooses is a path that can point anywhere. One workspace
per server; run a second server for a second checkout.

**Roles are not addressable.** One model id exists. `ensemble/coder` is a 404
like any other unknown model, and delegation is the manager's decision.

## How it fits together

| | |
| --- | --- |
| `src/port.rs` | the Model Port — the one interface every adapter implements, and the shared error type |
| `src/backend/claude_code.rs` | the `claude` CLI as a subprocess |
| `src/backend/openai.rs` | anything speaking `/chat/completions` |
| `src/router.rs` | roles → backends; the ensemble itself |
| `src/harness/delegation.rs` | the ensemble's own tools, shared by both surfaces |
| `src/harness/serve/` | the OpenAI-compatible chat API and the manager loop |
| `src/harness/http.rs` | a small synchronous HTTP/1.1 server, transport only |
| `src/harness/mcp.rs` | the MCP interface, over stdio |
| `src/config.rs` | `ensemble.yaml`: load, resolve the environment, validate |
| `docs/HARNESS.md` | what was verified against which harness, and the limitations |

The layering is load-bearing in one direction: **a backend-specific detail may
never travel upward.** The router knows roles and `ModelPort`; it does not know
that one adapter spawns a process and the other opens a socket. The MCP server
knows the router; it does not know there are backends at all. Adding a third
backend type is a file in `src/backend/` and a variant in two matches.

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

`nix develop` is the contributor shell. It carries the toolchain and no
ensemble binary, because the binary you want there is the one `cargo` just
built from your working tree, not one built from the last commit. To run the
tests the way CI does, without entering a shell at all, `nix flake check`.

122 tests, none of which need the network, a model server or a Claude
subscription.

| suite | what it covers |
| --- | --- |
| `src/config.rs` | every validation rule, environment precedence, secrets, typos |
| `src/backend/*.rs` | argv construction, response parsing, every error path, with the subprocess and the transport injected |
| `src/router.rs` | role → backend routing, prompts, context, provenance |
| `src/harness/mcp.rs` | the protocol: handshake, tool schemas, notifications, malformed input |
| `tests/end_to_end.rs` | the real binary: CLI, real subprocesses, MCP over real pipes |
| `tests/real_http.rs` | the real HTTP client against a loopback server |

Two bugs were found by the integration suite rather than by review, and both
have regression tests: a timeout that took 120 seconds because an orphaned
grandchild still held the stdout pipe, and a 503 whose body was discarded by
the HTTP client, turning "model is still loading" into "http status 503".

## Limitations

Read [`docs/HARNESS.md`](docs/HARNESS.md) for the full list. The short version:

- **Delegation is stateless and single-shot.** A role cannot ask a clarifying
  question; the caller must pass everything in `task` and `context`.
- **Claude-backed roles return prose, never tool calls.** Right for a planner,
  a reviewer or a manager. They cannot be driven as function-calling models,
  and such a manager cannot run `serve`.
- **No retry or concurrency cap.** `serve` bounds steps and delegations per
  request, but nothing stops a model calling a Claude-backed role in a loop
  across requests and spending the subscription limit.
- **The project directory is fixed when the server starts.** `--dir`, else the
  working directory. It cannot vary per call or per request, deliberately.
- **`serve` does not stream token by token.** The reply arrives in one burst
  once the turn finishes; SSE comments are the only progress until then.
  Forwarding real deltas would stream the manager's internal delegation to a
  harness that must not see it.
- **A client disconnect does not cancel an in-flight backend call.** The call
  runs to completion and its result is discarded.
- **`serve` holds one workspace and no persistence.** Conversation state lives
  in memory and is dropped on restart; a lost entry degrades to a manager that
  has forgotten it delegated, never to a broken transcript.
- **Process-group kill is Unix-only.** Elsewhere a timeout still returns on
  time, but an orphan may survive it.
- **The OpenCode MCP config block is unverified.** The HTTP provider block was
  verified with OpenCode 2.1.4; the MCP registration snippet has not been run.
  Both servers are verified — against Claude Code, over real pipes, and over a
  real socket in `tests/serve_api.rs` — so what is untested is the MCP
  registration snippet, not the servers behind them.

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
