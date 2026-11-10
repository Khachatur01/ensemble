//! A check for secrets in what someone shares, before it is kept. A share is
//! kept in git, so a key in it would stay in the history; the agent is told
//! what looked like one, and shares again without it.

/// A token that starts with `prefix` and has at least `min` more characters
/// of the kind keys are made of.
struct Token {
    name: &'static str,
    prefix: &'static str,
    min: usize,
}

const TOKENS: [Token; 10] = [
    Token {
        name: "an Anthropic API key",
        prefix: "sk-ant-",
        min: 20,
    },
    Token {
        name: "an OpenAI-style API key",
        prefix: "sk-",
        min: 20,
    },
    Token {
        name: "a GitHub token",
        prefix: "ghp_",
        min: 30,
    },
    Token {
        name: "a GitHub token",
        prefix: "gho_",
        min: 30,
    },
    Token {
        name: "a GitHub token",
        prefix: "github_pat_",
        min: 30,
    },
    Token {
        name: "a GitLab token",
        prefix: "glpat-",
        min: 20,
    },
    Token {
        name: "an AWS access key",
        prefix: "AKIA",
        min: 16,
    },
    Token {
        name: "a Slack token",
        prefix: "xoxb-",
        min: 20,
    },
    Token {
        name: "a Slack token",
        prefix: "xoxp-",
        min: 20,
    },
    Token {
        name: "a Google API key",
        prefix: "AIza",
        min: 35,
    },
];

/// What in `text` looks like a secret, by kind, or nothing.
pub fn find_secret(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    let key_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    for token in &TOKENS {
        let mut from = 0;
        while let Some(found) = text[from..].find(token.prefix) {
            let start = from + found;
            let after = start + token.prefix.len();
            // A key starts a word: `task-` is not `sk-`.
            let starts_word = text[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !key_char(c));
            let length = text[after..].chars().take_while(|c| key_char(*c)).count();
            if starts_word && length >= token.min {
                return Some(token.name);
            }
            from = after;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_private_keys_are_found_by_kind() {
        let key = format!("use sk-ant-{} here", "a".repeat(30));
        assert_eq!(find_secret(&key), Some("an Anthropic API key"));
        let key = format!("OPENAI=sk-{}", "Ab1_".repeat(8));
        assert_eq!(find_secret(&key), Some("an OpenAI-style API key"));
        assert_eq!(
            find_secret(&format!("token ghp_{}", "x".repeat(36))),
            Some("a GitHub token")
        );
        assert_eq!(
            find_secret("AKIAIOSFODNN7EXAMPLE is the id"),
            Some("an AWS access key")
        );
        assert_eq!(
            find_secret(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----"
            ),
            Some("a private key")
        );
    }

    #[test]
    fn ordinary_text_is_no_secret() {
        for text in [
            "We decided to use sk- prefixes for SKUs: sk-12.",
            "the task-runner-for-everything-in-the-repository-and-more runs",
            "AKIA is the prefix AWS uses",
            "a mask-of-many-words-that-goes-on-and-on-and-on is not a key",
        ] {
            assert_eq!(find_secret(text), None, "{text}");
        }
    }
}
