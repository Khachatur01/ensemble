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

/// OpenCode agents on two providers, and one Claude Code agent. Every
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
        type: ann_pool
        try:
          - { provider: openai-local, name: qwen3-coder, context_window: 32768 }
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
        type: ann_pool
        try:
          - { provider: openai-remote, name: qwen3-coder, context_window: 32768 }
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

/// The providers of the runner the teams of these tests run on: two
/// OpenAI-compatible ones of REFERENCE's, two more and an Anthropic one of
/// `with_fallback`'s, and Anthropic's API as `claude-api`.
const RUNNER_BACKENDS: &str = r#"
      openai-local: { type: openai-compatible-api, base_url: "http://127.0.0.1:8000/v1", api_key_env: OPENAI_LOCAL_API_KEY, timeout_seconds: 600, max_concurrent: 1 }
      openai-remote: { type: openai-compatible-api, base_url: "http://10.10.10.5:8000/v1", api_key_env: OPENAI_REMOTE_API_KEY, timeout_seconds: 300, max_concurrent: 1 }
      home: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 5, max_concurrent: 1 }
      central: { type: openai-compatible-api, base_url: "http://127.0.0.1:2/v1", timeout_seconds: 5, max_concurrent: 1 }
      claude: { type: anthropic-api, base_url: "https://api.anthropic.com", timeout_seconds: 5, max_concurrent: 1 }
      claude-api: { type: anthropic-api, base_url: "https://api.anthropic.com/", api_key_env: ANTHROPIC_API_KEY, timeout_seconds: 90, max_concurrent: 1 }
"#;

