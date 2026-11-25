//! Editing `config.yaml` as text, so that comments and order survive: adding an
//! agent to the helper team with every setting written, and removing one. The
//! result is always checked by loading it before it is written anywhere.

use super::*;

/// What an agent block is made of, for each adapter type. Every setting is
/// written; none is left to a default.
pub struct NewAgent<'a> {
    pub adapter: &'a str,
    /// What the agent is for; the first line is what the calling agent sees.
    pub prompt: Option<&'a str>,
    /// A model: a provider's `provider/model` for `opencode`, the model's name
    /// for `claude-code` and for an agent on the ANN pool.
    pub model: Option<&'a str>,
    /// The provider a `direct` agent, or an `opencode` agent with one, runs on.
    pub provider: Option<&'a str>,
    /// Whether it may change files.
    pub writes: bool,
}

/// The block of `name` under `agents:`, indented to sit at 2 spaces.
pub fn agent_block(name: &str, a: &NewAgent<'_>) -> Result<String, String> {
    let mut out = format!("  {name}:\n");
    if let Some(prompt) = a.prompt.filter(|p| !p.trim().is_empty()) {
        out.push_str(&format!("    prompt: {}\n", yaml_string(prompt)));
    }
    out.push_str("    max_seconds: 1800\n    max_concurrent: 1\n");
    out.push_str(&format!(
        "    isolation: {}\n",
        if a.writes { "worktree" } else { "shared" }
    ));
    out.push_str("    budget:\n      type: unlimited\n    adapter:\n");
    match a.adapter {
        CLAUDE_CODE_AGENT => {
            let tools = if a.writes {
                "[Read, Grep, Glob, Edit, Write, Bash]"
            } else {
                "[Read, Grep, Glob]"
            };
            out.push_str(&format!(
                "      type: {CLAUDE_CODE_AGENT}\n      model: {}\n      tools: {tools}\n      permission_mode: dontAsk\n      max_turns: 30\n      command: claude\n",
                a.model.unwrap_or("sonnet")
            ));
        }
        OPENCODE_AGENT => {
            let tools = if a.writes {
                "[read, glob, grep, list, edit, bash]"
            } else {
                "[read, glob, grep, list]"
            };
            let model = match (a.provider, a.model) {
                (Some(provider), Some(model)) => format!(
                    "      model:\n        type: {ANN_POOL_MODEL}\n        try:\n          - provider: {provider}\n            name: {model}\n            context_window: 32768\n        when_none: fail\n"
                ),
                (Some(_), None) => {
                    return Err("an opencode agent on the ANN pool needs --model, the id the provider serves it under".into());
                }
                (None, model) => format!(
                    "      model:\n        type: {PROVIDER_MODEL}\n        name: {}\n",
                    model.unwrap_or("opencode/big-pickle")
                ),
            };
            out.push_str(&format!(
                "      type: {OPENCODE_AGENT}\n{model}      tools: {tools}\n      system_prompt: append\n      max_steps: 30\n      command: opencode\n"
            ));
        }
        CODEX_AGENT => {
            let sandbox = if a.writes {
                "workspace-write"
            } else {
                "read-only"
            };
            out.push_str(&format!(
                "      type: {CODEX_AGENT}\n      model: {}\n      sandbox: {sandbox}\n      command: codex\n",
                a.model.unwrap_or("gpt-5-codex")
            ));
        }
        DIRECT_AGENT => {
            let provider = a.provider.ok_or(
                "a direct agent needs --provider, one of the providers of its runner's ANN pool",
            )?;
            let model = a
                .model
                .ok_or("a direct agent needs --model, the name the server serves it under")?;
            out.push_str(&format!(
                "      type: {DIRECT_AGENT}\n      try:\n        - provider: {provider}\n          model: {model}\n      when_none: fail\n      max_tokens: 1000\n"
            ));
        }
        other => {
            return Err(format!(
                "unknown adapter '{other}'. Adapters: {CLAUDE_CODE_AGENT}, {OPENCODE_AGENT}, {CODEX_AGENT}, {DIRECT_AGENT}"
            ));
        }
    }
    Ok(out)
}

/// A YAML string safe to put after a key: plain when it can be, else quoted.
fn yaml_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// The line numbers of `key:` at exactly `indent` between `from` and `to`.
fn find_key(lines: &[&str], from: usize, to: usize, indent: usize, key: &str) -> Option<usize> {
    (from..to).find(|&i| {
        let line = lines[i];
        indent_of(line) == indent
            && line.trim_start().strip_prefix(key).is_some_and(|rest| {
                rest.strip_prefix(':').is_some_and(|after| {
                    after.trim().is_empty()
                        || after.trim_start().starts_with('#')
                        || after.trim_start().starts_with('{')
                })
            })
    })
}

/// Where the block that starts at line `start` (at `indent`) ends: the next
/// line that is not blank, not a comment and not indented deeper.
fn block_end(lines: &[&str], start: usize, indent: usize) -> usize {
    let mut end = start + 1;
    let mut last_content = start;
    while end < lines.len() {
        let line = lines[end];
        if !is_blank_or_comment(line) {
            if indent_of(line) <= indent {
                break;
            }
            last_content = end;
        }
        end += 1;
    }
    last_content + 1
}

