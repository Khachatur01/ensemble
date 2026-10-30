//! The `web_fetch` tool: read a public web page or file as text.
//!
//! Only public addresses are fetched. A host that resolves to loopback, a
//! private range, link-local (cloud metadata) or anything else that is not
//! the public internet is refused, and so is every redirect that leads to
//! one, so a model cannot use this to reach the model server, the router or
//! a service on the user's machine.
//!
//! What it does not stop: the URL itself can carry what the role has read to
//! any public server. Give `fetch` to a role only as far as you would let it
//! talk to the internet.

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const MAX_BODY_BYTES: u64 = 2_000_000;

/// Fetch `url` and return its text, with HTML turned into plain text.
pub fn get(url: &str) -> Result<String, String> {
    get_with(url, &is_public)
}

/// [`get`], with the address check supplied, so tests can serve locally.
fn get_with(url: &str, allowed: &dyn Fn(IpAddr) -> bool) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        // Followed by hand below, so every hop is checked.
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        if !current.starts_with("http://") && !current.starts_with("https://") {
            let scheme = current.split(':').next().unwrap_or("");
            return Err(format!(
                "only http and https URLs can be fetched, not '{scheme}'"
            ));
        }
        let uri: ureq::http::Uri = current
            .parse()
            .map_err(|e| format!("not a valid URL ({e}): {current}"))?;
        check(&uri, allowed)?;
        let mut resp = agent
            .get(&current)
            .header(
                "accept",
                "text/html, text/plain, application/json, */*;q=0.5",
            )
            .call()
            .map_err(|e| format!("could not fetch {current}: {e}"))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{current} redirected ({status}) without a Location"))?;
            current = resolve(&uri, location)?;
            continue;
        }
        let kind = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !is_text(&kind) {
            return Err(format!(
                "{current} is {kind}, not text; this tool reads only text"
            ));
        }
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|e| format!("could not read {current}: {e}"))?;
        let text = if kind.contains("html") {
            html_to_text(&body)
        } else {
            body
        };
        if !(200..300).contains(&status) {
            return Err(format!("{current} answered {status}:\n{text}"));
        }
        return Ok(format!("[{current}]\n{text}"));
    }
    Err(format!(
        "more than {MAX_REDIRECTS} redirects; stopped at {current}"
    ))
}

fn check(uri: &ureq::http::Uri, allowed: &dyn Fn(IpAddr) -> bool) -> Result<(), String> {
    let scheme = uri.scheme_str().unwrap_or("");
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => {
            return Err(format!(
                "only http and https URLs can be fetched, not '{scheme}'"
            ));
        }
    };
    let host = uri.host().ok_or("the URL has no host")?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<IpAddr> = (bare, uri.port_u16().unwrap_or(default_port))
        .to_socket_addrs()
        .map_err(|e| format!("could not resolve {host}: {e}"))?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} has no address"));
    }
    if let Some(ip) = addrs.iter().find(|ip| !allowed(**ip)) {
        return Err(format!(
            "{host} is {ip}, which is not a public address; only the public internet can be fetched"
        ));
    }
    Ok(())
}

/// A redirect target, which may be relative to the page that sent it.
fn resolve(base: &ureq::http::Uri, location: &str) -> Result<String, String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    let scheme = base.scheme_str().unwrap_or("https");
    let authority = base.authority().map(|a| a.as_str()).unwrap_or("");
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{rest}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    let path = base.path();
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    Ok(format!("{scheme}://{authority}{dir}{location}"))
}

fn is_text(content_type: &str) -> bool {
    content_type.is_empty()
        || content_type.starts_with("text/")
        || ["json", "xml", "javascript", "yaml", "toml"]
            .iter()
            .any(|k| content_type.contains(k))
}

/// Is this an address on the public internet?
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80) // link-local
        }
    }
}

/// Elements whose content is never prose.
const SKIP: &[&str] = &["script", "style", "noscript", "svg", "template", "head"];
/// Elements that start a new line.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "br",
    "hr",
    "tr",
    "table",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "main",
    "aside",
    "pre",
    "blockquote",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "title",
    "form",
    "figure",
    "figcaption",
];

