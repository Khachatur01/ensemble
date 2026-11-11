//! The hub server's REST API, under `/api/v1/`, for people and the UI to come:
//! signing up, and an admin managing users. JSON in and out, a bearer token
//! where one is needed. It exists only on a hub server; an embedded hub has one
//! user and no registration.
//!
//! | Method and path | Who |
//! |---|---|
//! | `POST /api/v1/register` `{name}` | anyone, when registration is `approval` |
//! | `GET /api/v1/me` | any token, a pending one too |
//! | `POST /api/v1/me/tokens` | an active user |
//! | `GET /api/v1/users` | admin |
//! | `POST /api/v1/users/{name}/approve` `{role}` | admin |
//! | `PUT /api/v1/users/{name}` `{role}` | admin |
//! | `DELETE /api/v1/users/{name}/tokens` | admin |
//! | `DELETE /api/v1/users/{name}` | admin |
//! | `GET /api/v1/roles` | admin |

use crate::http::{self, Head};
use crate::server::Server;
use crate::users::{User, Users, UsersError};
use ensemble_config::{Permission, Registration};
use serde_json::{Value, json};
use std::net::TcpStream;

/// A refusal: the status, and why.
struct Refusal(u16, String);

type Answer = Result<(u16, Value), Refusal>;

impl From<UsersError> for Refusal {
    fn from(e: UsersError) -> Refusal {
        let status = match e {
            UsersError::NotFound(_) => 404,
            UsersError::Conflict(_) => 409,
            UsersError::Invalid(_) => 400,
            UsersError::Io(_) => 500,
        };
        Refusal(status, e.to_string())
    }
}

