use super::*;

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The providers runners in these tests have.
const RUNNER_BACKENDS: &str = r#"
      openai-local: { type: openai-compatible-api, base_url: "http://127.0.0.1:8000/v1", api_key_env: OPENAI_LOCAL_API_KEY, timeout_seconds: 600, max_concurrent: 1 }
      openai-remote: { type: openai-compatible-api, base_url: "http://10.10.10.5:8000/v1", api_key_env: OPENAI_REMOTE_API_KEY, timeout_seconds: 300, max_concurrent: 1 }
      home: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 5, max_concurrent: 1 }
      claude-api: { type: anthropic-api, base_url: "https://api.anthropic.com/", api_key_env: ANTHROPIC_API_KEY, timeout_seconds: 90, max_concurrent: 1 }
"#;

/// Why a runner was refused: serde would not read its providers, or they
/// do not resolve.
#[derive(Debug)]
enum Refused {
    Syntax(String),
    Invalid(Vec<String>),
}

/// A runner without a hub whose providers are `providers`, the lines of a YAML
/// mapping indented six spaces, or `{}`, its keys read from `env`.
fn runner_on(providers: &str, env: &dyn Env) -> Result<Runner, Refused> {
    let providers: BTreeMap<String, ProviderSetting> =
        serde_norway::from_str(providers).map_err(|e| Refused::Syntax(e.to_string()))?;
    RunnerSettings {
        ann_pool: AnnPoolSetting {
            providers,
            models: Vec::new(),
        },
        ..RunnerSettings::starting("laptop", "/code")
    }
    .resolve(&ensemble_core::state::RunnerDir::new("/r"), env)
    .map_err(|ConfigError::Invalid { problems, .. }| Refused::Invalid(problems))
}

/// The problems of the runner whose providers are `providers`.
fn runner_problems(providers: &str) -> String {
    match runner_on(providers, &env(&[])).unwrap_err() {
        Refused::Invalid(problems) => problems.join("\n"),
        other => panic!("expected Invalid, got: {other:?}"),
    }
}

/// The message of the runner whose providers serde refuses.
fn runner_refusal(providers: &str) -> String {
    match runner_on(providers, &env(&[])).unwrap_err() {
        Refused::Syntax(detail) => detail,
        other => panic!("expected Syntax, got: {other:?}"),
    }
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

// ---------------------------------------------------------------------------
// The hub's settings
// ---------------------------------------------------------------------------

/// An LLM of the pool, as a test adds it.
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
    hub.ann_pool.add("local", "small", llm());
    hub.distill.candidates = vec![ModelSetting {
        provider: "local".into(),
        model: "small".into(),
    }];
    hub
}

fn hub_problems(hub: &HubSettings) -> String {
    let ConfigError::Invalid { problems, .. } = hub
        .resolve(&ensemble_core::state::HubDir::new("/hubs/ana"), &env(&[]))
        .unwrap_err();
    problems.join("\n")
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
        .add("local", "nomic", ModelKind::Embedding { dimensions: 8 });
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
    hub.distill.candidates.push(ModelSetting {
        provider: "local".into(),
        model: "new".into(),
    });
    let said = hub_problems(&hub);
    for wanted in [
        "embed: local:small is not an embedding model: it is llm, medium",
        "must be from 0 to 1",
        "distill.try[1]: local:nomic is not an LLM: it is embedding, 8 dimensions",
        "distill.try[2]: local:new is not a model of the ANN pool; `model add local:new` adds it",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn a_model_of_the_pool_is_of_one_of_its_providers_once() {
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
    hub.ann_pool.models.push(PoolModel {
        provider: "gone".into(),
        id: "x".into(),
        kind: llm(),
        missing: false,
        shared: false,
    });
    hub.ann_pool.models.push(PoolModel {
        shared: true,
        ..hub.ann_pool.models[0].clone()
    });
    let said = hub_problems(&hub);
    for wanted in [
        "ann_pool.providers.busy: `max_concurrent` must be greater than 0",
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
    hub.ann_pool.add("gpu", "local:big", llm());
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
fn a_new_runner_has_no_harness_and_one_added_is_checked() {
    let r = runner_with(|_| {}).unwrap();
    assert!(r.harnesses.each().is_empty(), "only what a person adds");
    let cli = |command: &str, max_seconds, max_concurrent| CliHarness {
        missing: false,
        command: command.into(),
        max_seconds,
        max_steps: 50,
        max_concurrent,
        models: Vec::new(),
    };
    let r = runner_with(|r| {
        r.harnesses.claude_code = Some(CliHarness {
            models: vec![HarnessModel {
                id: "sonnet".into(),
                tier: Tier::Strong,
            }],
            ..cli("claude", 1800, 1)
        });
        r.harnesses.builtin = Some(BuiltinHarness {
            max_seconds: 1800,
            max_steps: 50,
            max_concurrent: 1,
            max_tokens: 4000,
        });
    })
    .unwrap();
    assert_eq!(
        r.harnesses
            .each()
            .iter()
            .map(|h| (h.kind, h.command.as_deref()))
            .collect::<Vec<_>>(),
        [
            (HarnessKind::ClaudeCode, Some("claude")),
            (HarnessKind::Builtin, None)
        ],
        "builtin runs no program"
    );
    assert_eq!(
        r.harnesses.get(HarnessKind::Builtin).unwrap().max_tokens,
        Some(4000)
    );
    let said = runner_with(|r| {
        r.harnesses.codex = Some(cli("codex", 0, 1));
        r.harnesses.builtin = Some(BuiltinHarness {
            max_seconds: 1800,
            max_steps: 0,
            max_concurrent: 1,
            max_tokens: 0,
        });
        r.harnesses.claude_code = Some(cli("claude", 1800, 0));
        r.harnesses.opencode = Some(CliHarness {
            models: vec![HarnessModel {
                id: "x".into(),
                tier: Tier::Small,
            }],
            ..cli(" ", 1800, 1)
        });
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
    for wanted in [
        "harnesses.builtin: `max_tokens` must be greater than 0",
        "harnesses.claude-code: `max_concurrent` must be greater than 0",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
    assert!(
        said.contains("harnesses.opencode: `command` is empty"),
        "{said}"
    );
    assert!(
        said.contains(
            "harnesses.opencode: drives the LLMs of the ANN pool, and has no models of its own"
        ),
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
        r.ann_pool.add("local", "small", llm());
        r.ann_pool.add("local", "other", llm());
        r.ann_pool.add("claude", "haiku", llm());
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
    })
    .unwrap_err()
    .to_string();
    for wanted in [
        "ann_pool.models.claude:haiku: is shared, and only a model of an openai-compatible-api provider of the runner's own may be",
        "ann_pool: a runner shares its models through a hub, and it names no hub",
        "ann_pool: a provider that is another runner is reached through a hub",
    ] {
        assert!(said.contains(wanted), "{wanted}: {said}");
    }
}

#[test]
fn a_providers_key_is_named_for_it() {
    assert_eq!(
        provider_key_variable("team-gpu.2"),
        "ENSEMBLE_PROVIDER_TEAM_GPU_2_KEY"
    );
}
