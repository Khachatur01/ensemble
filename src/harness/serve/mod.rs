//! The harness interface a harness does not have to be told about: an
//! OpenAI-compatible chat API.
//!
//! The user picks `ensemble/manager` as their model and that is the whole
//! integration. No MCP server to register, no second agent in front of this
//! one, no plugin. Ensemble owns the manager, which model runs it, and how the
//! work is divided behind it.
//!
//! WHY THIS IS ORCHESTRATION AND NOT PROMPT SMUGGLING
//! --------------------------------------------------
//! `docs/HARNESS.md` once argued that a chat endpoint could not carry a team,
//! because a `/chat/completions` request has no field for which role should do
//! the work — so driving one would mean encoding roles into prompt text and
//! parsing them back out, which breaks the first time a model paraphrases
//! itself. That objection was right, and it is answered rather than ignored:
//! delegation is a real tool, with named typed arguments, executed inside
//! Ensemble. Nothing is parsed out of prose, and the harness never sees it.
//!
//! WHAT THE HARNESS CANNOT REACH
//! -----------------------------
//! * **Roles.** One model id is served. `ensemble/coder` is a 404 like any
//!   other unknown model; the manager decides who works.
//! * **The project directory.** It comes from the server's `--dir`, fixed at
//!   startup. Not from a request field, and not from anything a model wrote —
//!   a path a model can choose is a path that can point anywhere.
//! * **The internal tools.** They never appear in a response, so there is
//!   nothing for the harness to execute or to refuse.

pub mod manager;
pub mod state;
pub mod wire;

use crate::harness::http;
use crate::router::Ensemble;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use state::ConversationStore;
use wire::{ApiError, ChatRequest, Completion, MODEL_ID};

/// How often an idle stream gets a comment, so the client does not time out
/// waiting on a delegation that is still running.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// SSE content is sent in pieces this size rather than one large delta.
const PIECE_BYTES: usize = 512;

pub const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone)]
pub struct Options {
    pub host: String,
    pub port: u16,
    /// The bearer token clients must present, already read from the
    /// environment. Never the name of the variable, and never logged.
    pub api_key: Option<String>,
    pub max_steps: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            api_key: None,
            max_steps: manager::DEFAULT_MAX_STEPS,
        }
    }
}

/// Is this address reachable only from this machine?
pub fn is_loopback(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A name, not an address. `localhost` is the one that is loopback by
        // definition; anything else has to be resolved, and a name that
        // resolves to a public address is exactly the case this guards.
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

pub fn resolve_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}:{port} resolved to no address"))
}

/// Refuse to bind a manager that cannot do the job.
///
/// Over MCP a `claude-code-cli` manager is fine: it is asked for work and
/// answers with it, using Claude Code's own tools. Here it would have to drive
/// a tool schema this process supplied, and it cannot. Better a refusal at
/// startup than a 502 on the user's first message.
pub fn preflight(ensemble: &Ensemble) -> Result<(), String> {
    let role = crate::config::Config::MANAGER;
    let caps = ensemble
        .capabilities_of(role)
        .map_err(|e| format!("serve: {e}"))?;
    if caps.tool_calling {
        return Ok(());
    }
    let (backend, _) = ensemble.route_of(role).map_err(|e| format!("serve: {e}"))?;
    let type_id = ensemble
        .config()
        .backends
        .get(backend)
        .map(|b| b.type_id())
        .unwrap_or("unknown");
    Err(format!(
        "serve: the manager runs on backend '{backend}' ({type_id}), which cannot call a \
         caller-supplied tool schema. Over this API the harness's tools have to reach the \
         manager and their results have to come back, and this backend can do neither. \
         Point `manager.backend` at an openai-compatible-api backend, or use \
         `ensemble mcp`, where a {type_id} manager is perfectly good."
    ))
}

/// Why `serve` is or is not available, for `ensemble check`.
pub fn availability(ensemble: &Ensemble) -> String {
    match preflight(ensemble) {
        Ok(()) => format!("available ({MODEL_ID})"),
        Err(_) => match ensemble.route_of(crate::config::Config::MANAGER) {
            Ok((backend, _)) => format!(
                "unavailable — the manager's backend '{backend}' cannot call tools; \
                 `ensemble mcp` still works"
            ),
            Err(e) => format!("unavailable — {e}"),
        },
    }
}

/// Compare without leaking the matching prefix through timing.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

pub struct ApiHandler {
    ensemble: Arc<Ensemble>,
    store: Arc<ConversationStore>,
    api_key: Option<String>,
    max_steps: u32,
}

impl ApiHandler {
    pub fn new(ensemble: Arc<Ensemble>, options: &Options) -> Self {
        ApiHandler {
            ensemble,
            store: Arc::new(ConversationStore::new()),
            api_key: options.api_key.clone(),
            max_steps: options.max_steps,
        }
    }

    fn error(e: &ApiError) -> http::Response {
        http::Response::json(e.status, e.body())
    }

    fn authorized(&self, req: &http::Request) -> Result<(), ApiError> {
        let Some(expected) = &self.api_key else {
            return Ok(());
        };
        match req.bearer() {
            Some(got) if secret_eq(got, expected) => Ok(()),
            // The message says what is wrong and nothing about what was sent.
            _ => Err(ApiError {
                status: 401,
                message: "a bearer token is required on this server".into(),
                kind: "invalid_request_error",
                param: None,
                code: "invalid_api_key",
            }),
        }
    }

