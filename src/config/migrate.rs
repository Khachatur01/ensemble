//! Version 1 (one team per project, `.ensemble/config.yaml`) to version 2 (an
//! organization of teams): the same settings, under one team called `default`.

use serde_norway::{Mapping, Value};

/// The team a migrated config puts everything in.
pub const MIGRATED_TEAM: &str = "default";

/// Rewrite the version 1 document `text` as version 2. Comments are lost; keys
/// this module does not know are kept at the top, where loading refuses them
/// by name. A document of version 2 is returned as it is, and one of another
/// version keeps its version, so that loading it reports the version.
pub fn to_v2(text: &str) -> Result<String, String> {
    let doc: Value = serde_norway::from_str(text).map_err(|e| e.to_string())?;
    let Value::Mapping(old) = doc else {
        return Err("the document is not a mapping".into());
    };
    if old.get("version") == Some(&Value::Number(2.into())) {
        return Ok(text.to_string());
    }
    let key = |s: &str| Value::String(s.to_string());
    let mut top = Mapping::new();
    let mut team = Mapping::new();
    team.insert(
        key("prompt"),
        Value::String("The agents of this project.".into()),
    );
    let mut tracker = Mapping::new();
    tracker.insert(key("type"), key("local_markdown"));
    team.insert(key("tracker"), Value::Mapping(tracker));

    for (k, v) in old {
        match k.as_str() {
            Some("version") => {
                let v1 = v == Value::Number(1.into());
                top.insert(k, if v1 { Value::Number(2.into()) } else { v });
            }
            Some("llm_backends") => {
                top.insert(k, v);
            }
            Some("gates") | Some("agents") => {
                team.insert(k, v);
            }
            _ => {
                top.insert(k, v);
            }
        }
    }
    if !top.contains_key("limits") {
        top.insert(key("limits"), Value::Mapping(Mapping::new()));
    }
    let mut teams = Mapping::new();
    teams.insert(key(MIGRATED_TEAM), Value::Mapping(team));
    top.insert(key("teams"), Value::Mapping(teams));
    serde_norway::to_string(&Value::Mapping(top)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_and_agents_move_under_one_team() {
        let v2 = to_v2("version: 1\nllm_backends: {}\ngates: {}\nagents: {}\n").unwrap();
        let doc: Value = serde_norway::from_str(&v2).unwrap();
        assert_eq!(doc["version"], Value::Number(2.into()));
        let team = &doc["teams"][MIGRATED_TEAM];
        assert!(team["agents"].is_mapping() && team["gates"].is_mapping());
        assert_eq!(
            team["tracker"]["type"],
            Value::String("local_markdown".into())
        );
        assert!(doc.get("agents").is_none());
    }

    #[test]
    fn another_version_is_left_for_loading_to_report() {
        let v2 = to_v2("version: 9\nllm_backends: {}\n").unwrap();
        assert!(v2.contains("version: 9"), "{v2}");
    }

    #[test]
    fn an_unknown_top_level_key_is_kept_so_loading_refuses_it() {
        let v2 = to_v2("version: 1\nbackends: {}\n").unwrap();
        assert!(v2.contains("backends"), "{v2}");
    }
}
