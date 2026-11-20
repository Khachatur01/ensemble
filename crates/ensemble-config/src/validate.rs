//! Turning the document as written into a validated `Config`.

use super::raw::*;
use super::*;

/// Load the helper team config at `path`. It names the LLM backends its
/// agents run on, which the runner defines ([`Config::with_backends`]), and
/// reads nothing from the environment: it may live in a shared repository.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound {
            path: path.to_path_buf(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Unreadable {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    parse(&text, path)
}

pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
    let raw: RawConfig = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: explain(&e),
    })?;
    validate(raw, path)
}

/// What serde said, which already says where in the document, with a missing
/// key put as the instruction it is.
/// A budget as written, checked at `at`.
pub(super) fn resolve_budget(at: &str, raw: &RawBudget, problems: &mut Vec<String>) -> Budget {
    match raw {
        RawBudget::Unlimited(_) => Budget::Unlimited,
        RawBudget::PerHour(b) => {
            if b.delegations == 0 {
                problems.push(format!("{at}: `delegations` must be greater than 0"));
            }
            Budget::PerHour {
                delegations: b.delegations,
            }
        }
        RawBudget::PerHourTokens(b) => {
            if b.tokens == 0 {
                problems.push(format!("{at}: `tokens` must be greater than 0"));
            }
            Budget::PerHourTokens { tokens: b.tokens }
        }
    }
}

pub(super) fn explain(e: &serde_norway::Error) -> String {
    let mut detail = e.to_string();
    if let Some((before, rest)) = detail.split_once("missing field `")
        && let Some((key, after)) = rest.split_once('`')
    {
        detail = format!("{before}`{key}` is required{after}");
    }
    detail
}

