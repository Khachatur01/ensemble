# Harness integration: what was verified, and what it cannot do

Requirement 4 of the brief says to investigate the harness's extension
mechanisms, choose the simplest supported one, and not assume a model API alone
is enough for agent orchestration. This is the record of that investigation:
what was probed, what the result was, and which limitations follow.

Every claim marked **verified** was observed against a running binary on
2026-10-28. Claims that are not marked were read rather than run, and are
flagged as such.

There are now **two** surfaces. The chat API came second and is the one to
reach for; the MCP server is unchanged and remains the answer for a
`claude-code-cli` manager or a harness that is not OpenAI-compatible. The
investigation below is kept as written, with one section amended where the new
surface answers an objection it raised.

| | `ensemble serve` | `ensemble mcp` |
| --- | --- | --- |
| what the harness configures | a model provider, then picks `ensemble/manager` | an MCP server |
| who runs the manager | Ensemble | the harness's own agent, on its own model |
| who sees delegation | nobody outside Ensemble | the harness's model, as a tool call |
| manager backend | `openai-compatible-api` only | any |
| ports | one, loopback by default | none |
| harness tools reach the team | yes, through the manager | no |

---

## The first answer: an MCP server over stdio

Ensemble is a subprocess the harness spawns, speaking JSON-RPC 2.0 as
newline-delimited JSON on stdin/stdout. It offers three tools:

| tool | what it does |
| --- | --- |
| `ensemble_delegate` | send a task to a role, get the work back |
| `ensemble_roles` | list the team and what each member is for |
| `ensemble_health` | check every backend is reachable; spends no tokens |

That is the whole harness-facing surface. There is no plugin to install, no
provider module to register, and no HTTP port to open.

### Verified end to end

Against Claude Code 2.1.223 acting as the MCP client, with a real
`ensemble.yaml` and the real `claude` CLI behind the `reviewer` role:

```console
$ claude mcp add --scope local ensemble -- /path/to/ensemble mcp
Added stdio MCP server ensemble

$ claude mcp list
ensemble: /path/to/ensemble mcp - ✔ Connected

$ claude -p "List the tools the 'ensemble' MCP server provides."
- `ensemble_delegate`
- `ensemble_health`
- `ensemble_roles`

$ claude -p "Use ensemble_delegate to delegate to the 'reviewer' role the task:
             Reply with exactly the token ensemble-mcp-delegated-ok.
             Then report verbatim what the tool returned."
The tool returned, verbatim:
ensemble-mcp-delegated-ok
```

The full chain ran: harness → MCP stdio → Ensemble router → the `reviewer`
role → the Claude Code CLI adapter → `claude -p` → back. `tests/end_to_end.rs`
covers the same path with a stub CLI, so it runs in CI without a subscription.

---

## Why not the alternatives

The architecture names OpenCode as the example harness. Three of its extension
points were examined; two of them cannot carry this.

### A plugin-registered tool — does not reach the model

On OpenCode 2.0.32 the tool array a model is offered is fixed:

```
edit, glob, grep, question, read, shell, skill, subagent, webfetch,
websearch, write, execute
```

`tool.transform(reg => reg.add({...}))` *does* register a tool — it appears in
`reg.list()` and `tool.hook("execute.before")` fires for it — but it is never
added to that array, so a model calling it is told
`No tool named "..." is currently available`. A plugin is therefore usable for
*intercepting* tool calls and not for *offering* one, which is the opposite of
what Ensemble needs.

### A provider module — blocked by an unpublished schema

OpenCode will import a local file named by `providers.<id>.package` and call
`model(modelID, settings)` on it; the error it gives when the export is missing
states that contract outright. But the returned object is then validated
against an internal schema that a plain AI SDK language model does not satisfy
(`Cannot initialize cc/claude-sonnet: Schema validation failed`), and that
schema is minified and unpublished. `specificationVersion` is the obvious
suspect — the bundled SDK is `"v3"`, not AI SDK v5's `"v2"` — but correcting it
does not satisfy the validator either. Guessing at the rest would invent a
capability and break on the next release.

### An OpenAI-compatible provider block — not orchestration *by itself*

