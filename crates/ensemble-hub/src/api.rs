//! The hub server's REST API, under `/api/v1/`, for people and the UI to come:
//! each route runs one of the hub's operations ([`accounts`](crate::accounts)),
//! signed in with the request's bearer token. JSON in and out; an error is
//! `{error, code, field?, hint?}`.
//!
//! | Method and path | Operation |
//! |---|---|
//! | `POST /api/v1/register` `{name, password}` | `account.register` |
//! | `POST /api/v1/login` `{name, password}` | `account.login` |
//! | `POST /api/v1/logout` | `account.logout` |
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
//! | `GET /api/v1/ops` | the operations the token may run, each with the JSON Schema of its input, for a UI to make its forms from |
//! | `POST /api/v1/ops/{name}` `{…its input}` | any operation, such as `backend.add` or `knowledge.models` |
//!
//! The routes above are the accounts' own; every operation, theirs too, is
//! also reached by its name under `/api/v1/ops/`, so an interface needs no
//! route of its own for what it shows. A secret field, such as a backend's
//! key, is given in the body and never answered back.

use crate::accounts::view;
use crate::http::{self, Head};
use crate::server::Server;
use ensemble_core::runtime::blocking;
use ensemble_ops::{Caller, Code, Error};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;

/// What a request is answered with: the status and the body.
type Answer = Result<(u16, Value), Error>;

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
        let answer = match http::read_body(&mut stream, head, already).await {
            Ok(body) => blocking(|| {
                crate::accounts::reached_at(head.reached_at(), || self.route(head, &body))
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
            head.bearer()
                .and_then(|t| self.users().ok()?.identify(t).map(|u| u.name.clone()))
        })
        .unwrap_or_else(|| "-".into());
        say(&format!("{} {} {who} {status}", head.method, head.path));
        http::respond(&mut stream, status, &body)
            .await
            .map_err(|e| e.to_string())?;
        if unread {
            // Closing with a body still unread resets the connection, and the
            // client may see the reset before the answer: read it away first.
            http::drain(&mut stream).await;
        }
        Ok(())
    }

    fn route(&self, head: &Head, body: &[u8]) -> Answer {
        let Some(rest) = head.path.strip_prefix("/api/v1/") else {
            return Err(Error::not_found(format!("no such path: {}", head.path)));
        };
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
            .map_or(Caller::Anyone, |a| a.caller(head.bearer()));
        let run = |name: &str, input: Value| self.operations.run(&self.hub, &caller, name, input);
        let ok = |out: Value| Ok((200, out));
        let made = |out: Value| Ok((201, out));
        match (head.method.as_str(), parts.as_slice()) {
            ("POST", ["register"]) => made(run("account.register", Value::Object(body()?))?),
            ("POST", ["login"]) => ok(run("account.login", Value::Object(body()?))?),
            ("POST", ["logout"]) => ok(run("account.logout", json!({}))?),
            ("GET", ["me"]) => match run("me.show", json!({})) {
                // One who signed up and waits is told so, with nothing allowed.
                Err(e) if e.code == Code::Unauthorized => {
                    let users = self.users().map_err(Error::failed)?;
                    let user = head.bearer().and_then(|t| users.identify(t)).ok_or(e)?;
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
                        })
                    })
                    .collect();
                ok(json!({ "operations": operations }))
            }
            ("POST", ["ops", name]) => ok(run(name, Value::Object(body()?))?),
            (_, ["register" | "login" | "logout" | "me" | "users" | "roles" | "ops"])
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
    fn join_codes_are_made_by_an_admin_or_by_a_session_and_sign_a_runner_in_once() {
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
                json!({"code": code, "runner": "lab"}),
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
        let token = join(made["code"].as_str().unwrap()).unwrap();
        let hello = block_on(ensemble_link::Link::new(&hub.url, &token).hello()).unwrap();
        assert_eq!(hello.user, "ana");
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
            .find(|o| o["name"] == "backend.add")
            .expect("backend.add is listed");
        assert_eq!(
            add["input"]["properties"]["key"]["writeOnly"], true,
            "{add}"
        );
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
        assert!(!names.contains(&"backend.add"), "{names:?}");
        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/ops/backend.list",
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
