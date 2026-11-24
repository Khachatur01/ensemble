//! The runner's relay socket to its hub (`ensemble_link::relay`), beside its
//! link. On it, the runner serves others' calls to the backends it shares
//! (`llm.direct.shares`), calling its own backend, and carries its agents'
//! calls to the backends of other runners it reaches through the hub
//! (`llm.hub`), which a loopback endpoint of its own serves them
//! (`ensemble_link::loopback`). `ensemble-runner serve` keeps it when
//! runner.yaml names a hub and shares or reaches a backend there.
//!
//! A shared backend is asked only for what an agent needs of it: its models,
//! chats, completions and embeddings, never another path of its server.

use ensemble_config::{BackendSettings, LlmBackend, Runner};
use ensemble_core::runtime;
use ensemble_link::client::{Failure, Token, relay_socket};
use ensemble_link::loopback::Opener;
use ensemble_link::relay::{CHUNK, Frame, Message, Offer, Open, UNAVAILABLE, body_frames};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Why a socket ended when the token was replaced.
const REPLACED: &str = "signed in again with a new token";
/// What of a shared backend a call may ask for.
const SERVED: [(&str, &str); 4] = [
    ("GET", "/models"),
    ("POST", "/chat/completions"),
    ("POST", "/completions"),
    ("POST", "/embeddings"),
];

/// A runner's relay socket, ready to connect.
pub struct Relay {
    url: String,
    token: Token,
    offer: Offer,
    /// The runner's own backends it shares, by name.
    shared: BTreeMap<String, LlmBackend>,
    /// What is sent on the socket now; none while it is down.
    outbox: Mutex<Option<UnboundedSender<Frame>>>,
    /// Where the answers of the calls it made go, by its number for each.
    answers: Mutex<HashMap<u64, UnboundedSender<Frame>>>,
    /// Where the requests of the calls it serves go, by the hub's number for
    /// each, until each has ended.
    serving: Mutex<HashMap<u64, UnboundedSender<Frame>>>,
    next: AtomicU64,
}

impl Relay {
    /// The relay of `runner`, reaching the hub at `url` with `token`, and
    /// again with the new one when it is replaced.
    pub fn new(url: &str, token: Token, runner: &Runner) -> Relay {
        let shared = runner
            .llm
            .shared()
            .into_iter()
            .map(|b| (b.name.clone(), b.clone()))
            .collect();
        Relay {
            url: url.to_string(),
            token,
            offer: Offer {
                name: runner.name.clone(),
                shares: runner.llm.shares.clone(),
                max_shared_calls: runner.llm.max_shared_calls,
                models: BTreeMap::new(),
            },
            shared,
            outbox: Mutex::new(None),
            answers: Mutex::new(HashMap::new()),
            serving: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
        }
    }

