# Plan: make Ensemble complete, easy to use and functional

This plan is written so any coding agent can carry it out with no other context.
Read "Product model" and "Ground rules" first. Then do the phases in order. Each
phase ends in a green `cargo fmt --check`, `cargo clippy -- -D warnings` and
`cargo test`, and in one commit.

> **Direction change, November 2026.** `ARCHITECTURE.md` records where Ensemble
> is going: one binary with a hub role and a node role, a flat helper team per
> user, tasks assigned to people, and a knowledge pipeline. Config version 3 has
> landed: the "organization" with a `teams:` map below is now a flat **helper
> team**, `--org` is `--team`, and the `llm` adapter is `direct`. The phases
> below are the record of how level 0 was built; read "organization" and "team"
> in them as "helper team". What comes next is the build order in
> `ARCHITECTURE.md`.

## Product model (decided with the owner; this replaces the old per-project model)

- **Manager.** The agent the user talks to (Claude Code, OpenCode, ...). Ensemble
  is registered in it as an MCP server. The manager is not a member of a team.
  Note the word clash: `claude-code` is also an adapter type for team members.
  Keep the two roles named apart in the CLI and docs (`--manager`).
- **Organization.** One directory, usually its own git repo, that holds the whole
  configuration: shared `llm_backends`, shared `limits`, and a `teams:` map.
  Locally it is a path; remotely it is a git URL pinned to a commit SHA. A user
  connects exactly one organization (`--org`). There are no multiple team
  sources, no layering, no includes, no merging, no presets and no upward search.
- **Team.** A named entry under `teams:`. It has a `prompt` (what the team is
  for; the manager reads it to choose a team), its `agents`, its `gates` and its
  `tracker`. Agents are named by `team/agent`; the short `agent` form works when
  the name is unique across the organization.
- **Project / workdir.** The folder the manager opened. It can be a parent of
  several repos. **Nothing of Ensemble lives in it**: no `.ensemble/`, no
  config, no tickets. Ensemble reads and writes there only what an agent run
  changes.
- **State.** Run logs, usage counters, trust records and local-markdown tasks
  live in a per-user state directory, keyed by a hash of the workdir:
  `$XDG_STATE_HOME/ensemble/<hash>/` (default `~/.local/state/ensemble/<hash>/`).
  State is never configuration. Remote organizations are cached in
  `$XDG_CACHE_HOME/ensemble/orgs/` (default `~/.cache/ensemble/orgs/`).
- **Secrets.** Never in an organization repo. Variables a config names
  (`api_key_env`, `token_env`) are read from the manager's process environment,
  plus an optional `--env-file <path>` given to `connect`.
- **Worktrees.** `isolation: worktree` needs a git repo, and the workdir may be a
  parent of several. Tasks and delegations take an optional `repo` argument: a
  path inside the workdir. Reject any path that escapes it (resolve symlinks and
  `..`). Without `repo`, the workdir itself must be a git repo, or the run fails
  with a message that says so.

Example `config.yaml` of an organization:

```yaml
version: 2
llm_backends: { ... }          # shared by the whole organization
limits: { ... }                # shared server and subscription limits
teams:
  hardware:
    prompt: Designs and checks KiCad boards.
    tracker: { type: jira, ... }
    gates:
      drc: { command: kicad-cli pcb drc board.kicad_pcb, timeout_seconds: 300 }
    agents:
      layout: { ... }
      reviewer: { ... }
  software:
    prompt: Builds the automation backend in Rust.
    tracker: { type: local_markdown }
    gates:
      test: { command: cargo test, timeout_seconds: 900 }
    agents:
      coder: { ... }
```

Start with a single `config.yaml`. If it grows too large, allow the fixed layout
`teams/<name>.yaml`. That is a convention the loader knows, not an include.

## Ground rules

1. **No defaults in config.** Every setting is written explicitly, and a missing
   one is refused by name. The only optional settings are those where leaving
   them out means "not used" (an agent's `prompt`, a backend's `api_key_env`).
   Commands that write config (`setup`, `add-agent`) write every setting; the
   generator fills values in, the parser never does.
