//! Turning the document as written into a validated `Config`.

use super::raw::*;
use super::*;

pub fn load(path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound {
            path: path.to_path_buf(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Unreadable {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    let dotenv = env_path(path);
    let file_vars = if dotenv.is_file() {
        let vars = std::fs::read_to_string(&dotenv).map_err(|e| ConfigError::Unreadable {
            path: dotenv.clone(),
            detail: e.to_string(),
        })?;
        parse_dotenv(&vars, &dotenv)?
    } else {
        BTreeMap::new()
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
pub(super) fn explain(e: &serde_norway::Error) -> String {
    let mut detail = e.to_string();
    if let Some((before, rest)) = detail.split_once("missing field `")
        && let Some((key, after)) = rest.split_once('`')
    {
        detail = format!("{before}`{key}` is required{after}");
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

    // --- gates ---
    let mut gates = BTreeMap::new();
    for (name, g) in &raw.gates {
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
        if name.trim().is_empty() {
            problems.push("agents: an agent name cannot be empty".into());
            continue;
        }
        if ra.max_seconds == 0 {
            problems.push(format!(
                "agents.{name}: `max_seconds` must be greater than 0"
            ));
        }
        if ra.max_concurrent == 0 {
            problems.push(format!(
                "agents.{name}: `max_concurrent` must be greater than 0"
            ));
        }
        let budget = match &ra.budget {
            RawBudget::Unlimited(_) => Budget::Unlimited,
            RawBudget::PerHour(b) => {
                if b.delegations == 0 {
                    problems.push(format!(
                        "agents.{name}.budget: `delegations` must be greater than 0"
                    ));
                }
                Budget::PerHour {
                    delegations: b.delegations,
                }
            }
        };
        let at = format!("agents.{name}.adapter");
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
            RawAdapter::Llm(l) => {
                Adapter::Llm(llm_adapter(&at, l, &raw.llm_backends, &mut problems))
            }
        };
        agents.insert(
            name.clone(),
            AgentConfig {
                name: name.clone(),
                prompt: ra.prompt.clone(),
                max_seconds: ra.max_seconds,
                max_concurrent: ra.max_concurrent,
                isolation: ra.isolation,
                budget,
                adapter,
            },
        );
    }

    // A backend nothing uses is not an error — it is a spare, and deleting it
    // from a shared config is how you break someone else's setup. Say so once
    // and carry on.
    for name in backends.keys() {
        let used = agents.values().any(|a| a.llm_backend() == Some(name));
        if !used {
            eprintln!("ensemble: note: LLM backend '{name}' is defined but no agent uses it");
        }
    }

    if problems.is_empty() {
        Ok(Config {
            version: raw.version,
            llm_backends: backends,
            agents,
            gates,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

pub(super) fn llm_adapter(
    at: &str,
    ad: &RawLlmAdapter,
    declared: &BTreeMap<String, RawLlmBackend>,
    problems: &mut Vec<String>,
) -> LlmAdapter {
    if !declared.contains_key(&ad.backend) {
        problems.push(undefined_backend(at, &ad.backend, declared));
    }
    if ad.model.trim().is_empty() {
        problems.push(format!("{at}: `model` is empty"));
    }
    if ad.max_tokens == 0 {
        problems.push(format!("{at}: `max_tokens` must be greater than 0"));
    }
    LlmAdapter {
        backend: ad.backend.clone(),
        model: ad.model.clone(),
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
        RawOpenCodeModel::LlmBackend(b) => {
            if !declared.contains_key(&b.backend) {
                problems.push(undefined_backend(at, &b.backend, declared));
            }
            // On a backend the model is the id the endpoint serves; Ensemble
            // names the provider.
            if b.name.trim().is_empty() {
                problems.push(format!("{at}: `model.name` is empty"));
            }
            if b.context_window == 0 {
                problems.push(format!(
                    "{at}: `model.context_window` must be greater than 0"
                ));
            }
            OpenCodeModel::LlmBackend {
                backend: b.backend.clone(),
                model: b.name.clone(),
                context_window: b.context_window,
            }
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

pub(super) fn resolve_backend(
    name: &str,
    rb: &RawLlmBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    match rb {
        RawLlmBackend::OpenAiCompatibleApi(b) => resolve_openai(name, b, env),
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