    /// Keep the socket until `stop` is set, connecting again when it breaks.
    /// A hub that refuses it (a name taken) makes it return, saying why.
    pub async fn work(
        self: &Arc<Self>,
        stop: &AtomicBool,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        let mut backoff = Duration::from_secs(1);
        let mut replaced = self.token.watch();
        while !stop.load(Ordering::Relaxed) {
            replaced.mark_unchanged();
            match self.session(stop, say).await {
                Ok(()) => return Ok(()),
                Err(Failure::Hub(e)) => return Err(e),
                Err(Failure::Broken(e)) if e == REPLACED => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
                // The link says when the hub is down; this one keeps quiet.
                Err(Failure::Broken(_)) => {}
            }
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                () = runtime::until_set(stop) => {}
                _ = replaced.changed() => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
        Ok(())
    }

    /// One socket's life. It ends when the token is replaced, as
    /// [`REPLACED`]. Calls going on when it ends are answered unavailable.
    async fn session(
        self: &Arc<Self>,
        stop: &AtomicBool,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), Failure> {
        let mut replaced = self.token.watch();
        replaced.mark_unchanged();
        let mut socket = relay_socket(&self.url, &self.token.get())
            .await
            .map_err(Failure::Broken)?;
        // What each shared backend serves now, for the hub to list.
        let mut offer = self.offer.clone();
        for (name, backend) in &self.shared {
            offer.models.insert(name.clone(), models_of(backend).await);
        }
        let offer = serde_json::to_string(&offer).map_err(|e| Failure::Broken(e.to_string()))?;
        socket
            .send(Message::text(offer))
            .await
            .map_err(|e| Failure::Broken(e.to_string()))?;
        let answer = match tokio::time::timeout(Duration::from_secs(30), socket.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).unwrap_or_default(),
            _ => {
                return Err(Failure::Broken(
                    "the hub did not answer the relay's offer".into(),
                ));
            }
        };
        if let Some(e) = answer["error"].as_str() {
            return Err(Failure::Hub(e.to_string()));
        }
        if !self.offer.shares.is_empty() {
            say(&format!(
                "sharing {} through the hub as {}",
                self.offer.shares.join(", "),
                answer["runner"].as_str().unwrap_or("?")
            ));
        }
        let (outbox, mut sending) = unbounded_channel();
        *self.outbox.lock().unwrap() = Some(outbox);
        let result = loop {
            tokio::select! {
                () = runtime::until_set(stop) => break Ok(()),
                Ok(()) = replaced.changed() => break Err(Failure::Broken(REPLACED.into())),
                Some(frame) = sending.recv() => {
                    if let Err(e) = socket.send(frame.to_message()).await {
                        break Err(Failure::Broken(e.to_string()));
                    }
                }
                message = socket.next() => match message {
                    Some(Ok(Message::Close(_))) | None => {
                        break Err(Failure::Broken("the hub closed the relay".into()));
                    }
                    Some(Ok(m)) => {
                        if let Some(frame) = Frame::from_message(&m) {
                            self.frame(frame);
                        }
                    }
                    Some(Err(e)) => break Err(Failure::Broken(e.to_string())),
                },
            }
        };
        *self.outbox.lock().unwrap() = None;
        for (call, answer) in self.answers.lock().unwrap().drain() {
            let _ = answer.send(Frame::error(
                call,
                UNAVAILABLE,
                "this runner's relay to its hub broke",
            ));
        }
        self.serving.lock().unwrap().clear();
        let _ = socket.close(None).await;
        result
    }