/// HTML to readable text: scripts and styles dropped, tags removed, block
/// elements on their own lines, list items marked, entities decoded.
pub fn html_to_text(html: &str) -> String {
    // ASCII lowercasing keeps every byte offset, so indices carry over.
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while i < html.len() {
        if !html[i..].starts_with('<') {
            let next = html[i..].find('<').map_or(html.len(), |j| i + j);
            let text = decode(&html[i..next]);
            let words: Vec<&str> = text.split_whitespace().collect();
            if !words.is_empty() {
                if text.starts_with(char::is_whitespace) && !out.ends_with(['\n', ' ']) {
                    out.push(' ');
                }
                out.push_str(&words.join(" "));
                if text.ends_with(char::is_whitespace) {
                    out.push(' ');
                }
            }
            i = next;
            continue;
        }
        if lower[i..].starts_with("<!--") {
            i = lower[i..].find("-->").map_or(html.len(), |j| i + j + 3);
            continue;
        }
        let Some(close) = html[i..].find('>') else {
            break;
        };
        let end = i + close + 1;
        let tag = &lower[i + 1..end - 1];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        i = end;
        if !closing && SKIP.contains(&name.as_str()) && !tag.ends_with('/') {
            let closer = format!("</{name}");
            i = match lower[i..].find(&closer) {
                Some(j) => lower[i + j..]
                    .find('>')
                    .map_or(html.len(), |k| i + j + k + 1),
                None => html.len(),
            };
            continue;
        }
        if name == "li" && !closing {
            out.push_str("\n- ");
        } else if BLOCK.contains(&name.as_str()) {
            out.push('\n');
        }
    }
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest[1..]
            .find(';')
            .filter(|&j| j <= 10)
            .map(|j| &rest[1..=j]);
        let decoded = entity.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => {
                let n = e.strip_prefix('#')?;
                let code = match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => n.parse().ok()?,
                };
                char::from_u32(code)
            }
        });
        match (entity, decoded) {
            (Some(e), Some(c)) => {
                out.push(c);
                rest = &rest[e.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn html_becomes_readable_text() {
        let html = r#"<html><head><title>T</title><style>p{}</style></head>
            <body><script>alert("x")</script><!-- note -->
            <h1>Title &amp; more</h1><p>One
            two <b>bold</b>.</p><ul><li>a</li><li>b &#x263A;</li></ul>
            <p>x&nbsp;&lt;y&gt; &unknown; &#169;</p></body></html>"#;
        assert_eq!(
            html_to_text(html),
            "Title & more\n\nOne two bold.\n\n- a\n- b ☺\n\nx <y> &unknown; ©"
        );
    }

    #[test]
    fn only_public_addresses_pass() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn local_and_non_http_urls_are_refused_before_any_request() {
        let err = get("http://127.0.0.1:11434/api/tags").unwrap_err();
        assert!(err.contains("not a public address"), "{err}");
        let err = get("http://localhost/").unwrap_err();
        assert!(err.contains("not a public address"), "{err}");
        let err = get("file:///etc/passwd").unwrap_err();
        assert!(err.contains("only http and https"), "{err}");
    }

    #[test]
    fn redirect_targets_resolve_against_the_page() {
        let base: ureq::http::Uri = "https://a.example/docs/page.html".parse().unwrap();
        assert_eq!(resolve(&base, "/x").unwrap(), "https://a.example/x");
        assert_eq!(
            resolve(&base, "y.html").unwrap(),
            "https://a.example/docs/y.html"
        );
        assert_eq!(
            resolve(&base, "//b.example/z").unwrap(),
            "https://b.example/z"
        );
        assert_eq!(
            resolve(&base, "http://c.example/").unwrap(),
            "http://c.example/"
        );
    }

    /// Serve canned responses on a local port, one per connection.
    fn serve(responses: Vec<String>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        port
    }

    fn response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn a_page_is_fetched_through_a_redirect_and_converted() {
        let port = serve(vec![
            response("302 Found", "location: /final\r\n", ""),
            response(
                "200 OK",
                "content-type: text/html; charset=utf-8\r\n",
                "<p>Hello <i>there</i></p>",
            ),
        ]);
        let out = get_with(&format!("http://127.0.0.1:{port}/start"), &|_| true).unwrap();
        assert_eq!(out, format!("[http://127.0.0.1:{port}/final]\nHello there"));
    }

    #[test]
    fn a_redirect_to_a_local_address_is_refused() {
        let port = serve(vec![response(
            "302 Found",
            "location: http://127.0.0.1:9/secret\r\n",
            "",
        )]);
        let first = std::cell::Cell::new(true);
        let err = get_with(&format!("http://127.0.0.1:{port}/"), &|_| {
            first.replace(false)
        })
        .unwrap_err();
        assert!(err.contains("not a public address"), "{err}");
    }

    #[test]
    fn binary_content_and_error_statuses_are_reported() {
        let port = serve(vec![
            response("200 OK", "content-type: image/png\r\n", "PNG"),
            response("404 Not Found", "content-type: text/plain\r\n", "gone"),
        ]);
        let url = format!("http://127.0.0.1:{port}/");
        assert!(get_with(&url, &|_| true).unwrap_err().contains("image/png"));
        let err = get_with(&url, &|_| true).unwrap_err();
        assert!(err.contains("404") && err.contains("gone"), "{err}");
    }
}
