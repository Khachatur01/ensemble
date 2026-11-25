//! The hub server's REST API, under `/api/v1/`, for people and the UI to come:
//! each route runs one of the hub's operations ([`accounts`](crate::accounts)),
//! signed in with the request's bearer token. JSON in and out; an error is
//! `{error, code, field?, hint?}`.
//!
//! | Method and path | Operation |
//! |---|---|
//! | `POST /api/v1/register` `{name, password}` | `account.register` |
//! | `POST /api/v1/login` `{name, password}` | `account.login` |
//! | `POST /api/v1/join` `{code, client}` | `token.join` |
//! | `POST /api/v1/logout` | `account.logout` |
//! | `POST /api/v1/session` `{name, password}` | `account.login`, kept in a cookie |
//! | `DELETE /api/v1/session` | `account.logout`, and the cookie cleared |
//! | `GET /api/v1/me` | `me.show` |
//! | `PUT /api/v1/me` `{tracker_user}` | `me.tracker` |
//! | `PUT /api/v1/me/password` `{current, new}` | `me.password` |
//! | `GET /api/v1/me/tokens` | `me.tokens` |
//! | `POST /api/v1/me/tokens` `{name, expires_days?, scopes?}` | `me.token` |
//! | `DELETE /api/v1/me/tokens/{id}` | `me.revoke` |
//! | `POST /api/v1/me/join-codes` | `me.join-code` |
//! | `GET /api/v1/users` | `user.list` |
//! | `POST /api/v1/users` `{name, role, password}` | `user.add` |
//! | `GET /api/v1/roles` | `role.list` |
//! | `POST /api/v1/users/{name}/approve` `{role}` | `user.approve` |
//! | `PUT /api/v1/users/{name}` `{role?, tracker_user?}` | `user.role`, `user.tracker` |
//! | `PUT /api/v1/users/{name}/password` `{password}` | `user.password` |
//! | `POST /api/v1/users/{name}/join-codes` | `user.join-code` |
//! | `DELETE /api/v1/users/{name}/tokens` | `user.revoke` |
//! | `DELETE /api/v1/users/{name}` | `user.remove` |
//! | `GET /api/v1/ops` | the operations the token may run, each with the JSON Schema of its input and its fields in order, for a UI to make its forms from |
//! | `POST /api/v1/ops/{name}` `{…its input}` | any operation, such as `provider.add` or `knowledge.models` |
//! | `GET /api/v1/openapi.json` | this API as an OpenAPI 3.1 document ([`openapi`](crate::openapi)), for anyone |
//!
//! The routes above are the accounts' own; every operation, theirs too, is
//! also reached by its name under `/api/v1/ops/`, so an interface needs no
//! route of its own for what it shows. A secret field, such as a provider's
//! key, is given in the body and never answered back.
//!
//! A request signs in with its bearer token, or, from a browser, with the
//! session `POST /api/v1/session` keeps in the [`SESSION_COOKIE`]: HttpOnly,
//! so no script of a page reads it, and SameSite=Strict, so no other site's
//! page sends it. A request that changes anything from a page of another
//! origin is refused, as is one signed in by the cookie that names no origin.

use crate::accounts::view;
use crate::http::{self, Head};
use crate::server::Server;
use ensemble_core::runtime::blocking;
use ensemble_ops::{Caller, Code, Error};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;

/// What a request is answered with: the status and the body.
type Answer = Result<(u16, Value), Error>;

/// The cookie a browser's session is kept in.
pub(crate) const SESSION_COOKIE: &str = "ensemble_session";

/// The token a request signs in with: its bearer token, else its session
/// cookie's.
pub(crate) fn token(head: &Head) -> Option<&str> {
    head.bearer().or_else(|| head.cookie(SESSION_COOKIE))
}

/// The cookie that keeps the session `token` for `seconds`, Secure when the
/// hub was reached over TLS; with no token, the cookie that clears it.
fn session_cookie(head: &Head, token: Option<&str>, seconds: u64) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        token.unwrap_or(""),
        if token.is_some() { seconds } else { 0 }
    );
    if head
        .reached_at()
        .is_some_and(|base| base.starts_with("https://"))
    {
        cookie.push_str("; Secure");
    }
    cookie
}

/// The status of an operation's error.
fn status(code: Code) -> u16 {
    match code {
        Code::Invalid => 400,
        Code::Unauthorized => 401,
        Code::Forbidden => 403,
        Code::NotFound => 404,
        Code::Conflict => 409,
        Code::TooMany => 429,
        Code::Failed => 500,
        Code::Unavailable => 503,
    }
}