pub(super) fn validate(raw: RawConfig, path: &Path) -> Result<Config, ConfigError> {
    let mut problems: Vec<String> = Vec::new();

    if raw.version != SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands version {SUPPORTED_VERSION}",
            raw.version
        ));
    }

    // --- shared limits ---
    let mut limits: BTreeMap<String, Limit> = BTreeMap::new();
    for (name, l) in &raw.limits {
        let at = format!("limits.{name}");
        if name.trim().is_empty() {
            problems.push("limits: a limit name cannot be empty".into());
            continue;
        }
        if l.max_concurrent == 0 {
            problems.push(format!("{at}: `max_concurrent` must be greater than 0"));
        }
        let budget = resolve_budget(&at, &l.budget, &mut problems);
        limits.insert(
            name.clone(),
            Limit {
                max_concurrent: l.max_concurrent,
                budget,
            },
        );
    }

    // --- gates ---
    let mut gates = BTreeMap::new();
    for (name, g) in &raw.gates {
        if name.trim().is_empty() || name.contains('/') {
            problems.push(format!(
                "gates: the gate name '{name}' must not be empty or contain '/'"
            ));
            continue;
        }
        if g.command.trim().is_empty() {
            problems.push(format!("gates.{name}: `command` is empty"));
        }
        if g.timeout_seconds == 0 {
            problems.push(format!(
                "gates.{name}: `timeout_seconds` must be greater than 0"
            ));
        }
        gates.insert(
            name.clone(),
            Gate {
                command: g.command.clone(),
                timeout_seconds: g.timeout_seconds,
            },
        );
    }

    // --- agents ---
    let mut agents: BTreeMap<String, AgentConfig> = BTreeMap::new();
    for (name, ra) in &raw.agents {
        if name.trim().is_empty() || name.contains('/') {
            problems.push(format!(
                "agents: the agent name '{name}' must not be empty or contain '/'"
            ));
            continue;
        }
        let path = format!("agents.{name}");
        if ra.max_seconds == 0 {
            problems.push(format!("{path}: `max_seconds` must be greater than 0"));
        }
        // Its own limits, written out, or a shared one by name; never a mix.
        let (max_concurrent, budget, limit) = match (&ra.limit, ra.max_concurrent, &ra.budget) {
            (Some(name), None, None) => match limits.get(name) {
                Some(l) => (l.max_concurrent, l.budget, Some(name.clone())),
                None => {
                    let known: Vec<&str> = limits.keys().map(String::as_str).collect();
                    problems.push(format!(
                        "{path}: limit '{name}' is not defined. Defined limits: {}.",
                        if known.is_empty() {
                            "(none)".to_string()
                        } else {
                            known.join(", ")
                        }
                    ));
                    (1, Budget::Unlimited, None)
                }
            },
            (None, Some(max), Some(budget)) => {
                if max == 0 {
                    problems.push(format!("{path}: `max_concurrent` must be greater than 0"));
                }
                (
                    max,
                    resolve_budget(&format!("{path}.budget"), budget, &mut problems),
                    None,
                )
            }
            _ => {
                problems.push(format!(
                    "{path}: give either `limit`, the name of a shared one, or both `max_concurrent` and `budget`; not a mix and not neither"
                ));
                (1, Budget::Unlimited, None)
            }
        };
        let at = format!("{path}.adapter");
        let adapter = match &ra.adapter {
            RawAdapter::ClaudeCode(c) => {
                Adapter::ClaudeCode(claude_code_adapter(&at, c, &mut problems))
            }
            RawAdapter::OpenCode(o) => Adapter::OpenCode(opencode_adapter(
                &at,
                o,
                ra.prompt.as_deref(),
                &mut problems,
            )),
            RawAdapter::Codex(c) => Adapter::Codex(codex_adapter(&at, c, &mut problems)),
            RawAdapter::Direct(d) => Adapter::Direct(direct_adapter(&at, d, &mut problems)),
            RawAdapter::Builtin(b) => Adapter::Builtin(builtin_adapter(&at, b, &mut problems)),
        };
        agents.insert(
            name.clone(),
            AgentConfig {
                name: name.clone(),
                prompt: ra.prompt.clone(),
                max_seconds: ra.max_seconds,
                max_concurrent,
                isolation: ra.isolation,
                budget,
                limit,
                adapter,
            },
        );
    }

    if problems.is_empty() {
        Ok(Config {
            version: raw.version,
            llm_backends: BTreeMap::new(),
            limits,
            gates,
            agents,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

pub(super) fn direct_adapter(
    at: &str,
    ad: &RawDirectAdapter,
    problems: &mut Vec<String>,
) -> DirectAdapter {
    if ad.candidates.is_empty() {
        problems.push(format!(
            "{at}: `try` is empty; name at least one model, as {{ backend, model }}"
        ));
    }
    let mut candidates = Vec::new();
    for (i, c) in ad.candidates.iter().enumerate() {
        let here = format!("{at}.try[{i}]");
        if c.model.trim().is_empty() {
            problems.push(format!("{here}: `model` is empty"));
        }
        candidates.push(Candidate {
            backend: c.backend.clone(),
            model: c.model.clone(),
            context_window: None,
        });
    }
    if ad.max_tokens == 0 {
        problems.push(format!("{at}: `max_tokens` must be greater than 0"));
    }
    DirectAdapter {
        fallback: Fallback {
            candidates,
            when_none: ad.when_none,
        },
        max_tokens: ad.max_tokens,
    }
}

pub(super) fn builtin_adapter(
    at: &str,
    ad: &RawBuiltinAdapter,
    problems: &mut Vec<String>,
) -> BuiltinAdapter {
    if ad.candidates.is_empty() {
        problems.push(format!(
            "{at}: `try` is empty; name at least one model, as {{ backend, model, context_window }}"
        ));
    }
    let mut candidates = Vec::new();
    for (i, c) in ad.candidates.iter().enumerate() {
        let here = format!("{at}.try[{i}]");
        if c.model.trim().is_empty() {
            problems.push(format!("{here}: `model` is empty"));
        }
        if c.context_window == 0 {
            problems.push(format!("{here}: `context_window` must be greater than 0"));
        }
        candidates.push(Candidate {
            backend: c.backend.clone(),
            model: c.model.clone(),
            context_window: Some(c.context_window),
        });
    }
    let mut seen = Vec::new();
    for tool in &ad.tools {
        if !BUILTIN_TOOLS.contains(&tool.as_str()) {
            problems.push(format!(
                "{at}: tool '{tool}' is not one a builtin agent has; it has {}",
                BUILTIN_TOOLS.join(", ")
            ));
        } else if seen.contains(&tool) {
            problems.push(format!("{at}: tool '{tool}' is listed twice"));
        }
        seen.push(tool);
    }
    if ad.max_steps == 0 {
        problems.push(format!("{at}: `max_steps` must be greater than 0"));
    }
    if ad.max_tokens == 0 {
        problems.push(format!("{at}: `max_tokens` must be greater than 0"));
    }
    BuiltinAdapter {
        fallback: Fallback {
            candidates,
            when_none: ad.when_none,
        },
        tools: ad.tools.clone(),
        max_steps: ad.max_steps,
        max_tokens: ad.max_tokens,
    }
}
/// Check that each model of a `try` list at `at` names a backend of `kinds`
/// (each name with its type), defined `where_`, and, for OpenCode, an
/// OpenAI-compatible one.
pub(super) fn check_candidates(
    at: &str,
    candidates: &[Candidate],
    opencode: bool,
    kinds: &BTreeMap<String, &str>,
    where_: &str,
    problems: &mut Vec<String>,
) {
    for (i, c) in candidates.iter().enumerate() {
        let here = format!("{at}[{i}]");
        match kinds.get(&c.backend) {
            None => problems.push(undefined_backend(&here, &c.backend, kinds, where_)),
            Some(&ANTHROPIC_API) if opencode => problems.push(format!(
                "{here}: LLM backend '{}' is of type '{ANTHROPIC_API}', which OpenCode cannot use: it needs an '{OPENAI_COMPATIBLE_API}' one. Use a model of type '{PROVIDER_MODEL}', a `claude-code` agent, or a `direct` agent",
                c.backend
            )),
            Some(_) => {}
        }
    }
}

/// A model names a backend `kinds` does not have, which are defined `where_`.
fn undefined_backend(
    at: &str,
    backend: &str,
    kinds: &BTreeMap<String, &str>,
    where_: &str,
) -> String {
    let known: Vec<&str> = kinds.keys().map(String::as_str).collect();
    format!(
        "{at}: LLM backend '{backend}' is not defined{where_}. Defined LLM backends: {}.",
        if known.is_empty() {
            "(none)".to_string()
        } else {
            known.join(", ")
        }
    )
}

pub(super) fn opencode_adapter(
    at: &str,
    ad: &RawOpenCodeAdapter,
    prompt: Option<&str>,
    problems: &mut Vec<String>,
) -> OpenCodeAdapter {
    for tool in &ad.tools {
        let (name, pattern) = match tool.split_once('(') {
            Some((n, rest)) => (n, rest.strip_suffix(')')),
            None => (tool.as_str(), Some("")),
        };
        let name_ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-*.".contains(c));
        if !name_ok || pattern.is_none_or(|p| p.contains(')')) {
            problems.push(format!(
                "{at}: tool '{tool}' must be an OpenCode permission, such as read or edit, or a rule such as bash(git *)"
            ));
        }
    }
    if ad.max_steps == 0 {
        problems.push(format!("{at}: `max_steps` must be greater than 0"));
    }
    if ad.system_prompt == SystemPrompt::Replace && prompt.is_none_or(|p| p.trim().is_empty()) {
        problems.push(format!(
            "{at}: `system_prompt: replace` needs the agent's `prompt`: it is what takes the place of OpenCode's own"
        ));
    }
    let model = match &ad.model {
        RawOpenCodeModel::Provider(p) => {
            if p.name.trim().is_empty() {
                problems.push(format!("{at}: `model.name` is empty"));
            } else if !p.name.contains('/') {
                problems.push(format!(
                    "{at}: model '{}' must be provider/model, as `opencode models` lists it, or run on an LLM backend with a `model` of type '{LLM_BACKEND_MODEL}'",
                    p.name
                ));
            }
            OpenCodeModel::Provider {
                model: p.name.clone(),
            }
        }
        RawOpenCodeModel::LlmBackend(m) => {
            if m.candidates.is_empty() {
                problems.push(format!(
                    "{at}: `model.try` is empty; name at least one model, as {{ backend, name, context_window }}"
                ));
            }
            let mut candidates = Vec::new();
            for (i, b) in m.candidates.iter().enumerate() {
                let here = format!("{at}.model.try[{i}]");
                // On a backend the model is the id the endpoint serves; Ensemble
                // names the provider.
                if b.name.trim().is_empty() {
                    problems.push(format!("{here}: `name` is empty"));
                }
                if b.context_window == 0 {
                    problems.push(format!("{here}: `context_window` must be greater than 0"));
                }
                candidates.push(Candidate {
                    backend: b.backend.clone(),
                    model: b.name.clone(),
                    context_window: Some(b.context_window),
                });
            }
            OpenCodeModel::LlmBackend(Fallback {
                candidates,
                when_none: m.when_none,
            })
        }
    };
    if ad.command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    OpenCodeAdapter {
        command: ad.command.clone(),
        model,
        tools: ad.tools.clone(),
        system_prompt: ad.system_prompt,
        max_steps: ad.max_steps,
    }
}

pub(super) fn claude_code_adapter(
    at: &str,
    ad: &RawClaudeCodeAdapter,
    problems: &mut Vec<String>,
) -> ClaudeCodeAdapter {
    if ad.model.trim().is_empty() {
        problems.push(format!("{at}: `model` is empty"));
    }
    if let Some(bad) = ad
        .tools
        .iter()
        .find(|t| t.trim().is_empty() || t.contains(','))
    {
        problems.push(format!(
            "{at}: tool '{bad}' must be one Claude Code tool or rule per list entry, such as Read or Bash(git:*)"
        ));
    }
    if ad.permission_mode.trim().is_empty() {
        problems.push(format!("{at}: `permission_mode` is empty"));
    }
    if ad.max_turns == 0 {
        problems.push(format!("{at}: `max_turns` must be greater than 0"));
    }
    if ad.command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    ClaudeCodeAdapter {
        command: ad.command.clone(),
        model: ad.model.clone(),
        tools: ad.tools.clone(),
        permission_mode: ad.permission_mode.clone(),
        max_turns: ad.max_turns,
    }
}

pub(super) fn codex_adapter(
    at: &str,
    ad: &RawCodexAdapter,
    problems: &mut Vec<String>,
) -> CodexAdapter {
    if ad.model.trim().is_empty() {
        problems.push(format!("{at}: `model` is empty"));
    }
    if ad.command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    if !CODEX_SANDBOXES.contains(&ad.sandbox.as_str()) {
        problems.push(format!(
            "{at}: sandbox '{}' is unknown. Sandboxes: {}.",
            ad.sandbox,
            CODEX_SANDBOXES.join(", ")
        ));
    }
    CodexAdapter {
        command: ad.command.clone(),
        model: ad.model.clone(),
        sandbox: ad.sandbox.clone(),
    }
}

pub(super) fn resolve_backend(
    at: &str,
    name: &str,
    rb: &RawLlmBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    match rb {
        RawLlmBackend::OpenAiCompatibleApi(b) => resolve_openai(at, name, b, env),
        RawLlmBackend::AnthropicApi(b) => {
            // The same checks: a URL, a timeout and an optional key.
            let as_openai = RawOpenAiBackend {
                base_url: b.base_url.clone(),
                api_key_env: b.api_key_env.clone(),
                timeout_seconds: b.timeout_seconds,
            };
            resolve_openai(at, name, &as_openai, env).map(|mut done| {
                if let BackendSettings::OpenAiCompatible { base_url, api_key } = done.settings {
                    done.settings = BackendSettings::Anthropic { base_url, api_key };
                }
                done
            })
        }
    }
}

/// Check the URL and timeout, and read the optional key from the environment.
pub(super) fn resolve_openai(
    at: &str,
    name: &str,
    b: &RawOpenAiBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    let mut problems = Vec::new();
    if b.timeout_seconds == 0 {
        problems.push(format!("{at}: `timeout_seconds` must be greater than 0"));
    }
    let base_url = b.base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "{at}: `base_url` '{base_url}' must start with http:// or https://"
        ));
    }

    // A key is optional — a local llama-server usually wants none — but a
    // *named* variable that is empty is a different thing from no variable at
    // all: somebody expected authentication. Say so rather than sending an
    // empty bearer token and reading a 401 later.
    let api_key = match &b.api_key_env {
        None => None,
        Some(key_var) => match env.get(key_var).filter(|v| !v.trim().is_empty()) {
            Some(k) => Some(k),
            None => {
                eprintln!(
                    "ensemble: note: {at} names ${key_var} for its API key, \
                     but it is unset; requests will be sent unauthenticated"
                );
                None
            }
        },
    };

    if problems.is_empty() {
        Ok(LlmBackend {
            name: name.to_string(),
            timeout_seconds: b.timeout_seconds,
            settings: BackendSettings::OpenAiCompatible {
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key,
            },
        })
    } else {
        Err(problems)
    }
}

