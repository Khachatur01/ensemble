use super::raw::*;
use super::*;

/// The documents below are written in version 1 shape, one team, which is how
/// most of them read best; this turns each into version 2 before loading it.
fn parse(text: &str, path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    let text = migrate::to_v2(text).unwrap_or_else(|_| text.to_string());
    super::parse(&text, path, env)
}

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
    match &c.agents[&format!("default/{agent}")].adapter {
        Adapter::OpenCode(oc) => oc.clone(),
        other => panic!("not an opencode agent: {other:?}"),
    }
}

fn claude(c: &Config, agent: &str) -> ClaudeCodeAdapter {
    match &c.agents[&format!("default/{agent}")].adapter {
        Adapter::ClaudeCode(cc) => cc.clone(),
        other => panic!("not a claude-code agent: {other:?}"),
    }
}

/// OpenCode agents on two LLM backends, and one Claude Code agent. Every
/// setting is written, as it has to be.
const REFERENCE: &str = r#"
version: 1
llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:8000/v1"
    api_key_env: OPENAI_LOCAL_API_KEY
    timeout_seconds: 600
  openai-remote:
    type: openai-compatible-api
    base_url: "http://10.10.10.5:8000/v1"
    api_key_env: OPENAI_REMOTE_API_KEY
    timeout_seconds: 300
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
        backend: openai-local
        name: qwen3-coder
        context_window: 32768
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
        backend: openai-remote
        name: qwen3-coder
        context_window: 32768
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

fn problems_of(err: ConfigError) -> Vec<String> {
    match err {
        ConfigError::Invalid { problems, .. } => problems,
        other => panic!("expected Invalid, got: {other}"),
    }
}

fn joined_problems(doc: &str) -> String {
    problems_of(parse(doc, &p(), &env(&[])).unwrap_err()).join("\n")
}

/// The message of a document that serde refuses.
fn refusal(doc: &str) -> String {
    match parse(doc, &p(), &env(&[])).unwrap_err() {
        ConfigError::Syntax { detail, .. } => detail,
        other => panic!("expected Syntax, got: {other}"),
    }
}

#[test]
fn the_reference_config_loads() {
    let c = parse(REFERENCE, &p(), &env(&[])).expect("reference config must load");
    assert_eq!(c.version, 2);
    assert_eq!(c.llm_backends.len(), 2);
    assert_eq!(c.llm_backends["openai-remote"].timeout_seconds, 300);
    assert_eq!(
        c.agent_names(),
        vec!["default/coder", "default/planner", "default/reviewer"]
    );
    let coder = &c.agents["default/coder"];
    assert_eq!(coder.llm_backend(), Some("openai-remote"));
    assert_eq!(coder.runs_on(), "opencode on openai-remote");
    assert_eq!(coder.context_window(), Some(32768));
    assert_eq!(coder.max_seconds, 900);
    assert!(coder.can_change_files());
    assert_eq!(c.agents["default/reviewer"].llm_backend(), None);
    assert_eq!(c.agents["default/reviewer"].context_window(), None);
}

#[test]
fn the_removed_ensemble_type_points_to_opencode() {
    let doc = REFERENCE.replace("type: claude-code", "type: ensemble");
    let detail = refusal(&doc);
    assert!(
        detail.contains("teams.default.agents.reviewer.adapter")
            && detail.contains("'ensemble' was removed")
            && detail.contains("type 'opencode'"),
        "{detail}"
    );
}

#[test]
fn settings_of_the_removed_harness_are_unknown_fields() {
    let doc = REFERENCE.replace(
        "      max_steps: 30\n      command: opencode\n  coder:",
        "      max_steps: 30\n      temperature: 0.2\n      command: opencode\n  coder:",
    );
    let detail = refusal(&doc);
    assert!(detail.contains("temperature"), "{detail}");
}

