//! What Ensemble knows of models before it asks them anything: how strong
//! an LLM of a well-known family is, by its id, or by how many parameters it
//! has. A model it does not know is medium, and the person who adds it
//! may say otherwise.

use ensemble_config::{HarnessKind, Tier};

/// Families known by a word of their id, the first that matches winning, so
/// `mini` and `nano` come before the families they belong to.
const FAMILIES: &[(&str, Tier)] = &[
    ("nano", Tier::Small),
    ("mini", Tier::Medium),
    ("haiku", Tier::Medium),
    ("flash", Tier::Medium),
    ("opus", Tier::Strong),
    ("sonnet", Tier::Strong),
    ("gpt-5", Tier::Strong),
    ("gpt-4.1", Tier::Strong),
    ("gpt-4o", Tier::Strong),
    ("o3", Tier::Strong),
    ("o4", Tier::Strong),
    ("gemini", Tier::Strong),
    ("deepseek-r1", Tier::Strong),
    ("deepseek-v3", Tier::Strong),
    ("kimi-k2", Tier::Strong),
    ("glm-4.5", Tier::Strong),
];

/// The models Ensemble knows of a harness that drives its own vendor's LLMs,
/// with how strong each is: Claude Code's by its aliases, and Codex's. They
/// are only suggested: a harness drives those a person added to it, which
/// belong to the harness, not the ANN pool; OpenCode and `builtin` have none
/// of their own.
pub fn harness_models(kind: HarnessKind) -> Vec<(&'static str, Tier)> {
    let ids: &[&str] = match kind {
        HarnessKind::ClaudeCode => &["haiku", "sonnet", "opus"],
        HarnessKind::Codex => &["gpt-5-codex", "gpt-5", "gpt-5-mini"],
        HarnessKind::OpenCode | HarnessKind::Builtin => &[],
    };
    ids.iter()
        .map(|id| (*id, family(id).expect("the catalog knows its own models")))
        .collect()
}

/// The tier of the model `id`: by its family, else by the size its id says,
/// such as `qwen3.5:9b`; none when the id says neither.
pub fn tier(id: &str) -> Option<Tier> {
    family(id).or_else(|| billions_in(&name_of(id)).map(by_size))
}

/// The tier of the model `id` by its family alone: an LLM Ensemble knows.
pub fn family(id: &str) -> Option<Tier> {
    let name = name_of(id);
    FAMILIES
        .iter()
        .find(|(word, _)| has_word(&name, word))
        .map(|(_, tier)| *tier)
}

/// The id, lower case, without the `provider/` a server often puts before it.
fn name_of(id: &str) -> String {
    let id = id.to_ascii_lowercase();
    id.rsplit('/').next().unwrap_or(&id).to_string()
}

/// The tier of an LLM of `billions` parameters.
pub fn by_size(billions: f64) -> Tier {
    if billions <= 4.0 {
        Tier::Small
    } else if billions <= 34.0 {
        Tier::Medium
    } else {
        Tier::Strong
    }
}

/// The parameters a size such as Ollama's `7.6B` or `500M` says, in billions.
pub fn billions(size: &str) -> Option<f64> {
    let size = size.trim().to_ascii_lowercase();
    if let Some(n) = size.strip_suffix('b') {
        n.parse().ok()
    } else if let Some(n) = size.strip_suffix('m') {
        n.parse::<f64>().ok().map(|n| n / 1000.0)
    } else {
        None
    }
}

/// Whether `word` is in `name` with no letter or digit right before it.
fn has_word(name: &str, word: &str) -> bool {
    name.match_indices(word).any(|(at, _)| {
        name[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

/// The first size an id says, such as the `9` of `qwen3.5:9b` or the `30` of
/// `qwen3-coder:30b-a3b`: a number right before a `b` that ends a word.
fn billions_in(name: &str) -> Option<f64> {
    let bytes = name.as_bytes();
    for (at, _) in name.match_indices('b') {
        let ends = bytes.get(at + 1).is_none_or(|c| !c.is_ascii_alphanumeric());
        let start = name[..at]
            .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map_or(0, |i| i + 1);
        let starts = name[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphabetic());
        if ends
            && starts
            && start < at
            && let Ok(n) = name[start..at].parse()
        {
            return Some(n);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_is_known_by_its_family_then_by_the_size_its_id_says() {
        assert_eq!(tier("claude-opus-4-1"), Some(Tier::Strong));
        assert_eq!(tier("claude-haiku-4-5"), Some(Tier::Medium));
        assert_eq!(tier("gpt-5-mini"), Some(Tier::Medium));
        assert_eq!(tier("openai/gpt-5"), Some(Tier::Strong));
        assert_eq!(tier("qwen3.5:9b"), Some(Tier::Medium));
        assert_eq!(tier("qwen3-coder:30b-a3b"), Some(Tier::Medium));
        assert_eq!(tier("qwen3.5:72b"), Some(Tier::Strong));
        assert_eq!(tier("llama3.2:1b"), Some(Tier::Small));
        assert_eq!(tier("Qwen3-Embedding-0.6B"), Some(Tier::Small));
        assert_eq!(tier("my-model"), None);
        assert_eq!(tier("bomb"), None, "a b inside a word is no size");
    }

    #[test]
    fn only_claude_code_and_codex_have_models_of_their_own() {
        let tiers = |kind| {
            harness_models(kind)
                .into_iter()
                .map(|(_, t)| t)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            tiers(HarnessKind::ClaudeCode),
            [Tier::Medium, Tier::Strong, Tier::Strong]
        );
        assert_eq!(
            tiers(HarnessKind::Codex),
            [Tier::Strong, Tier::Strong, Tier::Medium]
        );
        assert!(harness_models(HarnessKind::OpenCode).is_empty());
        assert!(harness_models(HarnessKind::Builtin).is_empty());
    }

    #[test]
    fn a_size_is_read_in_billions() {
        assert_eq!(billions("7.6B"), Some(7.6));
        assert_eq!(billions("500M"), Some(0.5));
        assert_eq!(billions("big"), None);
        assert_eq!(by_size(70.0), Tier::Strong);
    }
}