impl Server<'_> {
    /// Answer one REST request on `stream`, then close it.
    pub(crate) async fn rest(
        &self,
        mut stream: TcpStream,
        head: &Head,
        already: &[u8],
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        let mut unread = false;
        let mut cookie = None;
        let answer = match http::read_body(&mut stream, head, already).await {
            Ok(body) => blocking(|| {
                crate::accounts::reached_at(head.reached_at(), || {
                    self.route(head, &body, &mut cookie)
                })
            }),
            Err(e) => {
                unread = true;
                Err(Error::invalid(e))
            }
        };
        let (status, body) = match answer {
            Ok(ok) => ok,
            Err(e) => {
                let mut body = serde_json::to_value(&e).unwrap_or_default();
                body["error"] = json!(e.message);
                (status(e.code), body)
            }
        };
        let who = blocking(|| {
            token(head).and_then(|t| self.users().ok()?.identify(t).map(|u| u.name.clone()))
        })
        .unwrap_or_else(|| "-".into());
        say(&format!("{} {} {who} {status}", head.method, head.path));
        let headers: Vec<(&str, &str)> =
            cookie.iter().map(|c| ("set-cookie", c.as_str())).collect();
        http::respond_with(&mut stream, status, &headers, Some(&body))
            .await
            .map_err(|e| e.to_string())?;
        if unread {
            // Closing with a body still unread resets the connection, and the
            // client may see the reset before the answer: read it away first.
            http::drain(&mut stream).await;
        }
        Ok(())
    }

    /// Answer the request `head` with `body`, setting `cookie` to the cookie
    /// to send back, if any.
    fn route(&self, head: &Head, body: &[u8], cookie: &mut Option<String>) -> Answer {
        let Some(rest) = head.path.strip_prefix("/api/v1/") else {
            return Err(Error::not_found(format!("no such path: {}", head.path)));
        };
        if !head.is_reading() {
            if head.is_from_elsewhere() {
                return Err(Error::new(
                    Code::Forbidden,
                    "a page of another origin may not change anything here",
                ));
            }
            if head.bearer().is_none()
                && head.cookie(SESSION_COOKIE).is_some()
                && head.header("origin").is_none()
            {
                return Err(Error::new(
                    Code::Forbidden,
                    "a request signed in by the session cookie must say its origin",
                ));
            }
        }
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        let body = || -> Result<Map<String, Value>, Error> {
            if body.is_empty() {
                return Ok(Map::new());
            }
            match serde_json::from_slice(body) {
                Ok(Value::Object(fields)) => Ok(fields),
                Ok(_) => Err(Error::invalid("the body must be a JSON object")),
                Err(e) => Err(Error::invalid(format!("the body is not JSON: {e}"))),
            }
        };
        // The body, with what the path names.
        let with = |name: &str, value: &str| -> Result<Value, Error> {
            let mut fields = body()?;
            fields.insert(name.into(), json!(value));
            Ok(Value::Object(fields))
        };
        let caller = self
            .hub
            .accounts()
            .map_or(Caller::Anyone, |a| a.caller(token(head)));
        let run = |name: &str, input: Value| self.operations.run(&self.hub, &caller, name, input);
        let ok = |out: Value| Ok((200, out));
        let made = |out: Value| Ok((201, out));
        match (head.method.as_str(), parts.as_slice()) {
            ("POST", ["register"]) => made(run("account.register", Value::Object(body()?))?),
            ("POST", ["login"]) => ok(run("account.login", Value::Object(body()?))?),
            ("POST", ["join"]) => made(run("token.join", Value::Object(body()?))?),
            ("POST", ["logout"]) => ok(run("account.logout", json!({}))?),
            ("POST", ["session"]) => {
                let session = run("account.login", Value::Object(body()?))?;
                let hours = self.hub.accounts()?.rules().session_hours;
                *cookie = Some(session_cookie(
                    head,
                    session["token"].as_str(),
                    u64::from(hours) * 3600,
                ));
                ok(json!({ "name": session["name"], "expires": session["expires"] }))
            }
            ("DELETE", ["session"]) => {
                let ended = match run("account.logout", json!({})) {
                    Ok(_) => true,
                    Err(e) if e.code == Code::Unauthorized => false,
                    Err(e) => return Err(e),
                };
                *cookie = Some(session_cookie(head, None, 0));
                ok(json!({ "signed_out": ended }))
            }
            ("GET", ["me"]) => match run("me.show", json!({})) {
                // One who signed up and waits is told so, with nothing allowed.
                Err(e) if e.code == Code::Unauthorized => {
                    let users = self.users().map_err(Error::failed)?;
                    let user = token(head).and_then(|t| users.identify(t)).ok_or(e)?;
                    let mut out = view(user);
                    out["permissions"] = json!([]);
                    ok(out)
                }
                answer => ok(answer?),
            },
            ("PUT", ["me"]) => {
                let fields = body()?;
                if !fields.contains_key("tracker_user") {
                    return Err(
                        Error::invalid("`tracker_user` is required; empty clears it")
                            .on("tracker_user"),
                    );
                }
                ok(run("me.tracker", Value::Object(fields))?)
            }
            ("PUT", ["me", "password"]) => ok(run("me.password", Value::Object(body()?))?),
            ("GET", ["me", "tokens"]) => ok(run("me.tokens", json!({}))?),
            ("POST", ["me", "tokens"]) => made(run("me.token", Value::Object(body()?))?),
            ("DELETE", ["me", "tokens", id]) => ok(run("me.revoke", json!({ "id": id }))?),
            ("POST", ["me", "join-codes"]) => made(run("me.join-code", json!({}))?),
            ("GET", ["users"]) => ok(run("user.list", json!({}))?),
            ("POST", ["users"]) => {
                let fields = body()?;
                if !fields.contains_key("password") {
                    return Err(Error::invalid("`password` is required").on("password"));
                }
                made(run("user.add", Value::Object(fields))?)
            }
            ("GET", ["roles"]) => ok(run("role.list", json!({}))?),
            ("POST", ["users", name, "approve"]) => ok(run("user.approve", with("name", name)?)?),
            ("PUT", ["users", name]) => {
                let fields = body()?;
                let (role, tracker) = (fields.get("role"), fields.get("tracker_user"));
                if role.is_none() && tracker.is_none() {
                    return Err(Error::invalid("give `role`, `tracker_user`, or both"));
                }
                if let Some(role) = role {
                    run("user.role", json!({ "name": name, "role": role }))?;
                }
                if let Some(tracker) = tracker {
                    run(
                        "user.tracker",
                        json!({ "name": name, "tracker_user": tracker }),
                    )?;
                }
                ok(run("user.show", json!({ "name": name }))?)
            }
            ("PUT", ["users", name, "password"]) => ok(run("user.password", with("name", name)?)?),
            ("POST", ["users", name, "join-codes"]) => {
                made(run("user.join-code", json!({ "name": name }))?)
            }
            ("DELETE", ["users", name, "tokens"]) => {
                ok(run("user.revoke", json!({ "name": name }))?)
            }
            ("DELETE", ["users", name]) => ok(run("user.remove", json!({ "name": name }))?),
            ("GET", ["ops"]) => {
                let operations: Vec<Value> = self
                    .operations
                    .operations()
                    .iter()
                    .filter(|op| caller.may(op.spec.access).is_ok())
                    .map(|op| {
                        json!({
                            "name": op.spec.name,
                            "summary": op.spec.summary,
                            "input": op.spec.input_schema(),
                            "fields": op.spec.input.iter().map(|f| f.name).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                ok(json!({ "operations": operations }))
            }
            ("POST", ["ops", name]) => ok(run(name, Value::Object(body()?))?),
            ("GET", ["openapi.json"]) => {
                let owner = self.hub.hub().map(|h| h.owner).unwrap_or_default();
                let base = crate::accounts::hub_base(&self.hub);
                ok(crate::openapi::document(&self.operations, &base, &owner))
            }
            (
                _,
                [
                    "register" | "login" | "join" | "logout" | "session" | "me" | "users" | "roles"
                    | "ops" | "openapi.json",
                ],
            )
            | (_, ["ops", _])
            | (_, ["me", "password" | "tokens" | "join-codes"])
            | (_, ["me", "tokens", _])
            | (_, ["users", _])
            | (_, ["users", _, "approve" | "password" | "tokens" | "join-codes"]) => Ok((
                405,
                json!({ "error": format!("{} is not allowed on {}", head.method, head.path) }),
            )),
            _ => Err(Error::not_found(format!("no such path: {}", head.path))),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::server::testing::{PASSWORD, start};
    use ensemble_core::runtime::block_on;
    use serde_json::{Value, json};

    /// One REST call: the status and the JSON answer.
    fn call(
        base: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!(
            "{}{path}",
            base.replace("ws://", "http://").trim_end_matches('/')
        );
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut resp = match method {
            "GET" | "DELETE" => {
                let mut req = if method == "GET" {
                    agent.get(&url)
                } else {
                    agent.delete(&url)
                };
                if let Some(a) = &auth {
                    req = req.header("authorization", a);
                }
                req.call().unwrap()
            }
            _ => {
                let mut req = if method == "PUT" {
                    agent.put(&url)
                } else {
                    agent.post(&url)
                };
                if let Some(a) = &auth {
                    req = req.header("authorization", a);
                }
                req.header("content-type", "application/json")
                    .send(body.unwrap_or(json!({})).to_string())
                    .unwrap()
            }
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// One HTTP request as a browser makes it, with `headers`: the status,
    /// the `set-cookie` header and the body as text.
    fn browser(
        base: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (u16, Option<String>, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}{path}", base.replace("ws://", "http://"));
        let mut req = ureq::http::Request::builder().method(method).uri(&url);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut resp = agent
            .run(
                req.header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .unwrap();
        let cookie = resp
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        (resp.status().as_u16(), cookie, text)
    }

    #[test]
    fn a_browser_keeps_its_session_in_a_cookie_no_page_of_another_origin_may_use() {
        let hub = start();
        let base = hub
            .url
            .replace("ws://", "http://")
            .trim_end_matches('/')
            .to_string();
        let origin = ("origin", base.as_str());
        let login = json!({"name": "root", "password": PASSWORD});
        let (status, cookie, text) = browser(
            &base,
            "POST",
            "/api/v1/session",
            &[origin],
            Some(login.clone()),
        );
        assert_eq!(status, 200, "{text}");
        assert!(!text.contains("ens_"), "no script sees the token: {text}");
        let cookie = cookie.unwrap();
        assert!(
            cookie.starts_with("ensemble_session=ens_")
                && cookie.contains("HttpOnly")
                && cookie.contains("SameSite=Strict")
                && !cookie.contains("Secure"),
            "{cookie}"
        );
        let session = cookie.split(';').next().unwrap().to_string();
        let sent = ("cookie", session.as_str());

        let (status, _, me) = browser(&base, "GET", "/api/v1/me", &[sent], None);
        assert_eq!(status, 200, "{me}");
        assert!(me.contains("\"root\""), "{me}");
        let codes = "/api/v1/me/join-codes";
        let (status, _, _) = browser(&base, "POST", codes, &[sent], None);
        assert_eq!(status, 403, "a change by the cookie names its origin");
        let elsewhere = ("origin", "https://evil.example");
        let (status, _, _) = browser(&base, "POST", codes, &[sent, elsewhere], None);
        assert_eq!(status, 403, "another origin");
        let (status, _, _) = browser(&base, "POST", "/api/v1/session", &[elsewhere], Some(login));
        assert_eq!(status, 403, "no login from another origin's page");
        let (status, _, made) = browser(&base, "POST", codes, &[sent, origin], None);
        assert_eq!(status, 201, "{made}");

        let (status, cleared, _) =
            browser(&base, "DELETE", "/api/v1/session", &[sent, origin], None);
        assert_eq!(status, 200);
        assert!(cleared.unwrap().contains("Max-Age=0"));
        let (status, _, _) = browser(&base, "GET", "/api/v1/me", &[sent], None);
        assert_eq!(status, 401, "the session ended");
    }

    #[test]
    fn tasks_knowledge_and_runners_are_operations_the_running_hub_answers() {
        let tmp = ensemble_core::workspace::testing::TempDir::new();
        let service = crate::board::Service::new(&tmp.0);
        let hub = crate::server::testing::start_with_tasks(Box::leak(Box::new(service)));
        let op = |token: &str, name: &str, input: Value| {
            call(
                &hub.url,
                "POST",
                &format!("/api/v1/ops/{name}"),
                Some(token),
                Some(input),
            )
        };
        let (status, out) = op(&hub.ana, "task.projects", json!({}));
        assert_eq!((status, out["projects"].clone()), (200, json!([])), "{out}");
        let (status, out) = op(
            &hub.ana,
            "task.create",
            json!({"project": "shop", "title": "Fix"}),
        );
        assert_eq!(status, 400, "{out}");
        let (status, made) = op(
            &hub.ana,
            "task.create",
            json!({
                "project": "shop", "title": "Fix the cart", "task": "The cart loses items.",
                "done_when": ["cargo test passes"], "gate": "cargo test",
                "gate_timeout_seconds": 600, "gate_attempts": 2,
            }),
        );
        assert_eq!(status, 200, "{made}");
        let id = made["id"].as_str().unwrap().to_string();
        let (_, out) = op(&hub.ana, "task.projects", json!({}));
        assert_eq!(out["projects"], json!([{"project": "shop", "tasks": 1}]));
        let (_, out) = op(&hub.ana, "task.list", json!({"project": "shop"}));
        assert_eq!(out["tasks"][0]["ticket"]["id"], id.as_str(), "{out}");
        assert_eq!(
            out["tasks"][0]["ticket"]["gate"],
            json!({"command": "cargo test", "timeout_seconds": 600, "attempts": 2})
        );
        let (status, shown) = op(&hub.ana, "task.show", json!({"project": "shop", "id": id}));
        assert_eq!(status, 200, "{shown}");
        assert!(shown["task"].is_object(), "{shown}");
        let (status, _) = op(
            &hub.ana,
            "task.show",
            json!({"project": "shop", "id": "t-9"}),
        );
        assert_eq!(status, 404);
        let (status, out) = op(
            &hub.ana,
            "task.gate",
            json!({"project": "shop", "id": id, "gate": "make test"}),
        );
        assert_eq!(
            status, 400,
            "a gate needs its time limit and attempts: {out}"
        );
        let (status, out) = op(&hub.ana, "task.gate", json!({"project": "shop", "id": id}));
        assert_eq!(status, 200, "{out}");
        let (_, out) = op(&hub.ana, "task.list", json!({"project": "shop"}));
        assert_eq!(out["tasks"][0]["ticket"]["gate"], Value::Null, "taken away");
        let (status, out) = op(
            &hub.ana,
            "task.cancel",
            json!({"project": "shop", "id": id}),
        );
        assert_eq!(
            (status, out["cancel"].as_str()),
            (200, Some("before")),
            "{out}"
        );
        let (_, out) = op(&hub.root, "task.projects", json!({}));
        assert_eq!(out["projects"], json!([]), "each account's own");
        // Given to another person, with a gate they confirm.
        let (_, made) = op(
            &hub.ana,
            "task.create",
            json!({
                "project": "shop", "title": "Ship", "task": "Ship it.", "gate": "make check",
                "gate_timeout_seconds": 60, "gate_attempts": 1,
            }),
        );
        let given = json!({"project": "shop", "id": made["id"], "to": "rex"});
        let (status, out) = op(&hub.ana, "task.reassign", given);
        assert_eq!(status, 400, "rex's role has no tasks: {out}");
        let given = json!({"project": "shop", "id": made["id"], "to": "root"});
        let (status, out) = op(&hub.ana, "task.reassign", given);
        assert_eq!(status, 200, "{out}");
        let theirs = out["id"].as_str().unwrap().to_string();
        let (_, out) = op(&hub.root, "task.list", json!({"project": "shop"}));
        assert_eq!(out["tasks"][0]["gate_unconfirmed"], true, "{out}");
        assert_eq!(out["tasks"][0]["moves"][0]["from"], "ana", "{out}");
        let (status, out) = op(
            &hub.root,
            "task.confirm-gate",
            json!({"project": "shop", "id": theirs}),
        );
        assert_eq!(status, 200, "{out}");
        let (_, out) = op(&hub.root, "task.list", json!({"project": "shop"}));
        assert_eq!(out["tasks"][0]["gate_unconfirmed"], false, "{out}");
        let (_, out) = op(&hub.root, "task.projects", json!({}));
        assert_eq!(out["projects"], json!([{"project": "shop", "tasks": 1}]));
        let (status, _) = op(&hub.rex, "task.projects", json!({}));
        assert_eq!(status, 403, "no `tasks`");

        let (status, out) = op(&hub.ana, "runner.list", json!({}));
        assert_eq!((status, out["runners"].clone()), (200, json!([])), "{out}");
        let (status, shared) = op(
            &hub.ana,
            "knowledge.share",
            json!({"title": "Deploys", "text": "We deploy on Fridays.", "visibility": "hub"}),
        );
        assert_eq!(status, 200, "{shared}");
        assert_eq!(shared["visibility"], "hub");
        let (status, found) = op(&hub.rex, "knowledge.find", json!({"query": "deploy"}));
        assert_eq!(status, 200, "{found}");
        assert!(found["text"].is_string(), "{found}");
        let (status, _) = op(
            &hub.rex,
            "knowledge.share",
            json!({"title": "x", "text": "y"}),
        );
        assert_eq!(status, 403, "a reader shares nothing");
    }

    fn login(base: &str, name: &str, password: &str) -> (u16, Value) {
        call(
            base,
            "POST",
            "/api/v1/login",
            None,
            Some(json!({"name": name, "password": password})),
        )
    }

    #[test]
    fn someone_signs_up_is_approved_logs_in_and_makes_a_scoped_token() {
        let hub = start();
        let (status, body) = call(
            &hub.url,
            "POST",
            "/api/v1/register",
            None,
            Some(json!({"name": "dana", "password": PASSWORD})),
        );
        assert_eq!(status, 201, "{body}");
        assert!(body.get("token").is_none(), "no token before approval");
        let (status, body) = login(&hub.url, "dana", PASSWORD);
        assert_eq!(status, 403, "pending: {body}");

        let (_, root) = login(&hub.url, "root", PASSWORD);
        let root = root["token"].as_str().unwrap().to_string();
        let (status, users) = call(&hub.url, "GET", "/api/v1/users", Some(&root), None);
        assert_eq!(status, 200);
        let text = users.to_string();
        assert!(!text.contains("sha256") && !text.contains(PASSWORD) && !text.contains("salt"));
        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/users/dana/approve",
            Some(&root),
            Some(json!({"role": "member"})),
        );
        assert_eq!(status, 200);

        let (status, session) = login(&hub.url, "dana", PASSWORD);
        assert_eq!(status, 200, "{session}");
        let session = session["token"].as_str().unwrap().to_string();
        let (status, made) = call(
            &hub.url,
            "POST",
            "/api/v1/me/tokens",
            Some(&session),
            Some(json!({"name": "laptop", "expires_days": 30, "scopes": ["recall"]})),
        );
        assert_eq!(status, 201, "{made}");
        assert_eq!(made["scopes"], json!(["recall"]));
        assert!(made["expires"].is_string());
        let token = made["token"].as_str().unwrap().to_string();

        let link = ensemble_link::Link::new(&hub.url, &token);
        let hello = block_on(link.hello()).unwrap();
        assert_eq!(hello.permissions, vec!["recall"]);
        let share = block_on(link.ask("share", json!({"title": "t", "text": "x"})));
        assert!(share.unwrap_err().contains("do not allow `share`"));

        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/me/tokens",
            Some(&token),
            Some(json!({"name": "more"})),
        );
        assert_eq!(status, 403, "an access token makes no tokens");
        let (_, listed) = call(&hub.url, "GET", "/api/v1/me/tokens", Some(&session), None);
        let listed = listed["tokens"].as_array().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].to_string().contains(&token[4..]));
        let id = listed[0]["id"].as_str().unwrap().to_string();
        let (status, _) = call(
            &hub.url,
            "DELETE",
            &format!("/api/v1/me/tokens/{id}"),
            Some(&session),
            None,
        );
        assert_eq!(status, 200);
        assert!(block_on(ensemble_link::Link::new(&hub.url, &token).hello()).is_err());

        let (status, _) = call(&hub.url, "POST", "/api/v1/logout", Some(&session), None);
        assert_eq!(status, 200);
        let (status, _) = call(&hub.url, "GET", "/api/v1/me", Some(&session), None);
        assert_eq!(status, 401, "logged out");
    }

    #[test]
    fn a_password_change_ends_sessions_and_failed_logins_lock_the_name() {
        let hub = start();
        let (_, session) = login(&hub.url, "ana", PASSWORD);
        let session = session["token"].as_str().unwrap().to_string();
        let (status, _) = call(
            &hub.url,
            "PUT",
            "/api/v1/me/password",
            Some(&session),
            Some(json!({"current": "not the password", "new": "a brand new secret"})),
        );
        assert_eq!(status, 401);
        let (status, _) = call(
            &hub.url,
            "PUT",
            "/api/v1/me/password",
            Some(&session),
            Some(json!({"current": PASSWORD, "new": "a brand new secret"})),
        );
        assert_eq!(status, 200);
        let (status, _) = call(&hub.url, "GET", "/api/v1/me", Some(&session), None);
        assert_eq!(status, 401, "sessions end");
        assert_eq!(login(&hub.url, "ana", "a brand new secret").0, 200);

        // The test hub locks a name after 3 failures.
        for _ in 0..3 {
            assert_eq!(login(&hub.url, "rex", "wrong password!!").0, 401);
        }
        assert_eq!(
            login(&hub.url, "rex", PASSWORD).0,
            429,
            "locked, even with the right one"
        );
    }

    #[test]
    fn admins_add_accounts_reset_passwords_change_roles_and_remove() {
        let hub = start();
        let (_, root) = login(&hub.url, "root", PASSWORD);
        let root = root["token"].as_str().unwrap().to_string();
        let (status, body) = call(
            &hub.url,
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"name": "eve", "role": "reader", "password": PASSWORD})),
        );
        assert_eq!(status, 201, "{body}");
        assert_eq!(login(&hub.url, "eve", PASSWORD).0, 200);
        let (status, _) = call(
            &hub.url,
            "PUT",
            "/api/v1/users/eve/password",
            Some(&root),
            Some(json!({"password": "reset by the admin"})),
        );
        assert_eq!(status, 200);
        assert_eq!(login(&hub.url, "eve", "reset by the admin").0, 200);
        let (status, _) = call(
            &hub.url,
            "PUT",
            "/api/v1/users/eve",
            Some(&root),
            Some(json!({"role": "god"})),
        );
        assert_eq!(status, 400);
        let (_, ana) = login(&hub.url, "ana", PASSWORD);
        let ana = ana["token"].as_str().unwrap().to_string();
        let (status, _) = call(&hub.url, "GET", "/api/v1/users", Some(&ana), None);
        assert_eq!(status, 403, "a member is no admin");
        let (status, _) = call(&hub.url, "DELETE", "/api/v1/users/eve", Some(&root), None);
        assert_eq!(status, 200);
        assert_eq!(login(&hub.url, "eve", "reset by the admin").0, 401);
        let (_, roles) = call(&hub.url, "GET", "/api/v1/roles", Some(&root), None);
        assert_eq!(roles["roles"]["reader"], json!(["recall"]));

        let (status, body) = call(
            &hub.url,
            "PUT",
            "/api/v1/users/rex",
            Some(&root),
            Some(json!({"tracker_user": "rex.g"})),
        );
        assert_eq!(
            (status, body["tracker_name"].as_str()),
            (200, Some("rex.g")),
            "{body}"
        );
        let (status, body) = call(
            &hub.url,
            "PUT",
            "/api/v1/me",
            Some(&ana),
            Some(json!({"tracker_user": "rex.g"})),
        );
        assert_eq!(status, 409, "taken by rex: {body}");
        let (status, _) = call(
            &hub.url,
            "PUT",
            "/api/v1/me",
            Some(&ana),
            Some(json!({"tracker_user": "ana.p"})),
        );
        assert_eq!(status, 200);
        let (_, me) = call(&hub.url, "GET", "/api/v1/me", Some(&ana), None);
        assert_eq!(me["tracker_name"], "ana.p");
    }

    #[test]
    fn join_codes_are_made_by_an_admin_or_by_a_session_and_sign_a_client_in_once() {
        let hub = start();
        let (_, root) = login(&hub.url, "root", PASSWORD);
        let root = root["token"].as_str().unwrap().to_string();
        let (status, made) = call(
            &hub.url,
            "POST",
            "/api/v1/users/rex/join-codes",
            Some(&root),
            None,
        );
        assert_eq!(status, 201, "{made}");
        assert!(made["expires"].is_string());
        assert!(
            made["note"]
                .as_str()
                .unwrap()
                .contains("ensemble-runner join"),
            "{made}"
        );
        let code = made["code"].as_str().unwrap().to_string();
        assert!(
            call(&hub.url, "GET", "/api/v1/me", Some(&code), None).0 == 401,
            "a code signs in as no one"
        );
        let join = |code: &str| {
            block_on(ensemble_link::client::sign_in(
                &hub.url,
                ensemble_link::protocol::JOIN,
                json!({"code": code, "client": "runner lab"}),
            ))
        };
        let token = join(&code).unwrap();
        let hello = block_on(ensemble_link::Link::new(&hub.url, &token).hello()).unwrap();
        assert_eq!(hello.user, "rex");
        assert!(join(&code).is_err(), "spent");

        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/users/rex/join-codes",
            Some(&hub.ana),
            None,
        );
        assert_eq!(status, 403, "a member is no admin");
        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/me/join-codes",
            Some(&hub.ana),
            None,
        );
        assert_eq!(status, 403, "an access token makes no codes");
        let (_, session) = login(&hub.url, "ana", PASSWORD);
        let session = session["token"].as_str().unwrap().to_string();
        let (status, made) = call(
            &hub.url,
            "POST",
            "/api/v1/me/join-codes",
            Some(&session),
            None,
        );
        assert_eq!(status, 201, "{made}");
        // Any client spends one, over the REST API as over the link.
        let body = json!({"code": made["code"], "client": "script"});
        let (status, joined) = call(&hub.url, "POST", "/api/v1/join", None, Some(body.clone()));
        assert_eq!(status, 201, "{joined}");
        let token = joined["token"].as_str().unwrap();
        let (status, me) = call(&hub.url, "GET", "/api/v1/me", Some(token), None);
        assert_eq!((status, me["name"].as_str()), (200, Some("ana")), "{me}");
        let (status, _) = call(&hub.url, "POST", "/api/v1/join", None, Some(body));
        assert_eq!(status, 401, "spent");
        let (status, _) = call(
            &hub.url,
            "GET",
            "/api/v1/me/join-codes",
            Some(&session),
            None,
        );
        assert_eq!(status, 405);
    }

    #[test]
    fn every_operation_is_reached_by_its_name_and_listed_with_its_input() {
        let hub = start();
        let (_, root) = login(&hub.url, "root", PASSWORD);
        let root = root["token"].as_str().unwrap().to_string();
        let (status, listed) = call(&hub.url, "GET", "/api/v1/ops", Some(&root), None);
        assert_eq!(status, 200, "{listed}");
        let ops = listed["operations"].as_array().unwrap();
        let add = ops
            .iter()
            .find(|o| o["name"] == "provider.add")
            .expect("provider.add is listed");
        assert_eq!(
            add["input"]["properties"]["key"]["writeOnly"], true,
            "{add}"
        );
        assert_eq!(add["fields"][0], "name", "its fields in order: {add}");
        assert!(ops.iter().any(|o| o["name"] == "knowledge.models"));
        let (status, roles) = call(
            &hub.url,
            "POST",
            "/api/v1/ops/role.list",
            Some(&root),
            Some(json!({})),
        );
        assert_eq!(status, 200, "{roles}");
        assert!(roles.to_string().contains("reader"), "{roles}");

        // A reader sees and runs only what its role lets.
        let (_, listed) = call(&hub.url, "GET", "/api/v1/ops", Some(&hub.rex), None);
        let names: Vec<&str> = listed["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"provider.add"), "{names:?}");
        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/ops/provider.list",
            Some(&hub.rex),
            Some(json!({})),
        );
        assert_eq!(status, 403);
        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/ops/nope.never",
            Some(&root),
            Some(json!({})),
        );
        assert_eq!(status, 404);
        let (status, _) = call(&hub.url, "GET", "/api/v1/ops/role.list", Some(&root), None);
        assert_eq!(status, 405);
    }

    #[test]
    fn anyone_reads_the_openapi_document_of_every_route_and_operation() {
        let hub = start();
        let (status, doc) = call(&hub.url, "GET", "/api/v1/openapi.json", None, None);
        assert_eq!(status, 200, "{doc}");
        assert_eq!(doc["openapi"], "3.1.0");
        let paths = doc["paths"].as_object().unwrap();
        let add = &paths["/api/v1/ops/provider.add"]["post"];
        assert_eq!(add["security"], json!([{ "bearer": [] }]));
        let key = &add["requestBody"]["content"]["application/json"]["schema"]["properties"]["key"];
        assert_eq!(key["writeOnly"], true, "{add}");
        let register = &paths["/api/v1/register"]["post"];
        assert_eq!(register["security"], json!([]), "anyone signs up");
        let approve = &paths["/api/v1/users/{name}/approve"]["post"];
        let body = &approve["requestBody"]["content"]["application/json"]["schema"];
        assert!(
            body["properties"].get("name").is_none(),
            "the path names it"
        );
        assert_eq!(body["required"], json!(["role"]), "{body}");
        assert!(paths.contains_key("/api/v1/ops/settings.owner"));

        // Each route the document shows is one the API answers.
        let (_, session) = login(&hub.url, "root", PASSWORD);
        let root = session["token"].as_str().unwrap().to_string();
        for route in crate::openapi::ROUTES {
            let path = format!("/api/v1{}", route.path)
                .replace("{name}", "nobody")
                .replace("{id}", "nobody");
            assert!(
                paths.contains_key(&format!("/api/v1{}", route.path)),
                "{path}"
            );
            if route.path == "/logout" {
                continue;
            }
            let (status, answer) = call(&hub.url, route.method, &path, Some(&root), None);
            let error = answer["error"].as_str().unwrap_or_default();
            assert!(
                status != 405 && !error.starts_with("no such path"),
                "{} {path}: {status} {answer}",
                route.method
            );
        }
    }

    #[test]
    fn sign_ups_are_limited_and_bad_requests_are_named() {
        let hub = start();
        let register = |name: &str, password: &str| {
            call(
                &hub.url,
                "POST",
                "/api/v1/register",
                None,
                Some(json!({"name": name, "password": password})),
            )
            .0
        };
        assert_eq!(register("a1", PASSWORD), 201);
        assert_eq!(register("a2", PASSWORD), 201);
        assert_eq!(register("a3", PASSWORD), 409, "max_pending is 2");
        assert_eq!(register("ana", PASSWORD), 409, "a taken name");
        assert_eq!(register("b1", "short"), 400, "a short password");
        let (status, _) = call(&hub.url, "POST", "/api/v1/register", None, Some(json!({})));
        assert_eq!(status, 400);
        let (status, _) = call(&hub.url, "GET", "/api/v1/nothing", None, None);
        assert_eq!(status, 404);
        let (status, _) = call(&hub.url, "GET", "/api/v1/register", None, None);
        assert_eq!(status, 405);
        let big = json!({"name": "x".repeat(crate::http::MAX_BODY)});
        let (status, _) = call(&hub.url, "POST", "/api/v1/register", None, Some(big));
        assert_eq!(status, 400);
    }
}