2. **Tagged variants.** What differs between cases is a `type:` variant carrying
   exactly its own settings (see `Adapter`, `Budget`, `OpenCodeModel` in
   `src/config.rs`). New variants (`tracker`, backend types, budget types) follow it.
3. **No built-in agent loop.** Ensemble only connects agent CLIs, or makes one
   chat request for a light `llm` agent. `llm_backends` are turned into each
   CLI's config in memory, per run.
4. **Ensemble never merges on its own.** A merge happens only when the user
   types `ensemble merge`; the manager gets no merge tool.
5. **Keep in step with every change:** `ensemble.schema.json` (a test in
   `src/config.rs` checks it), the sample config, `README.md`, the tests.
6. **Tests need no network, no model server and no real CLI.** Shell scripts
   stand in for CLIs (`workspace::tests::executable`). Real-CLI checks are a
   separate opt-in suite.
7. Match the surrounding code: comment density, naming, the `port::Error`
   vocabulary. Errors are written for the model that reads them, and end with
   the command that fixes them where one exists.
8. Commit style: short imperative subject ending in a full stop (see `git log`).
9. **Security.** A config decides what runs on the user's machine (agent
   `command`, gate `command` through `sh -c`, `tools`, `permission_mode`). Treat
   a remote organization as untrusted code until trusted (phase 5).

## Decisions that reverse an earlier choice (already agreed with the owner unless marked)

- "One `.ensemble/` per project, no user-level anything" is replaced by one
  organization plus a per-user state and cache directory. Agreed.
- "Profiles are dropped" stays true in spirit: there are no presets and no
  includes. `teams:` is one file, not layering. Agreed.
- Update the stored project notes (the `project-level-config-only`,
  `ensemble-profiles-rethink` and `autonomous-queue-design` memories) when
  phase 3 lands, so they match.
- **Not yet confirmed; ask before phase 6 or 7:** human task commands (an
  earlier decision said no `ensemble submit` command) and the daemon (the runner
  was to live inside `ensemble mcp`).
- **Chosen by the assistant, owner did not answer; confirm in phase 3 and 8:**
  per-user state directory (not state inside the organization repo); the
  tracker's label mapping lives in the tracker block, not on each agent; no
  wildcard trust for a publisher.

## Phase 1. Real-CLI checks

The owner dropped CI: there are no GitHub workflows, and background work runs on the
local computer, not on a remote server. Items 1, 3 and the release workflow of
phase 14 are therefore not built, and the `.github/` directory was removed.

Goal: a safety net before the large changes.

1. `.github/workflows/ci.yml`: on push and pull request run `cargo fmt --check`,
   `cargo clippy -- -D warnings`, `cargo test` on `ubuntu-latest` and
   `macos-latest`, and a Nix job running `nix build`.
