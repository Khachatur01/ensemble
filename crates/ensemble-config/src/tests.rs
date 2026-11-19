use super::raw::*;
use super::*;

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn p() -> PathBuf {
    PathBuf::from("config.yaml")
}

fn opencode(c: &Config, agent: &str) -> OpenCodeAdapter {
    match &c.agents[agent].adapter {
        Adapter::OpenCode(oc) => oc.clone(),
        other => panic!("not an opencode agent: {other:?}"),
    }
}

fn claude(c: &Config, agent: &str) -> ClaudeCodeAdapter {
    match &c.agents[agent].adapter {
        Adapter::ClaudeCode(cc) => cc.clone(),
        other => panic!("not a claude-code agent: {other:?}"),
    }
}

/// OpenCode agents on two LLM backends, and one Claude Code agent. Every
/// setting is written, as it has to be.
const REFERENCE: &str = r#"
version: 1
limits: {}
gates: {}
agents:
  planner:
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
          - { backend: openai-local, name: qwen3-coder, context_window: 32768 }
        when_none: fail
      tools: [read, glob, grep, list]
      system_prompt: append
      max_steps: 30
      command: opencode
  coder:
    max_seconds: 900
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model:
        type: llm_backend
        try:
          - { backend: openai-remote, name: qwen3-coder, context_window: 32768 }
        when_none: fail
      tools: [read, edit, bash]
      system_prompt: append
      max_steps: 30
      command: opencode
  reviewer:
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
"#;

/// The backends of the runner the teams of these tests run on: two
/// OpenAI-compatible ones of REFERENCE's, two more and an Anthropic one of
/// `with_fallback`'s, and Anthropic's API as `claude-api`.
const RUNNER_BACKENDS: &str = r#"
      openai-local: { type: openai-compatible-api, base_url: "http://127.0.0.1:8000/v1", api_key_env: OPENAI_LOCAL_API_KEY, timeout_seconds: 600 }
      openai-remote: { type: openai-compatible-api, base_url: "http://10.10.10.5:8000/v1", api_key_env: OPENAI_REMOTE_API_KEY, timeout_seconds: 300 }
      home: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 5 }
      central: { type: openai-compatible-api, base_url: "http://127.0.0.1:2/v1", timeout_seconds: 5 }
      claude: { type: anthropic-api, base_url: "https://api.anthropic.com", timeout_seconds: 5 }
      claude-api: { type: anthropic-api, base_url: "https://api.anthropic.com/", api_key_env: ANTHROPIC_API_KEY, timeout_seconds: 90 }
"#;

/// A runner without a hub whose `llm.direct.backends` are `backends`, the
/// lines of a mapping indented six spaces, or `{}`, its keys read from `env`.
fn runner_on(backends: &str, env: &dyn Env) -> Result<Runner, ConfigError> {
    let backends = if backends.trim() == "{}" {
        " {}\n".to_string()
    } else {
        format!("\n{}\n", backends.trim_matches('\n'))
    };
    parse_runner(
        &format!(
            "version: 1\nname: laptop\nworks_for: owner\nworkspace: /code\nllm:\n  direct:\n    backends:{backends}    shares: []\n    max_shared_calls: 1\n"
        ),
        Path::new("runner.yaml"),
        env,
    )
}

/// The team `doc` on the runner of RUNNER_BACKENDS.
fn team(doc: &str) -> Result<Config, ConfigError> {
    let runner = runner_on(RUNNER_BACKENDS, &env(&[])).expect("the test runner loads");
    parse(doc, &p())?.with_backends(runner.llm.backends(None))
}

fn problems_of(err: ConfigError) -> Vec<String> {
    match err {
        ConfigError::Invalid { problems, .. } => problems,
        other => panic!("expected Invalid, got: {other}"),
    }
}

fn joined_problems(doc: &str) -> String {
    problems_of(team(doc).unwrap_err()).join("\n")
}

/// The message of a document that serde refuses.
fn refusal(doc: &str) -> String {
    match team(doc).unwrap_err() {
        ConfigError::Syntax { detail, .. } => detail,
        other => panic!("expected Syntax, got: {other}"),
    }
}

/// The problems of the runner whose backends are `backends`.
fn runner_problems(backends: &str) -> String {
    problems_of(runner_on(backends, &env(&[])).unwrap_err()).join("\n")
}

/// The message of the runner whose backends serde refuses.
fn runner_refusal(backends: &str) -> String {
    match runner_on(backends, &env(&[])).unwrap_err() {
        ConfigError::Syntax { detail, .. } => detail,
        other => panic!("expected Syntax, got: {other}"),
    }
}

#[test]
fn the_reference_config_loads() {
    let c = parse(REFERENCE, &p()).expect("reference config must load");
    assert!(c.llm_backends.is_empty(), "a team defines no backend");
    let c = team(REFERENCE).unwrap();
    assert_eq!(c.version, 1);
    assert_eq!(c.llm_backends.len(), 6, "the runner's, every one");
    assert_eq!(c.llm_backends["openai-remote"].timeout_seconds, 300);
    assert_eq!(c.agent_names(), vec!["coder", "planner", "reviewer"]);
    let coder = &c.agents["coder"];
    assert_eq!(coder.llm_backend(), Some("openai-remote"));
    assert_eq!(coder.runs_on(), "opencode on openai-remote");
    assert_eq!(coder.context_window(), Some(32768));
    assert_eq!(coder.max_seconds, 900);
    assert!(coder.can_change_files());
    assert_eq!(c.agents["reviewer"].llm_backend(), None);
    assert_eq!(c.agents["reviewer"].context_window(), None);
}

#[test]
fn a_missing_setting_is_refused_by_name_and_place() {
    // (the line to take out, where it belongs, its key)
    for (line, at, key) in [
        ("    max_seconds: 900\n", "agents.coder", "max_seconds"),
        (
            "      max_steps: 30\n      command: opencode\n  reviewer:",
            "agents.coder",
            "max_steps",
        ),
        (
            "      system_prompt: append\n      max_steps: 30\n      command: opencode\n  reviewer:",
            "agents.coder",
            "system_prompt",
        ),
        ("      max_turns: 30\n", "agents.reviewer", "max_turns"),
        ("      command: claude\n", "agents.reviewer", "command"),
        (
            "{ backend: openai-remote, name: qwen3-coder, context_window: 32768 }",
            "agents.coder",
            "context_window",
        ),
    ] {
        // Put back what follows the line, so only the one key is gone.
        let kept = match key {
            "max_steps" => "      command: opencode\n  reviewer:",
            "system_prompt" => "      max_steps: 30\n      command: opencode\n  reviewer:",
            "context_window" => "{ backend: openai-remote, name: qwen3-coder }",
            _ => "",
        };
        let doc = REFERENCE.replacen(line, kept, 1);
        assert_ne!(doc, REFERENCE, "the fixture lost its {key} line");
        let detail = refusal(&doc);
        assert!(
            detail.contains(at) && detail.contains(&format!("`{key}` is required")),
            "{key}: {detail}"
        );
    }
}

#[test]
fn a_config_without_its_agents_is_refused_and_one_defining_backends_too() {
    let detail = refusal("version: 1\nlimits: {}\ngates: {}\n");
    assert!(detail.contains("`agents` is required"), "{detail}");
    let detail = refusal("version: 1\nlimits: {}\nagents: {}\n");
    assert!(detail.contains("`gates` is required"), "{detail}");
    assert!(team("version: 1\nlimits: {}\ngates: {}\nagents: {}\n").is_ok());
    // A team names backends; the runner defines them.
    let detail = refusal("version: 1\nllm_backends: {}\nlimits: {}\ngates: {}\nagents: {}\n");
    assert!(detail.contains("unknown field `llm_backends`"), "{detail}");
}

#[test]
fn a_backend_missing_a_setting_is_refused_by_name_and_place() {
    for (backend, key) in [
        (
            "      x: { type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\" }",
            "timeout_seconds",
        ),
        (
            "      x: { type: openai-compatible-api, timeout_seconds: 5 }",
            "base_url",
        ),
    ] {
        let detail = runner_refusal(backend);
        assert!(
            detail.contains("llm.direct.backends")
                && detail.contains(&format!("`{key}` is required")),
            "{key}: {detail}"
        );
    }
}

#[test]
fn a_setting_of_another_adapter_type_is_unknown_and_so_is_a_stray_context_window() {
    let doc = REFERENCE.replace(
        "      model: sonnet\n",
        "      model: sonnet\n      max_steps: 3\n",
    );
    let detail = refusal(&doc);
    assert!(detail.contains("max_steps"), "{detail}");

    // The window belongs to the model on an LLM backend, nowhere else.
    let doc = REFERENCE.replace(
        "    adapter:\n      type: claude-code",
        "    context_window: 8000\n    adapter:\n      type: claude-code",
    );
    assert!(refusal(&doc).contains("context_window"));

    let provider = "model:\n        type: provider\n        name: opencode/big-pickle\n        context_window: 1\n";
    let doc = format!(
        "version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      {provider}      tools: []\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n"
    );
    assert!(refusal(&doc).contains("context_window"));
}

#[test]
fn an_opencode_model_is_a_provider_model_or_an_llm_backend_model() {
    let doc = |model: &str| {
        format!(
            "version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      model: {model}\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n"
        )
    };
    let c = team(&doc("{ type: provider, name: opencode/big-pickle }")).unwrap();
    let oc = opencode(&c, "x");
    assert_eq!(oc.model_id(), "opencode/big-pickle");
    assert_eq!((oc.llm_backend(), oc.context_window()), (None, None));

    let detail = refusal(&doc("{ type: local }"));
    assert!(
        detail.contains("unknown variant `local`")
            && detail.contains("provider")
            && detail.contains("llm_backend"),
        "{detail}"
    );
    let detail = refusal(&doc("opencode/big-pickle"));
    assert!(detail.contains("agents.x"), "{detail}");
}

