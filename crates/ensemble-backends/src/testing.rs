//! A model server for other crates' tests: an OpenAI-compatible endpoint on
//! loopback, with no network and no model.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

/// A running stand-in for a model server.
pub struct ModelServer {
    /// Its `base_url`, `/v1` included.
    pub url: String,
    /// The body of every chat request it got, in order.
    pub requests: Arc<Mutex<Vec<String>>>,
}

/// Serve `models` on `GET /models`, and answer chat requests with `answers`,
/// one per request, in order; the last one again once they run out. It runs
/// until the test ends.
pub fn model_server(models: &[&str], answers: &[&str]) -> ModelServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let data: Vec<String> = models
        .iter()
        .map(|m| format!("{{\"id\":\"{m}\"}}"))
        .collect();
    let list = format!("{{\"data\":[{}]}}", data.join(","));
    let chats: Vec<String> = answers
        .iter()
        .map(|a| {
            serde_json::json!({
                "choices": [{ "message": { "content": a }, "finish_reason": "stop" }]
            })
            .to_string()
        })
        .collect();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        let mut next = 0;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(&stream);
            let (mut first, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                if first.is_empty() {
                    first = line.clone();
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let reply = if first.starts_with("GET") {
                list.clone()
            } else {
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
                let reply = chats[next.min(chats.len() - 1)].clone();
                next += 1;
                reply
            };
            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    ModelServer {
        url: format!("http://127.0.0.1:{port}/v1"),
        requests,
    }
}

/// A URL nothing listens on.
pub fn nowhere() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}
