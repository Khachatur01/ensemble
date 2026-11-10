//! Turning the document as written into a validated `Config`.

use super::raw::*;
use super::*;

/// Load the helper team config at `path`. Variables the environment does not
/// set may come from `env_file`, a file of `NAME=value` lines the caller names;
/// nothing is read beside the config, because it can live in a shared repo.
pub fn load(path: &Path, env_file: Option<&Path>, env: &dyn Env) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound {
            path: path.to_path_buf(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Unreadable {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    let file_vars = match env_file {
        Some(dotenv) => {
            let vars = std::fs::read_to_string(dotenv).map_err(|e| ConfigError::Unreadable {
                path: dotenv.to_path_buf(),
                detail: e.to_string(),
            })?;
            parse_dotenv(&vars, dotenv)?
        }
        None => BTreeMap::new(),
    };
    parse(
        &text,
        path,
        &Layered {
            first: env,
            then: &file_vars,
        },
    )
}

pub fn parse(text: &str, path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    let raw: RawConfig = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: explain(&e),
    })?;
    validate(raw, path, env)
}

/// What serde said, which already says where in the document, with a missing
/// key put as the instruction it is and the removed `ensemble` type pointed to
/// its replacement.
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
    if detail.contains(&format!("unknown variant `{RENAMED_LLM_AGENT}`")) {
        detail.push_str(&format!(
            ". Type '{RENAMED_LLM_AGENT}' is called '{DIRECT_AGENT}' since version 3; `ensemble migrate` renames it."
        ));
    }
    if detail.contains(&format!("unknown variant `{REMOVED_ENSEMBLE_AGENT}`")) {
        detail.push_str(&format!(
            ". Type '{REMOVED_ENSEMBLE_AGENT}' was removed; Ensemble no longer runs agents itself. To run a model on an LLM backend, use type '{OPENCODE_AGENT}' with a `model` of type '{LLM_BACKEND_MODEL}'."
        ));
    }
    detail
}

pub(super) fn validate(raw: RawConfig, path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    let mut problems: Vec<String> = Vec::new();

    if raw.version != SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands version {SUPPORTED_VERSION}",
            raw.version
        ));
    }

    // --- backends ---
    let mut backends: BTreeMap<String, LlmBackend> = BTreeMap::new();
    for (name, rb) in &raw.llm_backends {
        match resolve_backend(name, rb, env) {
            Ok(b) => {
                backends.insert(name.clone(), b);
            }
            Err(mut errs) => problems.append(&mut errs),
        }
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

    let tracker = match &raw.tracker {
        RawTracker::LocalMarkdown(_) => Tracker::LocalMarkdown,
    };

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
                &raw.llm_backends,
                &mut problems,
            )),
            RawAdapter::Codex(c) => Adapter::Codex(codex_adapter(&at, c, &mut problems)),
            RawAdapter::Direct(d) => {
                Adapter::Direct(direct_adapter(&at, d, &raw.llm_backends, &mut problems))
            }
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

    // A backend nothing uses is not an error — it is a spare, and deleting it
    // from a shared config is how you break someone else's setup. Say so once
    // and carry on.
    for name in backends.keys() {
        let used = agents
            .values()
            .any(|a| a.llm_backends().iter().any(|(b, _)| b == name));
        if !used {
            eprintln!("ensemble: note: LLM backend '{name}' is defined but no agent uses it");
        }
    }

    if problems.is_empty() {
        Ok(Config {
            version: raw.version,
            llm_backends: backends,
            limits,
            tracker,
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
    declared: &BTreeMap<String, RawLlmBackend>,
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
        if !declared.contains_key(&c.backend) {
            problems.push(undefined_backend(&here, &c.backend, declared));
        }
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

/// An adapter names a backend `llm_backends` does not have.
pub(super) fn undefined_backend(
    at: &str,
    backend: &str,
    declared: &BTreeMap<String, RawLlmBackend>,
) -> String {
    let known: Vec<&str> = declared.keys().map(String::as_str).collect();
    format!(
        "{at}: LLM backend '{backend}' is not defined. Defined LLM backends: {}.",
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
    declared: &BTreeMap<String, RawLlmBackend>,
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
                if !declared.contains_key(&b.backend) {
                    problems.push(undefined_backend(&here, &b.backend, declared));
                }
                if let Some(RawLlmBackend::AnthropicApi(_)) = declared.get(&b.backend) {
                    problems.push(format!(
                        "{here}: LLM backend '{}' is of type '{ANTHROPIC_API}', which OpenCode cannot use: it needs an '{OPENAI_COMPATIBLE_API}' one. Use a model of type '{PROVIDER_MODEL}', a `claude-code` agent, or a `direct` agent",
                        b.backend
                    ));
                }
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
    name: &str,
    rb: &RawLlmBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    match rb {
        RawLlmBackend::OpenAiCompatibleApi(b) => resolve_openai(name, b, env),
        RawLlmBackend::AnthropicApi(b) => {
            // The same checks: a URL, a timeout and an optional key.
            let as_openai = RawOpenAiBackend {
                base_url: b.base_url.clone(),
                api_key_env: b.api_key_env.clone(),
                timeout_seconds: b.timeout_seconds,
            };
            resolve_openai(name, &as_openai, env).map(|mut done| {
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
    name: &str,
    b: &RawOpenAiBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    let mut problems = Vec::new();
    if b.timeout_seconds == 0 {
        problems.push(format!(
            "llm_backends.{name}: `timeout_seconds` must be greater than 0"
        ));
    }
    let base_url = b.base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "llm_backends.{name}: `base_url` '{base_url}' must start with http:// or https://"
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
                    "ensemble: note: llm_backends.{name} names ${key_var} for its API key, \
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
