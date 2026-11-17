//! The hub server's REST API, under `/api/v1/`, for people and the UI to come:
//! accounts with a password, sign-in sessions, access tokens, and an admin
//! managing users. JSON in and out, a bearer token where one is needed. It
//! is the hub's alone: a node has no accounts.
//!
//! A **session** comes from logging in with the password, and only a session
//! may change the password or make and revoke access tokens, so a stolen access
//! token cannot make more. An **access token** may be narrowed by scopes, and
//! is what nodes and scripts use.
//!
//! | Method and path | Who |
//! |---|---|
//! | `POST /api/v1/register` `{name, password}` | anyone, when registration is `approval` |
//! | `POST /api/v1/login` `{name, password}` | anyone |
//! | `POST /api/v1/logout` | a session |
//! | `GET /api/v1/me` | any token |
//! | `PUT /api/v1/me` `{tracker_user}` | a session |
//! | `PUT /api/v1/me/password` `{current, new}` | a session |
//! | `GET /api/v1/me/tokens` | a session |
//! | `POST /api/v1/me/tokens` `{name, expires_days?, scopes?}` | a session |
//! | `DELETE /api/v1/me/tokens/{id}` | a session |
//! | `GET /api/v1/users` | admin |
//! | `POST /api/v1/users` `{name, role, password}` | admin |
//! | `GET /api/v1/roles` | admin |
//! | `POST /api/v1/users/{name}/approve` `{role}` | admin |
//! | `PUT /api/v1/users/{name}` `{role?, tracker_user?}` | admin |
//! | `PUT /api/v1/users/{name}/password` `{password}` | admin |
//! | `DELETE /api/v1/users/{name}/tokens` | admin |
//! | `DELETE /api/v1/users/{name}` | admin |

use crate::http::{self, Head};
use crate::server::Server;
use crate::users::{Kind, Signed, Token, User, Users, UsersError};
use ensemble_config::{Permission, Registration};
use ensemble_core::runtime::blocking;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// A refusal: the status, and why.
struct Refusal(u16, String);

type Answer = Result<(u16, Value), Refusal>;

impl From<UsersError> for Refusal {
    fn from(e: UsersError) -> Refusal {
        let status = match e {
            UsersError::NotFound(_) => 404,
            UsersError::Conflict(_) => 409,
            UsersError::Invalid(_) => 400,
            UsersError::Unauthorized(_) => 401,
            UsersError::Forbidden(_) => 403,
            UsersError::Io(_) => 500,
        };
        Refusal(status, e.to_string())
    }
}

/// The body's JSON, and its fields.
struct Body(Value);