#[test]
fn a_missing_setting_is_refused_by_name_and_place() {
    // (the line to take out, where it belongs, its key)
    for (line, at, key) in [
        (
            "    max_seconds: 900\n",
            "teams.default.agents.coder",
            "max_seconds",
        ),
        (
            "      max_steps: 30\n      command: opencode\n  reviewer:",
            "teams.default.agents.coder",
            "max_steps",
        ),
        (
            "      system_prompt: append\n      max_steps: 30\n      command: opencode\n  reviewer:",
            "teams.default.agents.coder",
            "system_prompt",
        ),
        (
            "      max_turns: 30\n",
            "teams.default.agents.reviewer",
            "max_turns",
        ),
        (
            "      command: claude\n",
            "teams.default.agents.reviewer",
            "command",
        ),
        (
            "    timeout_seconds: 300\n",
            "llm_backends",
            "timeout_seconds",
        ),
        (
            "    base_url: \"http://10.10.10.5:8000/v1\"\n",
            "llm_backends",
            "base_url",
        ),
        (
            "        context_window: 32768\n      tools: [read, edit, bash]",
            "teams.default.agents.coder",
            "context_window",
        ),
    ] {
        // Put back what follows the line, so only the one key is gone.
        let kept = match key {
            "max_steps" => "      command: opencode\n  reviewer:",
            "system_prompt" => "      max_steps: 30\n      command: opencode\n  reviewer:",
            "context_window" => "      tools: [read, edit, bash]",
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
fn a_config_without_its_agents_or_backends_is_refused() {
    let detail = refusal("version: 1\ngates: {}\nagents: {}\n");
    assert!(detail.contains("`llm_backends` is required"), "{detail}");
    let detail = refusal("version: 1\nllm_backends: {}\ngates: {}\n");
    assert!(detail.contains("`agents` is required"), "{detail}");
    let detail = refusal("version: 1\nllm_backends: {}\nagents: {}\n");
    assert!(detail.contains("`gates` is required"), "{detail}");
    assert!(
        parse(
            "version: 1\nllm_backends: {}\ngates: {}\nagents: {}\n",
            &p(),
            &env(&[])
        )
        .is_ok()
    );
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
        "version: 1\nllm_backends: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      {provider}      tools: []\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n"
    );
    assert!(refusal(&doc).contains("context_window"));
}

#[test]
fn an_opencode_model_is_a_provider_model_or_an_llm_backend_model() {
    let doc = |model: &str| {
        format!(
            "version: 1\nllm_backends: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      model: {model}\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n"
        )
    };
    let c = parse(
        &doc("{ type: provider, name: opencode/big-pickle }"),
        &p(),
        &env(&[]),
    )
    .unwrap();
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
    assert!(detail.contains("teams.default.agents.x"), "{detail}");
}

#[test]
fn opencode_s_own_prompt_is_kept_or_replaced_and_only_a_prompt_replaces_it() {
    let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
    assert_eq!(opencode(&c, "coder").system_prompt, SystemPrompt::Append);

    let replaced = REFERENCE.replacen("system_prompt: append", "system_prompt: replace", 1);
    let detail = joined_problems(&replaced);
    assert!(
        detail.contains("teams.default.agents.planner.adapter")
            && detail.contains("`system_prompt: replace` needs the agent's `prompt`"),
        "{detail}"
    );
    let with_prompt = replaced.replacen("  planner:\n", "  planner:\n    prompt: You plan.\n", 1);
    let c = parse(&with_prompt, &p(), &env(&[])).unwrap();
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
    assert!(parse(&doc, &p(), &env(&[])).is_ok());
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
        (
            "openai_compatible_api_backend",
            accepted_keys::<RawOpenAiBackend>(),
            "openai-compatible-api backend",
        ),
        ("team", accepted_keys::<RawTeam>(), "teams"),
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
            "llm_adapter",
            accepted_keys::<RawLlmAdapter>(),
            "llm adapter",
        ),
        (
            "opencode_llm_backend_model",
            accepted_keys::<RawBackendModel>(),
            "opencode llm_backend model",
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
        ("openai_compatible_api_backend", vec!["api_key_env"]),
        ("agent", vec!["prompt"]),
        ("team", vec![]),
        ("tracker_local_markdown", vec![]),
        ("gate", vec![]),
        ("budget_unlimited", vec![]),
        ("budget_per_hour", vec![]),
        ("claude_code_adapter", vec![]),
        ("opencode_adapter", vec![]),
        ("opencode_provider_model", vec![]),
        ("llm_adapter", vec![]),
        ("opencode_llm_backend_model", vec![]),
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
    assert_eq!(tag("openai_compatible_api_backend"), OPENAI_COMPATIBLE_API);
    assert_eq!(tag("claude_code_adapter"), CLAUDE_CODE_AGENT);
    assert_eq!(tag("tracker_local_markdown"), "local_markdown");
    assert_eq!(tag("opencode_adapter"), OPENCODE_AGENT);
    assert_eq!(tag("llm_adapter"), LLM_AGENT);
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
    let tmp = crate::workspace::testing::TempDir::new();
    let org = tmp.0.join("org");
    std::fs::create_dir_all(&org).unwrap();
    std::fs::write(
        org.join(CONFIG_FILENAME),
        migrate::to_v2(REFERENCE).unwrap(),
    )
    .unwrap();
    let vars = tmp.0.join("keys.env");
    std::fs::write(
        &vars,
        "OPENAI_LOCAL_API_KEY=from-file\nOPENAI_REMOTE_API_KEY=remote-file\n",
    )
    .unwrap();
    let path = config_path(&org);
    let key = |c: &Config, name: &str| match &c.llm_backends[name].settings {
        BackendSettings::OpenAiCompatible { api_key, .. } => api_key.clone(),
    };

    let c = load(&path, Some(&vars), &env(&[])).unwrap();
    assert_eq!(key(&c, "openai-local").as_deref(), Some("from-file"));
    assert_eq!(key(&c, "openai-remote").as_deref(), Some("remote-file"));

    // A variable that is really set wins over the file.
    let c = load(
        &path,
        Some(&vars),
        &env(&[("OPENAI_LOCAL_API_KEY", "from-env")]),
    )
    .unwrap();
    assert_eq!(key(&c, "openai-local").as_deref(), Some("from-env"));
}

#[test]
fn a_dotenv_beside_the_config_is_not_read() {
    let tmp = crate::workspace::testing::TempDir::new();
    std::fs::write(
        tmp.0.join(CONFIG_FILENAME),
        migrate::to_v2(REFERENCE).unwrap(),
    )
    .unwrap();
    std::fs::write(tmp.0.join(".env"), "OPENAI_LOCAL_API_KEY=from-file\n").unwrap();
    let c = load(&config_path(&tmp.0), None, &env(&[])).unwrap();
    match &c.llm_backends["openai-local"].settings {
        BackendSettings::OpenAiCompatible { api_key, .. } => assert_eq!(*api_key, None),
    }
}

#[test]
fn a_named_env_file_that_is_missing_is_an_error() {
    let tmp = crate::workspace::testing::TempDir::new();
    std::fs::write(
        tmp.0.join(CONFIG_FILENAME),
        migrate::to_v2(REFERENCE).unwrap(),
    )
    .unwrap();
    let err = load(
        &config_path(&tmp.0),
        Some(&tmp.0.join("nope.env")),
        &env(&[]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("nope.env"), "{err}");
}
#[test]
fn the_sample_loads_and_sets_every_setting_the_schema_has() {
    let c = parse(SAMPLE, &p(), &env(&[])).expect("the sample is a valid config");
    let kinds: Vec<&str> = c.agents.values().map(|a| a.kind()).collect();
    for kind in [CLAUDE_CODE_AGENT, OPENCODE_AGENT, LLM_AGENT] {
        assert!(kinds.contains(&kind), "the sample has no {kind} agent");
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
    let backends: Vec<&serde_json::Value> =
        doc["llm_backends"].as_object().unwrap().values().collect();
    let teams: Vec<&serde_json::Value> = doc["teams"].as_object().unwrap().values().collect();
    let trackers: Vec<&serde_json::Value> = teams.iter().map(|t| &t["tracker"]).collect();
    let agents: Vec<&serde_json::Value> = teams
        .iter()
        .flat_map(|t| t["agents"].as_object().unwrap().values())
        .collect();
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
        ("openai_compatible_api_backend", backends),
        ("team", teams.clone()),
        ("tracker_local_markdown", trackers),
        ("agent", agents.clone()),
        ("budget_unlimited", budgets("unlimited")),
        ("budget_per_hour", budgets("per_hour")),
        ("claude_code_adapter", adapters(CLAUDE_CODE_AGENT)),
        ("opencode_adapter", adapters(OPENCODE_AGENT)),
        ("llm_adapter", adapters(LLM_AGENT)),
        ("opencode_provider_model", models(PROVIDER_MODEL)),
        ("opencode_llm_backend_model", models(LLM_BACKEND_MODEL)),
    ] {
        assert_eq!(missing(def, objects), Vec::<String>::new(), "{def}");
    }
    let id = schema["$id"].as_str().unwrap().to_string();
    assert!(SAMPLE.starts_with(&format!("# yaml-language-server: $schema={id}")));
}

#[test]
fn a_claude_code_agent_is_what_its_adapter_says() {
    let doc = REFERENCE.replace(
            "      tools: [Read, Grep, Glob]\n      permission_mode: dontAsk\n      max_turns: 30\n      command: claude",
            "      tools: [Read, Edit, \"Bash(git diff:*)\"]\n      permission_mode: acceptEdits\n      max_turns: 9\n      command: /opt/claude",
        );
    let c = parse(&doc, &p(), &env(&[])).unwrap();
    let f = claude(&c, "reviewer");
    assert_eq!(f.tools, ["Read", "Edit", "Bash(git diff:*)"]);
    assert_eq!(
        (f.permission_mode.as_str(), f.max_turns),
        ("acceptEdits", 9)
    );
    assert_eq!(f.command, "/opt/claude");
    assert!(c.agents["default/reviewer"].can_change_files());
    assert_eq!(c.agents["default/reviewer"].runs_on(), CLAUDE_CODE_AGENT);

    // Read-only is what the config says, not something that is assumed.
    let r = REFERENCE.replace("tools: [Read, Grep, Glob]", "tools: []");
    let c = parse(&r, &p(), &env(&[])).unwrap();
    assert!(!c.agents["default/reviewer"].has_tools());
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
llm_backends: {}
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
    let c = parse(doc, &p(), &env(&[])).unwrap();
    let doer = opencode(&c, "doer");
    assert_eq!(doer.tools, ["read", "edit", "bash(git *)"]);
    assert_eq!(doer.max_steps, 12);
    assert_eq!(doer.command, "/opt/opencode");
    assert!(c.agents["default/doer"].can_change_files());

    let bad = r#"
version: 1
llm_backends: {}
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
    let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
    assert_eq!(opencode(&c, "planner").model_id(), "qwen3-coder");
}

#[test]
fn an_unknown_adapter_type_lists_the_known_ones() {
    let doc = REFERENCE.replace("type: claude-code", "type: codex");
    let detail = refusal(&doc);
    assert!(
        detail.contains("teams.default.agents.reviewer.adapter")
            && detail.contains("unknown variant `codex`")
            && detail.contains("claude-code")
            && detail.contains("opencode"),
        "{detail}"
    );
}

#[test]
fn isolation_is_shared_or_worktree() {
    let doc = REFERENCE.replacen("isolation: shared", "isolation: worktree", 1);
    let c = parse(&doc, &p(), &env(&[])).unwrap();
    assert_eq!(c.agents["default/planner"].isolation, Isolation::Worktree);
    let bad = REFERENCE.replacen("isolation: shared", "isolation: branch", 1);
    let detail = refusal(&bad);
    assert!(
        detail.contains("unknown variant `branch`") && detail.contains("worktree"),
        "{detail}"
    );
}

#[test]
fn a_budget_is_unlimited_or_a_number_of_runs_per_hour() {
    let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
    assert_eq!(c.agents["default/coder"].budget, Budget::Unlimited);
    let doc = REFERENCE.replacen(
        "    budget:\n      type: unlimited\n",
        "    budget:\n      type: per_hour\n      delegations: 5\n",
        1,
    );
    let c = parse(&doc, &p(), &env(&[])).unwrap();
    assert_eq!(
        c.agents["default/planner"].budget,
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
        "{REFERENCE}  summariser:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: llm\n      backend: openai-local\n      model: small\n      max_tokens: 400\n"
    );
    let c = parse(&doc, &p(), &env(&[])).unwrap();
    let a = &c.agents["default/summariser"];
    assert_eq!(
        (a.kind(), a.model(), a.llm_backend()),
        (LLM_AGENT, "small", Some("openai-local"))
    );
    assert_eq!(a.runs_on(), "llm on openai-local");
    assert!(!a.has_tools() && !a.can_change_files());
    let bad = doc.replace(
        "backend: openai-local\n      model: small\n      max_tokens: 400",
        "backend: nowhere\n      model: small\n      max_tokens: 0",
    );
    let joined = joined_problems(&bad);
    assert!(
        joined.contains("LLM backend 'nowhere' is not defined"),
        "{joined}"
    );
    assert!(
        joined.contains("`max_tokens` must be greater than 0"),
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
    let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
    assert_eq!(c.agents["default/coder"].max_seconds, 900);
}

#[test]
fn the_base_url_is_the_one_written_in_the_config() {
    // No variable overrides it: the file is the whole truth.
    let c = parse(
        REFERENCE,
        &p(),
        &env(&[("OPENAI_LOCAL_BASE_URL", "http://192.168.1.9:9000/v1")]),
    )
    .unwrap();
    match &c.llm_backends["openai-local"].settings {
        BackendSettings::OpenAiCompatible { base_url, .. } => {
            assert_eq!(base_url, "http://127.0.0.1:8000/v1");
        }
    }
}

#[test]
fn api_keys_are_read_from_the_environment_never_the_file() {
    let c = parse(
        REFERENCE,
        &p(),
        &env(&[("OPENAI_LOCAL_API_KEY", "sk-secret")]),
    )
    .unwrap();
    match &c.llm_backends["openai-local"].settings {
        BackendSettings::OpenAiCompatible { api_key, .. } => {
            assert_eq!(api_key.as_deref(), Some("sk-secret"))
        }
    }
    // And an unset one is simply absent, not an empty bearer token.
    match &c.llm_backends["openai-remote"].settings {
        BackendSettings::OpenAiCompatible { api_key, .. } => assert_eq!(*api_key, None),
    }
}

#[test]
fn a_backend_may_name_no_key_at_all() {
    let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 5 }
gates: {}
agents: {}
"#;
    let c = parse(yaml, &p(), &env(&[])).unwrap();
    match &c.llm_backends["x"].settings {
        BackendSettings::OpenAiCompatible { api_key, .. } => assert_eq!(*api_key, None),
    }
}

#[test]
fn an_inline_api_key_is_refused_by_name() {
    let yaml = r#"
version: 1
llm_backends:
  x:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:1/v1"
    timeout_seconds: 5
    api_key: "sk-oops"
gates: {}
agents: {}
"#;
    let detail = refusal(yaml);
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
        let yaml = format!(
            "version: 1\nllm_backends:\n  x:\n    type: openai-compatible-api\n    base_url: \"http://127.0.0.1:1/v1\"\n    timeout_seconds: 5\n    {key}\ngates: {{}}\nagents: {{}}\n"
        );
        let detail = refusal(&yaml);
        assert!(detail.contains("unknown field"), "{detail}");
    }
}

#[test]
fn an_agent_with_an_empty_model_is_fatal() {
    let doc = REFERENCE.replace("model: sonnet", "model: \"  \"");
    let joined = joined_problems(&doc);
    assert!(
        joined.contains("teams.default.agents.reviewer.adapter: `model` is empty"),
        "{joined}"
    );
}

#[test]
fn an_unknown_backend_type_lists_the_known_ones() {
    let yaml = r#"
version: 1
llm_backends:
  x: { type: ollama }
gates: {}
agents: {}
"#;
    let detail = refusal(yaml);
    assert!(detail.contains("llm_backends.x"), "{detail}");
    assert!(detail.contains("ollama"), "{detail}");
    assert!(detail.contains(OPENAI_COMPATIBLE_API), "{detail}");
}

#[test]
fn a_base_url_without_a_scheme_is_refused() {
    let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "127.0.0.1:8000/v1", timeout_seconds: 5 }
gates: {}
agents: {}
"#;
    assert!(joined_problems(yaml).contains("http://"));
}

#[test]
fn trailing_slashes_are_normalised_away() {
    let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "http://127.0.0.1:8000/v1/", timeout_seconds: 5 }
gates: {}
agents: {}
"#;
    let c = parse(yaml, &p(), &env(&[])).unwrap();
    match &c.llm_backends["x"].settings {
        BackendSettings::OpenAiCompatible { base_url, .. } => {
            assert_eq!(base_url, "http://127.0.0.1:8000/v1")
        }
    }
}