/// A runner without a hub whose providers are `providers`, the lines of a YAML
/// mapping indented six spaces, or `{}`, its keys read from `env`.
fn runner_on(providers: &str, env: &dyn Env) -> Result<Runner, ConfigError> {
    let providers: BTreeMap<String, ProviderSetting> =
        serde_norway::from_str(providers).map_err(|e| ConfigError::Syntax {
            path: PathBuf::from("database.db"),
            detail: e.to_string(),
        })?;
    RunnerSettings {
        ann_pool: AnnPoolSetting {
            providers,
            models: Vec::new(),
        },
        ..RunnerSettings::starting("laptop", "/code")
    }
    .resolve(&ensemble_core::state::RunnerDir::new("/r"), env)
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

/// The problems of the runner whose providers are `providers`.
fn runner_problems(providers: &str) -> String {
    problems_of(runner_on(providers, &env(&[])).unwrap_err()).join("\n")
}

/// The message of the runner whose providers serde refuses.
fn runner_refusal(providers: &str) -> String {
    match runner_on(providers, &env(&[])).unwrap_err() {
        ConfigError::Syntax { detail, .. } => detail,
        other => panic!("expected Syntax, got: {other}"),
    }
}

#[test]
fn the_reference_config_loads() {
    let c = parse(REFERENCE, &p()).expect("reference config must load");
    assert!(c.llm_backends.is_empty(), "a team defines no provider");
    let c = team(REFERENCE).unwrap();
    assert_eq!(c.version, 1);
    assert_eq!(c.llm_backends.len(), 6, "the runner's, every one");
    assert_eq!(c.llm_backends["openai-remote"].timeout_seconds, 300);
    assert_eq!(c.agent_names(), vec!["coder", "planner", "reviewer"]);
    let coder = &c.agents["coder"];
    assert_eq!(coder.provider(), Some("openai-remote"));
    assert_eq!(coder.runs_on(), "opencode on openai-remote");
    assert_eq!(coder.context_window(), Some(32768));
    assert_eq!(coder.max_seconds, 900);
    assert!(coder.can_change_files());
    assert_eq!(c.agents["reviewer"].provider(), None);
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
            "{ provider: openai-remote, name: qwen3-coder, context_window: 32768 }",
            "agents.coder",
            "context_window",
        ),
    ] {
        // Put back what follows the line, so only the one key is gone.
        let kept = match key {
            "max_steps" => "      command: opencode\n  reviewer:",
            "system_prompt" => "      max_steps: 30\n      command: opencode\n  reviewer:",
            "context_window" => "{ provider: openai-remote, name: qwen3-coder }",
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
fn a_config_without_its_agents_is_refused_and_one_defining_sources_too() {
    let detail = refusal("version: 1\nlimits: {}\ngates: {}\n");
    assert!(detail.contains("`agents` is required"), "{detail}");
    let detail = refusal("version: 1\nlimits: {}\nagents: {}\n");
    assert!(detail.contains("`gates` is required"), "{detail}");
    assert!(team("version: 1\nlimits: {}\ngates: {}\nagents: {}\n").is_ok());
    // A team names providers; the runner keeps them.
    let detail = refusal("version: 1\nllm_backends: {}\nlimits: {}\ngates: {}\nagents: {}\n");
    assert!(detail.contains("unknown field `llm_backends`"), "{detail}");
}

#[test]
fn a_source_missing_a_setting_is_refused_by_name_and_place() {
    for (provider, key) in [
        (
            "      x: { type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\" }",
            "timeout_seconds",
        ),
        (
            "      x: { type: openai-compatible-api, timeout_seconds: 5, max_concurrent: 1 }",
            "base_url",
        ),
    ] {
        let detail = runner_refusal(provider);
        assert!(
            detail.contains(&format!("missing field `{key}`")),
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

    // The window belongs to a model of the pool, nowhere else.
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
fn an_opencode_model_is_a_provider_model_or_a_model_of_the_pool() {
    let doc = |model: &str| {
        format!(
            "version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      model: {model}\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n"
        )
    };
    let c = team(&doc("{ type: provider, name: opencode/big-pickle }")).unwrap();
    let oc = opencode(&c, "x");
    assert_eq!(oc.model_id(), "opencode/big-pickle");
    assert_eq!((oc.provider(), oc.context_window()), (None, None));

    let detail = refusal(&doc("{ type: local }"));
    assert!(
        detail.contains("unknown variant `local`")
            && detail.contains("provider")
            && detail.contains("ann_pool"),
        "{detail}"
    );
    let detail = refusal(&doc("opencode/big-pickle"));
    assert!(detail.contains("agents.x"), "{detail}");
}

/// Two openai-compatible providers and an Anthropic one, and one agent of each
/// kind that runs on the pool, with `try` and `when_none` as given.
fn with_fallback(opencode_try: &str, direct_try: &str, when_none: &str) -> String {
    format!(
        "version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n  coder:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: opencode\n      model:\n        type: ann_pool\n        try: {opencode_try}\n        when_none: {when_none}\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n  summariser:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try: {direct_try}\n      when_none: {when_none}\n      max_tokens: 10\n"
    )
}

#[test]
fn an_agent_lists_its_models_in_order_and_says_what_happens_when_none_works() {
    let doc = with_fallback(
        "[{ provider: home, name: small, context_window: 8 }, { provider: central, name: big, context_window: 64 }]",
        "[{ provider: home, model: small }, { provider: claude, model: haiku }]",
        "wait",
    );
    let c = team(&doc).unwrap();
    let coder = &c.agents["coder"];
    assert_eq!(coder.pool_models(), [("home", "small"), ("central", "big")]);
    assert_eq!(coder.fallback().unwrap().when_none, WhenNone::Wait);
    assert_eq!(
        (coder.provider(), coder.model(), coder.context_window()),
        (Some("home"), "small", Some(8)),
        "what it says it runs on is its first model"
    );
    let bound = coder.bound_to(1);
    assert_eq!(
        (bound.provider(), bound.model(), bound.context_window()),
        (Some("central"), "big", Some(64))
    );
    assert_eq!(bound.pool_models().len(), 1);
    let summariser = &c.agents["summariser"];
    assert_eq!(
        summariser.pool_models(),
        [("home", "small"), ("claude", "haiku")]
    );
    assert_eq!(summariser.bound_to(1).runs_on(), "direct on claude");
}

#[test]
fn every_model_in_a_try_list_is_checked() {
    let joined = joined_problems(&with_fallback(
        "[]",
        "[{ provider: home, model: small }, { provider: nowhere, model: \"\" }]",
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
    // Its providers are checked against the runner's once the team is valid.
    let joined = joined_problems(&with_fallback(
        "[{ provider: home, name: small, context_window: 8 }, { provider: claude, name: haiku, context_window: 8 }]",
        "[{ provider: home, model: small }, { provider: nowhere, model: big }]",
        "fail",
    ));
    for wanted in [
        "agents.summariser.adapter.try[1]: there is no provider 'nowhere'",
        "agents.coder.adapter.model.try[1]: the provider 'claude' is of type 'anthropic-api'",
    ] {
        assert!(joined.contains(wanted), "{wanted}: {joined}");
    }
    let joined = joined_problems(&with_fallback(
        "[{ provider: home, name: small, context_window: 8 }, { provider: claude, name: haiku, context_window: 0 }]",
        "[{ provider: home, model: small }]",
        "fail",
    ));
    assert!(
        joined
            .contains("agents.coder.adapter.model.try[1]: `context_window` must be greater than 0"),
        "{joined}"
    );
    let detail = refusal(&with_fallback(
        "[{ provider: home, name: small, context_window: 8 }]",
        "[{ provider: home, model: small }]",
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
fn the_window_of_a_model_of_the_pool_must_be_positive() {
    let zero = REFERENCE.replacen("context_window: 32768", "context_window: 0", 1);
    assert!(joined_problems(&zero).contains("greater than 0"));
}

#[test]
fn an_agent_naming_an_undefined_source_is_fatal_and_lists_the_real_ones() {
    let doc = REFERENCE.replace("provider: openai-remote", "provider: opanai-remote");
    let joined = joined_problems(&doc);
    assert!(
        joined.contains("there is no provider 'opanai-remote'")
            && joined.contains("openai-local, openai-remote"),
        "{joined}"
    );
}

#[test]
fn a_source_no_agent_uses_is_not_an_error() {
    let doc = REFERENCE.replace("provider: openai-remote", "provider: openai-local");
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
            "opencode_ann_pool_model",
            accepted_keys::<RawBackendModel>(),
            "opencode ann_pool model",
        ),
        (
            "opencode_ann_pool_candidate",
            accepted_keys::<RawBackendCandidate>(),
            "opencode ann_pool model's try entry",
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
        ("opencode_ann_pool_model", vec![]),
        ("opencode_ann_pool_candidate", vec![]),
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
    assert_eq!(tag("opencode_ann_pool_model"), ANN_POOL_MODEL);
    assert_eq!(schema["properties"]["version"]["const"], SUPPORTED_VERSION);
}

#[test]
fn a_team_reads_no_keys_even_from_a_dotenv_beside_it() {
    let tmp = ensemble_core::workspace::testing::TempDir::new();
    std::fs::write(tmp.0.join(CONFIG_FILENAME), REFERENCE).unwrap();
    std::fs::write(tmp.0.join(".env"), "OPENAI_LOCAL_API_KEY=from-file\n").unwrap();
    let c = load(&config_path(&tmp.0)).unwrap();
    assert!(c.llm_backends.is_empty());
}

/// The sample team on a runner with the providers it names.
fn sample_team() -> Config {
    let runner = runner_on(
        "local: { type: openai-compatible-api, base_url: \"http://127.0.0.1:11434/v1\", timeout_seconds: 600, max_concurrent: 1 }\ncentral: { type: openai-compatible-api, base_url: \"https://llm.example.com/v1\", timeout_seconds: 600, max_concurrent: 1 }",
        &env(&[]),
    )
    .expect("the sample runner loads");
    parse(SAMPLE, &p())
        .and_then(|c| c.with_backends(runner.llm.backends(None)))
        .expect("the sample is a valid config on the sample runner")
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
        c.agents.values().any(|a| a.provider().is_some()),
        "the sample has no agent on the ANN pool"
    );
    assert!(
        c.agents
            .values()
            .any(|a| a.kind() == OPENCODE_AGENT && a.provider().is_none()),
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
        ("opencode_ann_pool_model", models(ANN_POOL_MODEL)),
        (
            "opencode_ann_pool_candidate",
            models(ANN_POOL_MODEL)
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
        models(ANN_POOL_MODEL)
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
        joined.contains("must be provider/model")
            && joined.contains("or run on a model of the ANN pool"),
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
fn in_the_pool_the_model_is_the_sources_own_id() {
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
fn an_llm_agent_is_one_request_to_a_defined_source() {
    let doc = format!(
        "{REFERENCE}  summariser:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: direct\n      try: [{{ provider: openai-local, model: small }}]\n      when_none: fail\n      max_tokens: 400\n"
    );
    let c = team(&doc).unwrap();
    let a = &c.agents["summariser"];
    assert_eq!(
        (a.kind(), a.model(), a.provider()),
        (DIRECT_AGENT, "small", Some("openai-local"))
    );
    assert_eq!(a.runs_on(), "direct on openai-local");
    assert!(!a.has_tools() && !a.can_change_files());
    let bad = doc.replace(
        "try: [{ provider: openai-local, model: small }]\n      when_none: fail\n      max_tokens: 400",
        "try: [{ provider: openai-local, model: small }]\n      when_none: fail\n      max_tokens: 0",
    );
    let joined = joined_problems(&bad);
    assert!(
        joined.contains("`max_tokens` must be greater than 0"),
        "{joined}"
    );
    let nowhere = doc.replace(
        "provider: openai-local, model: small",
        "provider: nowhere, model: small",
    );
    let joined = joined_problems(&nowhere);
    assert!(
        joined.contains("there is no provider 'nowhere'"),
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
    // A provider may name no key at all.
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
fn an_unknown_source_type_lists_the_known_ones() {
    let detail = runner_refusal("      x: { type: ollama }");
    assert!(detail.contains("ollama"), "{detail}");
    assert!(detail.contains(OPENAI_COMPATIBLE_API), "{detail}");
}

#[test]
fn a_base_url_without_a_scheme_or_a_zero_timeout_is_refused() {
    let said = runner_problems(
        "      x: { type: openai-compatible-api, base_url: \"127.0.0.1:8000/v1\", timeout_seconds: 0, max_concurrent: 1 }",
    );
    for wanted in [
        "ann_pool.providers.x: `base_url` '127.0.0.1:8000/v1' must start with http://",
        "ann_pool.providers.x: `timeout_seconds` must be greater than 0",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn trailing_slashes_are_normalised_away() {
    let r = runner_on(
        "      x: { type: openai-compatible-api, base_url: \"http://127.0.0.1:8000/v1/\", timeout_seconds: 5, max_concurrent: 1 }",
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
      model: { type: ann_pool, try: [{ provider: missing, name: "", context_window: 0 }], when_none: fail }
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
fn an_anthropic_source_loads_and_only_an_llm_agent_may_run_on_it() {
    let r = runner_on(RUNNER_BACKENDS, &env(&[("ANTHROPIC_API_KEY", "sk-ant")])).unwrap();
    let b = &r.llm.direct["claude-api"];
    assert_eq!(b.type_id(), ANTHROPIC_API);
    assert_eq!(b.settings.base_url(), "https://api.anthropic.com");
    assert_eq!(b.settings.api_key(), Some("sk-ant"));
    assert_eq!(b.timeout_seconds, 90);

    // An llm agent may name it.
    let llm = REFERENCE.replace(
        "      type: opencode\n      model:\n        type: ann_pool\n        try:\n          - { provider: openai-remote, name: qwen3-coder, context_window: 32768 }\n        when_none: fail\n      tools: [read, edit, bash]\n      system_prompt: append\n      max_steps: 30\n      command: opencode",
        "      type: direct\n      try: [{ provider: claude-api, model: claude-haiku-4-5 }]\n      when_none: fail\n      max_tokens: 500",
    );
    let c = team(&llm).unwrap();
    assert_eq!(c.agents["coder"].provider(), Some("claude-api"));

    // An OpenCode agent may not: it needs an openai-compatible one.
    let oc = REFERENCE.replace(
        "{ provider: openai-remote, name: qwen3-coder,",
        "{ provider: claude-api, name: claude-haiku-4-5,",
    );
    let joined = joined_problems(&oc);
    assert!(
        joined.contains("'claude-api' is of type 'anthropic-api', which OpenCode cannot use"),
        "{joined}"
    );
}

// ---------------------------------------------------------------------------
// The hub's settings
// ---------------------------------------------------------------------------

/// An LLM of the pool, as a test turns it on.
fn llm() -> ModelKind {
    ModelKind::Llm {
        tier: Tier::Medium,
        context_window: None,
        tools: None,
    }
}

/// An OpenAI-compatible server on loopback, as a provider.
fn server(port: u16) -> ProviderSetting {
    ProviderSetting::OpenAiCompatible {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key_env: None,
        timeout_seconds: 5,
        max_concurrent: 1,
    }
}

/// A hub of Ana's with one provider, `local`, which serves `small`, an LLM
/// that is on and distills.
fn hub_settings() -> HubSettings {
    let mut hub = HubSettings::starting("Ana");
    hub.ann_pool.providers.insert("local".into(), server(1));
    hub.ann_pool.turn_on("local", "small", llm());
    hub.distill.candidates = vec![ModelSetting {
        provider: "local".into(),
        model: "small".into(),
    }];
    hub
}

fn hub_problems(hub: &HubSettings) -> String {
    problems_of(
        hub.resolve(&ensemble_core::state::HubDir::new("/hubs/ana"), &env(&[]))
            .unwrap_err(),
    )
    .join("\n")
}

#[test]
fn a_new_hub_starts_on_this_computer_closed_with_no_source_and_loads() {
    let hub = HubSettings::starting("Ana")
        .resolve(&ensemble_core::state::HubDir::new("/hubs/ana"), &env(&[]))
        .unwrap();
    assert_eq!(hub.owner, "Ana");
    assert_eq!(hub.accounts.rules.registration, Registration::Closed);
    assert!(hub.llm.direct.is_empty() && hub.llm.relayed.is_empty());
    let k = &hub.connectors.knowledge;
    assert_eq!(
        k.vault,
        Path::new("/hubs/ana/knowledge/vault"),
        "in the hub's folder"
    );
    assert!(k.distill.fallback.candidates.is_empty(), "shares wait");
    assert_eq!(k.distill.fallback.when_none, WhenNone::Wait);
    assert!(k.embed.is_none() && hub.connectors.sources.filter.is_none());
    assert!(hub.connectors.tasks.trackers.is_empty());
    assert_eq!(hub.accounts.roles["admin"].len(), 4);
}

#[test]
fn a_hub_names_every_problem_of_its_settings() {
    let mut hub = hub_settings();
    hub.owner = " ".into();
    hub.registration = RegistrationSetting::Approval { max_pending: 0 };
    hub.session_hours = 0;
    hub.roles
        .insert("two words".into(), vec![Permission::Recall]);
    hub.distill.candidates[0].provider = "nowhere".into();
    hub.recall_max_notes = 0;
    let said = hub_problems(&hub);
    for wanted in [
        "owner: is empty",
        "registration: `max_pending` must be greater than 0",
        "session_hours: must be greater than 0",
        "the role name 'two words' must be one word",
        "distill.try[0]: there is no provider 'nowhere' in the ANN pool",
        "recall: the most notes",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn a_hub_uses_only_a_model_of_its_pool_that_is_on_and_of_the_kind_it_needs() {
    let mut hub = hub_settings();
    hub.ann_pool
        .turn_on("local", "nomic", ModelKind::Embedding { dimensions: 8 });
    hub.embed = Some(EmbedSetting {
        provider: "local".into(),
        model: "nomic".into(),
        min_similarity: 0.5,
    });
    let resolved = hub
        .resolve(&ensemble_core::state::HubDir::new("/h"), &env(&[]))
        .unwrap();
    assert_eq!(
        resolved
            .connectors
            .knowledge
            .embed
            .unwrap()
            .min_similarity_permille,
        500
    );
    hub.embed = Some(EmbedSetting {
        provider: "local".into(),
        model: "small".into(),
        min_similarity: 1.5,
    });
    hub.distill.candidates.push(ModelSetting {
        provider: "local".into(),
        model: "nomic".into(),
    });
    hub.ann_pool.models.push(PoolModel {
        provider: "local".into(),
        id: "new".into(),
        kind: ModelKind::Unknown,
        on: false,
        missing: false,
        shared: false,
    });
    hub.distill.candidates.push(ModelSetting {
        provider: "local".into(),
        model: "new".into(),
    });
    let said = hub_problems(&hub);
    for wanted in [
        "embed: local:small is not an embedding model: it is llm, medium",
        "must be from 0 to 1",
        "distill.try[1]: local:nomic is not an LLM: it is embedding, 8 dimensions",
        "distill.try[2]: local:new is off; `model on local:new` turns it on",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn a_model_of_the_pool_is_known_on_and_of_one_of_its_sources_once() {
    let mut hub = hub_settings();
    hub.ann_pool.providers.insert(
        "busy".into(),
        ProviderSetting::OpenAiCompatible {
            base_url: "http://127.0.0.1:2/v1".into(),
            api_key_env: None,
            timeout_seconds: 5,
            max_concurrent: 0,
        },
    );
    let unknown = |provider: &str, id: &str, on| PoolModel {
        provider: provider.into(),
        id: id.into(),
        kind: ModelKind::Unknown,
        on,
        missing: false,
        shared: false,
    };
    hub.ann_pool.models.push(unknown("local", "odd", true));
    hub.ann_pool.models.push(unknown("gone", "x", false));
    hub.ann_pool.models.push(PoolModel {
        shared: true,
        ..hub.ann_pool.models[0].clone()
    });
    let said = hub_problems(&hub);
    for wanted in [
        "ann_pool.providers.busy: `max_concurrent` must be greater than 0",
        "ann_pool.models.local:odd: is on, and what it is is not known",
        "ann_pool.models.gone:x: there is no provider 'gone'",
        "ann_pool.models.local:small: is listed twice",
        "ann_pool.models.local:small: a hub shares no model",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn the_source_filter_is_one_of_the_hubs_models_and_the_admins_prompt() {
    let mut hub = hub_settings();
    hub.source_filter = Some(SourceFilterSetting {
        models: hub.distill.clone(),
        prompt: "Salaries.".into(),
    });
    let resolved = hub
        .resolve(&ensemble_core::state::HubDir::new("/h"), &env(&[]))
        .unwrap();
    assert_eq!(
        resolved.connectors.sources.filter.unwrap().prompt,
        "Salaries."
    );
    // Unlike the distiller's, its list is never empty, and it says what is
    // confidential.
    hub.source_filter = Some(SourceFilterSetting {
        models: ModelsSetting {
            candidates: Vec::new(),
            ..hub.distill.clone()
        },
        prompt: " ".into(),
    });
    let said = hub_problems(&hub);
    assert!(said.contains("source_filter: `try` is empty"), "{said}");
    assert!(said.contains("source_filter.prompt: is empty"), "{said}");
}

#[test]
fn a_hub_inbox_names_its_tracker_and_label_and_each_tracker_its_own_folder() {
    let mut hub = hub_settings();
    let tracker = |dir: &str, label: &str| TrackerSetting::LocalMarkdown {
        dir: dir.into(),
        label: label.into(),
        poll_seconds: 30,
    };
    hub.trackers
        .insert("issues".into(), tracker("issues", "ensemble"));
    let resolved = hub
        .resolve(&ensemble_core::state::HubDir::new("/h"), &env(&[]))
        .unwrap();
    let t = &resolved.connectors.tasks.trackers["issues"];
    assert_eq!(
        t.kind,
        IssueTracker::LocalMarkdown {
            dir: PathBuf::from("/h/trackers/issues")
        }
    );
    assert_eq!(t.label, "ensemble");
    hub.trackers
        .insert("again".into(), tracker("issues", "a:b"));
    let said = hub_problems(&hub);
    assert!(
        said.contains("trackers.again.label: 'a:b' must be one word"),
        "{said}"
    );
    assert!(
        said.contains("as again does") || said.contains("as issues does"),
        "{said}"
    );
}

#[test]
fn a_hub_reaches_the_models_a_runner_shares_as_that_runner_names_them() {
    let mut hub = hub_settings();
    hub.ann_pool.providers.insert(
        "gpu".into(),
        ProviderSetting::Runner {
            runner: "ana/gpu-box".into(),
        },
    );
    hub.ann_pool.turn_on("gpu", "local:big", llm());
    hub.distill.candidates.push(ModelSetting {
        provider: "gpu".into(),
        model: "local:big".into(),
    });
    let resolved = hub
        .resolve(&ensemble_core::state::HubDir::new("/h"), &env(&[]))
        .unwrap();
    assert_eq!(resolved.llm.relayed["gpu"], "ana/gpu-box");
    assert!(!resolved.llm.direct.contains_key("gpu"));
    let relay = RelayAt {
        base_url: "http://127.0.0.1:9".into(),
        key: "k".into(),
    };
    assert_eq!(
        resolved.llm.backends(Some(&relay))["gpu"]
            .settings
            .base_url(),
        "http://127.0.0.1:9/gpu"
    );
    hub.ann_pool.providers.insert(
        "bad".into(),
        ProviderSetting::Runner {
            runner: "gpu-box".into(),
        },
    );
    assert!(
        hub_problems(&hub).contains("ann_pool.providers.bad.runner: 'gpu-box' must be a runner")
    );
}

#[test]
fn settings_are_written_as_json_and_an_unknown_one_is_refused() {
    let hub = hub_settings();
    let json = serde_json::to_value(&hub).unwrap();
    assert_eq!(
        json["registration"],
        serde_json::json!({ "type": "closed" })
    );
    assert_eq!(
        json["ann_pool"]["providers"]["local"]["type"],
        "openai-compatible-api"
    );
    assert_eq!(
        json["ann_pool"]["models"][0],
        serde_json::json!({
            "provider": "local",
            "id": "small",
            "kind": { "type": "llm", "tier": "medium", "context_window": null, "tools": null },
            "on": true,
            "missing": false,
            "shared": false,
        })
    );
    assert_eq!(json["distill"]["try"][0]["model"], "small");
    assert_eq!(
        serde_json::from_value::<HubSettings>(json.clone()).unwrap(),
        hub
    );
    let mut stray = json;
    stray["listen_on"] = serde_json::json!("x");
    assert!(serde_json::from_value::<HubSettings>(stray).is_err());
}

// ---------------------------------------------------------------------------
// A runner's settings
// ---------------------------------------------------------------------------

fn runner_with(edit: impl FnOnce(&mut RunnerSettings)) -> Result<Runner, ConfigError> {
    let mut runner = RunnerSettings::starting("laptop", "~/ensemble");
    edit(&mut runner);
    runner.resolve(
        &ensemble_core::state::RunnerDir::new("/home/ana/.local/state/ensemble/runner"),
        &env(&[]),
    )
}

#[test]
fn a_new_runner_works_in_its_workspace_and_loads() {
    let r = runner_with(|_| {}).unwrap();
    assert_eq!(r.name, "laptop");
    assert!(r.workspace.ends_with("ensemble") && r.hub.is_none());
    assert!(r.llm.direct.is_empty() && r.llm.models.is_empty());
    let relative = runner_with(|r| r.workspace = "work".into()).unwrap();
    assert_eq!(
        relative.workspace,
        Path::new("/home/ana/.local/state/ensemble/runner/work"),
        "a relative path is in the runner's folder"
    );
    let said = runner_with(|r| {
        r.name = "a b".into();
        r.hub = Some("https://hub".into());
    })
    .unwrap_err()
    .to_string();
    assert!(said.contains("name: 'a b' must be letters"), "{said}");
    assert!(
        said.contains("hub: 'https://hub' must start with ws://"),
        "{said}"
    );
}

#[test]
fn a_new_runner_has_one_harness_of_each_kind_each_off_and_its_runs_limited() {
    let r = runner_with(|_| {}).unwrap();
    let each = r.harnesses.each();
    assert_eq!(
        each.iter().map(|h| h.kind).collect::<Vec<_>>(),
        HarnessKind::ALL
    );
    assert!(each.iter().all(|h| !h.on), "every harness is off");
    assert_eq!(
        each.iter()
            .map(|h| h.command.as_deref())
            .collect::<Vec<_>>(),
        [Some("claude"), Some("codex"), Some("opencode"), None],
        "builtin runs no program"
    );
    let said = runner_with(|r| {
        r.harnesses.codex.max_seconds = 0;
        r.harnesses.builtin.max_steps = 0;
        r.harnesses.opencode.command = " ".into();
    })
    .unwrap_err()
    .to_string();
    assert!(
        said.contains("harnesses.codex: `max_seconds` must be greater than 0"),
        "{said}"
    );
    assert!(
        said.contains("harnesses.builtin: `max_steps` must be greater than 0"),
        "{said}"
    );
    assert!(
        said.contains("harnesses.opencode: `command` is empty"),
        "{said}"
    );
}

#[test]
fn a_runner_shares_models_of_its_openai_compatible_servers_and_reaches_others_through_its_hub() {
    let claude = ProviderSetting::Anthropic {
        base_url: "https://api.anthropic.com".into(),
        api_key_env: None,
        timeout_seconds: 5,
        max_concurrent: 2,
    };
    let gpu = ProviderSetting::Runner {
        runner: "kim/gpu-box".into(),
    };
    let pool = |r: &mut RunnerSettings| {
        r.ann_pool.providers.insert("local".into(), server(1));
        r.ann_pool.providers.insert("claude".into(), claude.clone());
        r.ann_pool.providers.insert("gpu".into(), gpu.clone());
        r.ann_pool.turn_on("local", "small", llm());
        r.ann_pool.turn_on("local", "other", llm());
        r.ann_pool.turn_on("claude", "haiku", llm());
        r.ann_pool.model_mut("local", "small").unwrap().shared = true;
    };
    let r = runner_with(|r| {
        r.hub = Some("ws://127.0.0.1:7700".into());
        pool(r);
    })
    .unwrap();
    let shared = r.llm.shared();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].provider.name, "local");
    assert_eq!(shared[0].max_concurrent, 1);
    assert_eq!(
        shared[0]
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["small"]
    );
    assert_eq!(r.llm.relayed["gpu"], "kim/gpu-box");
    assert_eq!(
        r.llm.backends(None).len(),
        2,
        "gpu only while it is relayed"
    );
    let said = runner_with(|r| {
        pool(r);
        r.ann_pool.model_mut("claude", "haiku").unwrap().shared = true;
        r.ann_pool.model_mut("local", "other").unwrap().on = false;
        r.ann_pool.model_mut("local", "other").unwrap().shared = true;
    })
    .unwrap_err()
    .to_string();
    for wanted in [
        "ann_pool.models.claude:haiku: is shared, and only a model of an openai-compatible-api provider of the runner's own may be",
        "ann_pool.models.local:other: is shared, and off",
        "ann_pool: a runner shares its models through a hub, and it names no hub",
        "ann_pool: a provider that is another runner is reached through a hub",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn a_team_on_a_runners_pool_names_llms_of_it_that_are_on() {
    let r = runner_with(|r| {
        r.ann_pool.providers.insert("home".into(), server(1));
        r.ann_pool.turn_on("home", "small", llm());
        r.ann_pool
            .turn_on("home", "e", ModelKind::Embedding { dimensions: 4 });
    })
    .unwrap();
    let doc = |model: &str| {
        format!(
            "version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n  s:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter: {{ type: direct, try: [{{ provider: home, model: {model} }}], when_none: fail, max_tokens: 10 }}\n"
        )
    };
    let team = parse(&doc("small"), &p())
        .unwrap()
        .with_pool(&r.llm, None)
        .unwrap();
    assert_eq!(team.llm_backends.len(), 1);
    let said = parse(&doc("e"), &p())
        .unwrap()
        .with_pool(&r.llm, None)
        .unwrap_err()
        .to_string();
    assert!(
        said.contains("agents.s.adapter.try[0]: home:e is not an LLM"),
        "{said}"
    );
    let said = parse(&doc("big"), &p())
        .unwrap()
        .with_pool(&r.llm, None)
        .unwrap_err()
        .to_string();
    assert!(
        said.contains("home:big is not a model of the ANN pool"),
        "{said}"
    );
}

#[test]
fn a_builtin_agent_runs_on_the_pool_with_tools_it_has() {
    let doc = format!(
        "{REFERENCE}  fixer:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: builtin\n      try:\n        - provider: openai-local\n          model: small\n          context_window: 32768\n      when_none: fail\n      tools: [read, grep]\n      max_steps: 10\n      max_tokens: 400\n"
    );
    let c = team(&doc).unwrap();
    let a = &c.agents["fixer"];
    assert_eq!(
        (a.kind(), a.model(), a.provider()),
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
fn a_sources_key_is_named_for_it() {
    assert_eq!(
        provider_key_variable("team-gpu.2"),
        "ENSEMBLE_PROVIDER_TEAM_GPU_2_KEY"
    );
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
