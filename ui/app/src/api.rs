//! The hub's REST API, as the page calls it: same-origin requests, signed in
//! by the session cookie the browser keeps and no script reads. The browser
//! names the page's origin on every request that changes anything, which
//! the hub checks.

use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestCredentials, RequestInit, Response};

/// Why a request failed: the hub's error, or the network's.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// The HTTP status, 0 when no answer came.
    pub status: u16,
    pub message: String,
    /// The input field it is about, if any.
    pub field: Option<String>,
}

impl Failure {
    fn broken(e: JsValue) -> Self {
        Failure {
            status: 0,
            message: e
                .as_string()
                .unwrap_or_else(|| "the hub could not be reached".into()),
            field: None,
        }
    }

    /// Is it that no one is signed in?
    pub fn signed_out(&self) -> bool {
        self.status == 401
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Ask the hub `method path`, with `body` as JSON: its answer, or why not.
pub async fn call(method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
    let init = RequestInit::new();
    init.set_method(method);
    init.set_credentials(RequestCredentials::SameOrigin);
    let headers = Headers::new().map_err(Failure::broken)?;
    headers
        .set("content-type", "application/json")
        .map_err(Failure::broken)?;
    init.set_headers(&headers);
    if let Some(body) = body {
        init.set_body(&JsValue::from_str(&body.to_string()));
    }
    let request = Request::new_with_str_and_init(path, &init).map_err(Failure::broken)?;
    let window = web_sys::window().ok_or_else(|| Failure::broken(JsValue::NULL))?;
    let response: Response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(Failure::broken)?
        .dyn_into()
        .map_err(Failure::broken)?;
    let text = JsFuture::from(response.text().map_err(Failure::broken)?)
        .await
        .map_err(Failure::broken)?
        .as_string()
        .unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let status = response.status();
    if (200..300).contains(&status) {
        Ok(value)
    } else {
        Err(Failure {
            status,
            message: value["error"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("the hub answered {status}")),
            field: value["field"].as_str().map(String::from),
        })
    }
}

pub async fn get(path: &str) -> Result<Value, Failure> {
    call("GET", path, None).await
}

/// Run the hub's operation `name` with `input`.
pub async fn op(name: &str, input: Value) -> Result<Value, Failure> {
    call("POST", &format!("/api/v1/ops/{name}"), Some(&input)).await
}

/// Sign in with a password: the hub keeps the session in its cookie.
pub async fn sign_in(name: &str, password: &str) -> Result<Value, Failure> {
    let body = json!({ "name": name, "password": password });
    call("POST", "/api/v1/session", Some(&body)).await
}

/// End the session, and have the hub clear its cookie.
pub async fn sign_out() -> Result<Value, Failure> {
    call("DELETE", "/api/v1/session", None).await
}

/// Who is signed in: their account and what they may do.
pub async fn me() -> Result<Value, Failure> {
    get("/api/v1/me").await
}