#[test]
fn an_unsupported_version_is_refused() {
    let yaml = "version: 3\nllm_backends: {}\ngates: {}\nagents: {}\n";
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
llm_backends:
  x: { type: openai-compatible-api, base_url: "nope", timeout_seconds: 0 }
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
      model: { type: llm_backend, backend: missing, name: "", context_window: 0 }
      tools: []
      system_prompt: append
      max_steps: 1
      command: opencode
"#;
    let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
    assert!(problems.len() >= 6, "expected several, got: {problems:#?}");
}

#[test]
fn zero_timeout_is_refused() {
    let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 0 }
gates: {}
agents: {}
"#;
    assert!(joined_problems(yaml).contains("timeout_seconds"));
}

#[test]
fn agents_are_found_by_name() {
    let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
    assert_eq!(c.agent("coder").unwrap().name, "default/coder");
    assert_eq!(c.agent("default/coder").unwrap().short_name(), "coder");
    assert!(c.agent("nope").is_none());
}

#[test]
fn a_prompt_is_optional_and_absent_means_none() {
    let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
    assert_eq!(c.agents["default/coder"].prompt, None);
    let with = REFERENCE.replacen(
            "  coder:\n    max_seconds: 900\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n",
            "  coder:\n    prompt: Writes code.\n    max_seconds: 900\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n",
            1,
        );
    let c = parse(&with, &p(), &env(&[])).unwrap();
    assert_eq!(
        c.agents["default/coder"].prompt.as_deref(),
        Some("Writes code.")
    );
}