impl Body {
    fn text(&self, key: &str) -> Result<String, Refusal> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| Refusal(400, format!("`{key}` is required")))
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
            Ok(body) => blocking(|| self.route(head, &body)),
            Err(e) => {
                unread = true;
                Err(Refusal(400, e))
            }
        };
        let (status, body) = match answer {
            Ok(ok) => ok,
            Err(Refusal(status, why)) => (status, json!({ "error": why })),
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
            return Err(Refusal(404, format!("no such path: {}", head.path)));
        };
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        let body = || -> Result<Body, Refusal> {
            if body.is_empty() {
                return Ok(Body(json!({})));
            }
            serde_json::from_slice(body)
                .map(Body)
                .map_err(|e| Refusal(400, format!("the body is not JSON: {e}")))
        };
        match (head.method.as_str(), parts.as_slice()) {
            ("POST", ["register"]) => self.register(&body()?),
            ("POST", ["login"]) => self.login(&body()?),
            ("POST", ["logout"]) => {
                let session = self.session(head)?;
                self.change(|u| u.revoke_token(&session.user.name, &session.token_id))?;
                Ok((200, json!({ "signed_out": true })))
            }
            ("GET", ["me"]) => self.me(head),
            ("PUT", ["me"]) => {
                let session = self.session(head)?;
                let body = body()?;
                let tracker_user = body.0.get("tracker_user").and_then(Value::as_str);
                if body.0.get("tracker_user").is_none() {
                    return Err(Refusal(
                        400,
                        "`tracker_user` is required; empty clears it".into(),
                    ));
                }
                self.change(|u| u.set_tracker_user(&session.user.name, tracker_user))?;
                Ok((
                    200,
                    json!({ "name": session.user.name, "tracker_user": tracker_user }),
                ))
            }
            ("PUT", ["me", "password"]) => {
                let session = self.session(head)?;
                let body = body()?;
                let (current, new) = (body.text("current")?, body.text("new")?);
                let name = session.user.name.clone();
                self.change(|users| {
                    users.login(&name, &current, 1).map_err(|_| {
                        UsersError::Unauthorized("the current password is wrong".into())
                    })?;
                    users.set_password(&name, &new)
                })?;
                Ok((
                    200,
                    json!({ "changed": true, "note": "Every session was ended; log in again." }),
                ))
            }
            ("GET", ["me", "tokens"]) => {
                let session = self.session(head)?;
                let users = self.users().map_err(|e| Refusal(500, e))?;
                let tokens = users.tokens(&session.user.name)?;
                Ok((
                    200,
                    json!({ "tokens": tokens.into_iter().map(token_view).collect::<Vec<_>>() }),
                ))
            }
            ("POST", ["me", "tokens"]) => self.make_token(head, &body()?),
            ("DELETE", ["me", "tokens", id]) => {
                let session = self.session(head)?;
                self.change(|u| u.revoke_token(&session.user.name, id))?;
                Ok((200, json!({ "id": id, "revoked": true })))
            }
            ("GET", ["users"]) => {
                self.admin(head)?;
                let users = self.users().map_err(|e| Refusal(500, e))?;
                Ok((
                    200,
                    json!({ "users": users.list().map(view).collect::<Vec<_>>() }),
                ))
            }
            ("POST", ["users"]) => {
                self.admin(head)?;
                let body = body()?;
                let (name, role, password) = (
                    body.text("name")?,
                    body.text("role")?,
                    body.text("password")?,
                );
                self.change(|u| u.create(&name, &role, &password, self.roles))?;
                Ok((
                    201,
                    json!({ "name": name, "role": role, "status": "active" }),
                ))
            }
            ("GET", ["roles"]) => {
                self.admin(head)?;
                let roles: serde_json::Map<String, Value> = self
                    .roles
                    .iter()
                    .map(|(name, perms)| (name.clone(), json!(names(perms))))
                    .collect();
                Ok((200, json!({ "roles": roles })))
            }
            ("POST", ["users", name, "approve"]) => {
                self.admin(head)?;
                let role = body()?.text("role")?;
                self.change(|users| users.approve(name, &role, self.roles))?;
                Ok((
                    200,
                    json!({ "name": name, "role": role, "status": "active" }),
                ))
            }
            ("PUT", ["users", name]) => {
                self.admin(head)?;
                let body = body()?;
                let role = body.0.get("role").and_then(Value::as_str);
                let tracker = body.0.get("tracker_user");
                if role.is_none() && tracker.is_none() {
                    return Err(Refusal(400, "give `role`, `tracker_user`, or both".into()));
                }
                self.change(|users| {
                    if let Some(role) = role {
                        users.set_role(name, role, self.roles)?;
                    }
                    if let Some(tracker) = tracker {
                        users.set_tracker_user(name, tracker.as_str())?;
                    }
                    Ok(())
                })?;
                let users = self.users().map_err(|e| Refusal(500, e))?;
                let user = users
                    .get(name)
                    .ok_or_else(|| Refusal(404, format!("no user called {name}")))?;
                Ok((200, view(user)))
            }
            ("PUT", ["users", name, "password"]) => {
                self.admin(head)?;
                let password = body()?.text("password")?;
                self.change(|users| users.set_password(name, &password))?;
                Ok((200, json!({ "name": name, "changed": true })))
            }
            ("DELETE", ["users", name, "tokens"]) => {
                self.admin(head)?;
                let revoked = self.change(|users| users.revoke(name))?;
                Ok((200, json!({ "name": name, "revoked": revoked })))
            }
            ("DELETE", ["users", name]) => {
                self.admin(head)?;
                self.change(|users| users.remove(name))?;
                Ok((200, json!({ "name": name, "removed": true })))
            }
            (_, ["register" | "login" | "logout" | "me" | "users" | "roles"])
            | (_, ["me", "password" | "tokens"])
            | (_, ["me", "tokens", _])
            | (_, ["users", _])
            | (_, ["users", _, "approve" | "password" | "tokens"]) => Err(Refusal(
                405,
                format!("{} is not allowed on {}", head.method, head.path),
            )),
            _ => Err(Refusal(404, format!("no such path: {}", head.path))),
        }
    }

    /// Read the users, change them, and write them, while no other request
    /// does.
    fn change<T>(&self, f: impl FnOnce(&mut Users) -> Result<T, UsersError>) -> Result<T, Refusal> {
        let _held = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        let mut users = self.users().map_err(|e| Refusal(500, e))?;
        let out = f(&mut users)?;
        users.save()?;
        Ok(out)
    }

    fn register(&self, body: &Body) -> Answer {
        let Registration::Approval { max_pending } = self.settings.registration else {
            return Err(Refusal(
                403,
                "this hub does not take sign-ups; ask its admin to add you".into(),
            ));
        };
        let (name, password) = (body.text("name")?, body.text("password")?);
        self.change(|users| users.register(&name, &password, max_pending))?;
        Ok((
            201,
            json!({ "name": name, "status": "pending", "note": "Log in once an admin approves you." }),
        ))
    }

    /// Check a name and password, minding the lockout, and start a session.
    fn login(&self, body: &Body) -> Answer {
        let (name, password) = (body.text("name")?, body.text("password")?);
        {
            let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, Some(until))) = failures.get(&name)
                && Instant::now() < *until
            {
                return Err(Refusal(
                    429,
                    "too many failed logins for this name; try again later".into(),
                ));
            }
        }
        let hours = self.settings.session_hours;
        match self.change(|users| users.login(&name, &password, hours)) {
            Ok((token, expires)) => {
                self.failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&name);
                Ok((
                    200,
                    json!({ "name": name, "token": token, "expires": expires }),
                ))
            }
            Err(Refusal(401, why)) => {
                let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
                let entry = failures.entry(name).or_insert((0, None));
                entry.0 += 1;
                if entry.0 >= self.settings.lockout_failures {
                    let minutes = u64::from(self.settings.lockout_minutes);
                    *entry = (0, Some(Instant::now() + Duration::from_secs(minutes * 60)));
                }
                Err(Refusal(401, why))
            }
            Err(other) => Err(other),
        }
    }

    fn me(&self, head: &Head) -> Answer {
        let users = self.users().map_err(|e| Refusal(500, e))?;
        let token = head
            .bearer()
            .ok_or_else(|| Refusal(401, "a valid token is needed".into()))?;
        if let Some(signed) = users.authenticate(token, self.roles) {
            let mut out = view(&signed.user);
            out["permissions"] = json!(names(&signed.permissions));
            out["token"] = json!({ "id": signed.token_id, "kind": signed.kind.as_str() });
            return Ok((200, out));
        }
        let user = users
            .identify(token)
            .ok_or_else(|| Refusal(401, "a valid token is needed".into()))?;
        let mut out = view(user);
        out["permissions"] = json!([]);
        Ok((200, out))
    }

    fn make_token(&self, head: &Head, body: &Body) -> Answer {
        let session = self.session(head)?;
        let label = body.text("name")?;
        let expires_days =
            match body.0.get("expires_days") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().and_then(|d| u32::try_from(d).ok()).ok_or_else(
                    || Refusal(400, "`expires_days` must be a whole number of days".into()),
                )?),
            };
        let scopes = match body.0.get("scopes") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                serde_json::from_value::<Vec<Permission>>(v.clone()).map_err(|_| {
                    Refusal(
                        400,
                        "`scopes` must be a list of share, recall, tasks, admin, compute or host"
                            .into(),
                    )
                })?,
            ),
        };
        let name = session.user.name.clone();
        let (id, token, expires) = self.change(|users| {
            users.create_token(&name, &label, expires_days, scopes.clone(), self.roles)
        })?;
        Ok((
            201,
            json!({
                "id": id,
                "name": label,
                "token": token,
                "scopes": scopes.as_deref().map(names),
                "expires": expires,
                "note": "Keep this token: it is shown once.",
            }),
        ))
    }

    /// Who the request's token signs in as, when it is valid.
    fn signed_in(&self, head: &Head) -> Result<Signed, Refusal> {
        let users = self.users().map_err(|e| Refusal(500, e))?;
        head.bearer()
            .and_then(|t| users.authenticate(t, self.roles))
            .ok_or_else(|| Refusal(401, "a valid token of an approved user is needed".into()))
    }

    /// The request's session: refused for an access token, which may not
    /// manage the account.
    fn session(&self, head: &Head) -> Result<Signed, Refusal> {
        let signed = self.signed_in(head)?;
        if signed.kind == Kind::Session {
            Ok(signed)
        } else {
            Err(Refusal(
                403,
                "this needs a session from logging in with your password, not an access token"
                    .into(),
            ))
        }
    }

    /// Refuse unless the request's token may `admin`.
    fn admin(&self, head: &Head) -> Result<Signed, Refusal> {
        let signed = self.signed_in(head)?;
        if signed.permissions.contains(&Permission::Admin) {
            Ok(signed)
        } else {
            Err(Refusal(
                403,
                "this needs a role, and a token, with the `admin` permission".into(),
            ))
        }
    }
}

