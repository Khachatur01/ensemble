//! Older config versions to version 3, a flat helper team: version 1 (one team
//! per project, `.ensemble/config.yaml`) and version 2 (an organization of
//! teams) carry the same settings in other shapes.

use serde_norway::{Mapping, Value};

/// Rewrite the document `text` as version 3. Comments are lost; keys this
/// module does not know are kept at the top, where loading refuses them by
/// name. A document of version 3 is returned as it is, and one of another
/// version keeps its version, so that loading it reports the version.
///
/// Version 1 had `gates` and `agents` at the top, no `limits` and no `tracker`;
/// both are added. Version 2 kept them under `teams`, each team with a `prompt`
/// and a `tracker`: the teams' agents and gates are pulled up, a name two teams
/// both used is prefixed with its team's, the prompts are dropped, and the
/// trackers must agree, because there is one now. An agent named `team/agent`
/// in a version 2 task is read as `agent` by the task store.
pub fn to_v3(text: &str) -> Result<String, String> {
    let doc: Value = serde_norway::from_str(text).map_err(|e| e.to_string())?;
    let Value::Mapping(old) = doc else {
        return Err("the document is not a mapping".into());
    };
    let version = old.get("version").cloned();
    if version == Some(Value::Number(3.into())) {
        return Ok(text.to_string());
    }
    let key = |s: &str| Value::String(s.to_string());
    let mut top = Mapping::new();
    let mut tracker: Option<Value> = None;
    // Left out when the document has none, so that loading says they are missing.
    let mut gates: Option<Mapping> = None;
    let mut agents: Option<Mapping> = None;

    for (k, v) in old {
        match k.as_str() {
            Some("version") => {
                let known = v == Value::Number(1.into()) || v == Value::Number(2.into());
                top.insert(k, if known { Value::Number(3.into()) } else { v });
            }
            Some("llm_backends") | Some("limits") => {
                top.insert(k, v);
            }
            Some("gates") if v.is_mapping() => {
                gates = v.as_mapping().cloned();
            }
            Some("agents") if v.is_mapping() => {
                agents = v.as_mapping().cloned();
            }
            Some("teams") if v.is_mapping() => {
                let (team_gates, team_agents) = (
                    gates.get_or_insert_with(Mapping::new),
                    agents.get_or_insert_with(Mapping::new),
                );
                flatten_teams(
                    v.as_mapping().unwrap(),
                    &mut tracker,
                    team_gates,
                    team_agents,
                )?;
            }
            _ => {
                top.insert(k, v);
            }
        }
    }
    if !top.contains_key("limits") {
        top.insert(key("limits"), Value::Mapping(Mapping::new()));
    }
    top.insert(
        key("tracker"),
        tracker.unwrap_or_else(|| {
            let mut local = Mapping::new();
            local.insert(key("type"), key("local_markdown"));
            Value::Mapping(local)
        }),
    );
    if let Some(gates) = gates {
        top.insert(key("gates"), Value::Mapping(gates));
    }
    if let Some(agents) = agents {
        top.insert(key("agents"), Value::Mapping(rename_adapters(agents)));
    }
    serde_norway::to_string(&Value::Mapping(top)).map_err(|e| e.to_string())
}

/// The agents and gates of every team of a version 2 document, pulled up to
/// one level. A name two teams both use becomes `<team>-<name>` for each of
/// them, so nothing is lost and nothing collides. Prompts are dropped; the
/// trackers must agree.
fn flatten_teams(
    teams: &Mapping,
    tracker: &mut Option<Value>,
    gates: &mut Mapping,
    agents: &mut Mapping,
) -> Result<(), String> {
    let shared = |pick: &dyn Fn(&Value) -> Option<&Mapping>| -> Vec<Value> {
        let mut seen: Vec<Value> = Vec::new();
        let mut twice: Vec<Value> = Vec::new();
        for team in teams.values() {
            for name in pick(team).map(Mapping::keys).into_iter().flatten() {
                if seen.contains(name) {
                    if !twice.contains(name) {
                        twice.push(name.clone());
                    }
                } else {
                    seen.push(name.clone());
                }
            }
        }
        twice
    };
    let agents_twice = shared(&|t| t.get("agents").and_then(Value::as_mapping));
    let gates_twice = shared(&|t| t.get("gates").and_then(Value::as_mapping));
    let mut tracker_of: Option<(String, Value)> = None;
    for (team_name, team) in teams {
        let team_name = team_name.as_str().unwrap_or_default().to_string();
        if let Some(t) = team.get("tracker") {
            match &tracker_of {
                None => tracker_of = Some((team_name.clone(), t.clone())),
                Some((first, seen)) if seen != t => {
                    return Err(format!(
                        "teams '{first}' and '{team_name}' have different trackers; version 3 has one tracker for the whole helper team. Make them the same, then migrate."
                    ));
                }
                Some(_) => {}
            }
        }
        let prefixed = |name: &Value, twice: &[Value]| -> Value {
            match name.as_str() {
                Some(n) if twice.contains(name) => Value::String(format!("{team_name}-{n}")),
                _ => name.clone(),
            }
        };
        if let Some(a) = team.get("agents").and_then(Value::as_mapping) {
            for (name, v) in a {
                agents.insert(prefixed(name, &agents_twice), v.clone());
            }
        }
        if let Some(g) = team.get("gates").and_then(Value::as_mapping) {
            for (name, v) in g {
                gates.insert(prefixed(name, &gates_twice), v.clone());
            }
        }
    }
    *tracker = tracker_of.map(|(_, t)| t);
    Ok(())
}