/// Two teams, each with a `coder`, and a `planner` in one of them.
fn two_teams() -> String {
    let agent = "        max_seconds: 60\n        max_concurrent: 1\n        isolation: shared\n        budget:\n          type: unlimited\n        adapter:\n          type: opencode\n          model: { type: provider, name: opencode/m }\n          tools: [read]\n          system_prompt: append\n          max_steps: 3\n          command: opencode\n";
    let team = |name: &str, agents: &[&str]| {
        let mut t = format!(
            "  {name}:\n    prompt: The {name} team.\n    tracker: {{ type: local_markdown }}\n    gates:\n      test: {{ command: make test, timeout_seconds: 5 }}\n    agents:\n"
        );
        for a in agents {
            t.push_str(&format!("      {a}:\n{agent}"));
        }
        t
    };
    format!(
        "version: 2\nllm_backends: {{}}\nteams:\n{}{}",
        team("hardware", &["coder"]),
        team("software", &["coder", "planner"])
    )
}

#[test]
fn an_agent_name_used_by_two_teams_needs_its_team() {
    let c = parse(&two_teams(), &p(), &env(&[])).unwrap();
    assert_eq!(
        c.agent_names(),
        vec!["hardware/coder", "software/coder", "software/planner"]
    );
    assert_eq!(c.qualify("planner").unwrap(), "software/planner");
    assert_eq!(c.qualify("hardware/coder").unwrap(), "hardware/coder");
    match c.qualify("coder").unwrap_err() {
        NameError::Ambiguous { candidates, .. } => {
            assert_eq!(candidates, vec!["hardware/coder", "software/coder"])
        }
        other => panic!("{other:?}"),
    }
    assert!(c.agent("coder").is_none(), "an ambiguous name is no agent");
    let message = c.qualify("coder").unwrap_err().to_string();
    assert!(message.contains("team/agent"), "{message}");
    assert!(
        c.qualify("nobody")
            .unwrap_err()
            .to_string()
            .contains("Agents:")
    );
}

