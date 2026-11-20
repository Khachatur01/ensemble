//! How a runner names the hub a token is for: its URL, the same however it
//! is written. The runner keeps its tokens in its database
//! (`ensemble_runner::kept`).

/// A hub URL as tokens are kept by: without a trailing `/`.
pub fn normal(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_url_is_the_same_with_or_without_its_trailing_slash() {
        assert_eq!(super::normal(" wss://hub.acme.com/ "), "wss://hub.acme.com");
    }
}
