//! The OpenAI-compatible adapter against a real socket.
//!
//! The unit suite injects a fake `Http`, which covers every branch of the
//! request path but proves nothing about `UreqHttp` itself — whether the URL is
//! assembled correctly, whether the bearer header is actually sent, whether a
//! non-2xx comes back as a status rather than being swallowed. This binds a
//! loopback listener and checks exactly that.
//!
//! The server is ~40 lines of `std::net` rather than a test dependency; it only
//! has to answer one request per connection.

use ensemble::config::{BackendSettings, LlmBackend};
use ensemble::port::{Message, ModelPort, Request};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

struct Captured {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

/// Serve `replies` (status, body), one per connection, and report what was
/// received. Returns the base URL and a receiver of captured requests.
fn stub_server(replies: Vec<(u16, String)>) -> (String, mpsc::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        for (status, body) in replies {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(&stream);

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                return;
            }
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();

            let mut authorization = None;
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                let lower = header.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("authorization:") {
                    authorization = Some(v.trim().to_string());
                }
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }

            let mut buf = vec![0u8; length];
            if length > 0 && reader.read_exact(&mut buf).is_err() {
                return;
            }

            let _ = tx.send(Captured {
                method,
                path,
                authorization,
                body: String::from_utf8_lossy(&buf).to_string(),
            });

            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        }
    });

    (format!("http://127.0.0.1:{port}/v1"), rx)
}

fn adapter(base_url: &str, api_key: Option<&str>) -> std::sync::Arc<dyn ModelPort> {
    let backend = LlmBackend {
        name: "openai-local".into(),
        timeout_seconds: 10,
        settings: BackendSettings::OpenAiCompatible {
            base_url: base_url.into(),
            api_key: api_key.map(str::to_string),
            base_url_env: "OPENAI_LOCAL_BASE_URL".into(),
        },
    };
    ensemble::backend::build(&backend).expect("build the adapter")
}

const COMPLETION: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"qwen3-coder",
  "choices":[{"index":0,"message":{"role":"assistant","content":"hello from the stub"},
  "finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":4}}"#;

#[test]
fn a_completion_round_trips_over_a_real_socket() {
    let (url, rx) = stub_server(vec![(200, COMPLETION.to_string())]);
    let port = adapter(&url, Some("sk-test-key"));

    let response = port
        .complete(&Request::new(
            "qwen3-coder",
            vec![Message::system("be brief"), Message::user("say hello")],
        ))
        .expect("the call should succeed");

    assert_eq!(response.text, "hello from the stub");
    assert_eq!(response.model, "qwen3-coder");
    assert_eq!(response.usage.prompt_tokens, 7);
    assert_eq!(response.usage.total(), 11);

    let got = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the server saw a request");
    assert_eq!(got.method, "POST");
    assert_eq!(
        got.path, "/v1/chat/completions",
        "the base URL must not be mangled"
    );
    assert_eq!(got.authorization.as_deref(), Some("bearer sk-test-key"));

    let body: serde_json::Value = serde_json::from_str(&got.body).expect("the body is JSON");
    assert_eq!(body["model"], "qwen3-coder");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "say hello");
}

#[test]
fn no_authorization_header_is_sent_when_no_key_is_configured() {
    // A local llama-server wants none, and an empty bearer token is worse than
    // no header: some servers reject it where they would have allowed anonymous.
    let (url, rx) = stub_server(vec![(200, COMPLETION.to_string())]);
    adapter(&url, None)
        .complete(&Request::new("qwen3-coder", vec![Message::user("hi")]))
        .unwrap();
    let got = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(got.authorization, None, "{:?}", got.authorization);
}

#[test]
fn a_server_error_surfaces_with_its_status_and_message() {
    let (url, _rx) = stub_server(vec![(
        503,
        r#"{"error":{"message":"model is still loading"}}"#.to_string(),
    )]);
    let err = adapter(&url, None)
        .complete(&Request::new("qwen3-coder", vec![Message::user("hi")]))
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("503"), "{text}");
    assert!(text.contains("model is still loading"), "{text}");
    assert!(err.is_retryable());
}

#[test]
fn a_401_points_at_the_key_variable() {
    let (url, _rx) = stub_server(vec![(
        401,
        r#"{"error":{"message":"invalid key"}}"#.to_string(),
    )]);
    let err = adapter(&url, Some("sk-wrong"))
        .complete(&Request::new("qwen3-coder", vec![Message::user("hi")]))
        .unwrap_err();
    assert!(
        err.to_string().contains("invalid key"),
        "the server's own message survives: {err}"
    );
    assert!(err.to_string().contains("api_key_env"), "{err}");
    assert!(!err.is_retryable());
}

#[test]
fn a_non_json_body_served_with_200_is_a_protocol_error() {
    // What a reverse proxy in front of a down model server actually returns.
    let (url, _rx) = stub_server(vec![(200, "<html>502 Bad Gateway</html>".to_string())]);
    let err = adapter(&url, None)
        .complete(&Request::new("qwen3-coder", vec![Message::user("hi")]))
        .unwrap_err();
    assert!(err.to_string().contains("unreadable response"), "{err}");
}

#[test]
fn health_queries_models_and_costs_no_tokens() {
    let (url, rx) = stub_server(vec![(200, r#"{"object":"list","data":[]}"#.to_string())]);
    adapter(&url, None).health().expect("healthy");
    let got = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(got.method, "GET");
    assert_eq!(got.path, "/v1/models");
}

#[test]
fn health_fails_against_a_refused_connection() {
    // Bind then drop, so the port is real and certainly closed.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let err = adapter(&format!("http://127.0.0.1:{port}/v1"), None)
        .health()
        .unwrap_err();
    assert!(err.to_string().contains("openai-local"), "{err}");
    assert!(err.is_retryable());
}