/// Two openai-compatible backends and an Anthropic one, and one agent of each
/// kind that runs on a backend, with `try` and `when_none` as given.
fn with_fallback(opencode_try: &str, direct_try: &str, when_none: &str) -> String {
    format!(
        "version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n  coder:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: opencode\n      model:\n        type: llm_backend\n        try: {opencode_try}\n        when_none: {when_none}\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n  summariser:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try: {direct_try}\n      when_none: {when_none}\n      max_tokens: 10\n"
    )
}

#[test]
fn an_agent_lists_its_models_in_order_and_says_what_happens_when_none_works() {
    let doc = with_fallback(
        "[{ backend: home, name: small, context_window: 8 }, { backend: central, name: big, context_window: 64 }]",
        "[{ backend: home, model: small }, { backend: claude, model: haiku }]",
        "wait",
    );
    let c = team(&doc).unwrap();
    let coder = &c.agents["coder"];
    assert_eq!(
        coder.llm_backends(),
        [("home", "small"), ("central", "big")]
    );
    assert_eq!(coder.fallback().unwrap().when_none, WhenNone::Wait);
    assert_eq!(
        (coder.llm_backend(), coder.model(), coder.context_window()),
        (Some("home"), "small", Some(8)),
        "what it says it runs on is its first model"
    );
    let bound = coder.bound_to(1);
    assert_eq!(
        (bound.llm_backend(), bound.model(), bound.context_window()),
        (Some("central"), "big", Some(64))
    );
    assert_eq!(bound.llm_backends().len(), 1);
    let summariser = &c.agents["summariser"];
    assert_eq!(
        summariser.llm_backends(),
        [("home", "small"), ("claude", "haiku")]
    );
    assert_eq!(summariser.bound_to(1).runs_on(), "direct on claude");
}

#[test]
fn every_model_in_a_try_list_is_checked() {
    let joined = joined_problems(&with_fallback(
        "[]",
        "[{ backend: home, model: small }, { backend: nowhere, model: \"\" }]",
        "fail",
    ));
    assert!(
        joined.contains("agents.coder.adapter: `model.try` is empty"),
        "{joined}"
    );
    assert!(
        joined.contains("agents.summariser.adapter.try[1]: `model` is empty"),
        "{joined}"
    );
    // Its backends are checked against the runner's once the team is valid.
    let joined = joined_problems(&with_fallback(
        "[{ backend: home, name: small, context_window: 8 }, { backend: claude, name: haiku, context_window: 8 }]",
        "[{ backend: home, model: small }, { backend: nowhere, model: big }]",
        "fail",
    ));
    for wanted in [
        "agents.summariser.adapter.try[1]: LLM backend 'nowhere' is not defined in runner.yaml's `llm`",
        "agents.coder.adapter.model.try[1]: LLM backend 'claude' is of type 'anthropic-api'",
    ] {
        assert!(joined.contains(wanted), "{wanted}: {joined}");
    }
    let joined = joined_problems(&with_fallback(
        "[{ backend: home, name: small, context_window: 8 }, { backend: claude, name: haiku, context_window: 0 }]",
        "[{ backend: home, model: small }]",
        "fail",
    ));
    assert!(
        joined
            .contains("agents.coder.adapter.model.try[1]: `context_window` must be greater than 0"),
        "{joined}"
    );
    let detail = refusal(&with_fallback(
        "[{ backend: home, name: small, context_window: 8 }]",
        "[{ backend: home, model: small }]",
        "retry",
    ));
    assert!(
        detail.contains("retry") && detail.contains("fail") && detail.contains("wait"),
        "{detail}"
    );
}

#[test]
fn opencode_s_own_prompt_is_kept_or_replaced_and_only_a_prompt_replaces_it() {
    let c = team(REFERENCE).unwrap();
    assert_eq!(opencode(&c, "coder").system_prompt, SystemPrompt::Append);

    let replaced = REFERENCE.replacen("system_prompt: append", "system_prompt: replace", 1);
    let detail = joined_problems(&replaced);
    assert!(
        detail.contains("agents.planner.adapter")
            && detail.contains("`system_prompt: replace` needs the agent's `prompt`"),
        "{detail}"
    );
    let with_prompt = replaced.replacen("  planner:\n", "  planner:\n    prompt: You plan.\n", 1);
    let c = team(&with_prompt).unwrap();
    assert_eq!(opencode(&c, "planner").system_prompt, SystemPrompt::Replace);

    let detail = refusal(&REFERENCE.replacen("system_prompt: append", "system_prompt: drop", 1));
    assert!(
        detail.contains("unknown variant `drop`") && detail.contains("append"),
        "{detail}"
    );
}

#[test]
fn the_window_of_an_llm_backend_model_must_be_positive() {
    let zero = REFERENCE.replacen("context_window: 32768", "context_window: 0", 1);
    assert!(joined_problems(&zero).contains("greater than 0"));
}

#[test]
fn an_agent_naming_an_undefined_backend_is_fatal_and_lists_the_real_ones() {
    let doc = REFERENCE.replace("backend: openai-remote", "backend: opanai-remote");
    let joined = joined_problems(&doc);
    assert!(
        joined.contains("LLM backend 'opanai-remote' is not defined")
            && joined.contains("openai-local, openai-remote"),
        "{joined}"
    );
}

#[test]
fn a_backend_no_agent_uses_is_not_an_error() {
    let doc = REFERENCE.replace("backend: openai-remote", "backend: openai-local");
    assert!(team(&doc).is_ok());
}

/// The keys serde accepts for `T`, read from its unknown-field error.
fn accepted_keys<T: serde::de::DeserializeOwned>() -> Vec<String> {
    let err = serde_norway::from_str::<T>("zzz_unknown: 1")
        .err()
        .expect("an unknown key is refused")
        .to_string();
    let list = err.split("expected").nth(1).expect("the error lists keys");
    let mut keys: Vec<String> = list
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    keys.sort();
    keys
}

fn schema() -> serde_json::Value {
    serde_json::from_str(SCHEMA).expect("the schema is JSON")
}