#[test]
fn gates_belong_to_their_team() {
    let doc = two_teams().replacen(
        "      test: { command: make test, timeout_seconds: 5 }\n",
        "      drc: { command: kicad-cli pcb drc, timeout_seconds: 5 }\n",
        1,
    );
    let c = parse(&doc, &p(), &env(&[])).unwrap();
    assert!(c.gate("hardware", "drc").is_some());
    assert!(c.gate("hardware", "test").is_none());
    assert!(c.gate("software", "test").is_some());
    assert!(c.gate("software", "drc").is_none());
}

#[test]
fn teams_and_agents_need_names_without_a_slash_and_a_prompt() {
    let slash = two_teams().replace("  hardware:", "  hard/ware:");
    let joined = joined_problems(&slash);
    assert!(joined.contains("hard/ware"), "{joined}");
    let empty = two_teams().replacen("prompt: The hardware team.", "prompt: \"\"", 1);
    let joined = joined_problems(&empty);
    assert!(
        joined.contains("teams.hardware: `prompt` is empty"),
        "{joined}"
    );
    let doc = two_teams().replacen("      coder:\n", "      co/der:\n", 1);
    let joined = joined_problems(&doc);
    assert!(joined.contains("co/der"), "{joined}");
}

#[test]
fn a_team_must_name_its_tracker() {
    let doc = two_teams().replacen("    tracker: { type: local_markdown }\n", "", 1);
    let detail = refusal(&doc);
    assert!(detail.contains("`tracker` is required"), "{detail}");
    let doc = two_teams().replacen("local_markdown", "carrier_pigeon", 1);
    let detail = refusal(&doc);
    assert!(
        detail.contains("carrier_pigeon") || detail.contains("unknown variant"),
        "{detail}"
    );
}

#[test]
fn a_user_can_keep_only_some_teams_and_an_unknown_one_is_named() {
    let mut c = parse(&two_teams(), &p(), &env(&[])).unwrap();
    let err = c.retain_teams(&["nope".into()]).unwrap_err();
    assert!(err.contains("Teams: hardware, software"), "{err}");
    c.retain_teams(&["software".into()]).unwrap();
    assert_eq!(c.agent_names(), vec!["software/coder", "software/planner"]);
    assert!(c.gate("software", "test").is_some() && c.gate("hardware", "test").is_none());
    assert_eq!(c.teams.len(), 1);
    // With the other team gone, the short name is no longer ambiguous.
    assert_eq!(c.qualify("coder").unwrap(), "software/coder");
}