> **Amended.** The objection below was correct about what it described, and it
> is the reason the first implementation was MCP. It is no longer the whole
> picture; see [The second answer](#the-second-answer-an-openai-compatible-chat-api).

This is the one that round-trips a full tool-calling turn with no gateway
involved, and it is a legitimate way to put *one model* behind a URL. What it
cannot do is carry a team **in the request**:

A `/chat/completions` request carries messages and nothing else. It has no
field for which role should do the work, no field for the project directory,
and no way to return provenance. Driving a team through it *that way* means
encoding all of that into the prompt text and parsing it back out — a protocol
invented inside a protocol, which breaks the moment a model paraphrases its own
preamble.

What changed is where the team lives. It does not have to be in the request at
all. The role goes in the arguments of a tool call the *manager* makes, inside
Ensemble, against a schema Ensemble supplied — named, typed, and never parsed
out of prose. The project directory comes from the server's `--dir`, which no
request can touch. Provenance goes to the server's log, where it cannot be
rewritten by a model. So the endpoint carries messages, and that is all it
needs to carry.

### Why MCP, concretely

* It is the one mechanism verified to put a callable tool in front of the
  model.
* It is a subprocess with no port, so nothing is listening on the machine and
  there is no local-process authentication question to get wrong.
* It is not OpenCode-specific. Claude Code, Zed, Cursor and others speak MCP,
  so the same binary and the same `ensemble.yaml` work in all of them. The
  integration above was in fact verified against Claude Code, because it is
  what was installed.

All three still hold. What MCP does not give you is a harness that needs no
configuration beyond picking a model, and a manager that Ensemble rather than
the harness owns.

---

## The second answer: an OpenAI-compatible chat API

```console
$ ensemble serve --dir /your/project
ensemble: serving ensemble/manager on http://127.0.0.1:8787/v1
```

Two endpoints: `GET /v1/models`, which lists exactly `ensemble/manager`, and
`POST /v1/chat/completions`, blocking or streaming.

### What happens inside one request

1. The harness posts its conversation and its own tool definitions.
2. Ensemble prepends the manager's configured prompt, then a system turn
   naming the roles and explaining that delegation runs inside Ensemble.
3. The manager is offered `ensemble_delegate`, `ensemble_roles`,
   `ensemble_health` **and** every tool the harness sent.
4. An ensemble tool call runs immediately, through the same
   `Ensemble::delegate` the MCP surface uses, and the result goes back to the
   manager. The loop continues. The harness never learns it happened. A model
   that names a role directly — `scout({"task": …})` — is making the same
   request, and Ensemble treats it as the delegation it means rather than
   forwarding a tool the harness does not have. If the harness happens to offer
   a tool with that name, the harness tool wins; the role stays reachable
   through `ensemble_delegate`.
5. A harness tool call ends the turn and comes back as an ordinary
   `tool_calls` completion, so the harness runs it and posts the result next
   time.
6. When one reply contains both, the internal calls run now and only the
   external ones are forwarded — the harness must answer exactly the ids it
   was given.

Bounded at eight manager calls and sixteen delegations per request; exhausting
the step budget forces one final tool-free call so the client always gets an
answer.

### Carrying the invisible turns across a round trip

Step 4 creates a problem step 5 exposes: the delegation turns are not in the
transcript the harness holds, so the next request is missing them. Ensemble
keeps its own copy, keyed on a canonical form of the whole harness-visible
transcript at the end of every completed turn — an answer, or a turn carrying
tool calls. On the next request the longest stored prefix the request still
begins with is restored, followed by the messages the harness has added since.
Full equality of those lines decides, so no collision can return another
conversation's turns.

Keying on every completed turn, not only on tool calls, is what lets an
internal delegation survive into the next *prompt*. A turn that delegated and
then answered hands the harness no tool call at all, but it did hand back an
answer, and the hidden delegation belongs behind it.

The store is bounded — 64 entries, 32 MiB, 30 minutes — and holds one live
entry per conversation. A miss is safe: the manager forgets it delegated, and
the transcript is still well-formed, because the assistant message the harness
holds contains only the calls it was asked to run and it answered all of them.

Replaying the delegations statelessly was considered and rejected: quadratic
in model calls, non-deterministic, and side-effecting — a `coder` role with
`Edit` permission would write the file twice.

What this store is *not* is the conversation itself. The harness owns that and
posts it back in full on every request; Ensemble keeps only the turns the
harness was never shown. So when a manager answers a third message as if it
were the first, the history went missing somewhere Ensemble can see but does
not control, and there are two candidates: a client that posted only the latest
turn, or a model server that dropped the front of an over-long prompt.
`ENSEMBLE_SERVE_LOG=1` tells them apart — one stderr line per request with the
role and size of every message that arrived, and how many the manager ended up
reading. Roles and sizes only; the content is the user's work and is never
logged. If the arriving shape grows with the conversation and the manager still
forgets, the prompt is overflowing the model server's context window
(llama-server `-c`, vLLM `--max-model-len`, Ollama `num_ctx`), which drops
tokens from the front — system prompt first.

### Registering it

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

> **Verified with OpenCode 2.1.4.** This exact provider block ran
> `ensemble/manager` end to end: model discovery, streaming, a delegated role,
> a two-request harness tool round trip, and a hidden delegation carried into
> the next prompt. `tests/serve_api.rs` covers the same paths against the real
> binary over a real socket. `ensemble serve` prints this block on startup, so
> it tracks the real port.

### Limitations of this surface specifically

**The manager must sit on an `openai-compatible-api` backend.** It has to
drive a tool schema Ensemble supplied, and `claude -p` cannot — see the
`allowed_tools` addendum below for what it does instead. `ensemble serve`
refuses to start rather than failing on the first message, and `ensemble
check` reports `serve: available` or `serve: unavailable` so you can find out
without starting anything.

**The backend type saying yes is not the same as the server saying yes.**
`openai-compatible-api` declares `tool_calling: true` because the protocol has
the field; whether this server, started with these flags, parses this model's
tool call into it is a different question. llama-server without `--jinja`,
vLLM without `--enable-auto-tool-choice --tool-call-parser`, and an Ollama
template with no `.Tools` branch all answer 200 with the model's call sitting
in `content` as prose — and an empty `tool_calls` is exactly what a manager
with a finished answer sends, so the turn ends after one step looking
entirely normal while nothing is ever delegated. `ensemble serve` therefore
spends one small model call at startup, with `tool_choice: required` and a
throwaway tool, and refuses to bind if no call comes back. `auto` would not
do: a model that chose to answer and a server that dropped the call it made
are indistinguishable. A probe that cannot *reach* the endpoint is
inconclusive rather than negative — the model server may not be up yet, and
that is `ensemble health`'s business. `ensemble check` never probes, because
a command that loads a file and reports what is in it must not quietly bill
you. Should the fault appear mid-turn anyway, `manager.rs` recognises a
written-out call in an otherwise finished reply and says so on stderr; it
does not change the turn, because it cannot tell the two apart with enough
confidence to act.

**Replies do not stream token by token.** `ModelPort::complete` returns a
finished answer, so the SSE stream is synthesised: correct chunk sequence,
correct `finish_reason`, correct `usage`, but the text arrives in one burst
when the turn completes. Comments (`: keep-alive`, `: ensemble: …`) go out
every ten seconds so the client does not time out. This is not only an
implementation shortcut — forwarding real upstream deltas would stream the
manager's internal delegation calls to a harness that must not see them, so
most of it would have to be filtered back out.

**A client disconnect does not cancel an in-flight backend call.** The
orchestration runs on its own thread and a blocking `complete` cannot be
interrupted; the call finishes and its result is discarded.

**One workspace per server, and no persistence.** The project directory is
settled from `--dir` at startup. Conversation state is in memory and is lost
on restart, degrading as described above.

**A tool-name collision is refused, not resolved.** If the harness offers a
tool called `ensemble_delegate` — which in practice means the MCP server and
this API are both active in one session — the request is a 400 that says so.
Shadowing it would mean the manager calls what it believes is a file edit and
gets a delegation.

### Security

The default bind is `127.0.0.1`, so nothing off the machine can reach it.
A non-loopback `--host` **refuses to start** without `--api-key-env`, which
names the environment variable holding the bearer token — the same convention
as `api_key_env` in `ensemble.yaml`, and for the same reason: the secret is
never in a file, never on a command line, and never logged. Tokens are
compared in constant time, and a refusal says nothing about what was sent.

The project directory is the one the server was started with. It is not a
request field and not something a model can write, because a path a model
chooses is a path that can point anywhere.

---

## Registering it

### Claude Code

```console
$ claude mcp add --scope local ensemble -- /path/to/ensemble mcp
```

Or in `.mcp.json`:

```json
{ "mcpServers": { "ensemble": { "command": "/path/to/ensemble", "args": ["mcp"] } } }
```

### OpenCode

In `opencode.json`:

```json
{ "mcp": { "ensemble": { "type": "local", "command": ["/path/to/ensemble", "mcp"] } } }
```

> **Not verified against OpenCode.** OpenCode 2.1.4 is installed and the HTTP
> provider block above was verified with it, but this MCP registration snippet
> has not been run. The MCP server itself is verified — against Claude Code, and
> by `tests/end_to_end.rs` driving the protocol over real pipes — so what is
> unverified is this config block, not the server behind it.

On OpenCode 2.0.32 specifically, two further things are worth knowing, both
previously observed:

* MCP tools are reached through OpenCode's `execute` (Code Mode) tool as
  `tools.ensemble["ensemble_delegate"](input)`, not as entries in the model's
  tool schema. They are reachable either way.
* Do not set `codemode: false` on the server. It does not promote the tools
  into the schema array; it appears to remove them from the Code Mode catalog
  instead, making them unreachable altogether.

---

## Limitations

Worth knowing before trusting any of this. Everything here applies to both
surfaces unless it says otherwise.

**The project directory comes from where the harness spawned the server.**
A harness starts an MCP server once per session with its own working directory,
and `ensemble mcp` captures that as the directory roles work in. It cannot
change per tool call, because the tools do not take one — deliberately, since a
path argument chosen by a model is a path argument that can point anywhere.
A harness that starts its background service from `$HOME` rather than the
project will give every role the wrong tree; `ensemble mcp --dir <path>` is
the override. `ensemble serve --dir <path>` is the same thing, settled once
when the server starts.

**Claude-backed roles return prose, never tool calls.** `claude -p` emits
Claude Code's *own* tool_use blocks, which Claude Code executes itself, and
returns text. It never returns an OpenAI `tool_calls` array against a schema
Ensemble supplied. That is right for a planner, a reviewer or a manager — roles
that do real work with their own tools and report back — and it means such a
role cannot be driven as a function-calling model. `supports_tool_calling()`
declares this rather than leaving it to be discovered.

**Delegation is stateless and single-shot.** Each `ensemble_delegate` call is
one request and one answer. There is no conversation carried between calls, so
a role cannot ask a clarifying question and continue; the calling model has to
pass what the role needs in `task` and `context`. This keeps the tool honest
about what it does, at the cost of multi-turn delegation.

**Ensemble does not execute a harness's tools on a model's behalf.** A
*delegated role* that replies with `tool_calls` and no content is refused with
an explanation rather than returned as an empty answer: a delegation offers no
tools, so there is nothing it could have been asking for. The manager under
`ensemble serve` is the one exception, and it is not an exception to the rule —
its harness tool calls are *forwarded* to the harness, which executes them.
Ensemble still runs no tool but its own delegation.

**There is no retry, and no concurrency cap.** A failed delegation says
whether it is worth retrying and the calling model decides. `ensemble serve`
bounds steps and delegations within one request, but nothing bounds spend
across requests, and nothing bounds how many delegations run at once.

**`claude --version` is the health check for the Claude backend.** It proves
the binary runs. It does not prove the subscription is valid, which would cost
a model call to establish.

**Timeouts kill a process group, which assumes Unix.** On Unix the child is
spawned into its own process group and a timeout signals the whole group, so a
`claude` wrapper's children die with it. On non-Unix only the leader is
signalled; the reader deadline still bounds the call, but an orphan may
survive. This was a real bug — killing only the leader left a 2-second timeout
taking 120 seconds, because the orphan still held the stdout pipe — and both
halves of the fix are covered by tests.

---

## Addendum: tool permissions are a hard boundary (verified 2026-10-28)

`allowed_tools` on a `claude-code-cli` backend is enforced by Claude Code as a
**whitelist**, not merely as a list of pre-approved tools under
`--permission-mode dontAsk`. This was previously assumed and is now observed.

Two roles, identical but for the backend they sit on, given the same task:

```console
$ ensemble run coder "Create a file named hello.txt containing exactly: ok"
ensemble: coder via claude-write/claude-sonnet-5 (4 in, 126 out)
Created `hello.txt` with the content `ok`.
$ cat hello.txt
ok

$ ensemble run reviewer "Create a file named hello2.txt containing exactly: ok"
ensemble: reviewer via claude/claude-opus-...
Both denials come from the permission mode, not from anything about the request.
$ ls hello2.txt
ls: cannot access 'hello2.txt': No such file or directory
```

`coder` sits on a backend with `allowed_tools: "Read,Grep,Glob,Edit,Write,Bash"`;
`reviewer` on one with `"Read,Grep,Glob"`. The file was created in one case and
not the other. So read-only roles are read-only in fact, and a role's write
capability is a property of the backend it is assigned to — not something the
role, or the model running it, can grant itself.

**Consequence: delegation cannot recurse.** A delegated `claude -p` whose
`allowed_tools` omits `mcp__*` cannot call back into Ensemble, even when the
project it runs in has the server registered:

```console
$ ensemble run scout "Call mcp__ensemble__ensemble_health and report what it returns."
I cannot call the tool. The `mcp__ensemble__ensemble_health` tool is currently
denied in your permission settings (running in "don't ask mode").
```

Neither shipped backend lists an `mcp__*` tool, so recursion is closed by
default. It would re-open for a backend whose `allowed_tools` named one — worth
remembering before widening that setting.
