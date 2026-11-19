//! A throwaway prototype: the same streamed chat request to a stand-in model
//! server, made directly and through a relay of three hops (a loopback proxy
//! on runner A, a WebSocket to the hub, a WebSocket to runner B, which calls
//! the server), to see what relaying costs.

use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const FIRST: Duration = Duration::from_millis(20);
const PER_TOKEN: Duration = Duration::from_millis(5);
const TOKENS: usize = 50;

async fn read_request(s: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut body = buf[end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await.unwrap();
        body.extend_from_slice(&chunk[..n]);
    }
    (head, body)
}

async fn stand_in() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let _ = read_request(&mut s).await;
                tokio::time::sleep(FIRST).await;
                s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n").await.unwrap();
                for _ in 0..TOKENS {
                    s.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"tok \"}}]}\n\n")
                        .await
                        .unwrap();
                    tokio::time::sleep(PER_TOKEN).await;
                }
                s.write_all(b"data: [DONE]\n\n").await.unwrap();
            });
        }
    });
    addr
}

/// Time to the first `data:` and in all.
async fn ask(addr: &str, body: &[u8]) -> (Duration, Duration) {
    let start = Instant::now();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.set_nodelay(true).unwrap();
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    let mut first = None;
    loop {
        let n = s.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if first.is_none() && buf.windows(5).any(|w| w == b"data:") {
            first = Some(start.elapsed());
        }
    }
    (first.unwrap(), start.elapsed())
}

fn frame(id: u64, bytes: &[u8]) -> Message {
    let mut v = id.to_be_bytes().to_vec();
    v.extend_from_slice(bytes);
    Message::binary(v)
}

/// The hub: the first link is runner B, the second runner A; frames go across.
async fn hub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let (b, _) = l.accept().await.unwrap();
        b.set_nodelay(true).unwrap();
        let b = tokio_tungstenite::accept_async(b).await.unwrap();
        let (a, _) = l.accept().await.unwrap();
        a.set_nodelay(true).unwrap();
        let a = tokio_tungstenite::accept_async(a).await.unwrap();
        let (mut a_tx, mut a_rx) = a.split();
        let (mut b_tx, mut b_rx) = b.split();
        loop {
            tokio::select! {
                Some(Ok(m)) = a_rx.next() => {
                    let m = match m { Message::Text(t) => { let v: serde_json::Value = serde_json::from_str(&t).unwrap(); Message::text(v.to_string()) } m => m };
                    b_tx.send(m).await.unwrap();
                }
                Some(Ok(m)) = b_rx.next() => {
                    let m = match m { Message::Text(t) => { let v: serde_json::Value = serde_json::from_str(&t).unwrap(); Message::text(v.to_string()) } m => m };
                    a_tx.send(m).await.unwrap();
                }
                else => return,
            }
        }
    });
    url
}

/// Runner B: serves calls by calling the stand-in.
async fn runner_b(hub: &str, server: String) {
    let (ws, _) = tokio_tungstenite::connect_async_with_config(hub, None, true)
        .await
        .unwrap();
    tokio::spawn(async move {
        let (mut tx, mut rx) = ws.split();
        let mut body = Vec::new();
        while let Some(Ok(m)) = rx.next().await {
            match m {
                Message::Binary(b) => body.extend_from_slice(&b[8..]),
                Message::Text(t) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v["event"] != "sent" {
                        continue;
                    }
                    let id = v["data"]["call"].as_u64().unwrap();
                    let mut s = TcpStream::connect(&server).await.unwrap();
                    s.set_nodelay(true).unwrap();
                    let head = format!(
                        "POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    );
                    s.write_all(head.as_bytes()).await.unwrap();
                    s.write_all(&body).await.unwrap();
                    body.clear();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16384];
                    let mut headed = false;
                    loop {
                        let n = s.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        if headed {
                            tx.send(frame(id, &chunk[..n])).await.unwrap();
                            continue;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            headed = true;
                            tx.send(Message::text(serde_json::json!({"event": "reply", "data": {"call": id, "status": 200, "content_type": "text/event-stream"}}).to_string())).await.unwrap();
                            if buf.len() > i + 4 {
                                tx.send(frame(id, &buf[i + 4..])).await.unwrap();
                            }
                        }
                    }
                    tx.send(Message::text(
                        serde_json::json!({"event": "done", "data": {"call": id}}).to_string(),
                    ))
                    .await
                    .unwrap();
                }
                _ => {}
            }
        }
    });
}

/// Runner A: a loopback proxy whose calls go over its link.
async fn runner_a(hub: &str) -> String {
    let (ws, _) = tokio_tungstenite::connect_async_with_config(hub, None, true)
        .await
        .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut tx, mut rx) = ws.split();
        let mut id = 0u64;
        while let Ok((mut s, _)) = l.accept().await {
            s.set_nodelay(true).unwrap();
            id += 1;
            let (_, body) = read_request(&mut s).await;
            tx.send(Message::text(serde_json::json!({"event": "call", "data": {"call": id, "runner": "b", "backend": "x", "method": "POST", "path": "/chat/completions"}}).to_string())).await.unwrap();
            for piece in body.chunks(64 * 1024) {
                tx.send(frame(id, piece)).await.unwrap();
            }
            tx.send(Message::text(
                serde_json::json!({"event": "sent", "data": {"call": id}}).to_string(),
            ))
            .await
            .unwrap();
            while let Some(Ok(m)) = rx.next().await {
                match m {
                    Message::Binary(b) => s.write_all(&b[8..]).await.unwrap(),
                    Message::Text(t) => {
                        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                        if v["event"] == "reply" {
                            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n").await.unwrap();
                        } else {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            drop(s);
        }
    });
    addr
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn relay_overhead() {
    let server = stand_in().await;
    let hub = hub().await;
    runner_b(&hub, server.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let proxy = runner_a(&hub).await;
    for size in [200usize, 100 * 1024] {
        let prompt = "x".repeat(size);
        let body = serde_json::json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": prompt}]}).to_string();
        let (mut d1, mut dt, mut r1, mut rt) = (vec![], vec![], vec![], vec![]);
        for _ in 0..3 {
            ask(&server, body.as_bytes()).await;
            ask(&proxy, body.as_bytes()).await;
        }
        for _ in 0..20 {
            let (f, t) = ask(&server, body.as_bytes()).await;
            d1.push(f);
            dt.push(t);
            let (f, t) = ask(&proxy, body.as_bytes()).await;
            r1.push(f);
            rt.push(t);
        }
        let (d1, dt, r1, rt) = (median(d1), median(dt), median(r1), median(rt));
        println!(
            "prompt {size} B: direct first {d1:?} total {dt:?}; relayed first {r1:?} total {rt:?}; overhead first {:?} total {:?}",
            r1.saturating_sub(d1),
            rt.saturating_sub(dt)
        );
    }
}