impl Config {
    /// This team on `backends`, the LLM backends of the runner that runs it,
    /// once every model its agents name is one of them.
    pub fn with_backends(
        mut self,
        backends: BTreeMap<String, LlmBackend>,
    ) -> Result<Config, ConfigError> {
        let kinds: BTreeMap<String, &str> = backends
            .iter()
            .map(|(name, b)| (name.clone(), b.type_id()))
            .collect();
        self.check_backends(&kinds)?;
        self.llm_backends = backends;
        Ok(self)
    }

    /// This team on the LLM backends of the runner whose `llm` is `llm`,
    /// once every model its agents name is one of them: its own, and those
    /// it reaches through the hub when it serves them at `relay`. Without a
    /// relay those are named but not there, and a run tries the next model.
    pub fn with_llm(
        mut self,
        llm: &RunnerLlm,
        relay: Option<&RelayAt>,
    ) -> Result<Config, ConfigError> {
        let kinds: BTreeMap<String, &str> = crate::llm::kinds(&llm.direct, &llm.hub)
            .into_iter()
            .collect();
        self.check_backends(&kinds)?;
        self.llm_backends = llm.backends(relay);
        Ok(self)
    }

    /// Does every model of its agents name a backend of `kinds`, each name
    /// with its type?
    fn check_backends(&self, kinds: &BTreeMap<String, &str>) -> Result<(), ConfigError> {
        let mut problems = Vec::new();
        for (name, agent) in &self.agents {
            let Some(fallback) = agent.fallback() else {
                continue;
            };
            let opencode = matches!(agent.adapter, Adapter::OpenCode(_));
            let at = if opencode {
                format!("agents.{name}.adapter.model.try")
            } else {
                format!("agents.{name}.adapter.try")
            };
            check_candidates(
                &at,
                &fallback.candidates,
                opencode,
                kinds,
                " in the runner's `llm`",
                &mut problems,
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid {
                path: self.source.clone(),
                problems,
            })
        }
    }
}
