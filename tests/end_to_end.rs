//! End-to-end tests: the real executables, `ensemble-hub` and
//! `ensemble-runner`, a real config file on disk, and a stand-in for the
//! OpenCode CLI.
//!
//! The unit suites cover the request path with fakes injected. These cover what
//! fakes cannot: that each executable parses its arguments, finds its config,
//! starts the agent's CLI with the right command line and environment, runs a
//! hub and a runner that work through tasks together, and asks a real socket
//! which models it serves.
//!
//! No test here needs the network, a model server or an agent CLI.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const HUB: &str = env!("CARGO_BIN_EXE_ensemble-hub");
const RUNNER: &str = env!("CARGO_BIN_EXE_ensemble-runner");

/// Port 1 is reserved and nothing listens there.
const NOWHERE: &str = "http://127.0.0.1:1/v1";
/// What has a runner look at its workspace and its hub every so many seconds.
const RESCAN_VARIABLE: &str = "ENSEMBLE_RUNNER_RESCAN_SECONDS";
/// Who a runner's commits are by.
const GIT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Runner"),
    ("GIT_AUTHOR_EMAIL", "runner@example.com"),
    ("GIT_COMMITTER_NAME", "Runner"),
    ("GIT_COMMITTER_EMAIL", "runner@example.com"),
];