/// `type: llm` became `type: direct` in version 3.
fn rename_adapters(agents: Mapping) -> Mapping {
    agents
        .into_iter()
        .map(|(name, mut agent)| {
            if let Some(adapter) = agent.get_mut("adapter").and_then(Value::as_mapping_mut)
                && adapter.get("type") == Some(&Value::String("llm".into()))
            {
                adapter.insert(Value::String("type".into()), Value::String("direct".into()));
            }
            (name, agent)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Value {
        serde_norway::from_str(&to_v3(text).unwrap()).unwrap()
    }

    #[test]
    fn version_one_gains_limits_and_a_tracker() {
        let doc = doc("version: 1\nllm_backends: {}\ngates: {}\nagents: {}\n");
        assert_eq!(doc["version"], Value::Number(3.into()));
        assert!(doc["agents"].is_mapping() && doc["gates"].is_mapping());
        assert!(doc["limits"].is_mapping());
        assert_eq!(
            doc["tracker"]["type"],
            Value::String("local_markdown".into())
        );
        assert!(doc.get("teams").is_none());
    }

    #[test]
    fn version_two_teams_are_pulled_up_and_their_prompts_dropped() {
        let doc = doc(
            "version: 2\nllm_backends: {}\nlimits: {}\nteams:\n  hardware:\n    prompt: Boards.\n    tracker: { type: local_markdown }\n    gates:\n      drc: { command: drc, timeout_seconds: 5 }\n    agents:\n      layout: { max_seconds: 1 }\n  software:\n    prompt: Code.\n    tracker: { type: local_markdown }\n    gates: {}\n    agents:\n      coder: { max_seconds: 1 }\n",
        );
        assert_eq!(doc["version"], Value::Number(3.into()));
        assert!(doc["agents"]["layout"].is_mapping() && doc["agents"]["coder"].is_mapping());
        assert!(doc["gates"]["drc"].is_mapping());
        assert_eq!(
            doc["tracker"]["type"],
            Value::String("local_markdown".into())
        );
        assert!(doc.get("teams").is_none());
        assert!(
            to_v3(&serde_norway::to_string(&doc).unwrap())
                .unwrap()
                .contains("version: 3")
        );
    }

    #[test]
    fn a_name_two_teams_share_is_prefixed_with_the_team() {
        let doc = doc(
            "version: 2\nllm_backends: {}\nlimits: {}\nteams:\n  hardware:\n    prompt: p\n    tracker: { type: local_markdown }\n    gates:\n      test: { command: a, timeout_seconds: 5 }\n    agents:\n      coder: { max_seconds: 1 }\n  software:\n    prompt: p\n    tracker: { type: local_markdown }\n    gates:\n      test: { command: b, timeout_seconds: 5 }\n    agents:\n      coder: { max_seconds: 2 }\n      planner: { max_seconds: 3 }\n",
        );
        let agents = doc["agents"].as_mapping().unwrap();
        let names: Vec<&str> = agents.keys().filter_map(Value::as_str).collect();
        assert_eq!(names, ["hardware-coder", "software-coder", "planner"]);
        let gates = doc["gates"].as_mapping().unwrap();
        let names: Vec<&str> = gates.keys().filter_map(Value::as_str).collect();
        assert_eq!(names, ["hardware-test", "software-test"]);
    }

    #[test]
    fn teams_with_different_trackers_are_refused_by_name() {
        let err = to_v3(
            "version: 2\nllm_backends: {}\nlimits: {}\nteams:\n  a:\n    prompt: p\n    tracker: { type: local_markdown }\n    gates: {}\n    agents: {}\n  b:\n    prompt: p\n    tracker: { type: jira }\n    gates: {}\n    agents: {}\n",
        )
        .unwrap_err();
        assert!(
            err.contains("'a' and 'b'") && err.contains("trackers"),
            "{err}"
        );
    }

    #[test]
    fn the_llm_adapter_is_renamed_direct() {
        let doc = doc(
            "version: 1\nllm_backends: {}\ngates: {}\nagents:\n  s:\n    adapter: { type: llm, backend: b, model: m, max_tokens: 1 }\n",
        );
        assert_eq!(
            doc["agents"]["s"]["adapter"]["type"],
            Value::String("direct".into())
        );
    }

    #[test]
    fn another_version_is_left_for_loading_to_report() {
        let v3 = to_v3("version: 9\nllm_backends: {}\n").unwrap();
        assert!(v3.contains("version: 9"), "{v3}");
    }

    #[test]
    fn an_unknown_top_level_key_is_kept_so_loading_refuses_it() {
        let v3 = to_v3("version: 1\nbackends: {}\n").unwrap();
        assert!(v3.contains("backends"), "{v3}");
    }
}