fn names(permissions: &[Permission]) -> Vec<&'static str> {
    permissions.iter().map(|p| p.as_str()).collect()
}

/// An account as the API shows it: never a password, a token or a hash.
fn view(user: &User) -> Value {
    json!({
        "name": user.name,
        "role": user.role,
        "status": user.status.as_str(),
        "registered": user.registered,
        "has_password": user.password.is_some(),
        "tracker_user": user.tracker_user,
        "tracker_name": user.tracker_name(),
        "tokens": user.tokens.iter().filter(|t| t.kind == Kind::Personal).count(),
    })
}

/// An access token as the API lists it: never the token or its hash.
fn token_view(token: &Token) -> Value {
    json!({
        "id": token.id,
        "name": token.name,
        "scopes": token.scopes.as_deref().map(names),
        "made": token.made,
        "expires": token.expires,
    })
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

        let link = crate::RemoteMemory::new(&hub.url, &token, None);
        let hello = block_on(link.hello()).unwrap();
        assert_eq!(hello.permissions, vec!["recall"]);
        use ensemble_knowledge::KnowledgeAccess;
        let share = block_on(link.call("share", &json!({"title": "t", "text": "x"}))).unwrap();
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
        assert!(block_on(crate::RemoteMemory::new(&hub.url, &token, None).hello()).is_err());

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