2. `tests/real_cli.rs`: every test `#[ignore]` and gated on `ENSEMBLE_REAL_CLI=1`.
   For each CLI found on `PATH`, run `health` and one trivial delegation ("reply
   with the word ok") in a temp directory; assert the parsed `AgentRun` has text,
   a model and steps.
3. `.github/workflows/real-cli.yml`: manual and weekly. It runs the ignored suite
   only if the needed secrets exist; otherwise it exits green with a note. No
   secrets in the repo.
4. Fixtures: capture one real event stream per CLI version into
   `tests/fixtures/<cli>/<version>.jsonl` and test that each parses.
5. Fix every `fmt` and `clippy` finding the CI exposes, in this commit.

## Phase 2. Split the large files

Goal: `src/config.rs` (2088 lines) and `src/harness/mcp.rs` (1562) are easy to
change before phase 3 rewrites the first.

1. Behavior-preserving moves. `cargo test` passes before and after; no test
   changes except `use` paths.
2. `config.rs` becomes `config/`: `mod.rs` (public types, `load`), `parse.rs`
   (YAML to types, "required by name" errors), `validate.rs`, `env.rs`
   (environment and secrets), `schema.rs`. Re-export so `crate::config::X` stays.
3. `harness/mcp.rs` becomes `harness/mcp/`: `mod.rs` (server loop), `protocol.rs`,
   `notify.rs`, `session.rs`.
4. One commit per file split.

## Phase 3. Organization model

Goal: the product model above, replacing `.ensemble/` in the project.

1. **Config.** New `version: 2` with `llm_backends`, `limits`, `teams`. Move
   `agents`, `gates` and `tracker` under a team. Refuse `version: 1` with a
   message that names `ensemble migrate`. `ensemble migrate` rewrites a v1
   project config into an organization with one team named `default`, writing
   every setting.
2. **Locating the organization.** `--org <path>` on `ensemble mcp` and every
   command. No `--org` is an error that names `ensemble setup`. No search, no
   fallback.
3. **Workdir.** The process's working directory, or `--dir`. Stop reading and
   writing `<workdir>/.ensemble/` entirely.
4. **State directory.** New module, `src/state.rs`: resolves and creates
   `<state>/<hash-of-canonical-workdir>/`, holding `runs/`, `usage.json`, and
   `tasks/` for the `local_markdown` tracker. Point `tasks::Store` at it.
   Usage counters for budgets move to a directory keyed by the organization, not
   the workdir, because a subscription is shared across projects.
5. **Names.** Agents are `team/agent`. The short form resolves only when unique;
   when not, the error lists the candidates.
6. **Gates** resolve inside the task's team: a task names a gate of its agent's
   team.
7. **`repo` argument** on `delegate`, `submit` and task tickets (see Product
   model). Worktrees are made in that repo; the changes snapshot covers it.
8. **Secrets.** Read from the process environment plus `--env-file`; delete the
   `.ensemble/.env` handling. Keep the precedence tests, adapted.
9. **Manager-facing tools.** `agents` returns teams first, one line each from
   the team `prompt`, and members on `agents team=<name>`. Do not list every
   agent of a large organization by default.
10. Update `show`, `check`, `health`, `run`, the schema, the sample, the tests
    (including `tests/end_to_end.rs`) and the README.
11. Update the stored project notes the "Decisions" section names.

## Phase 4. Simplicity: first minute, one command

Goal: a new user is working in under five minutes without reading the README.

1. **`ensemble setup`.** A short wizard:
   - find agent CLIs on `PATH` (`claude`, `opencode`, later `codex`);
   - probe local model servers: Ollama `11434`, llama-server `8080`, LM Studio `1234`;
   - ask "Which agent do you talk to?" (the manager);
   - offer a starter organization ("one team: coder and reviewer");
   - write the full explicit config into a directory the user names (default
     suggestion `~/ensemble-org`), run `git init` there if asked;
   - register the manager (see 2) and run `doctor`;
   - end with a sentence to copy, such as `Try: "have the team implement X"`.
   It also runs non-interactively with flags, for tests and scripts.
2. **`ensemble connect --manager <claude-code|opencode> --org <dir-or-url> [--only team,team] [--env-file path] [--global]`.**
   Print the real command, then run it: `claude mcp add --scope local ensemble -- ensemble mcp --org ...`
   or `opencode mcp add ensemble -- ensemble mcp --org ...`. `--global` maps to
   `--scope user` or `--global`. If the manager CLI is missing, print the command
   and exit non-zero. `--only` is a per-user filter, passed to `mcp`, not config.
3. **Teach the manager automatically.** Fill the MCP `initialize` result's
   `instructions` with the team names, one line each, and when to delegate. The
   user no longer edits `CLAUDE.md` or `AGENTS.md`. Keep it short; members come
   from `agents`.
4. **`ensemble doctor`.** Replaces `health` as the friendly entry point (keep
   `health` as an alias). It checks CLIs, backends, models, env variables, empty
   or near-duplicate team prompts, and prints for each failure the fixing
   command (for example `ensemble remove-agent software/reviewer`).
5. **`ensemble add-agent <team/name> --adapter <type>`** and **`remove-agent`.**
   Insert or delete a fully written block by text edit, so comments and order
   survive. Refuse an existing name.
6. **Starter organizations.** Ship two or three as embedded templates (software,
   hardware, docs) reachable by `ensemble setup --starter <name>`.
7. README: open with three lines (`setup`, then ask your agent), then the
   reference. Move adapter and task reference into `docs/`.
8. Tests: `setup` and `connect` against stub `claude` and `opencode` scripts
   that record their arguments; `doctor` output for each failure kind;
   `initialize` carries `instructions`.

## Phase 5. Remote organizations and trust

Goal: `--org github.com/acme/org@v1.2` works safely.

1. **Source forms.** `--org <path>` or `--org <git-url>[@<ref>]` (`https://`,
   `ssh://`, `git@host:...`, and the `github.com/...` shorthand). Shell out to
   `git`; store no credentials. Private repos use the user's SSH agent or
   credential helper.
2. **Pinning.** `connect` resolves the ref (tag or branch) to a full commit SHA
   and registers `--org <url>#<sha>`. `mcp` refuses an unpinned remote source;
   it never follows a branch at runtime.
3. **Cache.** Clone into `<cache>/orgs/<host>/<path>/<sha>/`. Later runs use it,
   offline. Verify the checked-out HEAD equals the SHA before use.
4. **Trust.** `ensemble org trust <url>#<sha>` prints in plain text every agent
   `command`, gate `command`, `tools` list and `permission_mode`, with warnings
   for the risky ones (`bash`, `edit` on `isolation: shared`, a mode that skips
   permission checks). It records approval for that exact SHA in the state
   directory. `ensemble mcp` refuses an untrusted remote organization and exits
   with the exact `trust` command; it cannot prompt because stdio is the
   protocol. A new SHA needs new approval. Local directories need none.
5. **Update.** `ensemble org update <org>` resolves the ref again, shows the diff
   of the config between the old and new SHA, and re-pins only on confirmation,
   then asks for trust.
6. No wildcard trust for a publisher. Per-SHA approval is the guarantee.
7. Tests: a local bare git repo as the "remote"; pin, cache, offline reuse,
   tampered cache refused, untrusted refused, moved branch not followed.

## Phase 6. Human task commands (needs owner confirmation)

1. `ensemble tasks [--state <state>]`, `ensemble task <id>`,
   `ensemble submit <team/agent> <title> [...]`, `ensemble answer <id> <text>`.
2. They read and write the state directory only; they start no worker.
3. Share formatting with `harness::queue::status`; one function, two callers.
4. Test: a task written by `submit` is picked up by a running `ensemble mcp`;
   `answer` unblocks a `NEEDS INPUT:` task.

## Phase 7. Run tasks without an MCP session (needs owner confirmation)

External trackers (phase 8) need something polling them, which a runner that
lives only in an agent session cannot do. So this phase is required for phase 8
unless the owner accepts session-only polling.

1. `ensemble serve`: a foreground process running the same `Queue` worker as
   `ensemble mcp`, with no MCP transport. On stop, interrupted tasks are marked
   `interrupted` and queued again.
2. A lock file in the state directory keeps two workers off one workdir;
   `ensemble mcp` then reads the queue and does not start a worker.
3. Document a systemd user unit and a launchd plist.
4. Tests: two processes and one lock; kill during a run, restart, the task resumes.

## Phase 8. Task trackers (local markdown only for now)

Goal: tasks come through a `Tracker` interface, with `local_markdown` as the only
implementation built. GitLab, Jira and Anning are deferred by the owner; do not
build them. Design the interface so they can be added without changing the queue.

### Interface

A `Tracker` trait in `src/tracker/mod.rs`, one instance per team:

- `ready() -> Vec<Issue>`: issues with no result whose `after` are all `done`.
- `get(id) -> Issue`.
- `create(ticket) -> id`.
- `comment(id, text)`: appends a line to the run log, `runs/<id>/log`.
- `set_status(id, status)`: `queued`, `running`, `waiting`, `done`, `failed`,
  `cancelled`, `interrupted`.
- `write_result(id, outcome)`: once; refuses a second write.
- `ask(id, question)` and `answer(id, text)`: for `NEEDS INPUT:`.

`Issue` is the existing `tasks::Ticket` plus its id. The queue
(`src/harness/queue.rs`) takes `&dyn Tracker` and no longer touches `tasks::Store`
directly. Move `Store` behind the trait as the `local_markdown` implementation.

### Config

```yaml
tracker: { type: local_markdown }
```

A tagged variant. `local_markdown` has no settings of its own, so it carries
none; the variant exists so other types can carry theirs. A team without a
`tracker` key is refused by name (no defaults).

### Done in this phase

Tasks live in `<state>/tasks/<team>/<id>/`. Ids are numbered across the folder
(not per team), so an id still names one task for `status`, `wait`, `after` and
the worktree branch. The queue reaches tickets through the `Tracker` trait
(`src/tracker.rs`); `LocalMarkdown` is the one implementation, and each ticket
carries the team of its folder.

### Starting point after phase 3

Phase 3 left one task store per working folder, `<state>/tasks/<id>-<slug>/`, whose
tickets name their agent as `team/agent` (or an unambiguous short name). This
phase splits it into one tracker per team as designed below, and moves each
ticket under its team.

### Where files live

Nothing in the project. Tasks go in the state directory, per team:

```
<state>/<workdir-hash>/tasks/<team>/<id>-<slug>/
  task.md        the ticket: front matter and the text below it
  result.md      written once when the task ends
  questions.md   the NEEDS INPUT conversation
<state>/<workdir-hash>/runs/<id>/    live state, attempt logs, run record
```

Tasks are therefore local to the user's machine, not committed with a repo. To
keep them in git, the user points `XDG_STATE_HOME` somewhere under version
control; build nothing more than that.

### Format (as today, with these changes)

`task.md` keeps the current front matter: `title`, `agent`, `context`, `goal`,
`constraints`, `files`, `done_when`, `after`, `gate`. Changes:

- `agent` is the short name inside the task's team. A cross-team name
  `team/agent` is refused: a tracker serves one team.
- `gate.name` names a gate of the same team.
- `repo` (new, optional): a path inside the workdir where the task works (see
  Product model).
- `after` ids refer to tasks of the same team.
- Ids: a number, then a slug, as today (`0002-add-retry-to-the-fetcher`). Numbers
  are per team. If `create` finds an id taken (state synced between machines) it
  takes the next number; an id is the whole folder name, so a collision never
  merges two tasks.
- A task folder written by hand is picked up within a few seconds, as today.
- Reject ids and `repo` values that escape their directory, as the current
  `an_id_cannot_leave_the_tasks_directory` test does.

### Behavior

Same as today's queue: oldest id first, each agent at most `max_concurrent` at
a time, gates with retries, `after` waits, `NEEDS INPUT:` stops the task until
`answer`, a session end marks running tasks `interrupted` and queues them again.
`result.md` stays write-once.

### Tests

The existing `src/tasks.rs` and queue tests move to the trait and keep passing.
Add: two teams in one state directory do not see each other's tasks; a task
naming another team's agent is refused; `ready()` skips tasks whose `after` are
unfinished; a second `write_result` is refused; ids cannot escape the tasks
directory.

### Deferred

`gitlab`, `jira`, `anning`: not now. When the owner asks, they implement the same
trait. External trackers also need phase 7 (a runner that outlives a session),
and Anning needs its API documentation first.

## Phase 9. Observability

1. Record per run in the state directory: agent, model, start and end time,
   `Usage`, steps, tool calls, outcome (data `AgentRun` already carries).
2. `ensemble stats [--since <duration>] [--json]`: per agent: runs, failures,
   total and mean seconds, tokens in and out. Missing usage shows `-`, not `0`.
3. Tests with stub runs producing known numbers.

## Phase 10. Mid-run questions for delegations

1. A delegation ending with `NEEDS INPUT:` returns a structured result:
   `needs_input: true`, the question, and a `session` token.
2. `delegate` accepts `continue: <session>`; it re-runs the same agent with the
   earlier text, the question and the answer.
3. True mid-run questions (CLI stays alive) are out of scope; headless modes do
   not offer them. Say so in the README.
4. Update the `delegate` schema and the MCP tests that check tool schemas.

## Phase 10b. An organization that works on its own (added by the owner)

The user is the organization's owner or stakeholder. They talk to their own agent
(the manager) and need not know who works on what, though they can look. The
organization works through its tasks without a session, and waits for planning
when it has none.

Decided with the owner:

- `local_markdown` is a tracker like GitLab or Jira, on markdown files. A task in
  it has an **assignee**: an agent. A task with none is in the **backlog**.
- An agent with no task assigned to it stays idle until one appears for it. Nothing
  pulls from the backlog by itself: the manager (or the owner) **assigns** backlog
  tasks.
- The agent working on a task may **finish** it, or **hand it off**: end its
  reply with `HAND OFF: <team/agent> <what remains>`. The task goes to that agent
  with the note and what was done; it is not finished.
- When no task is active (assigned, unfinished, not waiting for an answer), the
  organization says so, and says what waits: the backlog, and questions for the
  owner. It does this without ending the run (`status`, `tasks`, and a line from
  `serve`); `serve --exit-when-idle` ends it instead.
- The manager is the agent the user talks to. A team may also have a manager-like
  agent of its own; nothing here forbids it.

Built: tickets with no agent (backlog) and the `assign` tool and command; the
assignee kept in `assignee.txt` beside the immutable `task.md`; `HAND OFF:` with a
`handoffs.md` history the next agent is shown; the idle summary in `status`,
`tasks` and `serve`; `serve --exit-when-idle`.

## Phase 11. Limits and safety

1. **Shared limits.** `limits:` at the organization level, each entry named, with
   `max_concurrent` and `budget`. An agent writes either its own values or
   `limit: <name>` as a tagged variant, never a default.
2. **Token budgets.** `budget: { type: per_hour_tokens, tokens: N }`, counted from
   `Usage`. An adapter that reports no usage counts zero, and `doctor` warns.
3. **No sandbox.** Instead `ensemble check --warn` and `doctor` warn on an agent
   with `bash` or `edit` and `isolation: shared`, and on permission modes that
   skip checks.

## Phase 12. More adapters and backend types

Built: the `anthropic-api` backend type (for `llm` agents; OpenCode on it is refused
at load, by name), and the Codex adapter, written from Codex's documented `exec --json` output and
not yet run against a real Codex (see its README section).

1. **Codex adapter.** `Adapter::Codex` with every setting explicit (read
   `codex exec --help` and its `--json` output to choose them), a new
   `src/external/codex.rs` modeled on `opencode.rs`, router arm, `show`, schema,
   sample, README, stub-based tests, and a real-CLI test.
2. **`anthropic-api` backend type** (`base_url`, `api_key_env`, `timeout_seconds`)
   for `llm` agents. Claude Code keeps its own login. `health` and `doctor` list
   models from the right endpoint per type.
3. Reject at load an adapter and backend pairing that cannot work, by name.

## Phase 13. Merging help for worktree tasks

1. `ensemble review <id>` prints the branch, `git diff --stat` and the result;
   `ensemble diff <id>` prints the full diff.
2. `ensemble merge <id> [--squash] [--cleanup]`: only when the user types it.
   Refuse on a dirty tree, a task not `done`, or a conflict (abort the merge and
   list the files). Delete nothing without `--cleanup`.
3. A read-only MCP tool `review`. No merge tool.
4. Tests use real git repos, as `src/worktree.rs` does.

## Phase 14. Platforms and distribution

1. macOS: CI from phase 1 covers it; fix process-group kill and `sh` assumptions.
2. Windows: unless the owner says otherwise, print "Unix only, use WSL" and exit.
3. Releases: a tag workflow builds `x86_64` and `aarch64` for Linux and macOS and
   attaches the binaries.
4. crates.io: fill `Cargo.toml` metadata. Ask the owner for the license. Publish
   only when told.
5. README install section: release binaries, `cargo install`, Nix.

## Order and size

| phase | size | needs owner? |
| --- | --- | --- |
| 1 CI and real-CLI checks | medium | no |
| 2 split large files | medium | no |
| 3 organization model | large | confirm state directory |
| 4 simplicity | medium | no |
| 5 remote organizations and trust | medium | no |
| 6 human task commands | small | yes |
| 7 serve (daemon) | large | yes |
| 8 trackers (local markdown only) | medium | no |
| 9 observability | small | no |
| 10 mid-run questions | medium | no |
| 11 limits and safety | medium | no |
| 12 adapters and backends | large | no |
| 13 merging help | medium | no |
| 14 platforms and distribution | medium | license, Windows |

## Definition of done for every phase

- `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` pass.
- `ensemble.schema.json`, the sample config and `README.md` match the code.
- No new config setting has a default.
- One commit per phase, with a message that says what the user can now do.