    fn chat(&self, req: &http::Request) -> http::Response {
        // Validate completely before any 200 goes out: once the head is
        // written the status cannot be taken back, and a validation failure
        // deserves a real 400.
        let parsed = wire::parse_body(&req.body).and_then(|v| wire::parse_request(&v));
        let request = match parsed {
            Ok(r) => r,
            Err(e) => return Self::error(&e),
        };

        if request.stream {
            self.chat_streaming(request)
        } else {
            self.chat_blocking(&request)
        }
    }

    fn chat_blocking(&self, request: &ChatRequest) -> http::Response {
        let outcome = match manager::run(
            &self.ensemble,
            &self.store,
            request,
            self.max_steps,
            &|_| {},
        ) {
            Ok(o) => o,
            Err(e) => return Self::error(&e),
        };
        let completion = Completion {
            id: wire::completion_id(),
            created: wire::now_secs(),
            model: request.model.clone(),
            text: outcome.text,
            tool_calls: outcome.tool_calls,
            finish_reason: outcome.finish_reason,
            usage: outcome.usage,
        };
        http::Response::json(200, completion.body())
    }

    /// Serve the turn as SSE.
    ///
    /// The orchestration runs on its own thread and the connection thread owns
    /// the only writer — two threads on one socket would interleave bytes. The
    /// text is not streamed token by token; `ModelPort::complete` returns a
    /// finished answer, and forwarding real deltas would stream the manager's
    /// internal delegation calls to a harness that must not see them. What the
    /// client gets instead is a protocol-correct stream whose content arrives
    /// in one burst, with comments in the meantime so the connection does not
    /// idle out. Documented as a limitation; the sink below does not change
    /// when real streaming lands.
    fn chat_streaming(&self, request: ChatRequest) -> http::Response {
        let ensemble = Arc::clone(&self.ensemble);
        let store = Arc::clone(&self.store);
        let max_steps = self.max_steps;
        let model = request.model.clone();
        let include_usage = request.include_usage;

        http::Response::sse(move |sink| {
            // First bytes out immediately, before any model is asked
            // anything, so the client knows the stream is live.
            sink.comment("ensemble: working")?;

            let (tx, rx) = channel();
            let (ptx, prx) = channel::<String>();
            std::thread::spawn(move || {
                let result = manager::run(&ensemble, &store, &request, max_steps, &|note| {
                    let _ = ptx.send(note.to_string());
                });
                let _ = tx.send(result);
            });

            let outcome = loop {
                match rx.recv_timeout(HEARTBEAT) {
                    Ok(result) => break result,
                    Err(RecvTimeoutError::Timeout) => {
                        while let Ok(note) = prx.try_recv() {
                            sink.comment(&format!("ensemble: {note}"))?;
                        }
                        sink.comment("keep-alive")?;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        break Err(ApiError::upstream(
                            500,
                            "internal_error",
                            "the manager turn ended without producing a result",
                        ));
                    }
                }
            };

            match outcome {
                Ok(outcome) => {
                    let c = Completion {
                        id: wire::completion_id(),
                        created: wire::now_secs(),
                        model,
                        text: outcome.text,
                        tool_calls: outcome.tool_calls,
                        finish_reason: outcome.finish_reason,
                        usage: outcome.usage,
                    };
                    sink.event(&c.chunk_role())?;
                    for piece in wire::content_pieces(&c.text, PIECE_BYTES) {
                        sink.event(&c.chunk_content(&piece))?;
                    }
                    for (i, call) in c.tool_calls.iter().enumerate() {
                        sink.event(&c.chunk_tool_call(i, call))?;
                    }
                    sink.event(&c.chunk_finish())?;
                    if include_usage {
                        sink.event(&c.chunk_usage())?;
                    }
                }
                Err(e) => {
                    // The 200 head is already out, so the status cannot say
                    // this. No finish_reason first: that would claim the turn
                    // succeeded.
                    eprintln!("ensemble: serve: {} ({})", e.message, e.status);
                    sink.event(&wire::chunk_error(&e.message, e.code))?;
                }
            }
            sink.event("[DONE]")
        })
    }
}

impl http::Handler for ApiHandler {
    fn handle(&self, req: &http::Request) -> http::Response {
        if let Err(e) = self.authorized(req) {
            return Self::error(&e);
        }

        // Both the bare and the `/v1`-prefixed spellings, because clients
        // differ on whether the base URL already ends in `/v1`.
        let path = req.path.strip_suffix('/').unwrap_or(&req.path);
        let route = path.strip_prefix("/v1").unwrap_or(path);

        match (req.method.as_str(), route) {
            ("GET", "/models") => http::Response::json(200, wire::models_list()),
            ("POST", "/chat/completions") => self.chat(req),

            ("GET", "/chat/completions") | ("HEAD", "/chat/completions") => {
                Self::error(&ApiError::invalid(
                    405,
                    "method_not_allowed",
                    "use POST for /v1/chat/completions",
                ))
                .with_header("allow", "POST")
            }
            (_, "/models") => Self::error(&ApiError::invalid(
                405,
                "method_not_allowed",
                "use GET for /v1/models",
            ))
            .with_header("allow", "GET"),

            _ => Self::error(&ApiError::invalid(
                404,
                "unknown_url",
                format!(
                    "no route {} {}. This server serves GET /v1/models and \
                     POST /v1/chat/completions.",
                    req.method, req.path
                ),
            )),
        }
    }