    /// Send `frame` on the socket, when it is up.
    fn send(&self, frame: Frame) -> bool {
        self.outbox
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|o| o.send(frame).is_ok())
    }

    /// A frame from the hub: a call to serve, or the answer to one it made.
    fn frame(self: &Arc<Self>, frame: Frame) {
        let call = frame.call();
        if !frame.to_server() {
            let mut answers = self.answers.lock().unwrap();
            let Some(answer) = answers.get(&call) else {
                return;
            };
            let ends = frame.ends_answer();
            let gone = answer.send(frame).is_err();
            if gone || ends {
                answers.remove(&call);
            }
            drop(answers);
            if gone && !ends {
                // Whoever asked went away: the runner that serves it may stop.
                self.send(Frame::Cancel { call });
            }
            return;
        }
        match frame {
            Frame::Open { open, .. } => {
                let Some(backend) = self.shared.get(&open.backend).cloned() else {
                    self.send(Frame::error(
                        call,
                        404,
                        format!("this runner shares no backend '{}'", open.backend),
                    ));
                    return;
                };
                if !SERVED.contains(&(open.method.as_str(), open.path.as_str())) {
                    self.send(Frame::error(
                        call,
                        404,
                        format!(
                            "a shared backend serves its models, chats, completions and embeddings, not {} {}",
                            open.method, open.path
                        ),
                    ));
                    return;
                }
                let (up, request) = unbounded_channel();
                self.serving.lock().unwrap().insert(call, up);
                let relay = self.clone();
                tokio::spawn(async move { relay.serve(call, backend, open, request).await });
            }
            frame => {
                let mut serving = self.serving.lock().unwrap();
                let last = matches!(frame, Frame::End { .. } | Frame::Cancel { .. });
                if let Some(up) = serving.get(&call) {
                    let _ = up.send(frame);
                }
                if last {
                    serving.remove(&call);
                }
            }
        }
    }

    /// Serve call `call` with this runner's `backend`: read its request as it
    /// comes, ask the backend, and send its answer back as it comes.
    async fn serve(
        self: Arc<Self>,
        call: u64,
        backend: LlmBackend,
        open: Open,
        mut request: UnboundedReceiver<Frame>,
    ) {
        let mut body = Vec::new();
        loop {
            match request.recv().await {
                Some(Frame::Body { bytes, .. }) => body.extend_from_slice(&bytes),
                Some(Frame::End { .. }) => break,
                // Given up before it was asked.
                Some(Frame::Cancel { .. }) | None => return,
                Some(_) => {}
            }
        }
        let BackendSettings::OpenAiCompatible { base_url, api_key } = &backend.settings else {
            self.send(Frame::error(
                call,
                404,
                "a runner shares OpenAI-compatible backends alone",
            ));
            return;
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(backend.timeout_seconds))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                self.send(Frame::error(call, 502, format!("no HTTP client: {e}")));
                return;
            }
        };
        let url = format!("{base_url}{}", open.path);
        let mut req = if open.method == "GET" {
            client.get(&url)
        } else {
            client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
        };
        if let Some(key) = api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        let mut resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                self.send(Frame::error(
                    call,
                    502,
                    format!("the backend '{}' did not answer: {e}", backend.name),
                ));
                return;
            }
        };
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        self.send(Frame::Head {
            call,
            status: resp.status().as_u16(),
            content_type,
        });
        loop {
            // Given up while it is answered: stop asking.
            if matches!(request.try_recv(), Ok(Frame::Cancel { .. })) {
                return;
            }
            match resp.chunk().await {
                Ok(None) => {
                    self.send(Frame::Done { call });
                    return;
                }
                Ok(Some(bytes)) => {
                    for piece in bytes.chunks(CHUNK) {
                        if !self.send(Frame::Reply {
                            call,
                            bytes: piece.to_vec(),
                        }) {
                            return;
                        }
                    }
                }
                Err(e) => {
                    self.send(Frame::error(
                        call,
                        502,
                        format!(
                            "the answer of the backend '{}' broke off: {e}",
                            backend.name
                        ),
                    ));
                    return;
                }
            }
        }
    }
}

/// The calls of the runner's agents to backends it reaches through the hub,
/// for its loopback endpoint.
pub struct RelayOpener(pub Arc<Relay>);

impl Opener for RelayOpener {
    fn open(&self, open: Open, body: Vec<u8>) -> UnboundedReceiver<Frame> {
        let relay = &self.0;
        let (answer, answers) = unbounded_channel();
        let call = relay.next.fetch_add(1, Ordering::Relaxed) + 1;
        relay.answers.lock().unwrap().insert(call, answer.clone());
        let sent = relay.send(Frame::Open { call, open })
            && body_frames(call, &body).into_iter().all(|f| relay.send(f));
        if !sent {
            relay.answers.lock().unwrap().remove(&call);
            let _ = answer.send(Frame::error(
                call,
                UNAVAILABLE,
                "this runner is not connected to its hub, which the backend is reached through",
            ));
        }
        answers
    }
}

/// The models `backend` lists, or none when it does not answer in a few
/// seconds.
async fn models_of(backend: &LlmBackend) -> Vec<String> {
    let BackendSettings::OpenAiCompatible { base_url, api_key } = &backend.settings else {
        return Vec::new();
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return Vec::new();
    };
    let mut req = client.get(format!("{base_url}/models"));
    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let Ok(resp) = req.send().await else {
        return Vec::new();
    };
    let text = resp.text().await.unwrap_or_default();
    let listed: Value = serde_json::from_str(&text).unwrap_or_default();
    listed["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect()
}
