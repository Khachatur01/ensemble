//! The hub's web UI: the files of a single-page app, served at `/` on the
//! hub's port beside the REST API it calls, so the two share an origin and
//! the browser's session cookie. The files are the ones embedded when the
//! hub was built ([`FILES`]), or, while the UI is worked on, those of a
//! folder (`ensemble-hub serve --ui-dir`). A path that names no file and
//! looks like none is one of the app's own pages: it gets `index.html`.

use crate::http::{self, Head};
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use tokio::net::TcpStream;

include!(concat!(env!("OUT_DIR"), "/ui_files.rs"));

/// What a page of the UI may load and reach: its own files and the hub,
/// nothing of another origin, and no framing by another site.
const POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// What `/` says when the hub was built without its UI.
const NO_UI: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>Ensemble hub</title></head><body><h1>Ensemble hub</h1>\
<p>This hub was built without its web UI. Its REST API is at \
<a href=\"/api/v1/openapi.json\">/api/v1/openapi.json</a>.</p></body></html>";

/// Where the UI's files come from.
pub enum Ui {
    /// Those embedded in the executable.
    Embedded,
    /// Those of a folder, read for every request.
    Folder(PathBuf),
}

impl Ui {
    /// The file at `path`, under the UI's root.
    fn file(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        match self {
            Ui::Embedded => FILES
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, bytes)| Cow::Borrowed(*bytes)),
            Ui::Folder(dir) => std::fs::read(dir.join(path)).ok().map(Cow::Owned),
        }
    }

    /// The status, type and bytes that answer `GET path`.
    fn answer(&self, path: &str) -> (u16, &'static str, Cow<'static, [u8]>) {
        let Some(path) = relative(path) else {
            return (404, "text/plain", Cow::Borrowed(b"no such file".as_slice()));
        };
        if let Some(bytes) = self.file(&path) {
            return (200, content_type(&path), bytes);
        }
        if is_asset(&path) && path != "index.html" {
            return (404, "text/plain", Cow::Borrowed(b"no such file".as_slice()));
        }
        match self.file("index.html") {
            Some(bytes) => (200, content_type("index.html"), bytes),
            None => (
                200,
                content_type("index.html"),
                Cow::Borrowed(NO_UI.as_bytes()),
            ),
        }
    }

    /// Answer `head`, a GET or a HEAD of a path outside the API, on `stream`.
    /// A browser keeps each file and asks again with its [`tag`], answered
    /// with 304 while the file is the same.
    pub(crate) async fn serve(&self, mut stream: TcpStream, head: &Head) -> Result<(), String> {
        let (status, kind, bytes) = self.answer(&head.path);
        let etag = tag(&bytes);
        let same = status == 200 && head.header("if-none-match") == Some(etag.as_str());
        let (status, bytes): (u16, &[u8]) = match (same, head.method.as_str()) {
            (true, _) => (304, &[]),
            (false, "HEAD") => (status, &[]),
            (false, _) => (status, &bytes),
        };
        http::respond_bytes(
            &mut stream,
            status,
            kind,
            &[
                ("content-security-policy", POLICY),
                ("x-content-type-options", "nosniff"),
                ("referrer-policy", "no-referrer"),
                ("cache-control", "no-cache"),
                ("etag", &etag),
            ],
            bytes,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

/// The entity tag of a file: a hash of its bytes.
fn tag(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let hex: String = digest.as_ref()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("\"{hex}\"")
}

/// `path` under the UI's root, `index.html` for `/`; none when it would
/// leave the root.
fn relative(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Some("index.html".into());
    }
    let only_names = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (only_names && !path.contains('\\')).then(|| path.to_string())
}

/// Does `path` name one of the app's files, by its extension, rather than one
/// of its pages, which may have a dot too, such as `/settings/backend.add`?
fn is_asset(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or("");
    last.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension,
            "html"
                | "js"
                | "mjs"
                | "wasm"
                | "css"
                | "json"
                | "map"
                | "svg"
                | "png"
                | "ico"
                | "woff2"
                | "txt"
        )
    })
}

/// The media type of the file at `path`, by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn the_ui_answers_its_files_its_pages_with_index_html_and_nothing_outside_it() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("dist");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<html>app</html>").unwrap();
        std::fs::write(dir.join("assets/app.wasm"), b"\0asm").unwrap();
        std::fs::write(tmp.0.join("secret"), "no").unwrap();
        let ui = Ui::Folder(dir);
        let answer = |path: &str| {
            let (status, kind, bytes) = ui.answer(path);
            (status, kind, String::from_utf8_lossy(&bytes).into_owned())
        };
        assert_eq!(answer("/").2, "<html>app</html>");
        assert_eq!(
            answer("/assets/app.wasm"),
            (200, "application/wasm", "\0asm".into())
        );
        assert_eq!(answer("/tasks/shop/7").2, "<html>app</html>", "a page");
        assert_eq!(
            answer("/settings/backend.add").2,
            "<html>app</html>",
            "a dot"
        );
        assert_eq!(answer("/assets/gone.js").0, 404, "a file it lacks");
        assert_eq!(answer("/../secret").0, 404);
        assert_eq!(answer("/assets/../../secret").0, 404);
    }

    #[test]
    fn a_hub_built_without_its_ui_says_so() {
        let ui = Ui::Folder(TempDir::new().0.join("none"));
        let (status, kind, bytes) = ui.answer("/");
        assert_eq!((status, kind), (200, "text/html; charset=utf-8"));
        assert!(String::from_utf8_lossy(&bytes).contains("without its web UI"));
    }
}