    fn bad_request(&self, bad: &http::BadRequest) -> http::Response {
        let code = match bad {
            http::BadRequest::BodyTooLarge => "payload_too_large",
            http::BadRequest::HeadersTooLarge => "headers_too_large",
            http::BadRequest::UnsupportedTransferEncoding => "unsupported_transfer_encoding",
            http::BadRequest::UnsupportedVersion => "unsupported_http_version",
            http::BadRequest::Malformed(_) => "invalid_request",
        };
        Self::error(&ApiError::invalid(bad.status(), code, bad.message()))
    }

    fn at_capacity(&self) -> http::Response {
        Self::error(&ApiError::upstream(
            503,
            "at_capacity",
            "every worker is busy with another turn; try again shortly",
        ))
    }
}

/// Bind and serve until the process is stopped.
pub fn serve(ensemble: Arc<Ensemble>, options: &Options) -> Result<(), String> {
    preflight(&ensemble)?;

    if !is_loopback(&options.host) && options.api_key.is_none() {
        return Err(format!(
            "serve: refusing to listen on {} without authentication. Anything that can reach \
             that address could spend your model budget and run work in the project \
             directory. Pass --api-key-env <VAR> naming an environment variable that holds \
             the bearer token, or bind 127.0.0.1.",
            options.host
        ));
    }

    let addr = resolve_addr(&options.host, options.port)?;
    let mut cfg = http::ServerConfig::new(addr);
    // A manager turn can legitimately take minutes; only request *parsing* is
    // on a deadline.
    cfg.read_timeout = Duration::from_secs(60);
    cfg.write_timeout = Duration::from_secs(60);

    let server = http::Server::bind(cfg).map_err(|e| format!("serve: cannot bind {addr}: {e}"))?;
    let bound = server.local_addr();
    banner(&ensemble, bound, options);

    let handler = Arc::new(ApiHandler::new(ensemble, options));
    server.serve(handler).map_err(|e| format!("serve: {e}"))
}

/// Everything the operator needs to paste, on stderr so stdout stays clean.
fn banner(ensemble: &Ensemble, addr: SocketAddr, options: &Options) {
    let base = format!("http://{addr}/v1");
    eprintln!("ensemble: serving {MODEL_ID} on {base}");
    if let Ok((backend, model)) = ensemble.route_of(crate::config::Config::MANAGER) {
        eprintln!("ensemble: manager -> {backend} / {model}");
    }
    let roles = ensemble.delegable_roles();
    eprintln!(
        "ensemble: delegates to: {}",
        if roles.is_empty() {
            "(none configured)".to_string()
        } else {
            roles.join(", ")
        }
    );
    eprintln!(
        "ensemble: authentication: {}",
        if options.api_key.is_some() {
            "bearer token required"
        } else {
            "none (loopback only)"
        }
    );
    eprintln!(
        "ensemble: note: replies are streamed as one block once the turn completes; \
         progress appears as SSE comments."
    );
    eprintln!("ensemble: opencode.json:");
    eprintln!(
        r#"  {{"provider": {{"ensemble": {{"npm": "@ai-sdk/openai-compatible",
    "options": {{"baseURL": "{base}"}},
    "models": {{"manager": {{"name": "Ensemble manager"}}}}}}}}}}"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::harness::http::Handler;
    use crate::port::testing::{FakePort, Scripted, call};
    use crate::port::{ModelPort, Role};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    /// Two backends on purpose: the manager and the roles must be observable
    /// apart, or a test cannot tell "the manager answered" from "a delegate
    /// did".
    const YAML: &str = r#"
version: 1
backends:
  mgr:
    type: openai-compatible-api
    base_url_env: M
    base_url_default: "http://127.0.0.1:8000/v1"
  pool:
    type: openai-compatible-api
    base_url_env: P
    base_url_default: "http://127.0.0.1:8001/v1"
manager:
  backend: mgr
  model: manager-model
  prompt: "You are the manager.\nMore detail here."
roles:
  planner:
    backend: pool
    model: pool-model
    prompt: "Turns a goal into an ordered plan."
  coder:
    backend: pool
    model: pool-model