/// `text` with `block` added as the last agent. Refuses an agent that exists.
pub fn add_agent(text: &str, name: &str, block: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let agents_at = find_key(&lines, 0, lines.len(), 0, "agents")
        .ok_or("the config has no `agents:` to add an agent to")?;
    let agents_end = block_end(&lines, agents_at, 0);
    if find_key(&lines, agents_at + 1, agents_end, 2, name).is_some() {
        return Err(format!("the team already has an agent '{name}'"));
    }
    let mut out: Vec<String> = lines[..agents_at].iter().map(|l| l.to_string()).collect();
    // `agents: {}` becomes a block; a block gets the agent after its last one.
    let flow = lines[agents_at].contains('{');
    out.push("agents:".into());
    if flow {
        out.extend(block.lines().map(String::from));
        out.extend(lines[agents_at + 1..].iter().map(|l| l.to_string()));
    } else {
        out.extend(
            lines[agents_at + 1..agents_end]
                .iter()
                .map(|l| l.to_string()),
        );
        out.extend(block.lines().map(String::from));
        out.extend(lines[agents_end..].iter().map(|l| l.to_string()));
    }
    Ok(join(out, text))
}

/// `text` without the agent `name`.
pub fn remove_agent(text: &str, name: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let not_found = || format!("no agent '{name}' in the config");
    let agents_at = find_key(&lines, 0, lines.len(), 0, "agents").ok_or_else(not_found)?;
    let agents_end = block_end(&lines, agents_at, 0);
    let at = find_key(&lines, agents_at + 1, agents_end, 2, name).ok_or_else(not_found)?;
    let end = block_end(&lines, at, 2);
    let mut out: Vec<String> = lines[..at].iter().map(|l| l.to_string()).collect();
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    // The last agent leaves `agents: {}`, which is a team with none.
    let still_has = (agents_at + 1..agents_end)
        .filter(|i| *i < at || *i >= end)
        .any(|i| !is_blank_or_comment(lines[i]));
    if !still_has {
        out[agents_at] = "agents: {}".into();
    }
    Ok(join(out, text))
}

fn join(lines: Vec<String>, original: &str) -> String {
    let mut text = lines.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = "version: 1\nlimits: {}\ngates: {}\n# the agents\nagents:\n  coder:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget:\n      type: unlimited\n    adapter:\n      type: opencode\n      model:\n        type: provider\n        name: opencode/m\n      tools: [read]\n      system_prompt: append\n      max_steps: 3\n      command: opencode\n  # keep this comment with the next agent\n";

    const EMPTY: &str = "version: 1\nlimits: {}\ngates: {}\nagents: {}\n";

    fn reviewer() -> String {
        agent_block(
            "reviewer",
            &NewAgent {
                adapter: CLAUDE_CODE_AGENT,
                prompt: Some("Reviews: carefully."),
                model: None,
                provider: None,
                writes: false,
            },
        )
        .unwrap()
    }

    fn loads(text: &str) -> Config {
        parse(text, Path::new("config.yaml")).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    #[test]
    fn an_agent_is_added_last_with_everything_else_kept() {
        let out = add_agent(TEAM, "reviewer", &reviewer()).unwrap();
        assert!(out.contains("# the agents\n"), "comments survive");
        assert!(out.contains("# keep this comment with the next agent"));
        let c = loads(&out);
        assert_eq!(c.agent_names(), vec!["coder", "reviewer"]);
        let r = &c.agents["reviewer"];
        assert_eq!(r.kind(), CLAUDE_CODE_AGENT);
        assert_eq!(r.prompt.as_deref(), Some("Reviews: carefully."));
        assert!(!r.can_change_files());
    }

    #[test]
    fn an_agent_goes_into_an_empty_agents_mapping() {
        let out = add_agent(EMPTY, "reviewer", &reviewer()).unwrap();
        assert!(out.contains("agents:\n  reviewer:"), "{out}");
        assert_eq!(loads(&out).agent_names(), vec!["reviewer"]);
    }

    #[test]
    fn a_duplicate_is_refused() {
        let err = add_agent(TEAM, "coder", &reviewer()).unwrap_err();
        assert!(err.contains("already has"), "{err}");
    }

    #[test]
    fn an_agent_is_removed_with_its_block_and_the_last_one_leaves_an_empty_team() {
        let both = add_agent(TEAM, "reviewer", &reviewer()).unwrap();
        let out = remove_agent(&both, "coder").unwrap();
        assert_eq!(loads(&out).agent_names(), vec!["reviewer"]);
        let none = remove_agent(&out, "reviewer").unwrap();
        let c = loads(&none);
        assert!(c.agents.is_empty());
        assert!(none.contains("agents: {}"), "{none}");
        assert!(
            remove_agent(&none, "coder")
                .unwrap_err()
                .contains("no agent")
        );
    }

    #[test]
    fn every_adapter_writes_every_setting() {
        for (adapter, provider, model) in [
            (CLAUDE_CODE_AGENT, None, None),
            (CODEX_AGENT, None, None),
            (OPENCODE_AGENT, None, None),
            (OPENCODE_AGENT, Some("local"), Some("qwen3")),
            (DIRECT_AGENT, Some("local"), Some("small")),
        ] {
            let block = agent_block(
                "x",
                &NewAgent {
                    adapter,
                    prompt: None,
                    model,
                    provider,
                    writes: true,
                },
            )
            .unwrap();
            let text = format!("version: 1\nlimits: {{}}\ngates: {{}}\nagents:\n{block}");
            loads(&text);
        }
        assert!(
            agent_block(
                "x",
                &NewAgent {
                    adapter: DIRECT_AGENT,
                    prompt: None,
                    model: None,
                    provider: None,
                    writes: false
                }
            )
            .is_err()
        );
        assert!(
            agent_block(
                "x",
                &NewAgent {
                    adapter: "carrier",
                    prompt: None,
                    model: None,
                    provider: None,
                    writes: false
                }
            )
            .is_err()
        );
    }
}