/// The keys a schema object lists, less `type`, which the enum consumes.
fn schema_keys(node: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = node["properties"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "type")
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Every key of the schema object is required, but `optional` ones.
fn schema_required(node: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = node["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .filter(|k| k != "type")
        .collect();
    keys.sort();
    keys
}

#[test]
fn the_schema_names_exactly_the_keys_the_config_accepts() {
    let schema = schema();
    assert_eq!(
        schema_keys(&schema),
        accepted_keys::<RawConfig>(),
        "top level"
    );
    let defs = &schema["definitions"];
    for (def, keys, label) in [
        ("limit", accepted_keys::<RawLimit>(), "limits"),
        (
            "budget_per_hour_tokens",
            accepted_keys::<RawPerHourTokens>(),
            "per_hour_tokens budget",
        ),
        ("agent", accepted_keys::<RawAgent>(), "agents"),
        ("gate", accepted_keys::<RawGate>(), "gates"),
        (
            "budget_per_hour",
            accepted_keys::<RawPerHour>(),
            "per_hour budget",
        ),
        (
            "claude_code_adapter",
            accepted_keys::<RawClaudeCodeAdapter>(),
            "claude-code adapter",
        ),
        (
            "codex_adapter",
            accepted_keys::<RawCodexAdapter>(),
            "codex adapter",
        ),
        (
            "opencode_adapter",
            accepted_keys::<RawOpenCodeAdapter>(),
            "opencode adapter",
        ),
        (
            "opencode_provider_model",
            accepted_keys::<RawProviderModel>(),
            "opencode provider model",
        ),
        (
            "direct_adapter",
            accepted_keys::<RawDirectAdapter>(),
            "direct adapter",
        ),
        (
            "opencode_llm_backend_model",
            accepted_keys::<RawBackendModel>(),
            "opencode llm_backend model",
        ),
        (
            "opencode_llm_backend_candidate",
            accepted_keys::<RawBackendCandidate>(),
            "opencode llm_backend model's try entry",
        ),
        (
            "direct_candidate",
            accepted_keys::<RawDirectCandidate>(),
            "direct adapter's try entry",
        ),
        (
            "builtin_adapter",
            accepted_keys::<RawBuiltinAdapter>(),
            "builtin adapter",
        ),
        (
            "builtin_candidate",
            accepted_keys::<RawBuiltinCandidate>(),
            "builtin adapter's try entry",
        ),
    ] {
        assert_eq!(schema_keys(&defs[def]), keys, "{label}");
    }
}

#[test]
fn the_schema_requires_every_key_but_the_optional_two() {
    let schema = schema();
    let defs = &schema["definitions"];
    assert_eq!(
        schema_required(&schema),
        accepted_keys::<RawConfig>(),
        "top level"
    );
    let optional = |def: &str, keys: &[&str]| {
        let mut all = schema_keys(&defs[def]);
        all.retain(|k| !keys.contains(&k.as_str()));
        all
    };
    for (def, keys) in [
        ("agent", vec!["prompt", "max_concurrent", "budget", "limit"]),
        ("limit", vec![]),
        ("budget_per_hour_tokens", vec![]),
        ("gate", vec![]),
        ("budget_unlimited", vec![]),
        ("budget_per_hour", vec![]),
        ("claude_code_adapter", vec![]),
        ("codex_adapter", vec![]),
        ("opencode_adapter", vec![]),
        ("opencode_provider_model", vec![]),
        ("direct_adapter", vec![]),
        ("opencode_llm_backend_model", vec![]),
        ("opencode_llm_backend_candidate", vec![]),
        ("direct_candidate", vec![]),
    ] {
        assert_eq!(schema_required(&defs[def]), optional(def, &keys), "{def}");
    }
    // And nothing in it says what a missing key would have been.
    assert!(!SCHEMA.contains("\"default\":"), "the schema has a default");
}

#[test]
fn the_schemas_choices_are_the_codes() {
    let schema = schema();
    let defs = &schema["definitions"];
    let tag = |def: &str| defs[def]["properties"]["type"]["const"].clone();
    assert_eq!(tag("claude_code_adapter"), CLAUDE_CODE_AGENT);
    assert_eq!(tag("codex_adapter"), CODEX_AGENT);
    assert_eq!(tag("opencode_adapter"), OPENCODE_AGENT);
    assert_eq!(tag("direct_adapter"), DIRECT_AGENT);
    assert_eq!(tag("opencode_provider_model"), PROVIDER_MODEL);
    assert_eq!(tag("opencode_llm_backend_model"), LLM_BACKEND_MODEL);
    assert_eq!(schema["properties"]["version"]["const"], SUPPORTED_VERSION);
}

#[test]
fn a_dotenv_file_is_read_with_comments_quotes_and_export() {
    let text =
        "# keys\nexport A_KEY=sk-1 # trailing\n\nB=\"two words\"\nC='it#is'\nD=\nE = spaced \n";
    let vars = parse_dotenv(text, Path::new(".env")).unwrap();
    assert_eq!(vars["A_KEY"], "sk-1");
    assert_eq!(vars["B"], "two words");
    assert_eq!(vars["C"], "it#is");
    assert_eq!(vars["D"], "");
    assert_eq!(vars["E"], "spaced");
    assert_eq!(vars.len(), 5);
}

#[test]
fn a_malformed_dotenv_line_is_reported_with_its_number() {
    let Err(ConfigError::Invalid { problems, .. }) =
        parse_dotenv("OK=1\njust words\n9X=1\n", Path::new(".env"))
    else {
        panic!("expected invalid");
    };
    assert_eq!(
        problems,
        [
            "line 2: expected NAME=value",
            "line 3: '9X' is not a variable name"
        ]
    );
}

#[test]
fn an_env_file_fills_in_what_the_environment_lacks() {
    let tmp = ensemble_core::workspace::testing::TempDir::new();
    let path = runner_path(&tmp.0);
    let text = format!(
        "version: 1\nname: laptop\nworks_for: owner\nworkspace: /code\nllm:\n  direct:\n    backends:{RUNNER_BACKENDS}    shares: []\n    max_shared_calls: 1\n"
    );
    std::fs::write(&path, text).unwrap();
    let vars = tmp.0.join(".env");
    std::fs::write(
        &vars,
        "OPENAI_LOCAL_API_KEY=from-file\nOPENAI_REMOTE_API_KEY=remote-file\n",
    )
    .unwrap();
    let key = |r: &Runner, name: &str| r.llm.direct[name].settings.api_key().map(String::from);

    let r = load_runner(&path, Some(&vars), &env(&[])).unwrap().unwrap();
    assert_eq!(key(&r, "openai-local").as_deref(), Some("from-file"));
    assert_eq!(key(&r, "openai-remote").as_deref(), Some("remote-file"));

    // A variable that is really set wins over the file.
    let r = load_runner(
        &path,
        Some(&vars),
        &env(&[("OPENAI_LOCAL_API_KEY", "from-env")]),
    )
    .unwrap()
    .unwrap();
    assert_eq!(key(&r, "openai-local").as_deref(), Some("from-env"));

    // One named that is missing is an error.
    let err = load_runner(&path, Some(&tmp.0.join("nope.env")), &env(&[])).unwrap_err();
    assert!(err.to_string().contains("nope.env"), "{err}");
}

#[test]
fn a_team_reads_no_keys_even_from_a_dotenv_beside_it() {
    let tmp = ensemble_core::workspace::testing::TempDir::new();
    std::fs::write(tmp.0.join(CONFIG_FILENAME), REFERENCE).unwrap();
    std::fs::write(tmp.0.join(".env"), "OPENAI_LOCAL_API_KEY=from-file\n").unwrap();
    let c = load(&config_path(&tmp.0)).unwrap();
    assert!(c.llm_backends.is_empty());
}

/// The sample team on the backends of the sample runner, as `ensemble-runner
/// init --stdout` writes them.
fn sample_team() -> Config {
    let runner = parse_runner(runner::RUNNER_SAMPLE, Path::new("runner.yaml"), &env(&[]))
        .expect("the sample runner loads");
    parse(SAMPLE, &p())
        .and_then(|c| c.with_backends(runner.llm.backends(None)))
        .expect("the sample is a valid config on the sample runner")
}

#[test]
fn only_agents_on_llm_backends_may_work_for_other_accounts() {
    let c = sample_team();
    // OpenCode on an LLM backend, `direct` and `builtin`; never OpenCode on a
    // provider of its own, Codex or Claude Code, which use their CLI's login.
    assert_eq!(c.lendable_agents(), ["coder", "patcher", "summariser"]);
    assert!(!c.agents["explorer"].lendable());
    assert!(!c.agents["fixer"].lendable() && !c.agents["reviewer"].lendable());
}

#[test]
fn the_sample_loads_and_sets_every_setting_the_schema_has() {
    let c = sample_team();
    let kinds: Vec<&str> = c.agents.values().map(|a| a.kind()).collect();
    for kind in [
        CLAUDE_CODE_AGENT,
        OPENCODE_AGENT,
        CODEX_AGENT,
        DIRECT_AGENT,
        BUILTIN_AGENT,
    ] {
        assert!(
            kinds.contains(&kind),
            "the sample has no {kind} agent: {kinds:?}"
        );
    }
    assert!(
        c.agents.values().any(|a| a.llm_backend().is_some()),
        "the sample has no agent on an LLM backend"
    );
    assert!(
        c.agents
            .values()
            .any(|a| a.kind() == OPENCODE_AGENT && a.llm_backend().is_none()),
        "the sample has no agent on a provider"
    );

    let doc: serde_json::Value = serde_norway::from_str(SAMPLE).unwrap();
    let schema = schema();
    let defs = &schema["definitions"];
    let missing = |def: &str, objects: Vec<&serde_json::Value>| -> Vec<String> {
        assert!(!objects.is_empty(), "the sample has no {def}");
        schema_keys(&defs[def])
            .into_iter()
            .filter(|k| !objects.iter().any(|o| o.get(k).is_some()))
            .collect()
    };
    let agents: Vec<&serde_json::Value> = doc["agents"].as_object().unwrap().values().collect();
    let adapters = |kind: &str| -> Vec<&serde_json::Value> {
        agents
            .iter()
            .map(|a| &a["adapter"])
            .filter(|a| a["type"] == kind)
            .collect()
    };
    let budgets = |kind: &str| -> Vec<&serde_json::Value> {
        agents
            .iter()
            .map(|a| &a["budget"])
            .filter(|b| b["type"] == kind)
            .collect()
    };
    let models = |kind: &str| -> Vec<&serde_json::Value> {
        adapters(OPENCODE_AGENT)
            .into_iter()
            .map(|a| &a["model"])
            .filter(|m| m["type"] == kind)
            .collect()
    };
    for (def, objects) in [
        ("agent", agents.clone()),
        ("budget_unlimited", budgets("unlimited")),
        ("budget_per_hour", budgets("per_hour")),
        ("claude_code_adapter", adapters(CLAUDE_CODE_AGENT)),
        ("codex_adapter", adapters(CODEX_AGENT)),
        ("opencode_adapter", adapters(OPENCODE_AGENT)),
        ("direct_adapter", adapters(DIRECT_AGENT)),
        ("builtin_adapter", adapters(BUILTIN_AGENT)),
        (
            "builtin_candidate",
            adapters(BUILTIN_AGENT)
                .into_iter()
                .flat_map(|a| a["try"].as_array().unwrap())
                .collect(),
        ),
        ("opencode_provider_model", models(PROVIDER_MODEL)),
        ("opencode_llm_backend_model", models(LLM_BACKEND_MODEL)),
        (
            "opencode_llm_backend_candidate",
            models(LLM_BACKEND_MODEL)
                .into_iter()
                .flat_map(|m| m["try"].as_array().unwrap())
                .collect(),
        ),
        (
            "direct_candidate",
            adapters(DIRECT_AGENT)
                .into_iter()
                .flat_map(|a| a["try"].as_array().unwrap())
                .collect(),
        ),
    ] {
        assert_eq!(missing(def, objects), Vec::<String>::new(), "{def}");
    }
    assert!(
        models(LLM_BACKEND_MODEL)
            .iter()
            .any(|m| m["try"].as_array().unwrap().len() > 1),
        "the sample shows no agent with a fallback model"
    );
    let id = schema["$id"].as_str().unwrap().to_string();
    assert!(SAMPLE.starts_with(&format!("# yaml-language-server: $schema={id}")));
}

#[test]
fn a_claude_code_agent_is_what_its_adapter_says() {
    let doc = REFERENCE.replace(
            "      tools: [Read, Grep, Glob]\n      permission_mode: dontAsk\n      max_turns: 30\n      command: claude",
            "      tools: [Read, Edit, \"Bash(git diff:*)\"]\n      permission_mode: acceptEdits\n      max_turns: 9\n      command: /opt/claude",
        );
    let c = team(&doc).unwrap();
    let f = claude(&c, "reviewer");
    assert_eq!(f.tools, ["Read", "Edit", "Bash(git diff:*)"]);
    assert_eq!(
        (f.permission_mode.as_str(), f.max_turns),
        ("acceptEdits", 9)
    );
    assert_eq!(f.command, "/opt/claude");
    assert!(c.agents["reviewer"].can_change_files());
    assert_eq!(c.agents["reviewer"].runs_on(), CLAUDE_CODE_AGENT);

    // Read-only is what the config says, not something that is assumed.
    let r = REFERENCE.replace("tools: [Read, Grep, Glob]", "tools: []");
    let c = team(&r).unwrap();
    assert!(!c.agents["reviewer"].has_tools());
}

#[test]
fn a_claude_code_agent_takes_one_tool_per_entry_and_a_positive_turn_count() {
    let doc = REFERENCE
        .replace("[Read, Grep, Glob]", "[\"Read,Edit\", Grep]")
        .replace("max_turns: 30", "max_turns: 0")
        .replace("command: claude", "command: \" \"")
        .replace("permission_mode: dontAsk", "permission_mode: \"\"");
    let joined = joined_problems(&doc);
    for expected in [
        "one Claude Code tool or rule per list entry",
        "`max_turns` must be greater than 0",
        "`command` is empty",
        "`permission_mode` is empty",
    ] {
        assert!(joined.contains(expected), "{expected}: {joined}");
    }
}

#[test]
fn an_opencode_agent_checks_its_fields() {
    let doc = r#"
version: 1
limits: {}
gates: {}
agents:
  doer:
    max_seconds: 600
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: { type: provider, name: "ollama/qwen3.5:9b" }
      tools: [read, edit, "bash(git *)"]
      system_prompt: append
      max_steps: 12
      command: /opt/opencode
"#;
    let c = team(doc).unwrap();
    let doer = opencode(&c, "doer");
    assert_eq!(doer.tools, ["read", "edit", "bash(git *)"]);
    assert_eq!(doer.max_steps, 12);
    assert_eq!(doer.command, "/opt/opencode");
    assert!(c.agents["doer"].can_change_files());

    let bad = r#"
version: 1
limits: {}
gates: {}
agents:
  x:
    max_seconds: 60
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: { type: provider, name: big-pickle }
      tools: ["read,edit"]
      system_prompt: append
      max_steps: 0
      command: opencode
"#;
    let joined = joined_problems(bad);
    assert!(
        joined.contains("must be provider/model") && joined.contains("or run on an LLM backend"),
        "{joined}"
    );
    assert!(
        joined.contains("tool 'read,edit' must be an OpenCode permission"),
        "{joined}"
    );
    assert!(
        joined.contains("`max_steps` must be greater than 0"),
        "{joined}"
    );
}

#[test]
fn on_an_llm_backend_the_model_is_the_endpoints_own_id() {
    // No provider prefix: Ensemble names the provider.
    let c = team(REFERENCE).unwrap();
    assert_eq!(opencode(&c, "planner").model_id(), "qwen3-coder");
}

#[test]
fn an_unknown_adapter_type_lists_the_known_ones() {
    let doc = REFERENCE.replace("type: claude-code", "type: aider");
    let detail = refusal(&doc);
    assert!(
        detail.contains("agents.reviewer.adapter")
            && detail.contains("unknown variant `aider`")
            && detail.contains("claude-code")
            && detail.contains("opencode"),
        "{detail}"
    );
}

#[test]
fn isolation_is_shared_or_worktree() {
    let doc = REFERENCE.replacen("isolation: shared", "isolation: worktree", 1);
    let c = team(&doc).unwrap();
    assert_eq!(c.agents["planner"].isolation, Isolation::Worktree);
    let bad = REFERENCE.replacen("isolation: shared", "isolation: branch", 1);
    let detail = refusal(&bad);
    assert!(
        detail.contains("unknown variant `branch`") && detail.contains("worktree"),
        "{detail}"
    );
}

#[test]
fn a_budget_is_unlimited_or_a_number_of_runs_per_hour() {
    let c = team(REFERENCE).unwrap();
    assert_eq!(c.agents["coder"].budget, Budget::Unlimited);
    let doc = REFERENCE.replacen(
        "    budget:\n      type: unlimited\n",
        "    budget:\n      type: per_hour\n      delegations: 5\n",
        1,
    );
    let c = team(&doc).unwrap();
    assert_eq!(
        c.agents["planner"].budget,
        Budget::PerHour { delegations: 5 }
    );
    let zero = doc.replace("delegations: 5", "delegations: 0");
    assert!(joined_problems(&zero).contains("`delegations` must be greater than 0"));
    let stray = REFERENCE.replacen(
        "      type: unlimited\n",
        "      type: unlimited\n      delegations: 5\n",
        1,
    );
    assert!(refusal(&stray).contains("delegations"));
    let missing = REFERENCE.replacen("      type: unlimited\n", "      type: per_hour\n", 1);
    assert!(
        refusal(&missing).contains("`delegations` is required"),
        "{}",
        refusal(&missing)
    );
}

#[test]
fn an_llm_agent_is_one_request_to_a_defined_backend() {
    let doc = format!(
        "{REFERENCE}  summariser:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: direct\n      try: [{{ backend: openai-local, model: small }}]\n      when_none: fail\n      max_tokens: 400\n"
    );
    let c = team(&doc).unwrap();
    let a = &c.agents["summariser"];
    assert_eq!(
        (a.kind(), a.model(), a.llm_backend()),
        (DIRECT_AGENT, "small", Some("openai-local"))
    );
    assert_eq!(a.runs_on(), "direct on openai-local");
    assert!(!a.has_tools() && !a.can_change_files());
    let bad = doc.replace(
        "try: [{ backend: openai-local, model: small }]\n      when_none: fail\n      max_tokens: 400",
        "try: [{ backend: openai-local, model: small }]\n      when_none: fail\n      max_tokens: 0",
    );
    let joined = joined_problems(&bad);
    assert!(
        joined.contains("`max_tokens` must be greater than 0"),
        "{joined}"
    );
    let nowhere = doc.replace(
        "backend: openai-local, model: small",
        "backend: nowhere, model: small",
    );
    let joined = joined_problems(&nowhere);
    assert!(
        joined.contains("LLM backend 'nowhere' is not defined"),
        "{joined}"
    );
    let tools = doc.replace(
        "      max_tokens: 400\n",
        "      max_tokens: 400\n      tools: [read]\n",
    );
    assert!(refusal(&tools).contains("tools"));
}

#[test]
fn max_concurrent_must_be_positive() {
    let zero = REFERENCE.replacen("max_concurrent: 1", "max_concurrent: 0", 1);
    assert!(joined_problems(&zero).contains("`max_concurrent` must be greater than 0"));
}

#[test]
fn max_seconds_must_be_positive() {
    let zero = REFERENCE.replace("max_seconds: 900", "max_seconds: 0");
    assert!(joined_problems(&zero).contains("`max_seconds` must be greater than 0"));
    let c = team(REFERENCE).unwrap();
    assert_eq!(c.agents["coder"].max_seconds, 900);
}

#[test]
fn the_base_url_is_the_one_written_in_the_runner() {
    // No variable overrides it: the file is the whole truth.
    let r = runner_on(
        RUNNER_BACKENDS,
        &env(&[("OPENAI_LOCAL_BASE_URL", "http://192.168.1.9:9000/v1")]),
    )
    .unwrap();
    assert_eq!(
        r.llm.direct["openai-local"].settings.base_url(),
        "http://127.0.0.1:8000/v1"
    );
}

#[test]
fn api_keys_are_read_from_the_environment_never_the_file() {
    let r = runner_on(
        RUNNER_BACKENDS,
        &env(&[("OPENAI_LOCAL_API_KEY", "sk-secret")]),
    )
    .unwrap();
    assert_eq!(
        r.llm.direct["openai-local"].settings.api_key(),
        Some("sk-secret")
    );
    // And an unset one is simply absent, not an empty bearer token.
    assert_eq!(r.llm.direct["openai-remote"].settings.api_key(), None);
    // A backend may name no key at all.
    assert_eq!(r.llm.direct["home"].settings.api_key(), None);
}

#[test]
fn an_inline_api_key_is_refused_by_name() {
    let detail = runner_refusal(
        "      x: { type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5, api_key: sk-oops }",
    );
    assert!(
        detail.contains("api_key"),
        "must name the offending key: {detail}"
    );
}

#[test]
fn the_removed_url_settings_are_unknown() {
    for key in [
        "base_url_env: U",
        "base_url_default: \"http://127.0.0.1:1/v1\"",
    ] {
        let detail = runner_refusal(&format!(
            "      x: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5, {key} }}"
        ));
        assert!(detail.contains("unknown field"), "{detail}");
    }
}

#[test]
fn an_agent_with_an_empty_model_is_fatal() {
    let doc = REFERENCE.replace("model: sonnet", "model: \"  \"");
    let joined = joined_problems(&doc);
    assert!(
        joined.contains("agents.reviewer.adapter: `model` is empty"),
        "{joined}"
    );
}

#[test]
fn an_unknown_backend_type_lists_the_known_ones() {
    let detail = runner_refusal("      x: { type: ollama }");
    assert!(detail.contains("llm.direct.backends.x"), "{detail}");
    assert!(detail.contains("ollama"), "{detail}");
    assert!(detail.contains(OPENAI_COMPATIBLE_API), "{detail}");
}

#[test]
fn a_base_url_without_a_scheme_or_a_zero_timeout_is_refused() {
    let said = runner_problems(
        "      x: { type: openai-compatible-api, base_url: \"127.0.0.1:8000/v1\", timeout_seconds: 0 }",
    );
    for wanted in [
        "llm.direct.backends.x: `base_url` '127.0.0.1:8000/v1' must start with http://",
        "llm.direct.backends.x: `timeout_seconds` must be greater than 0",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn trailing_slashes_are_normalised_away() {
    let r = runner_on(
        "      x: { type: openai-compatible-api, base_url: \"http://127.0.0.1:8000/v1/\", timeout_seconds: 5 }",
        &env(&[]),
    )
    .unwrap();
    assert_eq!(
        r.llm.direct["x"].settings.base_url(),
        "http://127.0.0.1:8000/v1"
    );
}

#[test]
fn an_unsupported_version_is_refused() {
    let yaml = "version: 9\nlimits: {}\ngates: {}\nagents: {}\n";
    assert!(joined_problems(yaml).contains("version"));
}

#[test]
fn a_typo_in_an_adapter_key_is_an_error_not_a_default() {
    let doc = REFERENCE.replace("      model: sonnet", "      modle: sonnet");
    let detail = refusal(&doc);
    assert!(detail.contains("modle"), "{detail}");
}

#[test]
fn every_independent_problem_is_reported_in_one_pass() {
    let yaml = r#"
version: 9
limits: {}
gates: {}
agents:
  r:
    max_seconds: 0
    max_concurrent: 1
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: { type: llm_backend, try: [{ backend: missing, name: "", context_window: 0 }], when_none: fail }
      tools: []
      system_prompt: append
      max_steps: 1
      command: opencode
"#;
    let problems = problems_of(team(yaml).unwrap_err());
    assert!(problems.len() >= 4, "expected several, got: {problems:#?}");
}

#[test]
fn agents_are_found_by_name() {
    let c = team(REFERENCE).unwrap();
    assert_eq!(c.agent("coder").unwrap().name, "coder");
    assert!(c.agent("nope").is_none());
}

#[test]
fn a_prompt_is_optional_and_absent_means_none() {
    let c = team(REFERENCE).unwrap();
    assert_eq!(c.agents["coder"].prompt, None);
    let with = REFERENCE.replacen(
            "  coder:\n    max_seconds: 900\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n",
            "  coder:\n    prompt: Writes code.\n    max_seconds: 900\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n",
            1,
        );
    let c = team(&with).unwrap();
    assert_eq!(c.agents["coder"].prompt.as_deref(), Some("Writes code."));
}

/// A helper team of a `coder` and a `planner`, with one gate.
fn flat_team() -> String {
    let agent = "    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      model: { type: provider, name: opencode/m }\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n";
    format!(
        "version: 1\nlimits: {{}}\ngates:\n  test: {{ command: make test, timeout_seconds: 5 }}\nagents:\n  coder:\n{agent}  planner:\n{agent}"
    )
}

#[test]
fn agents_and_gates_are_found_by_their_plain_names() {
    let c = team(&flat_team()).unwrap();
    assert_eq!(c.agent_names(), vec!["coder", "planner"]);
    assert_eq!(c.agent("planner").unwrap().name, "planner");
    assert!(c.agent("software/planner").is_none());
    assert!(c.gate("test").is_some() && c.gate("drc").is_none());
}

#[test]
fn agent_and_gate_names_must_not_contain_a_slash() {
    let doc = flat_team().replacen("  coder:\n", "  co/der:\n", 1);
    let joined = joined_problems(&doc);
    assert!(joined.contains("co/der"), "{joined}");
    let doc = flat_team().replacen("  test:", "  te/st:", 1);
    let joined = joined_problems(&doc);
    assert!(joined.contains("te/st"), "{joined}");
}

#[test]
fn another_version_is_refused() {
    let doc = flat_team().replacen("version: 1", "version: 2", 1);
    let joined = team(&doc).unwrap_err().to_string();
    assert!(joined.contains("version: 2 is not supported"), "{joined}");
}

/// A document with one shared limit and two agents, written out in full.
fn with_limits(agent_a: &str, agent_b: &str) -> String {
    let adapter = "    isolation: shared\n    adapter: { type: opencode, model: { type: provider, name: opencode/m }, tools: [read], system_prompt: append, max_steps: 3, command: opencode }\n";
    format!(
        "version: 1\nlimits:\n  server:\n    max_concurrent: 1\n    budget: {{ type: per_hour_tokens, tokens: 5000 }}\ngates: {{}}\nagents:\n  a:\n    max_seconds: 60\n{agent_a}{adapter}  b:\n    max_seconds: 60\n{agent_b}{adapter}"
    )
}

#[test]
fn agents_can_share_a_named_limit_or_write_their_own_but_not_mix() {
    let shared = "    limit: server\n";
    let own = "    max_concurrent: 3\n    budget: { type: per_hour, delegations: 4 }\n";
    let c = team(&with_limits(shared, own)).unwrap();
    let a = &c.agents["a"];
    assert_eq!(a.limit.as_deref(), Some("server"));
    assert_eq!(a.max_concurrent, 1);
    assert_eq!(a.budget, Budget::PerHourTokens { tokens: 5000 });
    let b = &c.agents["b"];
    assert_eq!(b.limit, None);
    assert_eq!(
        (b.max_concurrent, b.budget),
        (3, Budget::PerHour { delegations: 4 })
    );
    assert_eq!(c.limits["server"].max_concurrent, 1);

    for (doc, needle) in [
        (
            with_limits("    limit: server\n    max_concurrent: 2\n", own),
            "not a mix",
        ),
        (
            with_limits("    limit: server\n    budget: { type: unlimited }\n", own),
            "not a mix",
        ),
        (with_limits("    max_concurrent: 2\n", own), "not a mix"),
        (with_limits("", own), "not neither"),
        (
            with_limits("    limit: nope\n", own),
            "limit 'nope' is not defined. Defined limits: server",
        ),
    ] {
        let joined = joined_problems(&doc);
        assert!(joined.contains(needle), "{needle}: {joined}");
    }
}

#[test]
fn a_limit_needs_real_numbers() {
    let doc = with_limits("    limit: server\n", "    limit: server\n").replace(
        "max_concurrent: 1\n    budget: { type: per_hour_tokens, tokens: 5000 }",
        "max_concurrent: 0\n    budget: { type: per_hour_tokens, tokens: 0 }",
    );
    let joined = joined_problems(&doc);
    assert!(
        joined.contains("limits.server: `max_concurrent` must be greater than 0"),
        "{joined}"
    );
    assert!(
        joined.contains("limits.server: `tokens` must be greater than 0"),
        "{joined}"
    );
}

#[test]
fn a_codex_agent_is_what_its_adapter_says_and_its_sandbox_is_one_codex_has() {
    let doc = REFERENCE.replace(
        "      type: claude-code\n      model: sonnet\n      tools: [Read, Grep, Glob]\n      permission_mode: dontAsk\n      max_turns: 30\n      command: claude",
        "      type: codex\n      model: gpt-5-codex\n      sandbox: workspace-write\n      command: /opt/codex",
    );
    let c = team(&doc).unwrap();
    let r = &c.agents["reviewer"];
    assert_eq!((r.kind(), r.model()), (CODEX_AGENT, "gpt-5-codex"));
    assert!(r.can_change_files() && r.has_tools());
    assert_eq!(r.runs_on(), CODEX_AGENT);
    match &r.adapter {
        Adapter::Codex(cx) => assert_eq!(
            (cx.command.as_str(), cx.sandbox.as_str()),
            ("/opt/codex", "workspace-write")
        ),
        other => panic!("{other:?}"),
    }
    let ro = team(&doc.replace("workspace-write", "read-only")).unwrap();
    assert!(!ro.agents["reviewer"].can_change_files());

    let bad = joined_problems(&doc.replace("workspace-write", "anything"));
    assert!(bad.contains("sandbox 'anything' is unknown. Sandboxes: read-only, workspace-write, danger-full-access"), "{bad}");
    let empty = joined_problems(&doc.replace("model: gpt-5-codex", "model: \"\""));
    assert!(empty.contains("`model` is empty"), "{empty}");
    let stray = refusal(&doc.replace(
        "sandbox: workspace-write",
        "sandbox: workspace-write\n      max_turns: 3",
    ));
    assert!(stray.contains("max_turns"), "{stray}");
}

#[test]
fn an_anthropic_backend_loads_and_only_an_llm_agent_may_run_on_it() {
    let r = runner_on(RUNNER_BACKENDS, &env(&[("ANTHROPIC_API_KEY", "sk-ant")])).unwrap();
    let b = &r.llm.direct["claude-api"];
    assert_eq!(b.type_id(), ANTHROPIC_API);
    assert_eq!(b.settings.base_url(), "https://api.anthropic.com");
    assert_eq!(b.settings.api_key(), Some("sk-ant"));
    assert_eq!(b.timeout_seconds, 90);

    // An llm agent may name it.
    let llm = REFERENCE.replace(
        "      type: opencode\n      model:\n        type: llm_backend\n        try:\n          - { backend: openai-remote, name: qwen3-coder, context_window: 32768 }\n        when_none: fail\n      tools: [read, edit, bash]\n      system_prompt: append\n      max_steps: 30\n      command: opencode",
        "      type: direct\n      try: [{ backend: claude-api, model: claude-haiku-4-5 }]\n      when_none: fail\n      max_tokens: 500",
    );
    let c = team(&llm).unwrap();
    assert_eq!(c.agents["coder"].llm_backend(), Some("claude-api"));

    // An OpenCode agent may not: it needs an openai-compatible one.
    let oc = REFERENCE.replace(
        "{ backend: openai-remote, name: qwen3-coder,",
        "{ backend: claude-api, name: claude-haiku-4-5,",
    );
    let joined = joined_problems(&oc);
    assert!(
        joined.contains("'claude-api' is of type 'anthropic-api', which OpenCode cannot use"),
        "{joined}"
    );
}

// ---------------------------------------------------------------------------
// hub.yaml
// ---------------------------------------------------------------------------

fn hub_schema() -> serde_json::Value {
    serde_json::from_str(hub::HUB_SCHEMA).expect("the hub schema is JSON")
}

const HUB: &str = "version: 1\nowner: Ana\nhttp: { listen: \"127.0.0.1:7700\" }\nllm:\n  direct:\n    backends:\n      local: { type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }\naccounts:\n  registration: { type: approval, max_pending: 2 }\n  sessions: { hours: 1 }\n  lockout: { failures: 3, minutes: 1 }\n  roles:\n    member: [share, recall]\n    reader: [recall]\nconnectors:\n  rest: {}\n  mcp: {}\n  runners: {}\n  knowledge:\n    vault: vault\n    distill:\n      try:\n        - { backend: local, model: small }\n      when_none: wait\n      max_tokens: 100\n    recall: { max_notes: 3, max_chars: 1000 }\n  tasks: { trackers: {} }\n  sources: { adapters: {} }\n";

fn hub_refusal(doc: &str) -> String {
    parse_hub(doc, Path::new("/hubs/ana/hub.yaml"), &env(&[]))
        .unwrap_err()
        .to_string()
}

#[test]
fn an_embedding_model_is_an_openai_compatible_backend_and_a_similarity_from_0_to_1() {
    let with = |embed: &str| {
        HUB.replace(
            "    recall: { max_notes: 3, max_chars: 1000 }\n",
            &format!("    embed: {embed}\n    recall: {{ max_notes: 3, max_chars: 1000 }}\n"),
        )
    };
    let hub = parse_hub(
        &with("{ backend: local, model: nomic-embed-text, min_similarity: 0.45 }"),
        Path::new("/hubs/ana/hub.yaml"),
        &env(&[]),
    )
    .unwrap();
    let embed = hub.connectors.knowledge.embed.unwrap();
    assert_eq!(
        (embed.backend.as_str(), embed.model.as_str()),
        ("local", "nomic-embed-text")
    );
    assert!((embed.min_similarity() - 0.45).abs() < 1e-6);
    assert!(
        parse_hub(HUB, Path::new("hub.yaml"), &env(&[]))
            .unwrap()
            .connectors
            .knowledge
            .embed
            .is_none(),
        "no block, words alone"
    );
    let err = hub_refusal(&with(
        "{ backend: elsewhere, model: e, min_similarity: 0.5 }",
    ));
    assert!(
        err.contains("'elsewhere' is not one of the backends of `llm`"),
        "{err}"
    );
    let err = hub_refusal(&with("{ backend: local, model: e, min_similarity: 2 }"));
    assert!(err.contains("must be from 0 to 1"), "{err}");
    let anthropic = with("{ backend: claude, model: e, min_similarity: 0.5 }").replace(
        "    backends:\n",
        "    backends:\n      claude: { type: anthropic-api, base_url: \"https://api.anthropic.com\", timeout_seconds: 5 }\n",
    );
    let err = hub_refusal(&anthropic);
    assert!(
        err.contains("not an openai-compatible-api backend"),
        "{err}"
    );
}

#[test]
fn a_hub_names_its_owner_its_backends_and_its_knowledge() {
    let hub = parse_hub(HUB, Path::new("/hubs/ana/hub.yaml"), &env(&[])).unwrap();
    assert_eq!(hub.owner, "Ana");
    let k = hub.connectors.knowledge;
    assert_eq!(
        k.vault,
        PathBuf::from("/hubs/ana/vault"),
        "relative to the hub"
    );
    assert_eq!(k.distill.backend(), "local");
    assert_eq!(k.distill.fallback.when_none, WhenNone::Wait);
    assert_eq!(k.recall.max_notes, 3);
    assert!(
        hub.connectors.tasks.trackers.is_empty(),
        "no tracker, no inbox"
    );
    let without = HUB.replace("  knowledge:", "  knowledgee:");
    assert!(hub_refusal(&without).contains("unknown field `knowledgee`"));
}

#[test]
fn a_hub_is_checked_like_a_team_and_every_problem_is_named() {
    let said = hub_refusal(
        &HUB.replace("owner: Ana", "owner: \" \"")
            .replace("backend: local", "backend: nowhere")
            .replace("max_notes: 3", "max_notes: 0")
            .replace("version: 1", "version: 2"),
    );
    for expected in [
        "owner: is empty",
        "connectors.knowledge.distill.try[0]: LLM backend 'nowhere' is not defined",
        "connectors.knowledge.recall: `max_notes` must be greater than 0",
        "this build understands hub version 1",
    ] {
        assert!(said.contains(expected), "{expected} in {said}");
    }
    let said = hub_refusal(&HUB.replace("    recall:", "    recal:"));
    assert!(said.contains("unknown field `recal`"), "{said}");
    let said = hub_refusal(&HUB.replace("owner: Ana\n", ""));
    assert!(said.contains("`owner` is required"), "{said}");
}

#[test]
fn every_connector_is_written_and_none_can_be_left_out() {
    for connector in ["rest", "mcp", "runners", "tasks", "sources"] {
        let doc: String = HUB
            .lines()
            .filter(|l| !l.starts_with(&format!("  {connector}:")))
            .map(|l| format!("{l}\n"))
            .collect();
        let said = hub_refusal(&doc);
        assert!(
            said.contains(&format!("`{connector}` is required")),
            "{connector}: {said}"
        );
    }
    let said = hub_refusal(&HUB.replace("  mcp: {}", "  mcp: { url: \"http://x\" }"));
    assert!(said.contains("unknown field `url`"), "{said}");
    let said = hub_refusal(&HUB.replace(
        "  sources: { adapters: {} }",
        "  sources: { adapters: { tg: { type: telegram } } }",
    ));
    assert!(
        said.contains("connectors.sources.adapters.tg: no source adapter is built yet"),
        "{said}"
    );
}

#[test]
fn a_hub_laid_out_the_old_way_is_told_where_each_block_went() {
    let old = "version: 1\nowner: Ana\nserver: { listen: \"127.0.0.1:7700\" }\nroles: { member: [share] }\ninterfaces: { cli: {} }\n";
    let said = hub_refusal(old);
    assert!(said.contains("server, roles, interfaces:"), "{said}");
    assert!(said.contains("`server.listen` is `http.listen`"), "{said}");
}

#[test]
fn a_hub_inbox_names_its_tracker_and_label() {
    let doc = HUB.replace(
        "  tasks: { trackers: {} }\n",
        "  tasks:\n    trackers:\n      issues: { type: local_markdown, dir: issues, label: ensemble, poll_seconds: 30 }\n",
    );
    let hub = parse_hub(&doc, Path::new("/hubs/ana/hub.yaml"), &env(&[])).unwrap();
    let tracker = &hub.connectors.tasks.trackers["issues"];
    assert_eq!(
        tracker.kind,
        IssueTracker::LocalMarkdown {
            dir: PathBuf::from("/hubs/ana/issues")
        }
    );
    assert_eq!(
        (tracker.label.as_str(), tracker.poll_seconds),
        ("ensemble", 30)
    );
    let said = hub_refusal(
        &doc.replace("label: ensemble", "label: \"ens:x\"")
            .replace("poll_seconds: 30", "poll_seconds: 0"),
    );
    for expected in [
        "connectors.tasks.trackers.issues.label: 'ens:x' must be one word",
        "connectors.tasks.trackers.issues: `poll_seconds` must be greater than 0",
    ] {
        assert!(said.contains(expected), "{expected} in {said}");
    }
    let said = hub_refusal(&doc.replace("type: local_markdown", "type: jira"));
    assert!(said.contains("unknown variant `jira`"), "{said}");
    let two = doc.replace(
        "      issues: {",
        "      more: { type: local_markdown, dir: more, label: ensemble, poll_seconds: 30 }\n      issues: {",
    );
    let hub = parse_hub(&two, Path::new("/hubs/ana/hub.yaml"), &env(&[])).unwrap();
    let names: Vec<&String> = hub.connectors.tasks.trackers.keys().collect();
    assert_eq!(names, ["issues", "more"], "a hub watches each");
    let same = two.replace("dir: more", "dir: issues");
    let said = hub_refusal(&same);
    assert!(
        said.contains("connectors.tasks.trackers.more: watches /hubs/ana/issues, as issues does"),
        "{said}"
    );
}

#[test]
fn a_hub_listens_somewhere_and_its_accounts_name_their_roles() {
    let hub = parse_hub(HUB, Path::new("hub.yaml"), &env(&[])).unwrap();
    assert_eq!(hub.http.listen.port(), 7700);
    let rules = hub.accounts.rules;
    assert_eq!(
        rules.registration,
        Registration::Approval { max_pending: 2 }
    );
    assert_eq!(
        (
            rules.session_hours,
            rules.lockout_failures,
            rules.lockout_minutes
        ),
        (1, 3, 1)
    );
    let said = hub_refusal(
        &HUB.replace("max_pending: 2", "max_pending: 0")
            .replace("hours: 1", "hours: 0")
            .replace("failures: 3", "failures: 0"),
    );
    assert!(
        said.contains("accounts.registration: `max_pending` must be greater than 0"),
        "{said}"
    );
    assert!(said.contains("accounts.sessions: `hours`"), "{said}");
    assert!(said.contains("accounts.lockout:"), "{said}");
    let closed = HUB.replace("{ type: approval, max_pending: 2 }", "{ type: closed }");
    let hub_closed = parse_hub(&closed, Path::new("hub.yaml"), &env(&[])).unwrap();
    assert_eq!(hub_closed.accounts.rules.registration, Registration::Closed);
    let said = hub_refusal(&HUB.replace("{ type: approval, max_pending: 2 }", "{ type: open }"));
    assert!(said.contains("unknown variant `open`"), "{said}");
    assert_eq!(hub.accounts.roles["reader"], vec![Permission::Recall]);

    let said = hub_refusal(&HUB.replace("[recall]\n", "[recall, delete]\n"));
    assert!(said.contains("unknown variant `delete`"), "{said}");
    let said = hub_refusal(&HUB.replace("127.0.0.1:7700", "localhost"));
    assert!(said.contains("http.listen: 'localhost'"), "{said}");
    let no_roles = HUB.replace(
        "  roles:\n    member: [share, recall]\n    reader: [recall]\n",
        "",
    );
    assert!(hub_refusal(&no_roles).contains("`roles` is required"));
    let said = hub_refusal(&HUB.replace(
        "  roles:\n    member: [share, recall]\n    reader: [recall]\n",
        "  roles: {}\n",
    ));
    assert!(
        said.contains("accounts.roles: name at least one role"),
        "{said}"
    );
}

#[test]
fn a_missing_hub_file_says_how_to_make_one() {
    let err = load_hub(Path::new("/nonexistent/hub.yaml"), None, &env(&[]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("ensemble-hub init"), "{err}");
}

#[test]
fn the_hub_schema_names_exactly_the_keys_the_hub_accepts() {
    let schema = hub_schema();
    let defs = &schema["definitions"];
    assert_eq!(
        schema_keys(&schema),
        accepted_keys::<hub::RawHub>(),
        "top level"
    );
    assert_eq!(
        schema_required(&schema),
        accepted_keys::<hub::RawHub>(),
        "nothing at the top may be left out"
    );
    let permissions: Vec<String> = defs["permission"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect();
    let parsed: Vec<&str> = permissions
        .iter()
        .map(|p| serde_norway::from_str::<Permission>(p).unwrap().as_str())
        .collect();
    assert_eq!(
        permissions, parsed,
        "the schema's permissions are the code's"
    );
    for (def, keys, optional) in [
        ("http", accepted_keys::<hub::RawHttp>(), vec![]),
        ("accounts", accepted_keys::<hub::RawAccounts>(), vec![]),
        ("connectors", accepted_keys::<hub::RawConnectors>(), vec![]),
        ("sources", accepted_keys::<hub::RawSources>(), vec![]),
        (
            "registration_approval",
            accepted_keys::<hub::RawApproval>(),
            vec![],
        ),
        ("sessions", accepted_keys::<hub::RawSessions>(), vec![]),
        ("lockout", accepted_keys::<hub::RawLockout>(), vec![]),
        ("tasks", accepted_keys::<hub::RawTasks>(), vec![]),
        (
            "tracker_local_markdown",
            accepted_keys::<hub::RawLocalMarkdownIssues>(),
            vec![],
        ),
        ("distill", accepted_keys::<hub::RawDistill>(), vec![]),
        (
            "knowledge",
            accepted_keys::<hub::RawKnowledge>(),
            vec!["embed"],
        ),
        ("recall", accepted_keys::<hub::RawRecall>(), vec![]),
        ("embed", accepted_keys::<hub::RawEmbed>(), vec![]),
        ("llm", accepted_keys::<llm::RawHubLlm>(), vec!["runners"]),
        ("direct", accepted_keys::<llm::RawDirect>(), vec![]),
        (
            "relayed_group",
            accepted_keys::<llm::RawRelayedGroup>(),
            vec![],
        ),
        ("relayed", accepted_keys::<llm::RawRelayed>(), vec![]),
        (
            "direct_candidate",
            accepted_keys::<RawDirectCandidate>(),
            vec![],
        ),
        (
            "openai_compatible_api_backend",
            accepted_keys::<RawOpenAiBackend>(),
            vec!["api_key_env"],
        ),
        (
            "anthropic_api_backend",
            accepted_keys::<RawAnthropicBackend>(),
            vec!["api_key_env"],
        ),
    ] {
        assert_eq!(schema_keys(&defs[def]), keys, "{def}");
        let mut required = keys.clone();
        required.retain(|k| !optional.contains(&k.as_str()));
        assert_eq!(schema_required(&defs[def]), required, "{def} required");
    }
    assert_eq!(
        defs["no_settings"],
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        "a connector with no settings is `{{}}`"
    );
    assert_eq!(
        schema["properties"]["version"]["const"],
        hub::HUB_SUPPORTED_VERSION
    );
    assert!(
        !hub::HUB_SCHEMA.contains("\"default\":"),
        "the schema has a default"
    );
}

#[test]
fn the_hub_sample_loads_and_sets_every_setting_the_schema_has() {
    let hub = parse_hub(hub::HUB_SAMPLE, Path::new("hub.yaml"), &env(&[])).unwrap();
    assert!(
        hub.connectors.knowledge.distill.fallback.candidates.len() > 1,
        "the sample shows a fallback"
    );
    assert!(hub.accounts.roles["member"].contains(&Permission::Tasks));
    let doc: serde_json::Value = serde_norway::from_str(hub::HUB_SAMPLE).unwrap();
    let schema = hub_schema();
    let defs = &schema["definitions"];
    let backends: Vec<&serde_json::Value> = doc["llm"]["direct"]["backends"]
        .as_object()
        .unwrap()
        .values()
        .collect();
    let of_type = |kind: &str| -> Vec<&serde_json::Value> {
        backends
            .iter()
            .copied()
            .filter(|b| b["type"] == kind)
            .collect()
    };
    let connectors = &doc["connectors"];
    let knowledge = &connectors["knowledge"];
    let tasks = &connectors["tasks"];
    assert_eq!(hub.connectors.tasks.trackers["issues"].label, "ensemble");
    for (def, objects) in [
        ("http", vec![&doc["http"]]),
        ("accounts", vec![&doc["accounts"]]),
        ("connectors", vec![connectors]),
        ("tasks", vec![tasks]),
        ("sources", vec![&connectors["sources"]]),
        (
            "registration_approval",
            vec![&doc["accounts"]["registration"]],
        ),
        ("sessions", vec![&doc["accounts"]["sessions"]]),
        ("lockout", vec![&doc["accounts"]["lockout"]]),
        (
            "tracker_local_markdown",
            tasks["trackers"].as_object().unwrap().values().collect(),
        ),
        ("knowledge", vec![knowledge]),
        ("distill", vec![&knowledge["distill"]]),
        ("recall", vec![&knowledge["recall"]]),
        ("embed", vec![&knowledge["embed"]]),
        ("llm", vec![&doc["llm"]]),
        ("direct", vec![&doc["llm"]["direct"]]),
        ("relayed_group", vec![&doc["llm"]["runners"]]),
        (
            "relayed",
            doc["llm"]["runners"]["backends"]
                .as_object()
                .unwrap()
                .values()
                .collect(),
        ),
        (
            "direct_candidate",
            knowledge["distill"]["try"]
                .as_array()
                .unwrap()
                .iter()
                .collect(),
        ),
        (
            "openai_compatible_api_backend",
            of_type(OPENAI_COMPATIBLE_API),
        ),
        ("anthropic_api_backend", of_type(ANTHROPIC_API)),
    ] {
        assert!(!objects.is_empty(), "the sample has no {def}");
        let missing: Vec<String> = schema_keys(&defs[def])
            .into_iter()
            .filter(|k| !objects.iter().any(|o| o.get(k).is_some()))
            .collect();
        assert_eq!(missing, Vec::<String>::new(), "{def}");
    }
    let id = schema["$id"].as_str().unwrap();
    assert!(hub::HUB_SAMPLE.starts_with(&format!("# yaml-language-server: $schema={id}")));
}

// ---------------------------------------------------------------------------
// runner.yaml

// ---------------------------------------------------------------------------

#[test]
fn a_runner_says_whom_it_works_for_and_its_sample_and_schema_agree() {
    let runner = parse_runner(runner::RUNNER_SAMPLE, Path::new("runner.yaml"), &env(&[])).unwrap();
    assert_eq!(runner.hub.as_deref(), Some("wss://hub.example.com"));
    assert_eq!(
        (runner.name.as_str(), runner.works_for),
        ("laptop", WorksFor::Owner)
    );
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    assert_eq!(runner.workspace, home.join("ensemble"), "~ is the home");
    assert_eq!(runner.llm.shares, ["local"]);
    assert_eq!(runner.llm.max_shared_calls, 1);
    assert_eq!(
        runner.llm.hub["team-gpu"],
        Relayed {
            runner: "ana/gpu-box".into(),
            backend: "local".into()
        }
    );

    let problems = |yaml: &str| {
        parse_runner(yaml, Path::new("runner.yaml"), &env(&[]))
            .unwrap_err()
            .to_string()
    };
    const NO_LLM: &str = "llm: { direct: { backends: {}, shares: [], max_shared_calls: 1 } }\n";
    let said = problems(&format!(
        "version: 1\nname: a/b\nworks_for: hub\nworkspace: \"\"\nhub: http://hub\n{NO_LLM}"
    ));
    for wanted in [
        "name: 'a/b'",
        "workspace: is empty",
        "hub: 'http://hub' must start with ws://",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
    let said = problems(&format!(
        "version: 1\nname: box\nworks_for: hub\nworkspace: /code\n{NO_LLM}"
    ));
    assert!(
        said.contains("works_for: a runner works for the accounts of a hub, and there is no `hub`"),
        "{said}"
    );
    let said = problems(&format!(
        "version: 1\nname: box\nworks_for: everyone\nworkspace: /code\n{NO_LLM}"
    ));
    assert!(said.contains("owner") && said.contains("hub"), "{said}");
    let said = problems(&format!(
        "version: 1\nname: box\nworkspace: /code\n{NO_LLM}"
    ));
    assert!(said.contains("works_for"), "nothing has a default: {said}");
    let said = problems("version: 1\nname: box\nworks_for: owner\nworkspace: /code\n");
    assert!(said.contains("`llm` is required"), "{said}");
    let bare = parse_runner(
        &format!("version: 1\nname: box\nworks_for: owner\nworkspace: work\n{NO_LLM}"),
        Path::new("/home/ana/.config/ensemble/runner/runner.yaml"),
        &env(&[]),
    )
    .unwrap();
    assert_eq!(
        bare.workspace,
        Path::new("/home/ana/.config/ensemble/runner/work"),
        "a relative path is beside the file"
    );
    assert!(bare.llm.direct.is_empty() && bare.llm.hub.is_empty() && bare.llm.shares.is_empty());
    assert!(
        load_runner(Path::new("/nonexistent/runner.yaml"), None, &env(&[]))
            .unwrap()
            .is_none()
    );
    let schema: serde_json::Value = serde_json::from_str(runner::RUNNER_SCHEMA).unwrap();
    assert_eq!(schema_keys(&schema), accepted_keys::<runner::RawRunner>());
    let mut required = accepted_keys::<runner::RawRunner>();
    required.retain(|k| k != "hub");
    assert_eq!(schema_required(&schema), required);
    let defs = &schema["definitions"];
    assert_eq!(
        schema["properties"]["works_for"]["enum"],
        serde_json::json!(["owner", "hub"])
    );
    for (def, keys, optional) in [
        ("llm", accepted_keys::<llm::RawRunnerLlm>(), vec!["hub"]),
        ("direct", accepted_keys::<llm::RawRunnerDirect>(), vec![]),
        (
            "relayed_group",
            accepted_keys::<llm::RawRelayedGroup>(),
            vec![],
        ),
        ("relayed", accepted_keys::<llm::RawRelayed>(), vec![]),
        (
            "openai_compatible_api_backend",
            accepted_keys::<RawOpenAiBackend>(),
            vec!["api_key_env"],
        ),
        (
            "anthropic_api_backend",
            accepted_keys::<RawAnthropicBackend>(),
            vec!["api_key_env"],
        ),
    ] {
        assert_eq!(schema_keys(&defs[def]), keys, "{def}");
        let mut required = keys.clone();
        required.retain(|k| !optional.contains(&k.as_str()));
        assert_eq!(schema_required(&defs[def]), required, "{def} required");
    }
    let tag = |def: &str| defs[def]["properties"]["type"]["const"].clone();
    assert_eq!(tag("openai_compatible_api_backend"), OPENAI_COMPATIBLE_API);
    assert_eq!(tag("anthropic_api_backend"), ANTHROPIC_API);
    // The sample sets every setting of a backend.
    let doc: serde_json::Value = serde_norway::from_str(runner::RUNNER_SAMPLE).unwrap();
    let backends: Vec<&serde_json::Value> = doc["llm"]["direct"]["backends"]
        .as_object()
        .unwrap()
        .values()
        .collect();
    for (def, kind) in [
        ("openai_compatible_api_backend", OPENAI_COMPATIBLE_API),
        ("anthropic_api_backend", ANTHROPIC_API),
    ] {
        let of_type: Vec<_> = backends.iter().filter(|b| b["type"] == kind).collect();
        let missing: Vec<String> = schema_keys(&defs[def])
            .into_iter()
            .filter(|k| !of_type.iter().any(|o| o.get(k).is_some()))
            .collect();
        assert_eq!(missing, Vec::<String>::new(), "{def}");
    }
    let id = schema["$id"].as_str().unwrap();
    assert!(runner::RUNNER_SAMPLE.starts_with(&format!("# yaml-language-server: $schema={id}")));
}

/// A runner of `hub` with the `llm` block `llm`.
fn runner_llm(llm: &str, hub: bool) -> Result<Runner, ConfigError> {
    let hub = if hub {
        "hub: wss://hub.example.com\n"
    } else {
        ""
    };
    parse_runner(
        &format!("version: 1\nname: box\nworks_for: owner\nworkspace: /code\n{hub}llm:\n{llm}"),
        Path::new("runner.yaml"),
        &env(&[]),
    )
}

#[test]
fn a_runner_shares_its_openai_compatible_backends_and_reaches_others_through_its_hub() {
    let llm = "  direct:\n    backends:\n      local: { type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }\n      claude: { type: anthropic-api, base_url: \"https://api.anthropic.com\", timeout_seconds: 5 }\n    shares: [local]\n    max_shared_calls: 2\n  hub:\n    backends:\n      gpu: { runner: ana/gpu-box, backend: local }\n";
    let r = runner_llm(llm, true).unwrap();
    assert_eq!(r.llm.shared().len(), 1);
    assert_eq!(r.llm.shared()[0].name, "local");
    // Without a relay to serve them at, the hub's backends are not there yet.
    assert_eq!(
        r.llm.backends(None).keys().collect::<Vec<_>>(),
        ["claude", "local"]
    );
    let at = RelayAt {
        base_url: "http://127.0.0.1:4000".into(),
        key: "k".into(),
    };
    let all = r.llm.backends(Some(&at));
    let gpu = &all["gpu"];
    assert_eq!(gpu.type_id(), OPENAI_COMPATIBLE_API);
    assert_eq!(gpu.settings.base_url(), "http://127.0.0.1:4000/gpu");
    assert_eq!(gpu.settings.api_key(), Some("k"));

    let said = runner_llm(llm, false).unwrap_err().to_string();
    assert!(
        said.contains("llm.direct.shares: a runner offers its backends through a hub"),
        "{said}"
    );
    assert!(
        said.contains("llm.hub: these backends are reached through a hub"),
        "{said}"
    );

    let said = runner_llm(
        &llm.replace("shares: [local]", "shares: [claude, nowhere, local, local]")
            .replace("max_shared_calls: 2", "max_shared_calls: 0")
            .replace("gpu: { runner: ana/gpu-box", "local: { runner: gpu-box"),
        true,
    )
    .unwrap_err()
    .to_string();
    for wanted in [
        "llm.direct.shares: 'claude' is not an openai-compatible-api backend",
        "llm.direct.shares: 'nowhere' is not one of llm.direct.backends",
        "llm.direct.shares: 'local' is listed twice",
        "llm.direct: `max_shared_calls` must be greater than 0",
        "llm.hub.backends.local: llm.direct has a backend 'local' too",
        "llm.hub.backends.local.runner: 'gpu-box' must be a runner as the hub knows it",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn a_hub_reaches_backends_runners_share_and_names_them_alone() {
    let doc = HUB
        .replace(
            "accounts:\n",
            "  runners:\n    backends:\n      gpu: { runner: ana/gpu-box, backend: local }\naccounts:\n",
        )
        .replace(
            "        - { backend: local, model: small }\n",
            "        - { backend: gpu, model: big }\n        - { backend: local, model: small }\n",
        )
        .replace(
            "    recall: { max_notes: 3, max_chars: 1000 }\n",
            "    embed: { backend: gpu, model: e, min_similarity: 0.5 }\n    recall: { max_notes: 3, max_chars: 1000 }\n",
        );
    let hub = parse_hub(&doc, Path::new("hub.yaml"), &env(&[])).unwrap();
    assert_eq!(hub.llm.runners["gpu"].runner, "ana/gpu-box");
    assert_eq!(hub.connectors.knowledge.distill.backend(), "gpu");
    assert_eq!(
        hub.llm.backends(None).len(),
        1,
        "its own alone, until it serves the rest"
    );
    let said = hub_refusal(&doc.replace("gpu: { runner", "local: { runner"));
    assert!(
        said.contains("llm.runners.backends.local: llm.direct has a backend 'local' too"),
        "{said}"
    );
}

#[test]
fn a_builtin_agent_runs_on_llm_backends_with_tools_it_has() {
    let doc = format!(
        "{REFERENCE}  fixer:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: builtin\n      try:\n        - backend: openai-local\n          model: small\n          context_window: 32768\n      when_none: fail\n      tools: [read, grep]\n      max_steps: 10\n      max_tokens: 400\n"
    );
    let c = team(&doc).unwrap();
    let a = &c.agents["fixer"];
    assert_eq!(
        (a.kind(), a.model(), a.llm_backend()),
        (BUILTIN_AGENT, "small", Some("openai-local"))
    );
    assert_eq!(a.runs_on(), "builtin on openai-local");
    assert_eq!(a.context_window(), Some(32768));
    assert!(a.has_tools() && !a.can_change_files());
    let writes = doc.replace("tools: [read, grep]", "tools: [read, edit]");
    assert!(team(&writes).unwrap().agents["fixer"].can_change_files());

    let bad = doc
        .replace("tools: [read, grep]", "tools: [read, read, browse]")
        .replace("context_window: 32768", "context_window: 0")
        .replace("max_steps: 10", "max_steps: 0");
    let joined = joined_problems(&bad);
    for expected in [
        "tool 'browse' is not one a builtin agent has",
        "tool 'read' is listed twice",
        "`context_window` must be greater than 0",
        "`max_steps` must be greater than 0",
    ] {
        assert!(joined.contains(expected), "{expected}: {joined}");
    }
}

#[test]
fn a_runner_has_no_interfaces_block_as_its_command_line_is_always_on() {
    let err = parse_runner(
        "version: 1\nname: laptop\nworks_for: owner\nworkspace: /code\nllm: { direct: { backends: {}, shares: [], max_shared_calls: 1 } }\ninterfaces: { cli: {} }\n",
        Path::new("runner.yaml"),
        &env(&[]),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("interfaces: is gone"), "{err}");
}

#[test]
fn a_key_is_kept_in_dotenv_with_the_other_lines_as_they_were() {
    let dir = std::env::temp_dir().join(format!("ensemble-dotenv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join(".env");
    set_dotenv(&path, "ENSEMBLE_BACKEND_CENTRAL_KEY", Some("sk-1")).unwrap();
    std::fs::write(
        &path,
        format!(
            "# mine\nOTHER=x\n{}",
            std::fs::read_to_string(&path).unwrap()
        ),
    )
    .unwrap();
    let tricky = "a b#c\"d\\e";
    set_dotenv(&path, "ENSEMBLE_BACKEND_CENTRAL_KEY", Some(tricky)).unwrap();
    set_dotenv(&path, "NEW", Some("plain-1")).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# mine\nOTHER=x\n"), "{text}");
    let vars = parse_dotenv(&text, &path).unwrap();
    assert_eq!(vars["ENSEMBLE_BACKEND_CENTRAL_KEY"], tricky);
    assert_eq!(vars["NEW"], "plain-1");
    set_dotenv(&path, "ENSEMBLE_BACKEND_CENTRAL_KEY", None).unwrap();
    let vars = parse_dotenv(&std::fs::read_to_string(&path).unwrap(), &path).unwrap();
    assert!(!vars.contains_key("ENSEMBLE_BACKEND_CENTRAL_KEY"));
    assert_eq!(vars["OTHER"], "x");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "readable by its owner alone");
    }
    assert_eq!(
        backend_key_variable("team-gpu.2"),
        "ENSEMBLE_BACKEND_TEAM_GPU_2_KEY"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mapping_is_written_inline_as_yaml_reads_it_back() {
    let value = serde_json::json!({
        "type": "local_markdown",
        "dir": "my issues: \"x\"",
        "poll_seconds": 30,
        "on": true,
        "list": ["a", "b"],
    });
    let line = settings::inline(&value);
    let back: serde_json::Value = serde_norway::from_str(&line).unwrap();
    assert_eq!(back, value);
}