impl Server<'_> {
    /// Answer one REST request on `stream`, then close it.
    pub(crate) fn rest(
        &self,
        mut stream: TcpStream,
        head: &Head,
        already: &[u8],
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        let mut unread = false;
        let answer = match http::read_body(&mut stream, head, already) {
            Ok(body) => self.route(head, &body),
            Err(e) => {
                unread = true;
                Err(Refusal(400, e))
            }
        };
        let (status, body) = match answer {
            Ok(ok) => ok,
            Err(Refusal(status, why)) => (status, json!({ "error": why })),
        };
        let who = head
            .bearer()
            .and_then(|t| self.users().ok()?.identify(t).map(|u| u.name.clone()))
            .unwrap_or_else(|| "-".into());
        say(&format!("{} {} {who} {status}", head.method, head.path));
        http::respond(&mut stream, status, &body).map_err(|e| e.to_string())?;
        if unread {
            // Closing with a body still unread resets the connection, and the
            // client may see the reset before the answer: read it away first.
            http::drain(&mut stream);
        }
        Ok(())
    }

    fn route(&self, head: &Head, body: &[u8]) -> Answer {
        let Some(rest) = head.path.strip_prefix("/api/v1/") else {
            return Err(Refusal(404, format!("no such path: {}", head.path)));
        };
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        let json = || -> Result<Value, Refusal> {
            if body.is_empty() {
                return Ok(json!({}));
            }
            serde_json::from_slice(body)
                .map_err(|e| Refusal(400, format!("the body is not JSON: {e}")))
        };
        match (head.method.as_str(), parts.as_slice()) {
            ("POST", ["register"]) => self.register(&json()?),
            ("GET", ["me"]) => self.me(head),
            ("POST", ["me", "tokens"]) => self.my_token(head),
            ("GET", ["users"]) => {
                self.admin(head)?;
                let users = self.users().map_err(|e| Refusal(500, e))?;
                Ok((
                    200,
                    json!({ "users": users.list().map(view).collect::<Vec<_>>() }),
                ))
            }
            ("GET", ["roles"]) => {
                self.admin(head)?;
                let roles: serde_json::Map<String, Value> = self
                    .roles
                    .iter()
                    .map(|(name, perms)| {
                        (
                            name.clone(),
                            json!(perms.iter().map(|p| p.as_str()).collect::<Vec<_>>()),
                        )
                    })
                    .collect();
                Ok((200, json!({ "roles": roles })))
            }
            ("POST", ["users", name, "approve"]) => {
                self.admin(head)?;
                let role = role_in(&json()?)?;
                self.change(|users| users.approve(name, &role, self.roles))?;
                Ok((
                    200,
                    json!({ "name": name, "role": role, "status": "active" }),
                ))
            }
            ("PUT", ["users", name]) => {
                self.admin(head)?;
                let role = role_in(&json()?)?;
                self.change(|users| users.set_role(name, &role, self.roles))?;
                Ok((200, json!({ "name": name, "role": role })))
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
            (_, ["register"] | ["me"] | ["me", "tokens"] | ["users"] | ["roles"])
            | (_, ["users", _] | ["users", _, "approve" | "tokens"]) => Err(Refusal(
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

    fn register(&self, body: &Value) -> Answer {
        let Registration::Approval { max_pending } = self.registration else {
            return Err(Refusal(
                403,
                "this hub does not take sign-ups; ask its admin to add you".into(),
            ));
        };
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Refusal(400, "`name` is required".into()))?
            .to_string();
        let token = self.change(|users| users.register(&name, max_pending))?;
        Ok((
            201,
            json!({
                "name": name,
                "status": "pending",
                "token": token,
                "note": "Keep this token: it is shown once. It works once an admin approves you.",
            }),
        ))
    }

    fn me(&self, head: &Head) -> Answer {
        let users = self.users().map_err(|e| Refusal(500, e))?;
        let user = head
            .bearer()
            .and_then(|t| users.identify(t))
            .ok_or_else(|| Refusal(401, "a valid token is needed".into()))?;
        let mut out = view(user);
        out["permissions"] = json!(self.permissions_of(user));
        Ok((200, out))
    }

    fn my_token(&self, head: &Head) -> Answer {
        let name = self.signed_in(head)?.name;
        let token = self.change(|users| users.token(&name))?;
        Ok((
            201,
            json!({ "name": name, "token": token, "note": "Keep this token: it is shown once." }),
        ))
    }

    /// The active user the request's token signs in as.
    fn signed_in(&self, head: &Head) -> Result<User, Refusal> {
        let users = self.users().map_err(|e| Refusal(500, e))?;
        head.bearer()
            .and_then(|t| users.authenticate(t))
            .cloned()
            .ok_or_else(|| Refusal(401, "a valid token of an approved user is needed".into()))
    }

    /// Refuse unless the request comes from a user whose role has `admin`.
    fn admin(&self, head: &Head) -> Result<User, Refusal> {
        let user = self.signed_in(head)?;
        if self
            .permissions_of(&user)
            .contains(&Permission::Admin.as_str())
        {
            Ok(user)
        } else {
            Err(Refusal(
                403,
                "this needs a role with the `admin` permission".into(),
            ))
        }
    }

    fn permissions_of(&self, user: &User) -> Vec<&'static str> {
        self.roles
            .get(&user.role)
            .map(|p| p.iter().map(|p| p.as_str()).collect())
            .unwrap_or_default()
    }
}

/// A user as the API shows them: never a token or its hash.
fn view(user: &User) -> Value {
    json!({
        "name": user.name,
        "role": user.role,
        "status": user.status.as_str(),
        "registered": user.registered,
        "tokens": user.tokens.len(),
    })
}

fn role_in(body: &Value) -> Result<String, Refusal> {
    body.get("role")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| Refusal(400, "`role` is required".into()))
}

#[cfg(test)]
mod tests {
    use crate::server::tests::start;
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
        let mut resp = match (method, body) {
            ("GET" | "DELETE", _) => {
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
            (_, body) => {
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

    #[test]
    fn someone_signs_up_waits_and_is_let_in_by_an_admin() {
        let hub = start();
        let (status, body) = call(
            &hub.url,
            "POST",
            "/api/v1/register",
            None,
            Some(json!({"name": "dana"})),
        );
        assert_eq!(status, 201, "{body}");
        let token = body["token"].as_str().unwrap().to_string();
        let (_, me) = call(&hub.url, "GET", "/api/v1/me", Some(&token), None);
        assert_eq!(me["status"], "pending");
        assert_eq!(me["permissions"], json!([]));
        let link = crate::RemoteMemory::new(&hub.url, &token, None);
        assert!(link.hello().is_err(), "the link refuses a pending user");

        let (status, _) = call(&hub.url, "GET", "/api/v1/users", Some(&hub.ana), None);
        assert_eq!(status, 403, "a member is no admin");
        let (status, users) = call(&hub.url, "GET", "/api/v1/users", Some(&hub.root), None);
        assert_eq!(status, 200);
        assert!(
            !users.to_string().contains("sha256"),
            "never a hash: {users}"
        );
        let pending: Vec<&Value> = users["users"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|u| u["status"] == "pending")
            .collect();
        assert_eq!(pending.len(), 1);
        let (_, roles) = call(&hub.url, "GET", "/api/v1/roles", Some(&hub.root), None);
        assert_eq!(roles["roles"]["reader"], json!(["recall"]));

        let (status, body) = call(
            &hub.url,
            "POST",
            "/api/v1/users/dana/approve",
            Some(&hub.root),
            Some(json!({"role": "member"})),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            crate::RemoteMemory::new(&hub.url, &token, None)
                .hello()
                .unwrap()
                .role,
            "member"
        );
        let (status, body) = call(&hub.url, "POST", "/api/v1/me/tokens", Some(&token), None);
        assert_eq!(status, 201);
        assert!(body["token"].as_str().unwrap().starts_with("ens_"));
    }

    #[test]
    fn admins_change_roles_revoke_tokens_and_turn_sign_ups_down() {
        let hub = start();
        let (status, _) = call(
            &hub.url,
            "PUT",
            "/api/v1/users/ana",
            Some(&hub.root),
            Some(json!({"role": "reader"})),
        );
        assert_eq!(status, 200);
        let (_, me) = call(&hub.url, "GET", "/api/v1/me", Some(&hub.ana), None);
        assert_eq!(me["permissions"], json!(["recall"]));
        let (status, body) = call(
            &hub.url,
            "PUT",
            "/api/v1/users/ana",
            Some(&hub.root),
            Some(json!({"role": "god"})),
        );
        assert_eq!(status, 400, "{body}");
        let (status, body) = call(
            &hub.url,
            "DELETE",
            "/api/v1/users/ana/tokens",
            Some(&hub.root),
            None,
        );
        assert_eq!((status, body["revoked"].as_u64()), (200, Some(1)));
        let (status, _) = call(&hub.url, "GET", "/api/v1/me", Some(&hub.ana), None);
        assert_eq!(status, 401);

        call(
            &hub.url,
            "POST",
            "/api/v1/register",
            None,
            Some(json!({"name": "eve"})),
        );
        let (status, _) = call(
            &hub.url,
            "DELETE",
            "/api/v1/users/eve",
            Some(&hub.root),
            None,
        );
        assert_eq!(status, 200);
        let (status, _) = call(
            &hub.url,
            "DELETE",
            "/api/v1/users/eve",
            Some(&hub.root),
            None,
        );
        assert_eq!(status, 404);
    }

    #[test]
    fn sign_ups_are_limited_and_bad_requests_are_named() {
        let hub = start();
        for name in ["a1", "a2"] {
            let (status, _) = call(
                &hub.url,
                "POST",
                "/api/v1/register",
                None,
                Some(json!({"name": name})),
            );
            assert_eq!(status, 201);
        }
        let (status, body) = call(
            &hub.url,
            "POST",
            "/api/v1/register",
            None,
            Some(json!({"name": "a3"})),
        );
        assert_eq!(status, 409, "max_pending is 2: {body}");
        let (status, _) = call(
            &hub.url,
            "POST",
            "/api/v1/register",
            None,
            Some(json!({"name": "ana"})),
        );
        assert_eq!(status, 409, "a taken name");
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