/// A scratch project that cleans itself up.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ensemble-it-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The helper team: the team `team` in the scratch computer's teams
    /// directory, so its agents are `team/<agent>`.
    fn team_dir(&self) -> PathBuf {
        self.0
            .with_extension("xdg-config")
            .join("ensemble/runner/teams/team")
    }

    /// Where the project's tasks and runs go: its state directory, under
    /// the scratch's own XDG state directory.
    fn state(&self) -> PathBuf {
        ensemble_core::state::dir_in(
            &self.0.with_extension("xdg-state").join("ensemble"),
            &self.0,
        )
        .unwrap()
    }

    /// This scratch computer's runner file, `runner.yaml`: the runner `desk`,
    /// working in [`Scratch::workspace`].
    fn runner_yaml(&self) -> PathBuf {
        let config = self.0.with_extension("xdg-config").join("ensemble/runner");
        std::fs::create_dir_all(&config).unwrap();
        let path = config.join("runner.yaml");
        if !path.exists() {
            std::fs::write(
                &path,
                format!(
                    "version: 1\nname: desk\naccess: private\nworkspace: \"{}\"\noffers: {{}}\n",
                    self.workspace().display()
                ),
            )
            .unwrap();
        }
        path
    }

    /// The scratch computer's runner's workspace: a folder of git
    /// repositories, each a project.
    fn workspace(&self) -> PathBuf {
        self.0.with_extension("workspace")
    }

    /// A git repository `name` in the workspace, with one commit: a project
    /// the runner works on.
    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.workspace().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        ensemble_agents::worktree::testing::repo(&dir);
        dir.canonicalize().unwrap()
    }

    /// Where the tasks and runs of the workspace's repository `name` go.
    fn state_of(&self, name: &str) -> PathBuf {
        ensemble_core::state::dir_in(
            &self.0.with_extension("xdg-state").join("ensemble"),
            &self.workspace().join(name),
        )
        .unwrap()
    }

    /// This scratch computer's runner folder, `~/.config/ensemble/runner/`.
    fn runner_dir(&self) -> PathBuf {
        self.0.with_extension("xdg-config").join("ensemble/runner")
    }

    /// A path in the project; `@team/<file>` is one in the helper team's
    /// folder, `@runner/<file>` one in the runner's, such as its `.env`, and
    /// `@state/<path>` one in the project's state directory.
    fn place(&self, name: &str) -> PathBuf {
        if let Some(rest) = name.strip_prefix("@runner/") {
            self.runner_dir().join(rest)
        } else if let Some(rest) = name.strip_prefix("@team/") {
            self.team_dir().join(rest)
        } else if let Some(rest) = name.strip_prefix("@state/") {
            self.state().join(rest)
        } else {
            self.0.join(name)
        }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.place(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).expect("write scratch file");
        if name == "@runner/.env" {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        p
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.place(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
    }

    /// What the stand-in CLI saved: `args` or `config`.
    fn seen(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.with_extension("bin").join(name))
            .unwrap_or_else(|e| panic!("read {name}: {e}"))
    }

    /// A stand-in for `opencode`, kept outside the project so it is not one
    /// of the files a run changes. It answers `--version`; otherwise it saves
    /// its arguments (one per line) and its inline config beside itself,
    /// runs `body`, and prints `answer` as OpenCode's JSON events.
    fn opencode(&self, answer: &str, body: &str) -> String {
        let bin = self.0.with_extension("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("opencode");
        let event = serde_json::json!({"type": "text", "part": {"text": answer}}).to_string();
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s\\n' \"$@\" > '{seen}/args'\nprintf '%s' \"$OPENCODE_CONFIG_CONTENT\" > '{seen}/config'\n{body}\nprintf '%s\\n' '{{\"type\":\"step_start\",\"part\":{{}}}}'\nprintf '%s\\n' '{}'\n",
            event.replace('\'', "'\\''"),
            seen = bin.display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_dir_all(self.0.with_extension("bin"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-data"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-config"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("machines"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-state"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("xdg-cache"));
        let _ = std::fs::remove_dir_all(self.workspace());
    }
}

/// The helper team: two OpenCode agents run by `command`: `reviewer` on a provider OpenCode
/// knows, and `coder` on the `openai-local` LLM backend at `backend_url`.
fn config_yaml(command: &str, backend_url: &str) -> String {
    format!(
        r#"
version: 1

llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url: "{backend_url}"
    api_key_env: ENSEMBLE_TEST_LOCAL_KEY
    timeout_seconds: 20

limits: {{}}

gates: {{}}

agents:
  reviewer:
    prompt: "Reviews code and reports findings."
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: {{ type: provider, name: opencode/reviewer-model }}
      tools: [read, glob, grep, list]
      system_prompt: replace
      max_steps: 30
      command: "{command}"
  coder:
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model:
        type: llm_backend
        try:
          - {{ backend: openai-local, name: qwen3-coder, context_window: 32768 }}
        when_none: fail
      tools: [read, edit]
      system_prompt: append
      max_steps: 30
      command: "{command}"
"#
    )
}

/// The executable `program`, run in `scratch` with none of the test variables
/// inherited: a stale one would silently change what is being tested.
fn command(scratch: &Scratch, program: &str) -> Command {
    let mut c = Command::new(program);
    c.current_dir(scratch.path())
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .env("XDG_CONFIG_HOME", scratch.0.with_extension("xdg-config"))
        .env("XDG_DATA_HOME", scratch.0.with_extension("xdg-data"))
        .env_remove("ENSEMBLE_TEST_LOCAL_URL")
        .env_remove("ENSEMBLE_TEST_LOCAL_KEY");
    c
}

/// The runner with only the scratch's directories set, and none of the test
/// variables: as it starts as a service, with a git identity to commit a
/// task's work as, but looking at its workspace and its hub every second, not
/// every half minute.
fn bare_command(scratch: &Scratch) -> Command {
    let mut c = Command::new(RUNNER);
    c.current_dir(scratch.path())
        .env(RESCAN_VARIABLE, "1")
        .envs(GIT_IDENTITY)
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .env("XDG_CONFIG_HOME", scratch.0.with_extension("xdg-config"))
        .env("XDG_DATA_HOME", scratch.0.with_extension("xdg-data"));
    c
}

fn runner_command(scratch: &Scratch) -> Command {
    command(scratch, RUNNER)
}

fn hub_command(scratch: &Scratch) -> Command {
    command(scratch, HUB)
}

fn ensemble_runner(scratch: &Scratch, args: &[&str]) -> std::process::Output {
    runner_command(scratch)
        .args(args)
        .output()
        .expect("run ensemble-runner")
}

fn ensemble_hub(scratch: &Scratch, args: &[&str]) -> std::process::Output {
    hub_command(scratch)
        .args(args)
        .output()
        .expect("run ensemble-hub")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Answer every request with `body` (200, JSON) until the test ends.
fn models_stub(body: &str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let body = body.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(&stream);
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            // Read the body too: closing with it unread resets the connection,
            // and the client can see the reset before the answer.
            let _ = reader.read_exact(&mut vec![0; length]);
            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        }
    });
    url
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

#[test]
fn check_accepts_a_valid_config_and_reports_the_team() {
    let s = Scratch::new("check-ok");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("team: ok (made here)"), "{text}");
    assert!(text.contains("\nready."), "{text}");
    assert!(
        text.contains("team/coder  ->  opencode on team/openai-local / qwen3-coder"),
        "{text}"
    );
    assert!(text.contains("openai-compatible-api"), "{text}");
}

#[test]
fn check_reports_several_problems_at_once() {
    let s = Scratch::new("check-many");
    s.write(
        "@team/config.yaml",
        r#"
version: 9
llm_backends:
  x: { type: openai-compatible-api, base_url: "nope", timeout_seconds: 5 }
limits: {}
gates: {}
agents:
  r:
    max_seconds: 60
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: { type: llm_backend, try: [{ backend: nowhere, name: "", context_window: 8000 }], when_none: fail }
      tools: []
      system_prompt: append
      max_steps: 3
      command: opencode
"#,
    );
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("version"), "{err}");
    assert!(err.contains("llm_backends.x"), "{err}");
    assert!(err.contains("nowhere"), "{err}");
}

#[test]
fn an_api_key_written_into_the_config_is_refused() {
    let s = Scratch::new("inline-key");
    s.write(
        "@team/config.yaml",
        r#"
version: 1
llm_backends:
  x:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:1/v1"
    timeout_seconds: 5
    api_key: "sk-should-never-be-here"
"#,
    );
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success(), "secrets in the config must not load");
    assert!(stderr_of(&out).contains("api_key"), "{}", stderr_of(&out));
}

#[test]
fn with_no_team_check_says_how_to_make_one() {
    let s = Scratch::new("no-org");
    // The project folder holds a config of the old kind; it is not looked at.
    s.write("config.yaml", "version: 2\n");
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success(), "no team means no helper team");
    assert!(
        stderr_of(&out).contains("ensemble-runner team new"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_broken_team_is_named_rather_than_skipped() {
    let s = Scratch::new("no-config");
    s.write("@team/config.yaml", "version: 1\n");
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("team 'team'"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn agents_prints_the_routing_table() {
    let s = Scratch::new("agents");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = ensemble_runner(&s, &["agent", "list"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("team/coder\topencode on team/openai-local\tqwen3-coder"),
        "{text}"
    );
    assert!(
        text.contains("team/reviewer\topencode\topencode/reviewer-model"),
        "{text}"
    );
}

#[test]
fn the_base_url_is_the_one_in_the_config_and_no_variable_changes_it() {
    let s = Scratch::new("url-from-config");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));

    let out = runner_command(&s)
        .args(["team", "check"])
        .env("ENSEMBLE_TEST_LOCAL_URL", "http://10.0.0.9:9999/v1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains(NOWHERE), "{}", stdout_of(&out));
    assert!(!stdout_of(&out).contains("10.0.0.9"), "{}", stdout_of(&out));
}

// ---------------------------------------------------------------------------
// Starting the agent's CLI
// ---------------------------------------------------------------------------

#[test]
fn run_starts_the_agents_cli_with_the_task_and_prints_its_answer() {
    let s = Scratch::new("run");
    let cli = s.opencode("the review is clean", "");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));

    let out = ensemble_runner(
        &s,
        &["agent", "run", "team/reviewer", "review", "this", "code"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("the review is clean"),
        "{}",
        stdout_of(&out)
    );
    // Provenance goes to stderr so stdout stays pipeable.
    assert!(
        stderr_of(&out).contains("team/reviewer via opencode / opencode/reviewer-model"),
        "{}",
        stderr_of(&out)
    );
    let args = s.seen("args");
    assert!(
        args.contains("-m\nopencode/reviewer-model\n--\nreview this code\n"),
        "{args}"
    );
    let config: serde_json::Value = serde_json::from_str(&s.seen("config")).unwrap();
    assert_eq!(
        config["agent"]["ensemble-team-reviewer"]["prompt"],
        "Reviews code and reports findings."
    );
    assert!(config.get("provider").is_none(), "{config}");
}

#[test]
fn an_agent_on_an_llm_backend_gets_its_endpoint_and_the_key_from_dotenv() {
    let s = Scratch::new("run-backend");
    let cli = s.opencode("done", "");
    s.write(
        "@team/config.yaml",
        &config_yaml(&cli, "http://127.0.0.1:8888/v1"),
    );
    s.write("@runner/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");

    let out = ensemble_runner(&s, &["agent", "run", "team/coder", "do it"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        s.seen("args")
            .contains("-m\nensemble-team-openai-local/qwen3-coder\n"),
        "{}",
        s.seen("args")
    );
    let config: serde_json::Value = serde_json::from_str(&s.seen("config")).unwrap();
    let provider = &config["provider"]["ensemble-team-openai-local"];
    assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:8888/v1");
    assert_eq!(provider["options"]["apiKey"], "sk-from-dotenv");
    assert_eq!(provider["models"]["qwen3-coder"]["limit"]["context"], 32768);
}

#[test]
fn show_prints_how_an_agent_starts_with_the_key_hidden() {
    let s = Scratch::new("show");
    s.write(
        "@team/config.yaml",
        &config_yaml("opencode", "http://127.0.0.1:8888/v1"),
    );
    s.write("@runner/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-secret-value\n");

    let out = ensemble_runner(&s, &["agent", "show", "team/coder"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.starts_with("opencode run --standalone --format json --agent ensemble-team-coder -m ensemble-team-openai-local/qwen3-coder -- '<task>'"),
        "{text}"
    );
    assert!(
        text.contains("\"baseURL\": \"http://127.0.0.1:8888/v1\""),
        "{text}"
    );
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("sk-secret-value"), "the key leaked: {text}");

    let out = ensemble_runner(&s, &["agent", "show", "nobody"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("team/coder, team/reviewer"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn files_the_agent_changed_are_reported() {
    let s = Scratch::new("changes");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.write("a.txt", "foo\n");

    let out = ensemble_runner(&s, &["agent", "run", "team/coder", "rename foo to bar"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(s.read("a.txt"), "bar\n");
}

#[test]
fn health_checks_the_cli_and_that_the_backend_serves_the_model() {
    let s = Scratch::new("health");
    let cli = s.opencode("", "");
    let serving = models_stub(r#"{"object":"list","data":[{"id":"qwen3-coder"}]}"#);
    s.write("@team/config.yaml", &config_yaml(&cli, &serving));
    let out = ensemble_runner(&s, &["agent", "health"]);
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    let text = stdout_of(&out);
    assert!(text.contains(&format!("opencode ({cli}): ok")), "{text}");
    assert!(text.contains("openai-local: ok"), "{text}");

    let other = models_stub(r#"{"object":"list","data":[{"id":"something-else"}]}"#);
    s.write("@team/config.yaml", &config_yaml(&cli, &other));
    let out = ensemble_runner(&s, &["agent", "health"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(text.contains("openai-local: FAILED"), "{text}");
    assert!(
        text.contains("does not serve 'qwen3-coder'. It serves: something-else."),
        "{text}"
    );

    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let out = ensemble_runner(&s, &["agent", "health"]);
    assert!(!out.status.success());
    assert!(
        stdout_of(&out).contains("could not reach the endpoint"),
        "{}",
        stdout_of(&out)
    );
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

#[test]
fn an_invalid_team_stops_the_runner_before_it_runs_anything() {
    // Better to fail at start, where it is seen, than to start every task and
    // fail it.
    let s = Scratch::new("runner-bad-config");
    s.write("@team/config.yaml", "version: 1\nbackends: {}\n");
    s.runner_yaml();
    let out = bare_command(&s).args(["serve"]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("backend"), "{}", stderr_of(&out));
}

#[test]
fn team_new_writes_a_sample_helper_team_that_checks_and_never_overwrites_by_accident() {
    let s = Scratch::new("init");
    let out = ensemble_runner(&s, &["team", "new", "team"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let written = std::fs::read_to_string(s.team_dir().join("config.yaml")).unwrap();
    assert!(written.contains("type: opencode"), "{written}");
    assert!(written.contains("\nagents:\n"), "{written}");
    assert!(!written.contains("teams:"), "{written}");
    assert!(
        !s.team_dir().join(".env").exists() && !s.team_dir().join(".gitignore").exists(),
        "a helper team holds no secrets, so it needs no .env"
    );
    assert!(
        !s.path().join(".ensemble").exists(),
        "nothing is written in the project"
    );

    let check = ensemble_runner(&s, &["team", "check"]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    let text = stdout_of(&check);
    assert!(text.contains("team/coder  ->"), "{text}");

    s.write("@team/config.yaml", "mine");
    let again = ensemble_runner(&s, &["team", "new", "team"]);
    assert!(!again.status.success());
    assert!(
        stderr_of(&again).contains("already"),
        "{}",
        stderr_of(&again)
    );
    assert_eq!(
        std::fs::read_to_string(s.team_dir().join("config.yaml")).unwrap(),
        "mine"
    );

    let printed = ensemble_runner(&s, &["team", "new", "--stdout"]);
    assert_eq!(stdout_of(&printed), written);
}

#[test]
fn variables_come_from_the_env_file_unless_the_environment_sets_them() {
    let s = Scratch::new("dotenv");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    s.write("@runner/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("authenticated"), "{text}");
    assert!(!text.contains("sk-from-dotenv"), "{text}");

    // --env-file is read instead of the runner's .env.
    let other = s.write("keys.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-other\n");
    let out = runner_command(&s)
        .arg("--env-file")
        .arg(&other)
        .args(["agent", "show", "team/coder"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));

    s.write("@runner/.env", "not a line\n");
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("line 1: expected NAME=value"),
        "{}",
        stderr_of(&out)
    );
    let out = runner_command(&s)
        .arg("--env-file")
        .arg(&other)
        .args(["team", "check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));

    // One that others can read is still read, with a note to make it private.
    let dotenv = s.write("@runner/.env", "ENSEMBLE_TEST_LOCAL_KEY=sk-from-dotenv\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dotenv, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("chmod 600"), "{}", stderr_of(&out));
}

#[test]
fn check_says_which_tasks_on_the_hub_this_runner_cannot_run() {
    let s = Scratch::new("check-tasks");
    let _hub = with_hub(&s);
    s.repo("shop");
    let gated = config_yaml("opencode", NOWHERE).replace(
        "gates: {}",
        "gates:\n  test: { command: \"true\", timeout_seconds: 5 }",
    );
    s.write("@team/config.yaml", &gated);
    tasks_of(&s, "shop")
        .add(&ensemble_tasks::tasks::Ticket {
            title: "Ok".into(),
            agent: "team/coder".into(),
            gate: Some(ensemble_tasks::tasks::GateRef {
                name: "test".into(),
                attempts: 1,
            }),
            text: "do it".into(),
            ..Default::default()
        })
        .unwrap();
    add_task(&s, "shop", "Look", "team/reviewer", "look");
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("tasks of shop: 2 on the hub ws://")
            && stdout_of(&out).contains("2 unfinished"),
        "{}",
        stdout_of(&out)
    );

    // The gate is gone, and the reviewer is another runner's now.
    s.write(
        "@team/config.yaml",
        &config_yaml("opencode", NOWHERE).replace("  reviewer:", "  critic:"),
    );
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("0001-ok: gate 'test' is not configured"),
        "{text}"
    );
    assert!(
        text.contains("0002-look: agent 'team/reviewer' is not one of this runner's"),
        "{text}"
    );
}

#[test]
fn a_light_agent_is_one_request_to_its_backend() {
    let s = Scratch::new("llm-agent");
    let url = models_stub(
        r#"{"data":[{"id":"small"},{"id":"qwen3-coder"}],"choices":[{"message":{"content":"three bullets"},"finish_reason":"stop"}]}"#,
    );
    let config = format!(
        "{}  summariser:\n    prompt: You summarise.\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: direct\n      try: [{{ backend: openai-local, model: small }}]\n      when_none: fail\n      max_tokens: 300\n",
        config_yaml("opencode", &url)
    );
    s.write("@team/config.yaml", &config);

    let out = ensemble_runner(&s, &["agent", "run", "team/summariser", "sum", "it", "up"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).starts_with("three bullets"),
        "{}",
        stdout_of(&out)
    );

    let out = ensemble_runner(&s, &["agent", "show", "team/summariser"]);
    let text = stdout_of(&out);
    assert!(
        text.starts_with(&format!("POST {url}/chat/completions\n")),
        "{text}"
    );
    assert!(
        text.contains("\"max_tokens\": 300") && text.contains("You summarise."),
        "{text}"
    );

    let check = stdout_of(&ensemble_runner(&s, &["team", "check"]));
    assert!(
        check.contains("team/summariser  ->  direct on team/openai-local / small"),
        "{check}"
    );
}

// ---------------------------------------------------------------------------
// Getting started
// ---------------------------------------------------------------------------

/// A directory of stand-ins for the client and agent CLIs. Each answers
/// `--version`, and otherwise saves its arguments, one per line, to
/// `<name>.args` beside itself.
fn stub_clis(scratch: &Scratch, names: &[&str]) -> PathBuf {
    let bin = scratch.0.with_extension("clis");
    std::fs::create_dir_all(&bin).unwrap();
    for name in names {
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s\\n' \"$@\" > '{}/{name}.args'\n",
                bin.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// The runner, with only `bin` on its PATH, so no real CLI is ever started.
fn with_path(scratch: &Scratch, bin: &Path) -> Command {
    let mut c = Command::new(RUNNER);
    c.current_dir(scratch.path())
        .env("PATH", bin)
        .env("XDG_STATE_HOME", scratch.0.with_extension("xdg-state"))
        .env("XDG_CONFIG_HOME", scratch.0.with_extension("xdg-config"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"))
        .env("XDG_DATA_HOME", scratch.0.with_extension("xdg-data"))
        .env("HOME", scratch.0.with_extension("home"));
    c
}

#[test]
fn setup_writes_the_runner_and_checks_its_teams() {
    let s = Scratch::new("setup");
    s.write(
        "@team/config.yaml",
        "version: 1\nllm_backends: {}\nlimits: {}\ngates: {}\nagents:\n  reviewer:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: { type: unlimited }\n    adapter: { type: claude-code, model: sonnet, tools: [Read], permission_mode: dontAsk, max_turns: 5, command: claude }\n",
    );
    let bin = stub_clis(&s, &["claude", "opencode"]);
    let out = with_path(&s, &bin)
        .args(["setup", "--yes", "--name", "desk", "--workspace", "~/code"])
        .output()
        .unwrap();
    let said = stdout_of(&out);
    assert!(out.status.success(), "{said}\n{}", stderr_of(&out));
    assert!(
        said.contains("Found: claude yes, opencode yes, codex no"),
        "{said}"
    );
    assert!(
        said.contains("holds no git repository")
            && said.contains("Done.")
            && said.contains("ensemble-runner serve"),
        "{said}"
    );
    let runner =
        s.0.with_extension("xdg-config")
            .join("ensemble/runner/runner.yaml");
    let written = std::fs::read_to_string(&runner).unwrap();
    assert!(
        written.contains("name: \"desk\"")
            && written.contains("workspace: \"~/code\"")
            && written.contains("version: 1"),
        "{written}"
    );
    assert!(
        !bin.join("claude.args").exists(),
        "nothing is registered with an agent CLI"
    );

    // A second run keeps the runner that is there.
    let again = with_path(&s, &bin)
        .args(["setup", "--yes"])
        .output()
        .unwrap();
    assert!(
        stdout_of(&again).contains("keeping it"),
        "{}",
        stdout_of(&again)
    );
    assert_eq!(std::fs::read_to_string(&runner).unwrap(), written);
}

#[test]
fn setup_without_any_agent_cli_says_so_and_writes_the_runner() {
    let s = Scratch::new("setup-nothing");
    let bin = stub_clis(&s, &[]);
    let out = with_path(&s, &bin)
        .args(["setup", "--yes", "--name", "desk"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("No agent CLI is on PATH"),
        "{}",
        stdout_of(&out)
    );
    let written = std::fs::read_to_string(
        s.0.with_extension("xdg-config")
            .join("ensemble/runner/runner.yaml"),
    )
    .unwrap();
    assert!(written.contains("workspace: \"~/ensemble\""), "{written}");
}

#[test]
fn doctor_says_what_is_wrong_and_how_to_fix_it() {
    let s = Scratch::new("doctor");
    s.write(
        "@team/config.yaml",
        &config_yaml("/nonexistent/opencode", NOWHERE),
    );
    let out = ensemble_runner(&s, &["doctor"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("problem: opencode (/nonexistent/opencode)"),
        "{text}"
    );
    assert!(text.contains("fix: install `opencode`"), "{text}");
    assert!(
        text.contains("ensemble-runner agent remove team/coder"),
        "{text}"
    );
    assert!(text.contains("problem: team/openai-local"), "{text}");
    assert!(text.contains("fix: start the model server"), "{text}");

    let s = Scratch::new("doctor-ok");
    s.write("@team/config.yaml", &config_yaml("true", NOWHERE));
    let url = models_stub(r#"{"data":[{"id":"qwen3-coder"}]}"#);
    s.write("@team/config.yaml", &config_yaml("true", &url));
    let out = ensemble_runner(&s, &["doctor"]);
    let text = stdout_of(&out);
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("problem"), "{text}");
}

#[test]
fn add_agent_and_remove_agent_edit_the_config_and_keep_it_loading() {
    let s = Scratch::new("add-agent");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble_runner(
        &s,
        &[
            "agent",
            "add",
            "team/tester",
            "--adapter",
            "claude-code",
            "--prompt",
            "Tests it.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let agents = ensemble_runner(&s, &["agent", "list"]);
    assert!(
        stdout_of(&agents).contains("team/tester\tclaude-code\tsonnet"),
        "{}",
        stdout_of(&agents)
    );

    let dup = ensemble_runner(
        &s,
        &["agent", "add", "team/tester", "--adapter", "claude-code"],
    );
    assert!(!dup.status.success());
    assert!(
        stderr_of(&dup).contains("already has"),
        "{}",
        stderr_of(&dup)
    );

    let bad = ensemble_runner(&s, &["agent", "add", "team/x", "--adapter", "llm"]);
    assert!(!bad.status.success(), "`llm` is no adapter any more");

    let gone = ensemble_runner(&s, &["agent", "remove", "team/tester"]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(!stdout_of(&ensemble_runner(&s, &["agent", "list"])).contains("tester"));
    let missing = ensemble_runner(&s, &["agent", "remove", "team/tester"]);
    assert!(!missing.status.success());
}

// ---------------------------------------------------------------------------
// Remote helper teams
// ---------------------------------------------------------------------------

/// A git repository holding `config.yaml`, as a `file://` URL and its commit.
fn origin(scratch: &Scratch, config: &str) -> (String, String) {
    let dir = scratch.0.with_extension("origin");
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    if !dir.join(".git").exists() {
        git(&["init", "--quiet", "-b", "main"]);
    }
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "c"]);
    (
        format!("file://{}", dir.display()),
        git(&["rev-parse", "HEAD"]),
    )
}

/// What `pulled.json` of the team `name` pins it to.
fn pinned_commit(scratch: &Scratch, name: &str) -> String {
    let file = scratch
        .0
        .with_extension("xdg-config")
        .join("ensemble/runner/teams")
        .join(name)
        .join("pulled.json");
    let pulled: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    pulled["commit"].as_str().unwrap().to_string()
}

#[test]
fn a_pulled_team_is_pinned_shown_trusted_and_then_used() {
    let s = Scratch::new("remote");
    let (url, sha) = origin(&s, &config_yaml("true", NOWHERE));

    // Without a terminal and without --yes there is nobody to ask: a refusal,
    // and nothing is kept.
    let refused = runner_command(&s)
        .args(["team", "pull", &url, "--name", "web"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("--yes"),
        "{}",
        stderr_of(&refused)
    );
    assert!(!stdout_of(&ensemble_runner(&s, &["team", "list"])).contains("web"));

    let pulled = runner_command(&s)
        .args([
            "team",
            "pull",
            &format!("{url}@main"),
            "--name",
            "web",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(pulled.status.success(), "{}", stderr_of(&pulled));
    let said = stdout_of(&pulled);
    assert!(said.contains("coder: runs `true`"), "{said}");
    assert!(
        said.contains(&format!("pulled {url}#{sha} as the team 'web'")),
        "{said}"
    );
    assert_eq!(pinned_commit(&s, "web"), sha);

    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains(&format!("web: ok (pulled from {url}#{sha})")),
        "{text}"
    );
    assert!(text.contains("web/coder"), "{text}");
    let again = runner_command(&s)
        .args(["team", "pull", &url, "--name", "web", "--yes"])
        .output()
        .unwrap();
    assert!(!again.status.success(), "a name is taken once");

    // A pin nobody trusted is not used, and says how to trust it.
    let (_, other) = origin(&s, &config_yaml("opencode", NOWHERE));
    let teams =
        s.0.with_extension("xdg-config")
            .join("ensemble/runner/teams");
    std::fs::create_dir_all(teams.join("sneaky")).unwrap();
    std::fs::write(
        teams.join("sneaky/pulled.json"),
        serde_json::json!({"url": url, "commit": other}).to_string(),
    )
    .unwrap();
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(!out.status.success(), "an untrusted commit is not used");
    assert!(
        stderr_of(&out).contains("team 'sneaky'")
            && stderr_of(&out).contains("ensemble-runner team pull"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn team_update_shows_what_changed_and_moves_only_once_trusted() {
    let s = Scratch::new("remote-update");
    let (url, old) = origin(&s, &config_yaml("true", NOWHERE));
    let pulled = runner_command(&s)
        .args(["team", "pull", &url, "--name", "web", "--yes"])
        .output()
        .unwrap();
    assert!(pulled.status.success(), "{}", stderr_of(&pulled));
    let (_, new) = origin(&s, &config_yaml("opencode", NOWHERE));

    let refused = runner_command(&s)
        .args(["team", "update", "web", "--ref", "main"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success(), "nobody trusted the new commit");
    assert!(
        stdout_of(&refused).contains("command"),
        "what changed is shown: {}",
        stdout_of(&refused)
    );
    assert_eq!(pinned_commit(&s, "web"), old, "the pin did not move");

    let moved = runner_command(&s)
        .args(["team", "update", "web", "--ref", "main", "--yes"])
        .output()
        .unwrap();
    assert!(moved.status.success(), "{}", stderr_of(&moved));
    assert!(
        stdout_of(&moved).contains(&format!("'web' is at {url}#{new} now")),
        "{}",
        stdout_of(&moved)
    );
    assert_eq!(pinned_commit(&s, "web"), new);
    let again = ensemble_runner(&s, &["team", "update", "web", "--ref", "main"]);
    assert!(
        stdout_of(&again).contains("already"),
        "{}",
        stdout_of(&again)
    );

    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let made = ensemble_runner(&s, &["team", "update", "team"]);
    assert!(!made.status.success());
    assert!(
        stderr_of(&made).contains("was not pulled"),
        "{}",
        stderr_of(&made)
    );
}

// ---------------------------------------------------------------------------
// The runner's tasks
// ---------------------------------------------------------------------------

/// Wait up to `seconds` for `done` to be true.
fn wait_for(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    done()
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
}

/// Call the tool `name` of the hub's MCP at `address`, with `token` as the
/// API key, as a client agent does: the text it answers.
fn mcp_tool(address: &str, token: &str, name: &str, arguments: serde_json::Value) -> String {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let mut resp = agent
        .post(format!("http://{address}/mcp"))
        .header("content-type", "application/json")
        .header("authorization", &format!("Bearer {token}"))
        .send(call.to_string())
        .unwrap();
    let answer: serde_json::Value =
        serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap();
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text: {answer}"))
        .to_string()
}

#[test]
fn the_runner_works_on_the_tasks_of_each_repository_in_its_workspace_and_stops_on_sigterm() {
    let s = Scratch::new("serve");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let shop = s.repo("shop");
    std::fs::write(shop.join("a.txt"), "foo\n").unwrap();
    // Not a repository, so not a project.
    std::fs::create_dir_all(s.workspace().join("notes")).unwrap();
    let (_hub, address, token) = with_hub(&s);
    let mut runner = start_runner(&s);
    // A client agent makes the task, on the hub alone.
    let said = mcp_tool(
        &address,
        &token,
        "submit",
        serde_json::json!({
            "project": "shop", "title": "Rename foo", "task": "rename foo to bar",
            "agent": "team/coder",
        }),
    );
    assert!(said.contains("0001-rename-foo"), "{said}");
    assert!(
        wait_for(30, || outcome_of(&s, "shop", "0001-rename-foo").is_some()),
        "the runner did not run the task"
    );
    let outcome = outcome_of(&s, "shop", "0001-rename-foo").unwrap();
    assert_eq!(
        outcome.status,
        ensemble_tasks::tasks::Status::Done,
        "{}",
        outcome.text
    );
    assert_eq!(
        std::fs::read_to_string(shop.join("a.txt")).unwrap(),
        "bar\n"
    );
    assert!(
        s.state_of("shop")
            .join("runs/0001-rename-foo/log.txt")
            .is_file(),
        "its run is kept in the repository's state directory"
    );
    assert!(
        !s.workspace().join("notes/.ensemble").exists(),
        "a folder that is no repository is left alone"
    );
    // What it reported is shared, for the vault to learn from.
    let shares = share_files(&hub_dir(&s));
    assert!(
        shares.iter().any(|f| f.contains("client: task")
            && f.contains("title: Rename foo")
            && f.contains("Its report:\nrenamed it")),
        "{shares:?}"
    );
    let status = ensemble_runner(&s, &["status"]);
    assert!(
        stdout_of(&status).contains("projects: shop") && stdout_of(&status).contains("running"),
        "{}",
        stdout_of(&status)
    );

    // One runner per computer: a second one refuses.
    let second = bare_command(&s)
        .args(["serve"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        stderr_of(&second).contains("already runs the runner"),
        "{}",
        stderr_of(&second)
    );

    sigterm(&runner);
    let status = runner.wait().unwrap();
    assert!(status.success(), "the runner ends cleanly on SIGTERM");
    assert!(
        !s.state_of("shop").join("worker.lock").exists(),
        "the repository's lock is released"
    );
}

#[test]
fn a_task_stopped_in_the_middle_is_run_again_when_the_runner_starts_again() {
    let s = Scratch::new("serve-resume");
    // The first run is slow and is stopped; the marker says it had started, so
    // the run after it answers at once.
    let marker = s.0.with_extension("bin").join("started");
    let body = format!(
        "if [ -e '{0}' ]; then :; else touch '{0}'; sleep 60; fi",
        marker.display()
    );
    let cli = s.opencode("done", &body);
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    s.repo("shop");
    let _hub = with_hub(&s);
    let mut first = start_runner(&s);
    add_task(&s, "shop", "Slow", "team/coder", "take your time");
    assert!(wait_for(30, || marker.exists()));
    assert_eq!(
        task_of(&s, "shop", "0001-slow").running_on.as_deref(),
        Some("desk")
    );
    sigterm(&first);
    first.wait().unwrap();
    assert!(
        outcome_of(&s, "shop", "0001-slow").is_none(),
        "a stopped task has no result"
    );
    let task = task_of(&s, "shop", "0001-slow");
    assert_eq!(
        (task.interrupted_on.as_deref(), task.pinned_to.as_deref()),
        (Some("desk"), Some("desk")),
        "its work is in desk's repository"
    );

    // The runner finds the repository again, and runs it again when it starts.
    let _second = start_runner(&s);
    assert!(
        wait_for(30, || outcome_of(&s, "shop", "0001-slow").is_some()),
        "the runner did not run it again"
    );
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[test]
fn every_run_leaves_a_record_that_stats_sums_up() {
    let s = Scratch::new("stats");
    let cli = s.opencode("fine", "");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let none = ensemble_runner(&s, &["agent", "stats"]);
    assert!(
        stdout_of(&none).contains("No runs recorded"),
        "{}",
        stdout_of(&none)
    );

    for _ in 0..2 {
        let out = ensemble_runner(&s, &["agent", "run", "team/reviewer", "look"]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    // A run that cannot start is a failure on the record.
    s.write(
        "@team/config.yaml",
        &config_yaml("/nonexistent/opencode", NOWHERE),
    );
    let out = ensemble_runner(&s, &["agent", "run", "team/reviewer", "look"]);
    assert!(!out.status.success());

    let table = ensemble_runner(&s, &["agent", "stats"]);
    let text = stdout_of(&table);
    let row = text
        .lines()
        .find(|l| l.starts_with("team/reviewer"))
        .unwrap_or_else(|| panic!("{text}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!((cells[1], cells[2]), ("3", "1"), "3 runs, 1 failure: {row}");

    let json = ensemble_runner(&s, &["agent", "stats", "--json", "--since", "1h"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(&json)).unwrap();
    assert_eq!(parsed[0]["agent"], "team/reviewer");
    assert_eq!(parsed[0]["runs"], 3);

    let bad = ensemble_runner(&s, &["agent", "stats", "--since", "soon"]);
    assert!(!bad.status.success());
    assert!(stderr_of(&bad).contains("duration"), "{}", stderr_of(&bad));
}

// ---------------------------------------------------------------------------
// A helper team that works on its own
// ---------------------------------------------------------------------------

#[test]
fn check_warns_about_what_agents_may_do_without_failing() {
    let s = Scratch::new("check-warn");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble_runner(&s, &["team", "check"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("warnings:"), "{text}");
    assert!(
        text.contains("coder can change files directly in your working folder"),
        "{text}"
    );
    assert!(text.contains("isolation: worktree"), "{text}");
    assert!(!text.contains("reviewer can change"), "{text}");
}

#[test]
fn show_prints_how_a_codex_agent_starts() {
    let s = Scratch::new("show-codex");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let out = ensemble_runner(
        &s,
        &[
            "agent",
            "add",
            "team/fixer",
            "--adapter",
            "codex",
            "--prompt",
            "Fixes it.",
            "--writes",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let shown = ensemble_runner(&s, &["agent", "show", "team/fixer"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let text = stdout_of(&shown);
    assert!(
        text.starts_with("codex exec --json --skip-git-repo-check --sandbox workspace-write --model gpt-5-codex -- "),
        "{text}"
    );
    assert!(text.contains("Fixes it."), "{text}");
    let agents = stdout_of(&ensemble_runner(&s, &["agent", "list"]));
    assert!(agents.contains("fixer\tcodex\tgpt-5-codex"), "{agents}");
}

#[test]
fn a_light_agent_on_an_anthropic_backend_shows_a_messages_request() {
    let s = Scratch::new("show-anthropic");
    let v1 = format!(
        "{}\n",
        config_yaml("opencode", NOWHERE).replacen(
            "llm_backends:\n",
            "llm_backends:\n  claude-api:\n    type: anthropic-api\n    base_url: \"https://api.anthropic.com\"\n    api_key_env: ENSEMBLE_TEST_ANTHROPIC_KEY\n    timeout_seconds: 30\n",
            1,
        )
    );
    s.write("@team/config.yaml", &v1);
    let out = ensemble_runner(
        &s,
        &[
            "agent",
            "add",
            "team/helper",
            "--adapter",
            "direct",
            "--backend",
            "claude-api",
            "--model",
            "claude-haiku-4-5",
            "--prompt",
            "You help.",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let shown = ensemble_runner(&s, &["agent", "show", "team/helper"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let text = stdout_of(&shown);
    assert!(
        text.starts_with("POST https://api.anthropic.com/v1/messages"),
        "{text}"
    );
    assert!(text.contains("\"system\": \"You help.\""), "{text}");
    let check = stdout_of(&ensemble_runner(&s, &["team", "check"]));
    assert!(check.contains("claude-api  [anthropic-api]"), "{check}");
}

/// Run git in `dir`, as T, and what it printed.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn show_lists_an_agents_models_in_order_and_probe_says_which_a_run_would_use() {
    let s = Scratch::new("show-probe");
    let central = models_stub(r#"{"data":[{"id":"big"}]}"#);
    s.write(
        "@team/config.yaml",
        &format!(
            "version: 1\nllm_backends:\n  home: {{ type: openai-compatible-api, base_url: \"{NOWHERE}\", timeout_seconds: 5 }}\n  central: {{ type: openai-compatible-api, base_url: \"{central}\", timeout_seconds: 5 }}\nlimits: {{}}\ngates: {{}}\nagents:\n  summariser:\n    max_seconds: 30\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try:\n        - {{ backend: home, model: small }}\n        - {{ backend: central, model: big }}\n      when_none: wait\n      max_tokens: 10\n"
        ),
    );
    let out = ensemble_runner(&s, &["agent", "show", "team/summariser"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("models, in order (the first is shown above; when none works: wait):"),
        "{text}"
    );
    assert!(
        text.contains("  1. team/home / small\n  2. team/central / big\n"),
        "{text}"
    );
    assert!(!text.contains("a run now"), "{text}");

    let probed = ensemble_runner(&s, &["agent", "show", "team/summariser", "--probe"]);
    assert!(probed.status.success(), "{}", stderr_of(&probed));
    assert!(
        stdout_of(&probed).contains("a run now would use 2."),
        "{}",
        stdout_of(&probed)
    );

    let health = ensemble_runner(&s, &["agent", "health"]);
    assert!(!health.status.success(), "home is down");
    let said = stdout_of(&health);
    assert!(
        said.contains("team/home: FAILED") && said.contains("team/central: ok"),
        "{said}"
    );
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

#[test]
fn hub_init_writes_a_hub_and_its_vault_and_check_says_which_model_distills() {
    let scratch = Scratch::new("hub-init");
    let hub = hub_dir(&scratch);
    let out = ensemble_hub(&scratch, &["check"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("ensemble-hub init"),
        "{}",
        stderr_of(&out)
    );

    let out = ensemble_hub(&scratch, &["init"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(hub.join("hub.yaml").exists());
    assert!(
        hub.join("vault/.git").exists(),
        "the vault is a git repository"
    );
    let again = ensemble_hub(&scratch, &["init"]);
    assert!(
        !again.status.success(),
        "it does not overwrite without --force"
    );

    let url = models_stub(r#"{"data":[{"id":"small"}]}"#);
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let (head, tail) = text.split_once("llm_backends:").unwrap();
    let (_, rest) = tail.split_once("# ---").unwrap();
    let rest = rest.split_once("knowledge:").unwrap().1;
    // The sample's embedding model is on a backend this test takes out.
    let rest = rest
        .split_once("  recall:")
        .unwrap()
        .1
        .replace("backend: central", "backend: up");
    std::fs::write(
        hub.join("hub.yaml"),
        format!(
            "{head}llm_backends:\n  down: {{ type: openai-compatible-api, base_url: \"{NOWHERE}\", timeout_seconds: 5 }}\n  up: {{ type: openai-compatible-api, base_url: \"{url}\", timeout_seconds: 5 }}\n\nknowledge:\n  vault: vault\n  distill:\n    try:\n      - {{ backend: down, model: small }}\n      - {{ backend: up, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall:{rest}"
        ),
    )
    .unwrap();
    let out = ensemble_hub(&scratch, &["check"]);
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stdout_of(&out).contains("distilled by small on up"),
        "{}",
        stdout_of(&out)
    );
    // The stub serves no embeddings: recall goes on by words, and that is no
    // failure.
    assert!(
        stdout_of(&out)
            .contains("recall: by words alone for now: nomic-embed-text on up cannot embed"),
        "{}",
        stdout_of(&out)
    );
}

/// The files under the vault's `shares/`.
fn share_files(hub: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![hub.join("vault/shares")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                found.push(std::fs::read_to_string(path).unwrap());
            }
        }
    }
    found
}

/// A model that answers every chat request with these items.
fn items_stub(items: &str) -> String {
    let chat = serde_json::json!({
        "choices": [{"message": {"content": items}, "finish_reason": "stop"}]
    });
    models_stub(&chat.to_string())
}

/// Point the scratch hub's distilling models at `url`, the first of them.
fn distill_on(hub: &Path, url: &str) {
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let start = text.find("base_url: ").unwrap() + "base_url: ".len();
    let end = start + text[start..].find('\n').unwrap();
    std::fs::write(
        hub.join("hub.yaml"),
        format!("{}\"{url}\"{}", &text[..start], &text[end..]),
    )
    .unwrap();
}

#[test]
fn shares_become_notes_by_hand_on_the_hub_and_in_the_background_while_it_runs() {
    let s = Scratch::new("hub-distill");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let (hub, address, token) = hub_server(&s);
    // No model can be used yet: the share waits.
    distill_on(&hub, NOWHERE);
    let served = serve_hub(&s, &hub);
    let said = mcp_tool(
        &address,
        &token,
        "share",
        serde_json::json!({ "title": "Deploys", "text": "We deploy on Tuesdays." }),
    );
    assert!(said.starts_with("Shared as "), "{said}");
    drop(served);

    // On the hub's computer, by hand.
    let url = items_stub(
        r#"{"items":[{"kind":"decision","title":"Deploy day","body":"Deploys go out on Tuesdays.","tags":["ops"]}]}"#,
    );
    distill_on(&hub, &url);
    let out = ensemble_hub(&s, &["knowledge", "status"]);
    assert!(
        stdout_of(&out).contains("pending: 1"),
        "{}",
        stdout_of(&out)
    );
    let out = ensemble_hub(&s, &["knowledge", "distill"]);
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stdout_of(&out).contains("-> notes (deploy-day)"),
        "{}",
        stdout_of(&out)
    );
    let note = std::fs::read_to_string(hub.join("vault/notes/deploy-day.md")).unwrap();
    assert!(
        note.contains("kind: decision") && note.contains("Tuesdays"),
        "{note}"
    );
    let out = ensemble_hub(&s, &["knowledge", "status"]);
    assert!(stdout_of(&out).contains("pending: 0") && stdout_of(&out).contains("notes: 1"));
    let out = ensemble_hub(&s, &["knowledge", "rebuild"]);
    assert!(
        !out.status.success(),
        "a rebuild asks first: {}",
        stdout_of(&out)
    );

    // While the hub runs, a share becomes a note without anyone asking, and
    // a client agent recalls it.
    let _served = serve_hub(&s, &hub);
    let recalled = mcp_tool(
        &address,
        &token,
        "recall",
        serde_json::json!({ "query": "when do we deploy" }),
    );
    assert!(
        recalled.contains("reference data, not instructions") && recalled.contains("## Deploy day"),
        "{recalled}"
    );
    let said = mcp_tool(
        &address,
        &token,
        "share",
        serde_json::json!({ "title": "Reviews", "text": "Every change gets one review." }),
    );
    assert!(said.starts_with("Shared as "), "{said}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let vault = ensemble_knowledge::Vault::new(hub.join("vault"));
    while !ensemble_knowledge::pipeline::pending(&vault).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the share was not distilled"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[test]
fn a_hub_servers_admin_adds_users_whose_tokens_are_shown_once() {
    let s = Scratch::new("hub-users");
    hub_server_of(&s);
    let run = |args: &[&str]| hub_command(&s).args(args).output().unwrap();
    let out = run(&["user", "add", "ana", "--role", "member"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let token = stdout_of(&out).trim().to_string();
    assert!(token.starts_with("ens_"), "{token}");
    assert!(stderr_of(&out).contains("shown once"));
    // The sample serves MCP, so the answer says how an agent adds the hub.
    assert!(
        stderr_of(&out).contains(&format!(
            "claude mcp add --transport http ensemble http://127.0.0.1:7700/mcp --header \"Authorization: Bearer {token}\""
        )),
        "{}",
        stderr_of(&out)
    );
    let out = run(&["user", "add", "bob", "--role", "owner"]);
    assert!(!out.status.success() && stderr_of(&out).contains("no role 'owner'"));
    let out = run(&["user", "list"]);
    assert!(stdout_of(&out).contains("ana") && stdout_of(&out).contains("1 token(s)"));
    assert!(!stdout_of(&out).contains(&token));
    let out = run(&["user", "revoke", "ana"]);
    assert!(
        stdout_of(&out).contains("revoked 1 token(s)"),
        "{}",
        stdout_of(&out)
    );
}

/// A hub server made by `ensemble-hub init`, listening on a free loopback port, with
/// `ana` a member. Returns the hub, its address and ana's token.
/// A hub server made by `ensemble-hub init --server`, beside the project.
/// The scratch computer's hub folder, `~/.config/ensemble/hub/`.
fn hub_dir(scratch: &Scratch) -> PathBuf {
    scratch.0.with_extension("xdg-config").join("ensemble/hub")
}

/// A hub made by `ensemble-hub init` on the scratch computer.
fn hub_server_of(scratch: &Scratch) -> PathBuf {
    let out = hub_command(scratch).args(["init"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    // No test reaches a hosted model: the sample's Anthropic backend goes.
    let hub = hub_dir(scratch);
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let start = text.find("  claude:\n").unwrap();
    let end = start + text[start..].find("\n\n").unwrap() + 1;
    let text = format!("{}{}", &text[..start], &text[end..]).replace(
        "      - backend: claude\n        model: claude-haiku-4-5\n",
        "",
    );
    std::fs::write(hub.join("hub.yaml"), text).unwrap();
    hub
}

/// The scratch computer's runner, naming the hub at `url`.
fn runner_names_hub(scratch: &Scratch, url: &str) {
    let config = scratch
        .0
        .with_extension("xdg-config")
        .join("ensemble/runner");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("runner.yaml"),
        format!(
            "version: 1\nname: desk\naccess: private\nworkspace: \"{}\"\nhub: \"{url}\"\noffers: {{}}\n",
            scratch.workspace().display()
        ),
    )
    .unwrap();
}

/// The scratch computer's runner, naming the hub at `url` and signed in to
/// it with `token`.
fn runner_with_hub(scratch: &Scratch, url: &str, token: &str) {
    runner_names_hub(scratch, url);
    let out = runner_command(scratch)
        .args(["login", url, "--token", token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
}

/// A port for a hub to listen on, free now. Taken below the range the system
/// gives out for outgoing connections, so that no test's connection takes it
/// before the hub binds it, and counted up per process, so that no two tests
/// pick the same one.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 400) as u16 * 25;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 10_000;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

fn hub_server(scratch: &Scratch) -> (PathBuf, String, String) {
    let hub = hub_server_of(scratch);
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    std::fs::write(
        hub.join("hub.yaml"),
        text.replace("127.0.0.1:7700", &address),
    )
    .unwrap();
    let out = hub_command(scratch)
        .args(["user", "add", "ana", "--role", "member"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    (hub, address, stdout_of(&out).trim().to_string())
}

/// The scratch computer with a hub of its own, running until dropped, which
/// its runner names and is signed in to as ana: where its tasks are kept.
/// Its address, and ana's token.
fn with_hub(scratch: &Scratch) -> (Served, String, String) {
    let (hub, address, token) = hub_server(scratch);
    let served = serve_hub(scratch, &hub);
    runner_with_hub(scratch, &format!("ws://{address}"), &token);
    (served, address, token)
}

/// The tasks of ana's `project` on the scratch computer's hub.
fn tasks_of(scratch: &Scratch, project: &str) -> ensemble_hub::board::DbBoard {
    ensemble_hub::board::DbBoard::new(&hub_dir(scratch), "ana", project)
}

/// Keep a task of ana's `project` on the hub, for `agent`, as a client
/// agent's `submit` does: its id.
fn add_task(scratch: &Scratch, project: &str, title: &str, agent: &str, text: &str) -> String {
    tasks_of(scratch, project)
        .add(&ensemble_tasks::tasks::Ticket {
            title: title.into(),
            agent: agent.into(),
            text: text.into(),
            ..Default::default()
        })
        .unwrap()
}

/// Task `id` of ana's `project`, as the hub has it now.
fn task_of(scratch: &Scratch, project: &str, id: &str) -> ensemble_tasks::tasks::Task {
    tasks_of(scratch, project)
        .detail(id)
        .unwrap()
        .unwrap_or_else(|| panic!("no task {id}"))
        .task
}

/// The result of task `id` of ana's `project`, once it has one.
fn outcome_of(
    scratch: &Scratch,
    project: &str,
    id: &str,
) -> Option<ensemble_tasks::tasks::Outcome> {
    tasks_of(scratch, project).detail(id).unwrap()?.outcome
}

/// A running `ensemble-hub serve`, stopped when the test ends, passed or
/// failed: left running, it would keep the test runner's output open.
struct Served(Child);

impl std::ops::Deref for Served {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Served {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// This scratch computer's runner, running its helper team until dropped.
fn start_runner(s: &Scratch) -> Served {
    s.runner_yaml();
    let mut runner = bare_command(s)
        .args(["serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = BufReader::new(runner.stderr.take().unwrap());
    let (mut line, mut all) = (String::new(), String::new());
    while !line.contains("' works in ") {
        line.clear();
        assert!(
            said.read_line(&mut line).unwrap() > 0,
            "the runner stopped: {all}"
        );
        all.push_str(&line);
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut said, &mut std::io::sink());
    });
    Served(runner)
}

fn serve_hub(scratch: &Scratch, _hub: &Path) -> Served {
    let mut child = hub_command(scratch)
        .args(["serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap() > 0, "it stopped: {seen}");
        seen.push_str(&line);
        if line.contains("listening on") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = err.read_to_string(&mut rest);
    });
    Served(child)
}

#[test]
fn a_hub_server_refuses_another_protocol_or_a_bad_token_and_stops_on_sigterm() {
    let s = Scratch::new("hub-serve");
    let (hub, address, _) = hub_server(&s);
    let mut child = serve_hub(&s, &hub);
    let upgrade = |headers: &str| {
        let mut stream = std::net::TcpStream::connect(&address).unwrap();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{headers}\r\n"
        )
        .unwrap();
        let mut status = String::new();
        BufReader::new(&stream).read_line(&mut status).unwrap();
        status
    };
    let status = upgrade("Authorization: Bearer ens_wrong\r\n");
    assert!(status.contains("426"), "no protocol named: {status}");
    let status = upgrade("Ensemble-Protocol: 2\r\nAuthorization: Bearer ens_wrong\r\n");
    assert!(status.contains("401"), "{status}");
    let status = upgrade("Ensemble-Protocol: 2\r\n");
    assert!(
        status.contains("101"),
        "no token: signing in only: {status}"
    );

    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn a_runner_signs_in_to_a_hub_server_and_takes_a_new_token_as_it_runs() {
    let s = Scratch::new("hub-remote");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let (hub, address, token) = hub_server(&s);
    let mut child = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    runner_names_hub(&s, &url);

    let out = runner_command(&s)
        .args(["login", &url, "--token", "ens_wrong"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("did not accept the token"),
        "{}",
        stderr_of(&out)
    );
    let status = stdout_of(&ensemble_runner(&s, &["status"]));
    assert!(
        status.contains("not signed in: `ensemble-runner login"),
        "nothing was saved: {status}"
    );

    let out = runner_command(&s)
        .args(["login", &url, "--token", &token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("as ana (member: share, recall, tasks, compute)"),
        "{}",
        stdout_of(&out)
    );
    // An admin adds an account on the server while the hub runs: the database
    // is shared between the two processes, and the new token works at once.
    let added = hub_command(&s)
        .args(["user", "add", "kim", "--role", "member"])
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", stderr_of(&added));
    let kim = stdout_of(&added).trim().to_string();
    let other = Scratch::new("hub-remote-kim");
    let out = runner_command(&other)
        .args(["login", &url, "--token", &kim])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("as kim"), "{}", stdout_of(&out));
    let tokens =
        s.0.with_extension("xdg-config")
            .join("ensemble/runner/hubs.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&tokens).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Signed in again, as kim: the running runner uses the new token at once.
    let _runner = start_runner(&s);
    let out = runner_command(&s)
        .args(["login", &url, "--token", &kim])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("The running runner: signed in to"),
        "{}",
        stdout_of(&out)
    );
    // Status asks the hub whether the token works.
    let status = stdout_of(&ensemble_runner(&s, &["status"]));
    assert!(
        status.contains(&format!("hub: {url}, signed in as kim (member:")),
        "{status}"
    );
    let out = ensemble_hub(&s, &["user", "revoke", "kim"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let status = stdout_of(&ensemble_runner(&s, &["status"]));
    assert!(
        status.contains("the hub cannot be used") && status.contains("did not accept the token"),
        "{status}"
    );

    let out = runner_command(&s).args(["logout", &url]).output().unwrap();
    assert!(stdout_of(&out).contains("signed out"));
    let _ = child.kill();
    let _ = child.wait();
}

/// POST JSON to a hub server's REST API: the status and the answer.
fn post_json(address: &str, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!("http://{address}{path}"))
        .header("content-type", "application/json")
        .send(body.to_string())
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn someone_signs_up_is_let_in_and_logs_in_from_a_runner_with_a_password() {
    let s = Scratch::new("hub-register");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let (hub, address, _) = hub_server(&s);
    let child = serve_hub(&s, &hub);
    let password = "correct horse battery";
    let (status, body) = post_json(
        &address,
        "/api/v1/register",
        serde_json::json!({"name": "dana", "password": password}),
    );
    assert_eq!(status, 201, "{body}");
    let url = format!("ws://{address}");
    let login = |user: &str| {
        let mut child = runner_command(&s)
            .args(["login", &url, "--user", user])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{password}").unwrap();
        child.wait_with_output().unwrap()
    };
    let out = login("dana");
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("approval"), "{}", stderr_of(&out));

    let run = |args: &[&str]| hub_command(&s).args(args).output().unwrap();
    let out = run(&["user", "list"]);
    assert!(stdout_of(&out).contains("dana") && stdout_of(&out).contains("pending"));
    let out = run(&["user", "approve", "dana", "--role", "member"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = login("dana");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("as dana (member"),
        "{}",
        stdout_of(&out)
    );
    drop(child);

    // A closed hub takes no sign-ups.
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    let closed = text.replace(
        "    type: approval               # anyone may sign up; an admin approves. Or: { type: closed }\n    max_pending: 20              # sign-ups waiting at once\n",
        "    type: closed\n",
    );
    assert_ne!(closed, text);
    std::fs::write(hub.join("hub.yaml"), closed).unwrap();
    let _child = serve_hub(&s, &hub);
    let (status, _) = post_json(
        &address,
        "/api/v1/register",
        serde_json::json!({"name": "eve", "password": password}),
    );
    assert_eq!(status, 403);
}

#[test]
fn a_runner_joins_a_hub_with_a_code_an_admin_made_and_spends_it() {
    let s = Scratch::new("hub-join");
    let (hub, address, _) = hub_server(&s);
    let _child = serve_hub(&s, &hub);
    s.runner_yaml();
    let url = format!("ws://{address}");
    let out = ensemble_hub(&s, &["user", "join-code", "ana"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let code = stdout_of(&out).trim().to_string();
    assert!(
        stderr_of(&out).contains(&format!("ensemble-runner join {url} {code}"))
            && stderr_of(&out).contains("24 hours"),
        "{}",
        stderr_of(&out)
    );

    let out = ensemble_runner(&s, &["join", &url, &code]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("as ana (member")
            && stdout_of(&out).contains("The runner names this hub now"),
        "{}",
        stdout_of(&out)
    );
    let status = stdout_of(&ensemble_runner(&s, &["status"]));
    assert!(
        status.contains(&format!("hub: {url}, signed in as ana")),
        "{status}"
    );
    // The token is the runner's, named after it.
    let out = ensemble_hub(&s, &["user", "show", "ana"]);
    assert!(stdout_of(&out).contains("tokens: 2"), "{}", stdout_of(&out));

    let other = Scratch::new("hub-join-again");
    let out = runner_command(&other)
        .args(["join", &url, &code])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("join code is not valid"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_runner_file_is_written_once_and_an_account_names_itself_in_the_tracker() {
    let s = Scratch::new("runner-init");
    let out = runner_command(&s).args(["init"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let runner =
        s.0.with_extension("xdg-config")
            .join("ensemble/runner/runner.yaml");
    let written = std::fs::read_to_string(&runner).unwrap();
    assert!(
        written.contains("offers:") && written.contains("workspace: ~/ensemble"),
        "{written}"
    );
    let again = runner_command(&s).args(["init"]).output().unwrap();
    assert!(
        !again.status.success(),
        "it does not overwrite without --force"
    );

    hub_server_of(&s);
    let run = |args: &[&str]| hub_command(&s).args(args).output().unwrap();
    assert!(
        run(&["user", "add", "ana", "--role", "member"])
            .status
            .success()
    );
    let out = run(&["user", "tracker", "ana", "ana.petrosyan"]);
    assert!(
        stdout_of(&out).contains("ana is ana.petrosyan in the tracker"),
        "{}",
        stdout_of(&out)
    );
    let out = run(&["user", "tracker", "ana"]);
    assert!(
        stdout_of(&out).contains("the account's own name"),
        "{}",
        stdout_of(&out)
    );
}

#[test]
fn an_issue_on_the_hub_server_is_worked_on_a_members_runner_and_reported_back() {
    let s = Scratch::new("hub-tasks");
    let cli = s.opencode("renamed it", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let shop = s.repo("shop");
    std::fs::write(shop.join("a.txt"), "foo\n").unwrap();
    let (hub, address, token) = hub_server(&s);
    let text = std::fs::read_to_string(hub.join("hub.yaml")).unwrap();
    std::fs::write(
        hub.join("hub.yaml"),
        text.replace("  poll_seconds: 30\n", "  poll_seconds: 1\n"),
    )
    .unwrap();
    let out = hub_command(&s)
        .args(["user", "tracker", "ana", "ana.p"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    std::fs::create_dir_all(hub.join("issues")).unwrap();
    let _hub = serve_hub(&s, &hub);

    // Ana's runner, with her helper team, signed in to the hub, and the
    // project in its workspace.
    runner_with_hub(&s, &format!("ws://{address}"), &token);
    let _runner = start_runner(&s);
    let project = "shop";
    let issue = hub.join("issues/7.md");
    std::fs::write(
        &issue,
        format!("---\ntitle: Rename foo\nassignee: ana.p\nlabels: [ensemble, \"ensemble:team/coder\"]\nproject: {project}\nstatus: open\n---\nRename foo to bar in a.txt.\n"),
    )
    .unwrap();
    assert!(
        wait_for(60, || std::fs::read_to_string(&issue)
            .unwrap_or_default()
            .contains("status: review")),
        "the issue was not reported back: {}",
        std::fs::read_to_string(&issue).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(shop.join("a.txt")).unwrap(),
        "bar\n"
    );
    let said = std::fs::read_to_string(&issue).unwrap();
    for expected in [
        "Picked up as task 0001-rename-foo, for team/coder",
        "is done:",
    ] {
        assert!(said.contains(expected), "{expected} in {said}");
    }
}

/// The runner on another computer of the scratch, `machine`: its own config
/// and state directories, with `runner_yaml` as its runner, working in the
/// machine's `workspace` folder unless it says otherwise, and signed in to the
/// hubs the scratch signed in to.
fn on_machine(scratch: &Scratch, machine: &str, runner_yaml: &str) -> Command {
    let home = scratch.0.with_extension("machines").join(machine);
    let config = home.join("config").join("ensemble/runner");
    std::fs::create_dir_all(&config).unwrap();
    let runner_yaml = if runner_yaml.contains("\nworkspace:") {
        runner_yaml.to_string()
    } else {
        runner_yaml.replacen(
            "\noffers:",
            &format!(
                "\nworkspace: \"{}\"\noffers:",
                home.join("workspace").display()
            ),
            1,
        )
    };
    std::fs::write(config.join("runner.yaml"), runner_yaml).unwrap();
    let tokens = scratch
        .0
        .with_extension("xdg-config")
        .join("ensemble/runner/hubs.json");
    if tokens.exists() {
        std::fs::copy(&tokens, config.join("hubs.json")).unwrap();
    }
    let mut c = Command::new(RUNNER);
    c.current_dir(scratch.path())
        .env(RESCAN_VARIABLE, "1")
        .envs(GIT_IDENTITY)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", scratch.0.with_extension("xdg-cache"));
    c
}

/// The team `team` of the computer `machine`, with `config` as its
/// `config.yaml`.
fn machine_team(scratch: &Scratch, machine: &str, team: &str, config: &str) {
    let dir = scratch
        .0
        .with_extension("machines")
        .join(machine)
        .join("config/ensemble/runner/teams")
        .join(team);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config).unwrap();
}

/// `ensemble-runner serve` on the computer `machine`, whose runner is
/// `runner_yaml`, started and waited for until it is connected.
fn serve_runner(scratch: &Scratch, url: &str, machine: &str, runner_yaml: &str) -> Served {
    // The runner names the hub it connects to.
    let runner_yaml = if runner_yaml.contains("\nhub:") {
        runner_yaml.to_string()
    } else {
        runner_yaml.replacen("\noffers:", &format!("\nhub: \"{url}\"\noffers:"), 1)
    };
    let mut child = on_machine(scratch, machine, &runner_yaml)
        .args(["serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap() > 0, "it stopped: {seen}");
        seen.push_str(&line);
        if line.contains("connected to ws") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = err.read_to_string(&mut rest);
    });
    Served(child)
}

#[test]
fn a_runner_serves_its_models_through_the_hub_and_a_shared_runner_lends_no_login() {
    let s = Scratch::new("runner-models");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    let out = runner_command(&s)
        .args(["login", &url, "--token", &token])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let server = models_stub(r#"{"choices":[{"message":{"content":"hi from the node"}}]}"#);
    let runner_yaml = |access: &str, offers: &str| {
        format!("version: 1\nname: {access}-box\naccess: {access}\nhub: \"{url}\"\n{offers}")
    };
    let models = format!(
        "offers:\n  models: {{ base_url: \"{server}\", timeout_seconds: 5, names: [small] }}\n"
    );
    let _runner = serve_runner(&s, &url, "private-box", &runner_yaml("private", &models));
    let mut answer = ureq::post(format!("http://{address}/nodes/v1/chat/completions"))
        .header("authorization", format!("Bearer {token}"))
        .send(r#"{"model": "small", "messages": [{"role": "user", "content": "hi"}]}"#)
        .unwrap();
    let body = answer.body_mut().read_to_string().unwrap();
    assert!(body.contains("hi from the node"), "{body}");

    // A member may not host a shared runner.
    let out = on_machine(&s, "shared-box", &runner_yaml("shared", &models))
        .args(["serve"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("`host` permission"),
        "{}",
        stderr_of(&out)
    );
    // And a shared runner never offers an agent on its CLI's own login.
    machine_team(&s, "shared-box", "work", &config_yaml("opencode", NOWHERE));
    let out = on_machine(
        &s,
        "shared-box",
        &runner_yaml("shared", "offers:\n  runs: { agents: [work/reviewer] }\n"),
    )
    .args(["serve"])
    .output()
    .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("never lends"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn a_running_runner_reloads_its_teams() {
    let s = Scratch::new("reload");
    s.write("@team/config.yaml", &config_yaml("opencode", NOWHERE));
    let _runner = start_runner(&s);

    // A team linked while the runner runs is used at once.
    let elsewhere = s.0.with_extension("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("config.yaml"), config_yaml("true", NOWHERE)).unwrap();
    let added = runner_command(&s)
        .args(["team", "link", "pcb"])
        .arg(&elsewhere)
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", stderr_of(&added));
    assert!(
        stdout_of(&added).contains("The runner uses it now: teams reloaded: 4 agent(s)"),
        "{}",
        stdout_of(&added)
    );

    // A team that cannot be loaded leaves the runner's teams as they were.
    std::fs::write(elsewhere.join("config.yaml"), "version: 1\n").unwrap();
    let out = ensemble_runner(&s, &["reload"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("kept its teams as they were")
            && stderr_of(&out).contains("team 'pcb'"),
        "{}",
        stderr_of(&out)
    );
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// The runner of the computer `machine`, named for it, with the hub at
/// `url`: started and waited for until it is ready.
fn machine_runner(scratch: &Scratch, machine: &str, url: &str) -> Served {
    let yaml =
        format!("version: 1\nname: {machine}\naccess: private\nhub: \"{url}\"\noffers: {{}}\n");
    let mut runner = on_machine(scratch, machine, &yaml)
        .args(["serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = BufReader::new(runner.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("' works in ") {
        line.clear();
        assert!(said.read_line(&mut line).unwrap() > 0, "the runner stopped");
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut said, &mut std::io::sink());
    });
    Served(runner)
}

#[test]
fn a_task_stopped_on_one_computer_is_carried_on_by_another_from_its_pushed_branch() {
    let s = Scratch::new("carry-on");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    let url = format!("ws://{address}");
    runner_with_hub(&s, &url, &token);

    // One repository on a remote, cloned as `proj` into the workspace of the
    // desk and of the laptop.
    let origin = s.0.with_extension("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git_in(
        &origin,
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    );
    s.write("README", "hello\n");
    git_in(s.path(), &["init", "--quiet", "-b", "main"]);
    git_in(s.path(), &["add", "README"]);
    git_in(s.path(), &["commit", "--quiet", "-m", "start"]);
    git_in(
        s.path(),
        &["push", "--quiet", &origin.to_string_lossy(), "main"],
    );
    // The first run does part 1, then is slow; the run after it, wherever it
    // is, finds part 1 and does part 2.
    let marker = s.0.with_extension("bin").join("started");
    let cli = s.opencode(
        "both parts done",
        &format!(
            "if [ -e part1.txt ]; then echo 2 > part2.txt; else echo 1 > part1.txt; touch '{}'; sleep 60; fi",
            marker.display()
        ),
    );
    let team = config_yaml(&cli, NOWHERE).replace("isolation: shared", "isolation: worktree");
    for machine in ["desk", "laptop"] {
        let workspace =
            s.0.with_extension("machines")
                .join(machine)
                .join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        git_in(
            &workspace,
            &["clone", "--quiet", &origin.to_string_lossy(), "proj"],
        );
        git_in(&workspace.join("proj"), &["config", "user.name", "T"]);
        git_in(&workspace.join("proj"), &["config", "user.email", "t@t"]);
        machine_team(&s, machine, "team", &team);
    }

    let mut desk = machine_runner(&s, "desk", &url);
    add_task(&s, "proj", "Two parts", "team/coder", "do both parts");
    assert!(
        wait_for(30, || marker.exists()),
        "the desk did not start it"
    );
    sigterm(&desk);
    desk.wait().unwrap();
    let task = task_of(&s, "proj", "0001-two-parts");
    assert_eq!(
        (task.interrupted_on.as_deref(), task.pinned_to.as_deref()),
        (Some("desk"), None),
        "its work can move"
    );
    assert!(
        git_in(
            &origin,
            &["ls-tree", "--name-only", "ensemble/0001-two-parts"]
        )
        .contains("part1.txt"),
        "the desk pushed its work so far"
    );

    // The laptop, whose workspace has `proj` too, takes it over.
    let _laptop = machine_runner(&s, "laptop", &url);
    assert!(
        wait_for(30, || outcome_of(&s, "proj", "0001-two-parts").is_some()),
        "the laptop did not carry it on"
    );
    let outcome = outcome_of(&s, "proj", "0001-two-parts").unwrap();
    assert!(
        outcome.status == ensemble_tasks::tasks::Status::Done
            && outcome.text.contains("both parts done"),
        "{}",
        outcome.text
    );
    let files = git_in(
        &origin,
        &["ls-tree", "--name-only", "ensemble/0001-two-parts"],
    );
    assert!(
        files.contains("part1.txt") && files.contains("part2.txt"),
        "{files}"
    );
}

/// A bare repository beside the scratch, `<name>.git`, with one commit on
/// `main` holding `a.txt`: a project's remote, for a runner to clone.
fn bare_origin(scratch: &Scratch, name: &str) -> PathBuf {
    let origin = scratch.0.with_extension(format!("{name}.git"));
    std::fs::create_dir_all(&origin).unwrap();
    git_in(
        &origin,
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    );
    let seed = scratch.0.with_extension(format!("{name}-seed"));
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("a.txt"), "foo\n").unwrap();
    git_in(&seed, &["init", "--quiet", "-b", "main"]);
    git_in(&seed, &["add", "a.txt"]);
    git_in(&seed, &["commit", "--quiet", "-m", "start"]);
    git_in(
        &seed,
        &["push", "--quiet", &origin.to_string_lossy(), "main"],
    );
    let _ = std::fs::remove_dir_all(&seed);
    origin
}

#[test]
fn a_repository_added_after_the_runner_started_gets_a_worker_and_a_task_brings_its_notes() {
    let s = Scratch::new("added-later");
    let cli = s.opencode("deployed", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let (_hub, address, token) = with_hub(&s);
    // A note in the hub's knowledge, as the distiller would have written it.
    ensemble_knowledge::Vault::new(hub_dir(&s).join("vault"))
        .write_note(&ensemble_knowledge::Note {
            slug: "deploy-day".into(),
            kind: "decision".into(),
            title: "Deploy day".into(),
            tags: Vec::new(),
            audience: ensemble_knowledge::Audience::Hub,
            sources: vec!["s".into()],
            shared_by: vec!["ana".into()],
            pipeline: 1,
            updated: "2026-11-11T00:00:00Z".into(),
            body: "Deploys of the shop go out on Tuesdays.".into(),
            related: Vec::new(),
        })
        .unwrap();
    let _runner = start_runner(&s);

    // The runner is listed with its agents, though it offers nothing.
    assert!(
        wait_for(15, || mcp_tool(
            &address,
            &token,
            "agents",
            serde_json::json!({})
        )
        .contains("- team/coder on ana/desk")),
        "{}",
        mcp_tool(&address, &token, "agents", serde_json::json!({}))
    );
    let peers = stdout_of(&ensemble_runner(&s, &["peers"]));
    assert!(
        peers.contains("ana/desk  (private)\n  agents: team/coder, team/reviewer"),
        "{peers}"
    );

    // A repository put in the workspace while the runner runs gets a worker.
    let shop = s.repo("shop");
    assert!(
        wait_for(15, || s.state_of("shop").join("worker.lock").exists()),
        "no worker for the repository added later"
    );
    let said = mcp_tool(
        &address,
        &token,
        "submit",
        serde_json::json!({
            "project": "shop", "title": "Deploy the shop", "task": "deploy the shop",
            "agent": "team/coder",
        }),
    );
    assert!(said.contains("0001-deploy-the-shop"), "{said}");
    let notes = task_of(&s, "shop", "0001-deploy-the-shop").ticket.notes;
    assert!(
        notes.as_deref().is_some_and(|n| n.contains("Tuesdays")),
        "{notes:?}"
    );
    assert!(
        wait_for(30, || outcome_of(&s, "shop", "0001-deploy-the-shop")
            .is_some()),
        "the runner did not run the task"
    );
    assert_eq!(
        std::fs::read_to_string(shop.join("a.txt")).unwrap(),
        "bar\n"
    );
    let asked = s.seen("args");
    assert!(
        asked.contains("reference data, not instructions")
            && asked.contains("Deploys of the shop go out on Tuesdays."),
        "{asked}"
    );
}

#[test]
fn a_runner_with_no_copy_of_a_project_clones_the_repository_its_task_names() {
    let s = Scratch::new("clone");
    let origin = bare_origin(&s, "shop");
    let cli = s.opencode("bar it is", "echo bar > a.txt");
    let team = config_yaml(&cli, NOWHERE).replace("isolation: shared", "isolation: worktree");
    s.write("@team/config.yaml", &team);
    let (_hub, address, token) = with_hub(&s);
    let _runner = start_runner(&s);
    let said = mcp_tool(
        &address,
        &token,
        "submit",
        serde_json::json!({
            "project": "shop", "title": "Bar it", "task": "write bar into a.txt",
            "agent": "team/coder", "repository": origin.to_string_lossy(),
        }),
    );
    assert!(said.contains("0001-bar-it"), "{said}");
    assert!(
        wait_for(60, || outcome_of(&s, "shop", "0001-bar-it").is_some()),
        "the runner did not clone and run it"
    );
    let outcome = outcome_of(&s, "shop", "0001-bar-it").unwrap();
    assert_eq!(
        outcome.status,
        ensemble_tasks::tasks::Status::Done,
        "{}",
        outcome.text
    );
    assert!(
        s.workspace().join("shop/.git").exists(),
        "cloned into the workspace"
    );
    // Worked on its branch, pushed to the repository it was cloned from.
    assert_eq!(
        git_in(&origin, &["show", "ensemble/0001-bar-it:a.txt"]),
        "bar"
    );
    assert_eq!(git_in(&origin, &["show", "main:a.txt"]), "foo");
}

#[test]
fn a_runner_asks_where_a_project_is_and_clones_what_it_is_told() {
    let s = Scratch::new("ask-where");
    let origin = bare_origin(&s, "shop");
    let cli = s.opencode("bar it is", "echo bar > a.txt");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let (_hub, address, token) = with_hub(&s);
    let _runner = start_runner(&s);
    mcp_tool(
        &address,
        &token,
        "submit",
        serde_json::json!({
            "project": "shop", "title": "Bar it", "task": "write bar into a.txt",
            "agent": "team/coder",
        }),
    );
    assert!(
        wait_for(30, || task_of(&s, "shop", "0001-bar-it").question.is_some()),
        "the runner did not ask"
    );
    let question = task_of(&s, "shop", "0001-bar-it").question.unwrap();
    assert!(
        question.contains("desk has no copy of the project shop")
            && question.contains("which repository"),
        "{question}"
    );
    assert!(!s.workspace().join("shop").exists());
    let said = mcp_tool(
        &address,
        &token,
        "answer",
        serde_json::json!({"project": "shop", "id": "0001-bar-it", "text": origin.to_string_lossy()}),
    );
    assert!(said.contains("Answered"), "{said}");
    assert!(
        wait_for(60, || outcome_of(&s, "shop", "0001-bar-it").is_some()),
        "the runner did not clone and run it"
    );
    let task = task_of(&s, "shop", "0001-bar-it");
    assert_eq!(task.status, Some(ensemble_tasks::tasks::Status::Done));
    assert_eq!(
        task.ticket.repository.as_deref(),
        Some(origin.to_string_lossy().as_ref()),
        "kept for every runner"
    );
    assert_eq!(
        std::fs::read_to_string(s.workspace().join("shop/a.txt")).unwrap(),
        "bar\n"
    );
}

#[test]
fn a_task_whose_repository_its_only_runner_cannot_clone_is_given_back_and_blocked() {
    let s = Scratch::new("give-back");
    let cli = s.opencode("never", "true");
    s.write("@team/config.yaml", &config_yaml(&cli, NOWHERE));
    let (_hub, address, token) = with_hub(&s);
    let _runner = start_runner(&s);
    let nowhere = s.0.with_extension("nowhere.git");
    mcp_tool(
        &address,
        &token,
        "submit",
        serde_json::json!({
            "project": "shop", "title": "Bar it", "task": "write bar into a.txt",
            "agent": "team/coder", "repository": nowhere.to_string_lossy(),
        }),
    );
    let shown = || {
        mcp_tool(
            &address,
            &token,
            "task",
            serde_json::json!({"project": "shop", "id": "0001-bar-it"}),
        )
    };
    assert!(
        wait_for(30, || shown().contains("\"blocked\": true")),
        "{}",
        shown()
    );
    let shown = shown();
    assert!(
        shown.contains("\"runner\": \"desk\"") && shown.contains("desk cannot clone"),
        "{shown}"
    );
    let listed = mcp_tool(
        &address,
        &token,
        "tasks",
        serde_json::json!({"project": "shop"}),
    );
    assert!(listed.contains("\"blocked\": true"), "{listed}");
    assert!(
        !s.workspace().join("shop").exists(),
        "a clone that failed leaves nothing"
    );
    assert_eq!(outcome_of(&s, "shop", "0001-bar-it"), None);
}

#[test]
fn settings_are_changed_from_the_command_line_and_those_nix_declares_are_kept() {
    let s = Scratch::new("config");
    let runner = s.runner_yaml();
    let get = |key: &str| {
        stdout_of(&ensemble_runner(&s, &["config", "get", key]))
            .trim()
            .to_string()
    };
    assert_eq!(get("name"), "desk");

    let out = ensemble_runner(&s, &["config", "set", "access", "shared"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(get("access"), "shared");
    // One that would not load is refused, and nothing changes.
    let out = ensemble_runner(&s, &["config", "set", "name", "a b/c"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("nothing was changed"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(get("name"), "desk");
    let out = ensemble_runner(&s, &["config", "unset", "name"]);
    assert!(!out.status.success(), "a runner needs a name");
    assert_eq!(
        stdout_of(&ensemble_runner(&s, &["config", "path"])).trim(),
        runner.display().to_string()
    );

    // What Nix declares is merged in, kept, and refused here.
    let declared = s.write("declared.yaml", "access: private\noffers: {}\n");
    let out = ensemble_runner(&s, &["config", "merge", declared.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(get("access"), "private");
    let out = ensemble_runner(&s, &["config", "set", "access", "shared"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("`access` is set in your Nix configuration"),
        "{}",
        stderr_of(&out)
    );
    let out = ensemble_runner(&s, &["config", "set", "name", "rogstrix"]);
    assert!(
        out.status.success(),
        "the rest stays the CLI's: {}",
        stderr_of(&out)
    );

    // A team made here, too; the running runner would reload it.
    let out = ensemble_runner(&s, &["team", "new", "web"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = ensemble_runner(
        &s,
        &[
            "team",
            "config",
            "web",
            "set",
            "gates.test.timeout_seconds",
            "60",
        ],
    );
    assert!(
        !out.status.success(),
        "a gate needs its command: {}",
        stdout_of(&out)
    );
    let out = ensemble_runner(&s, &["team", "config", "web", "get", "version"]);
    assert_eq!(stdout_of(&out).trim(), "1");
}

#[test]
fn signing_in_to_a_hub_names_it_in_the_runner_and_signing_out_takes_it_out() {
    let s = Scratch::new("login-names-hub");
    let (hub, address, token) = hub_server(&s);
    let _hub = serve_hub(&s, &hub);
    s.runner_yaml();
    let url = format!("ws://{address}");
    let out = ensemble_runner(&s, &["login", &url, "--token", &token]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("The runner names this hub now"),
        "{}",
        stdout_of(&out)
    );
    let hub_now = stdout_of(&ensemble_runner(&s, &["config", "get", "hub"]));
    assert_eq!(hub_now.trim(), url);
    let status = stdout_of(&ensemble_runner(&s, &["status"]));
    assert!(
        status.contains(&format!("hub: {url}, signed in")),
        "{status}"
    );

    let out = ensemble_runner(&s, &["logout", &url]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("The runner names no hub now"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        !ensemble_runner(&s, &["config", "get", "hub"])
            .status
            .success()
    );
}

#[test]
fn a_running_runner_makes_a_change_to_its_settings_itself() {
    let s = Scratch::new("config-running");
    s.runner_yaml();
    // Not running: the command makes it, and nothing needs saying.
    let out = ensemble_runner(&s, &["config", "set", "access", "shared"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).is_empty(), "{}", stderr_of(&out));
    let mut runner = start_runner(&s);
    let out = ensemble_runner(&s, &["config", "set", "access", "private"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("The running runner"),
        "the running runner made it: {}",
        stderr_of(&out)
    );
    // Its errors come back as they are.
    let out = ensemble_runner(&s, &["config", "set", "name", "a b/c"]);
    assert!(
        stderr_of(&out).contains("nothing was changed"),
        "{}",
        stderr_of(&out)
    );
    sigterm(&runner);
    let _ = runner.wait();
}

#[test]
fn a_running_hub_uses_a_changed_hub_yaml_at_once_or_starts_again() {
    let s = Scratch::new("hub-live");
    let (hub, _address, _token) = hub_server(&s);
    let _served = serve_hub(&s, &hub);
    // Roles: used at once.
    let out = ensemble_hub(&s, &["config", "set", "roles.reader", "[recall, share]"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("uses it now"),
        "{}",
        stderr_of(&out)
    );
    let roles = stdout_of(&ensemble_hub(&s, &["role", "list"]));
    assert!(roles.contains("reader:\n  - recall\n  - share"), "{roles}");
    // Where it listens: it starts again, there.
    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let out = ensemble_hub(&s, &["config", "set", "server.listen", &listen]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("starts again"),
        "{}",
        stderr_of(&out)
    );
    assert!(
        wait_for(15, || std::net::TcpStream::connect(&listen).is_ok()),
        "the hub listens on {listen}"
    );
    assert!(wait_for(5, || ensemble_hub(&s, &["user", "list"])
        .status
        .success()));
}
