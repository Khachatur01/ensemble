//! Editing `config.yaml` as text, so that comments and order survive: adding an
//! agent to a team with every setting written, and removing one. The result is
//! always checked by loading it before it is written anywhere.

use super::*;

/// What an agent block is made of, for each adapter type. Every setting is
/// written; none is left to a default.
pub struct NewAgent<'a> {
    pub adapter: &'a str,
    /// What the agent is for; the first line is what the manager sees.
    pub prompt: Option<&'a str>,
    /// A model: a provider's `provider/model` for `opencode`, the model's name
    /// for `claude-code` and for an agent on an LLM backend.
    pub model: Option<&'a str>,
    /// The LLM backend an `llm` agent, or an `opencode` agent with one, runs on.
    pub backend: Option<&'a str>,
    /// Whether it may change files.
    pub writes: bool,
}

/// The block of `name` under `agents:`, indented to sit at 6 spaces.
pub fn agent_block(name: &str, a: &NewAgent<'_>) -> Result<String, String> {
    let mut out = format!("      {name}:\n");
    if let Some(prompt) = a.prompt.filter(|p| !p.trim().is_empty()) {
        out.push_str(&format!("        prompt: {}\n", yaml_string(prompt)));
    }
    out.push_str("        max_seconds: 1800\n        max_concurrent: 1\n");
    out.push_str(&format!(
        "        isolation: {}\n",
        if a.writes { "worktree" } else { "shared" }
    ));
    out.push_str("        budget:\n          type: unlimited\n        adapter:\n");
    match a.adapter {
        CLAUDE_CODE_AGENT => {
            let tools = if a.writes {
                "[Read, Grep, Glob, Edit, Write, Bash]"
            } else {
                "[Read, Grep, Glob]"
            };
            out.push_str(&format!(
                "          type: {CLAUDE_CODE_AGENT}\n          model: {}\n          tools: {tools}\n          permission_mode: dontAsk\n          max_turns: 30\n          command: claude\n",
                a.model.unwrap_or("sonnet")
            ));
        }
        OPENCODE_AGENT => {
            let tools = if a.writes {
                "[read, glob, grep, list, edit, bash]"
            } else {
                "[read, glob, grep, list]"
            };
            let model = match (a.backend, a.model) {
                (Some(backend), Some(model)) => format!(
                    "          model:\n            type: {LLM_BACKEND_MODEL}\n            backend: {backend}\n            name: {model}\n            context_window: 32768\n"
                ),
                (Some(_), None) => {
                    return Err("an opencode agent on an LLM backend needs --model, the name the server serves it under".into());
                }
                (None, model) => format!(
                    "          model:\n            type: {PROVIDER_MODEL}\n            name: {}\n",
                    model.unwrap_or("opencode/big-pickle")
                ),
            };
            out.push_str(&format!(
                "          type: {OPENCODE_AGENT}\n{model}          tools: {tools}\n          system_prompt: append\n          max_steps: 30\n          command: opencode\n"
            ));
        }
        LLM_AGENT => {
            let backend = a
                .backend
                .ok_or("an llm agent needs --backend, one of llm_backends")?;
            let model = a
                .model
                .ok_or("an llm agent needs --model, the name the server serves it under")?;
            out.push_str(&format!(
                "          type: {LLM_AGENT}\n          backend: {backend}\n          model: {model}\n          max_tokens: 1000\n"
            ));
        }
        other => {
            return Err(format!(
                "unknown adapter '{other}'. Adapters: {CLAUDE_CODE_AGENT}, {OPENCODE_AGENT}, {LLM_AGENT}"
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

/// `text` with `block` added as the last agent of `team`. A team that is not
/// there is made, with `team_prompt`. Refuses an agent that exists.
pub fn add_agent(
    text: &str,
    team: &str,
    name: &str,
    block: &str,
    team_prompt: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let teams_at = find_key(&lines, 0, lines.len(), 0, "teams")
        .ok_or("the config has no `teams:` to add an agent to")?;
    let teams_end = block_end(&lines, teams_at, 0);
    let mut out: Vec<String> = Vec::new();

    let Some(team_at) = find_key(&lines, teams_at + 1, teams_end, 2, team) else {
        // A new team, at the end of `teams:`.
        out.extend(lines[..teams_end].iter().map(|l| l.to_string()));
        out.push(format!("  {team}:"));
        out.push(format!("    prompt: {}", yaml_string(team_prompt)));
        out.push("    tracker:".into());
        out.push("      type: local_markdown".into());
        out.push("    gates: {}".into());
        out.push("    agents:".into());
        out.extend(block.lines().map(String::from));
        out.extend(lines[teams_end..].iter().map(|l| l.to_string()));
        return Ok(join(out, text));
    };
    let team_end = block_end(&lines, team_at, 2);
    let agents_at = find_key(&lines, team_at + 1, team_end, 4, "agents")
        .ok_or_else(|| format!("team '{team}' has no `agents:`"))?;
    let agents_end = block_end(&lines, agents_at, 4);
    if find_key(&lines, agents_at + 1, agents_end, 6, name).is_some() {
        return Err(format!("team '{team}' already has an agent '{name}'"));
    }
    out.extend(lines[..agents_at].iter().map(|l| l.to_string()));
    // `agents: {}` becomes a block; a block gets the agent after its last one.
    let flow = lines[agents_at].contains('{');
    out.push("    agents:".into());
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

/// `text` without the agent `name` of `team`.
pub fn remove_agent(text: &str, team: &str, name: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let not_found = || format!("no agent '{team}/{name}' in the config");
    let teams_at = find_key(&lines, 0, lines.len(), 0, "teams").ok_or_else(not_found)?;
    let teams_end = block_end(&lines, teams_at, 0);
    let team_at = find_key(&lines, teams_at + 1, teams_end, 2, team).ok_or_else(not_found)?;
    let team_end = block_end(&lines, team_at, 2);
    let agents_at = find_key(&lines, team_at + 1, team_end, 4, "agents").ok_or_else(not_found)?;
    let agents_end = block_end(&lines, agents_at, 4);
    let at = find_key(&lines, agents_at + 1, agents_end, 6, name).ok_or_else(not_found)?;
    let end = block_end(&lines, at, 6);
    let mut out: Vec<String> = lines[..at].iter().map(|l| l.to_string()).collect();
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    // The last agent leaves `agents: {}`, which is a team with none.
    let still_has = (agents_at + 1..agents_end)
        .filter(|i| *i < at || *i >= end)
        .any(|i| !is_blank_or_comment(lines[i]));
    if !still_has {
        out[agents_at] = "    agents: {}".into();
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

    const ORG: &str = "version: 2\nllm_backends: {}\n# the teams\nteams:\n  software:\n    prompt: Code.\n    tracker:\n      type: local_markdown\n    gates: {}\n    agents:\n      coder:\n        max_seconds: 60\n        max_concurrent: 1\n        isolation: shared\n        budget:\n          type: unlimited\n        adapter:\n          type: opencode\n          model:\n            type: provider\n            name: opencode/m\n          tools: [read]\n          system_prompt: append\n          max_steps: 3\n          command: opencode\n      # keep this comment with the next agent\n  hardware:\n    prompt: Boards.\n    tracker:\n      type: local_markdown\n    gates: {}\n    agents: {}\n";

    fn reviewer() -> String {
        agent_block(
            "reviewer",
            &NewAgent {
                adapter: CLAUDE_CODE_AGENT,
                prompt: Some("Reviews: carefully."),
                model: None,
                backend: None,
                writes: false,
            },
        )
        .unwrap()
    }

    fn loads(text: &str) -> Config {
        parse(text, Path::new("config.yaml"), &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    #[test]
    fn an_agent_is_added_last_in_its_team_with_everything_else_kept() {
        let out = add_agent(ORG, "software", "reviewer", &reviewer(), "x").unwrap();
        assert!(out.contains("# the teams\n"), "comments survive");
        assert!(out.contains("# keep this comment with the next agent"));
        let c = loads(&out);
        assert_eq!(c.agent_names(), vec!["software/coder", "software/reviewer"]);
        let r = &c.agents["software/reviewer"];
        assert_eq!(r.kind(), CLAUDE_CODE_AGENT);
        assert_eq!(r.prompt.as_deref(), Some("Reviews: carefully."));
        assert!(!r.can_change_files());
        assert!(c.teams["hardware"].agents.is_empty());
    }

    #[test]
    fn an_agent_goes_into_an_empty_agents_mapping() {
        let out = add_agent(ORG, "hardware", "reviewer", &reviewer(), "x").unwrap();
        assert!(out.contains("    agents:\n      reviewer:"), "{out}");
        assert_eq!(
            loads(&out).teams["hardware"].agents,
            vec!["hardware/reviewer"]
        );
    }

    #[test]
    fn a_missing_team_is_made_and_a_duplicate_is_refused() {
        let out = add_agent(
            ORG,
            "docs",
            "writer",
            &reviewer().replace("reviewer", "writer"),
            "Writes docs.",
        )
        .unwrap();
        let c = loads(&out);
        assert_eq!(c.teams["docs"].summary(), "Writes docs.");
        assert_eq!(c.teams["docs"].agents, vec!["docs/writer"]);
        let err = add_agent(ORG, "software", "coder", &reviewer(), "x").unwrap_err();
        assert!(err.contains("already has"), "{err}");
    }

    #[test]
    fn an_agent_is_removed_with_its_block_and_the_last_one_leaves_an_empty_team() {
        let both = add_agent(ORG, "software", "reviewer", &reviewer(), "x").unwrap();
        let out = remove_agent(&both, "software", "coder").unwrap();
        assert_eq!(loads(&out).agent_names(), vec!["software/reviewer"]);
        let none = remove_agent(&out, "software", "reviewer").unwrap();
        let c = loads(&none);
        assert!(c.agents.is_empty() && c.teams.contains_key("software"));
        assert!(none.contains("    agents: {}"), "{none}");
        assert!(
            remove_agent(&none, "software", "coder")
                .unwrap_err()
                .contains("no agent")
        );
    }

    #[test]
    fn every_adapter_writes_every_setting() {
        for (adapter, backend, model) in [
            (CLAUDE_CODE_AGENT, None, None),
            (OPENCODE_AGENT, None, None),
            (OPENCODE_AGENT, Some("local"), Some("qwen3")),
            (LLM_AGENT, Some("local"), Some("small")),
        ] {
            let block = agent_block(
                "x",
                &NewAgent {
                    adapter,
                    prompt: None,
                    model,
                    backend,
                    writes: true,
                },
            )
            .unwrap();
            let text = format!(
                "version: 2\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }}\nteams:\n  t:\n    prompt: p\n    tracker: {{ type: local_markdown }}\n    gates: {{}}\n    agents:\n{block}"
            );
            loads(&text);
        }
        assert!(
            agent_block(
                "x",
                &NewAgent {
                    adapter: LLM_AGENT,
                    prompt: None,
                    model: None,
                    backend: None,
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
                    backend: None,
                    writes: false
                }
            )
            .is_err()
        );
    }
}