"#;

    fn cfg() -> config::Config {
        config::parse(
            YAML,
            std::path::Path::new("ensemble.yaml"),
            &BTreeMap::new(),
        )
        .unwrap()
    }

    struct Fixture {
        handler: ApiHandler,
        /// What the manager said, turn by turn.
        manager: Arc<FakePort>,
        /// What any delegated role said.
        pool: Arc<FakePort>,
    }

    fn build(manager: Arc<FakePort>, pool: Arc<FakePort>, options: Options) -> Fixture {
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        ports.insert("mgr".into(), manager.clone() as Arc<dyn ModelPort>);
        ports.insert("pool".into(), pool.clone() as Arc<dyn ModelPort>);
        let ensemble = Arc::new(Ensemble::with_ports(cfg(), ports, Some("/proj".into())));
        Fixture {
            handler: ApiHandler::new(ensemble, &options),
            manager,
            pool,
        }
    }

    /// A manager that answers `script` in order, and roles that always answer
    /// `role_reply`.
    fn harness(script: Vec<Scripted>, role_reply: &str) -> Fixture {
        build(
            Arc::new(FakePort::scripted("mgr", "fell off the script", script)),
            Arc::new(FakePort::ok("pool", role_reply)),
            Options::default(),
        )
    }

    fn get(path: &str) -> http::Request {
        http::Request {
            method: "GET".into(),
            path: path.into(),
            query: String::new(),
            version_minor: 1,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn post(path: &str, body: Value) -> http::Request {
        http::Request {
            method: "POST".into(),
            path: path.into(),
            query: String::new(),
            version_minor: 1,
            headers: Vec::new(),
            body: body.to_string().into_bytes(),
        }
    }

    fn chat(messages: Value) -> Value {
        json!({"model": MODEL_ID, "messages": messages})
    }

    fn read_tool() -> Value {
        json!([{"type": "function",
                "function": {"name": "read", "parameters": {"type": "object"}}}])
    }

    fn body_of(resp: http::Response) -> (u16, Value) {
        let status = resp.status;
        match resp.body {
            http::Body::Bytes(b) => (status, serde_json::from_slice(&b).unwrap()),
            http::Body::Stream(_) => panic!("expected a buffered body"),
        }
    }

    fn raw_sse(resp: http::Response) -> String {
        let mut out = Vec::new();
        http::write_response(&mut out, resp, false, 1).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The `data:` payloads, in order. Parsing them beats substring-matching
    /// the JSON: serde orders object keys, and a test that depends on that
    /// ordering breaks for no reason.
    fn parse_sse(raw: &str) -> (Vec<Value>, bool) {
        let mut events = Vec::new();
        let mut done = false;
        for line in raw.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload == "[DONE]" {
                done = true;
            } else {
                events.push(serde_json::from_str(payload).expect("each event is JSON"));
            }
        }
        (events, done)
    }

    fn sse_events(resp: http::Response) -> (Vec<Value>, bool) {
        parse_sse(&raw_sse(resp))
    }

    fn tool_turns(req: &crate::port::Request) -> Vec<&str> {
        req.messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect()
    }

    // --- routing ---------------------------------------------------------

    #[test]
    fn models_lists_the_manager_and_nothing_that_names_another_role() {
        let f = harness(vec![], "x");
        let (status, v) = body_of(f.handler.handle(&get("/v1/models")));
        assert_eq!(status, 200);
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
        assert_eq!(v["data"][0]["id"], MODEL_ID);
        let rendered = v.to_string();
        assert!(!rendered.contains("planner"), "{rendered}");
        assert!(!rendered.contains("pool-model"), "{rendered}");
    }

    #[test]
    fn the_v1_prefix_is_optional_because_clients_disagree_about_the_base_url() {
        let f = harness(vec![], "x");
        assert_eq!(f.handler.handle(&get("/models")).status, 200);
        assert_eq!(f.handler.handle(&get("/v1/models")).status, 200);
        assert_eq!(f.handler.handle(&get("/v1/models/")).status, 200);
    }

    #[test]
    fn an_unknown_path_is_a_404_that_names_the_real_routes() {
        let f = harness(vec![], "x");
        let (status, v) = body_of(f.handler.handle(&get("/v1/completions")));
        assert_eq!(status, 404);
        assert_eq!(v["error"]["code"], "unknown_url");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("/v1/models")
        );
    }

    #[test]
    fn a_get_on_chat_completions_is_405_with_an_allow_header() {
        let f = harness(vec![], "x");
        let resp = f.handler.handle(&get("/v1/chat/completions"));
        assert_eq!(resp.status, 405);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "allow" && v == "POST")
        );
    }

    // --- authentication --------------------------------------------------

    #[test]
    fn a_configured_token_is_required_and_a_wrong_one_is_refused() {
        let f = build(
            Arc::new(FakePort::ok("mgr", "hi")),
            Arc::new(FakePort::ok("pool", "hi")),
            Options {
                api_key: Some("sk-right".into()),
                ..Options::default()
            },
        );

        assert_eq!(f.handler.handle(&get("/v1/models")).status, 401);

        let mut wrong = get("/v1/models");
        wrong
            .headers
            .push(("authorization".into(), "Bearer sk-wrong".into()));
        let (status, v) = body_of(f.handler.handle(&wrong));
        assert_eq!(status, 401);
        assert_eq!(v["error"]["code"], "invalid_api_key");
        // The refusal says nothing about what was sent.
        assert!(!v.to_string().contains("sk-wrong"));

        let mut right = get("/v1/models");
        right
            .headers
            .push(("authorization".into(), "Bearer sk-right".into()));
        assert_eq!(f.handler.handle(&right).status, 200);
    }

    #[test]
    fn tokens_are_compared_without_leaking_their_matching_prefix() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
        assert!(!secret_eq("", "a"));
    }

    #[test]
    fn loopback_is_recognised_by_address_and_by_name() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("localhost"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("10.0.0.5"));
        assert!(!is_loopback("example.com"));
    }

    // --- a plain turn ----------------------------------------------------

    #[test]
    fn a_plain_answer_comes_back_as_one_assistant_message_with_finish_reason_stop() {
        let f = harness(vec![Scripted::text("Here is the plan.")], "x");
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role": "user", "content": "plan it"}])),
        )));
        assert_eq!(status, 200);
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["model"], MODEL_ID);
        assert_eq!(v["choices"][0]["message"]["content"], "Here is the plan.");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 3);
        assert_eq!(f.manager.call_count(), 1);
        assert_eq!(f.pool.call_count(), 0);
    }

    #[test]
    fn the_manager_prompt_leads_and_the_harness_prompt_follows_the_guidance() {
        let f = harness(vec![Scripted::text("ok")], "x");
        f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([
                {"role": "system", "content": "HARNESS RULES"},
                {"role": "user", "content": "go"}
            ])),
        ));
        let sent = f.manager.last().unwrap();
        let systems: Vec<&str> = sent
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(systems.len(), 3);
        assert!(systems[0].starts_with("You are the manager."));
        assert!(systems[1].contains("ensemble_delegate"), "{}", systems[1]);
        assert_eq!(systems[2], "HARNESS RULES");
    }

    #[test]
    fn the_guidance_names_every_configured_role_and_no_model_or_backend() {
        let f = harness(vec![Scripted::text("ok")], "x");
        f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        ));
        let sent = f.manager.last().unwrap();
        let guidance = &sent.messages[1].content;
        assert!(guidance.contains("planner"), "{guidance}");
        assert!(guidance.contains("coder"), "{guidance}");
        assert!(
            guidance.contains("Turns a goal into an ordered plan."),
            "{guidance}"
        );
        assert!(!guidance.contains("pool-model"), "{guidance}");
        assert!(!guidance.contains("manager-model"), "{guidance}");
    }

    #[test]
    fn the_manager_is_offered_the_delegation_tool_and_the_harness_tools() {
        let f = harness(vec![Scripted::text("ok")], "x");
        f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"go"}],
                   "tools": read_tool()}),
        ));
        let names: Vec<String> = f
            .manager
            .last()
            .unwrap()
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert!(
            names.contains(&"ensemble_delegate".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"read".to_string()), "{names:?}");
    }

    #[test]
    fn tool_choice_none_offers_the_manager_no_tools_at_all() {
        let f = harness(vec![Scripted::text("ok")], "x");
        f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"go"}],
                   "tool_choice": "none", "tools": read_tool()}),
        ));
        assert!(f.manager.last().unwrap().tools.is_empty());
    }

    // --- internal delegation ---------------------------------------------

    #[test]
    fn an_internal_delegation_runs_inside_the_request_and_never_appears_in_the_response() {
        let f = harness(
            vec![
                Scripted::call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"outline"}"#,
                ),
                Scripted::text("The planner suggests two steps."),
            ],
            "x",
        );
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"plan it"}])),
        )));

        assert_eq!(status, 200);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert!(v["choices"][0]["message"].get("tool_calls").is_none());
        assert!(!v.to_string().contains("ensemble_delegate"), "{v}");
        assert_eq!(f.manager.call_count(), 2);
        assert_eq!(f.pool.call_count(), 1, "the delegation really ran");
    }

    #[test]
    fn the_delegated_result_is_fed_back_to_the_manager_in_the_same_request() {
        let f = harness(
            vec![
                Scripted::call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"outline"}"#,
                ),
                Scripted::text("done"),
            ],
            "THE PLANNER ANSWER",
        );
        f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"plan it"}])),
        ));
        let second = f.manager.nth(1).unwrap();
        let tool_turn = second
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("the delegation result is in the history");
        assert_eq!(tool_turn.tool_call_id.as_deref(), Some("d1"));
        assert_eq!(tool_turn.content, "THE PLANNER ANSWER");
        // And the delegate was asked the task the manager wrote.
        assert_eq!(
            f.pool.last().unwrap().messages.last().unwrap().content,
            "outline"
        );
    }

    #[test]
    fn usage_sums_every_internal_call_not_only_the_last() {
        // Two manager turns and one delegation, each 1 in / 2 out.
        let f = harness(
            vec![
                Scripted::call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"x"}"#,
                ),
                Scripted::text("done"),
            ],
            "answer",
        );
        let (_, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        )));
        assert_eq!(v["usage"]["prompt_tokens"], 3);
        assert_eq!(v["usage"]["completion_tokens"], 6);
        assert_eq!(v["usage"]["total_tokens"], 9);
    }

    #[test]
    fn an_unknown_role_is_fed_back_as_a_tool_result_and_the_loop_continues() {
        let f = harness(
            vec![
                Scripted::call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"architect","task":"x"}"#,
                ),
                Scripted::text("I will use the planner instead."),
            ],
            "x",
        );
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        )));
        assert_eq!(status, 200, "a bad role is not an HTTP failure: {v}");
        let retry = f.manager.nth(1).unwrap();
        let turns = tool_turns(&retry);
        assert!(turns[0].contains("architect"), "{turns:?}");
        assert!(
            turns[0].contains("planner"),
            "lists the real ones: {turns:?}"
        );
        assert_eq!(f.pool.call_count(), 0, "nothing was dispatched");
    }

    #[test]
    fn malformed_delegation_arguments_are_reported_to_the_manager_not_corrected() {
        let f = harness(
            vec![
                Scripted::call("d1", "ensemble_delegate", "not json at all"),
                Scripted::text("sorry"),
            ],
            "x",
        );
        f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        ));
        let second = f.manager.nth(1).unwrap();
        let turns = tool_turns(&second);
        assert!(turns[0].contains("not valid JSON"), "{turns:?}");
        assert_eq!(f.pool.call_count(), 0);
    }

    #[test]
    fn the_step_budget_stops_the_loop_and_still_returns_an_answer() {
        let endless: Vec<Scripted> = (0..20)
            .map(|i| {
                Scripted::call(
                    &format!("d{i}"),
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"again"}"#,
                )
            })
            .collect();
        let f = harness(endless, "a role answer");
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        )));
        assert_eq!(status, 200);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        // The budget holds: one call per step, plus the forced final one.
        assert_eq!(
            f.manager.call_count() as u32,
            manager::DEFAULT_MAX_STEPS + 1
        );
        // And the last call is offered no tools, so it cannot delegate again.
        assert!(f.manager.last().unwrap().tools.is_empty());
    }

    #[test]
    fn the_delegation_budget_caps_a_manager_that_fans_out_without_end() {
        let many: Vec<crate::port::ToolCall> = (0..30)
            .map(|i| {
                call(
                    &format!("d{i}"),
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"x"}"#,
                )
            })
            .collect();
        let f = harness(vec![Scripted::calls(many), Scripted::text("enough")], "x");
        f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        ));
        assert_eq!(f.pool.call_count() as u32, manager::MAX_DELEGATIONS);
        let second = f.manager.nth(1).unwrap();
        let turns = tool_turns(&second);
        assert!(
            turns.iter().any(|t| t.contains("budget")),
            "the manager is told why the rest did not run"
        );
    }

    // --- harness tool round trip -----------------------------------------

    #[test]
    fn a_harness_tool_call_ends_the_turn_with_finish_reason_tool_calls() {
        let f = harness(
            vec![Scripted::call(
                "call_r",
                "read",
                r#"{"filePath":"src/main.rs"}"#,
            )],
            "x",
        );
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"read it"}],
                   "tools": read_tool()}),
        )));
        assert_eq!(status, 200);
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        let calls = &v["choices"][0]["message"]["tool_calls"];
        assert_eq!(calls[0]["id"], "call_r");
        assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(
            calls[0]["function"]["arguments"],
            r#"{"filePath":"src/main.rs"}"#
        );
        assert!(v["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn a_reply_mixing_a_delegation_and_a_harness_tool_forwards_only_the_harness_one() {
        let f = harness(
            vec![Scripted::calls(vec![
                call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"x"}"#,
                ),
                call("call_r", "read", r#"{"filePath":"a"}"#),
            ])],
            "THE PLAN",
        );
        let (_, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"go"}],
                   "tools": read_tool()}),
        )));

        let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1, "only the harness's: {v}");
        assert_eq!(calls[0]["id"], "call_r");
        assert!(!v.to_string().contains("ensemble_delegate"));
        assert_eq!(f.pool.call_count(), 1, "the internal one still ran");
    }

    #[test]
    fn the_second_request_sees_the_delegation_it_was_never_shown() {
        let f = harness(
            vec![
                Scripted::calls(vec![
                    call(
                        "d1",
                        "ensemble_delegate",
                        r#"{"role":"planner","task":"x"}"#,
                    ),
                    call("call_r", "read", r#"{"filePath":"a"}"#),
                ]),
                Scripted::text("Now I know both."),
            ],
            "THE PLAN",
        );
        body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"go"}],
                   "tools": read_tool()}),
        )));

        // What a harness sends next: what it had, plus what it was given, plus
        // the result it produced. It never saw the delegation.
        let second = json!({"model": MODEL_ID, "messages": [
            {"role":"user","content":"go"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_r","type":"function",
                 "function":{"name":"read","arguments":"{\"filePath\":\"a\"}"}}]},
            {"role":"tool","tool_call_id":"call_r","content":"file contents"}],
            "tools": read_tool()});
        let (status, v) = body_of(f.handler.handle(&post("/v1/chat/completions", second)));
        assert_eq!(status, 200);
        assert_eq!(v["choices"][0]["message"]["content"], "Now I know both.");

        let last = f.manager.last().unwrap();
        let turns = tool_turns(&last);
        assert!(
            turns.contains(&"THE PLAN"),
            "the hidden turn is back: {turns:?}"
        );
        assert!(turns.contains(&"file contents"), "{turns:?}");
        assert_eq!(f.pool.call_count(), 1, "the delegation is not run again");
    }

    #[test]
    fn a_lost_entry_degrades_to_a_valid_transcript_rather_than_a_dangling_call() {
        // The same follow-up, against a server that never stored anything.
        let f = harness(vec![Scripted::text("carrying on")], "x");
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [
                {"role":"user","content":"go"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_r","type":"function",
                     "function":{"name":"read","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_r","content":"contents"}],
                "tools": read_tool()}),
        )));
        assert_eq!(status, 200);
        assert_eq!(v["choices"][0]["message"]["content"], "carrying on");
        let sent = f.manager.last().unwrap();
        // Every tool turn still answers a call that is in the transcript.
        let ids: Vec<&str> = sent
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .map(|c| c.id.as_str())
            .collect();
        for m in sent.messages.iter().filter(|m| m.role == Role::Tool) {
            assert!(ids.contains(&m.tool_call_id.as_deref().unwrap()), "{ids:?}");
        }
    }

    #[test]
    fn two_interleaved_conversations_do_not_leak_turns_into_each_other() {
        let f = harness(
            vec![
                Scripted::calls(vec![
                    call(
                        "dA",
                        "ensemble_delegate",
                        r#"{"role":"planner","task":"A"}"#,
                    ),
                    call("call_a", "read", r#"{"filePath":"a"}"#),
                ]),
                Scripted::calls(vec![
                    call(
                        "dB",
                        "ensemble_delegate",
                        r#"{"role":"planner","task":"B"}"#,
                    ),
                    call("call_b", "read", r#"{"filePath":"b"}"#),
                ]),
                Scripted::text("A continues"),
            ],
            "ROLE ANSWER",
        );

        for seed in ["task A", "task B"] {
            body_of(f.handler.handle(&post(
                "/v1/chat/completions",
                json!({"model": MODEL_ID, "messages": [{"role":"user","content":seed}],
                       "tools": read_tool()}),
            )));
        }

        let follow_a = json!({"model": MODEL_ID, "messages": [
            {"role":"user","content":"task A"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_a","type":"function",
                 "function":{"name":"read","arguments":"{\"filePath\":\"a\"}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"a contents"}],
            "tools": read_tool()});
        let (status, v) = body_of(f.handler.handle(&post("/v1/chat/completions", follow_a)));
        assert_eq!(status, 200);
        assert_eq!(v["choices"][0]["message"]["content"], "A continues");

        // A's restored history mentions A's work and never B's calls.
        let sent = f.manager.last().unwrap();
        let ids: Vec<&str> = sent
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .map(|c| c.id.as_str())
            .collect();
        assert!(ids.contains(&"dA"), "{ids:?}");
        assert!(ids.contains(&"call_a"), "{ids:?}");
        assert!(
            !ids.contains(&"dB"),
            "no trace of the other conversation: {ids:?}"
        );
        assert!(!ids.contains(&"call_b"), "{ids:?}");
    }

    // --- streaming -------------------------------------------------------

    #[test]
    fn a_streaming_request_emits_valid_sse_and_a_final_done() {
        let f = harness(vec![Scripted::text("Hello there.")], "x");
        let resp = f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "stream": true,
                   "messages": [{"role":"user","content":"hi"}]}),
        ));
        assert!(resp.is_stream());

        // One turn, read two ways: the framing, then the events inside it.
        let raw = raw_sse(resp);
        assert!(raw.contains("content-type: text/event-stream"), "{raw}");
        assert!(
            raw.ends_with("0\r\n\r\n"),
            "the chunked stream is closed: {raw}"
        );

        let (events, done) = parse_sse(&raw);
        assert!(done, "the stream is terminated");
        assert_eq!(events[0]["object"], "chat.completion.chunk");
        assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
        let text: String = events
            .iter()
            .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "Hello there.");
        assert_eq!(
            events.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
        // Not asked for, so not sent: a strict client rejects a null usage.
        assert!(events.iter().all(|e| e.get("usage").is_none()));
    }

    #[test]
    fn a_streaming_request_can_ask_for_usage_in_a_final_chunk() {
        let f = harness(vec![Scripted::text("hi")], "x");
        let (events, _) = sse_events(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "stream": true,
                   "stream_options": {"include_usage": true},
                   "messages": [{"role":"user","content":"hi"}]}),
        )));
        let usage = events.last().unwrap();
        assert_eq!(usage["usage"]["total_tokens"], 3);
        assert_eq!(usage["choices"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_streamed_harness_tool_call_arrives_as_a_tool_call_delta() {
        let f = harness(
            vec![Scripted::call("call_r", "read", r#"{"filePath":"a"}"#)],
            "x",
        );
        let (events, done) = sse_events(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "stream": true,
                   "messages": [{"role":"user","content":"go"}],
                   "tools": read_tool()}),
        )));
        assert!(done);
        let delta = events
            .iter()
            .find_map(|e| e["choices"][0]["delta"].get("tool_calls").cloned())
            .expect("a tool_calls delta");
        assert_eq!(delta[0]["index"], 0);
        assert_eq!(delta[0]["id"], "call_r");
        assert_eq!(delta[0]["function"]["name"], "read");
        assert!(delta[0]["function"]["arguments"].is_string());
        assert_eq!(
            events.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[test]
    fn a_stream_that_fails_mid_turn_says_so_instead_of_claiming_it_finished() {
        let f = build(
            Arc::new(FakePort::failing("mgr", "Connection refused")),
            Arc::new(FakePort::ok("pool", "x")),
            Options::default(),
        );
        let (events, done) = sse_events(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "stream": true,
                   "messages": [{"role":"user","content":"hi"}]}),
        )));
        assert!(done);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["error"]["code"], "upstream_error");
        assert!(
            events[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Connection refused")
        );
        assert!(
            events.iter().all(|e| e.get("choices").is_none()),
            "never claims the turn finished"
        );
    }

    #[test]
    fn a_stream_begins_with_a_comment_so_the_client_sees_the_connection_is_live() {
        let f = harness(vec![Scripted::text("hi")], "x");
        let raw = raw_sse(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "stream": true,
                   "messages": [{"role":"user","content":"hi"}]}),
        )));
        let body = raw.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains(": ensemble: working"), "{body}");
    }

    #[test]
    fn a_streamed_delegation_is_as_invisible_as_a_blocking_one() {
        let f = harness(
            vec![
                Scripted::call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"x"}"#,
                ),
                Scripted::text("done"),
            ],
            "SECRET PLAN",
        );
        let (events, _) = sse_events(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "stream": true,
                   "messages": [{"role":"user","content":"go"}]}),
        )));
        let rendered = serde_json::to_string(&events).unwrap();
        assert!(!rendered.contains("ensemble_delegate"), "{rendered}");
        assert!(!rendered.contains("SECRET PLAN"), "{rendered}");
        assert_eq!(f.pool.call_count(), 1);
    }

    // --- invalid requests and upstream failure ---------------------------

    #[test]
    fn a_body_that_is_not_json_is_a_400_in_the_openai_shape() {
        let f = harness(vec![], "x");
        let mut req = post("/v1/chat/completions", json!({}));
        req.body = b"<html>".to_vec();
        let (status, v) = body_of(f.handler.handle(&req));
        assert_eq!(status, 400);
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn an_unknown_model_is_a_404_before_any_model_call_is_made() {
        let f = harness(vec![], "x");
        let (status, _) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": "gpt-4o", "messages": [{"role":"user","content":"hi"}]}),
        )));
        assert_eq!(status, 404);
        assert_eq!(f.manager.call_count(), 0);
    }

    #[test]
    fn a_harness_tool_named_ensemble_delegate_is_refused_before_any_model_call() {
        let f = harness(vec![], "x");
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"hi"}],
                   "tools": [{"type":"function","function":{"name":"ensemble_delegate",
                              "parameters":{"type":"object"}}}]}),
        )));
        assert_eq!(status, 400);
        assert_eq!(v["error"]["code"], "tool_name_conflict");
        assert_eq!(f.manager.call_count(), 0);
    }

    #[test]
    fn a_failing_backend_is_a_502_carrying_the_reason_and_the_backend_name() {
        let f = build(
            Arc::new(FakePort::failing("mgr", "Connection refused")),
            Arc::new(FakePort::ok("pool", "x")),
            Options::default(),
        );
        let (status, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"hi"}])),
        )));
        assert_eq!(status, 502);
        assert_eq!(v["error"]["code"], "upstream_error");
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.contains("mgr"), "{message}");
        assert!(message.contains("Connection refused"), "{message}");
    }

    #[test]
    fn a_read_failure_is_rendered_in_the_openai_shape_too() {
        let f = harness(vec![], "x");
        let (status, v) = body_of(f.handler.bad_request(&http::BadRequest::BodyTooLarge));
        assert_eq!(status, 413);
        assert_eq!(v["error"]["code"], "payload_too_large");
    }

    // --- the boundaries the harness must not cross -----------------------

    #[test]
    fn serve_refuses_a_manager_whose_backend_cannot_call_tools() {
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        ports.insert(
            "mgr".into(),
            Arc::new(FakePort::ok("mgr", "hi").without_tool_calling()) as Arc<dyn ModelPort>,
        );
        ports.insert(
            "pool".into(),
            Arc::new(FakePort::ok("pool", "hi")) as Arc<dyn ModelPort>,
        );
        let ensemble = Ensemble::with_ports(cfg(), ports, None);

        let err = preflight(&ensemble).unwrap_err();
        assert!(err.contains("mgr"), "{err}");
        assert!(err.contains("ensemble mcp"), "{err}");
        assert!(availability(&ensemble).starts_with("unavailable"));
    }

    #[test]
    fn serve_is_available_with_a_tool_calling_manager() {
        let f = harness(vec![], "x");
        assert!(preflight(&f.handler.ensemble).is_ok());
        assert!(availability(&f.handler.ensemble).starts_with("available"));
    }

    #[test]
    fn a_non_loopback_host_without_a_token_refuses_to_bind() {
        let f = harness(vec![], "x");
        let err = serve(
            Arc::clone(&f.handler.ensemble),
            &Options {
                host: "0.0.0.0".into(),
                port: 0,
                api_key: None,
                max_steps: 1,
            },
        )
        .unwrap_err();
        assert!(err.contains("--api-key-env"), "{err}");
        assert!(err.contains("0.0.0.0"), "{err}");
    }

    #[test]
    fn a_delegation_cannot_be_steered_onto_a_backend_or_a_model() {
        // Whatever extra keys the manager writes, only `role` is read, and it
        // is checked against the config.
        let f = harness(
            vec![
                Scripted::call(
                    "d1",
                    "ensemble_delegate",
                    r#"{"role":"planner","task":"x","model":"gpt-4o","backend":"evil"}"#,
                ),
                Scripted::text("done"),
            ],
            "ok",
        );
        f.handler.handle(&post(
            "/v1/chat/completions",
            chat(json!([{"role":"user","content":"go"}])),
        ));
        assert_eq!(f.pool.call_count(), 1);
        assert_eq!(
            f.pool.last().unwrap().model,
            "pool-model",
            "the config decides, not the model"
        );
    }

    #[test]
    fn the_project_directory_comes_from_the_server_not_the_request() {
        let f = harness(vec![Scripted::text("ok")], "x");
        f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "workdir": "/etc",
                   "messages": [{"role":"user","content":"go"}]}),
        ));
        assert_eq!(f.manager.last().unwrap().workdir.as_deref(), Some("/proj"));
    }

    #[test]
    fn the_tool_call_id_the_manager_wrote_is_the_one_the_harness_gets() {
        let weird = "call_ABC-123_xyz";
        let f = harness(vec![Scripted::call(weird, "read", r#"{"a":"b c"}"#)], "x");
        let (_, v) = body_of(f.handler.handle(&post(
            "/v1/chat/completions",
            json!({"model": MODEL_ID, "messages": [{"role":"user","content":"go"}],
                   "tools": read_tool()}),
        )));
        assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["id"], weird);
    }
}
