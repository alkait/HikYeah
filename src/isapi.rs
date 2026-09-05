// isapi.rs — minimal Hikvision ISAPI client: one digest-authenticated
// request (URLSession did the digest dance for free on the Mac; ureq needs
// the explicit 401 round trip) plus the read-only probes built on it.

use std::io::Read;
use std::time::Duration;

/// GET `path` on the camera with a 6 s timeout, like the Mac app. Answers
/// only 200 bodies; anything else (unreachable, bad credentials) is None.
pub fn get(host: &str, user: &str, password: &str, path: &str) -> Option<Vec<u8>> {
    request(host, user, password, path, None, Duration::from_secs(6))
}

/// One ISAPI round trip: a GET, or a POST when `body` carries a content
/// type and payload. Answers only 200 bodies (up to 20 MB); None otherwise.
pub fn request(
    host: &str,
    user: &str,
    password: &str,
    path: &str,
    body: Option<(&str, &[u8])>,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let url = format!("http://{host}{path}");
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let send = |auth: Option<&str>| {
        let mut req = match body {
            Some((ct, _)) => agent.post(&url).set("Content-Type", ct),
            None => agent.get(&url),
        };
        if let Some(a) = auth {
            req = req.set("Authorization", a);
        }
        match body {
            Some((_, bytes)) => req.send_bytes(bytes),
            None => req.call(),
        }
        .map_err(Box::new)
    };
    let resp = match send(None) {
        Ok(r) => r, // device without auth on this endpoint — take it
        Err(e) if matches!(*e, ureq::Error::Status(401, _)) => {
            let ureq::Error::Status(_, r) = *e else {
                unreachable!()
            };
            let www = r.header("www-authenticate")?.to_string();
            let mut prompt = digest_auth::parse(&www).ok()?;
            let method = if body.is_some() {
                digest_auth::HttpMethod::POST
            } else {
                digest_auth::HttpMethod::GET
            };
            let ctx = digest_auth::AuthContext::new_with_method(
                user,
                password,
                path,
                None::<&[u8]>,
                method,
            );
            let answer = prompt.respond(&ctx).ok()?.to_header_string();
            send(Some(&answer)).ok()?
        }
        Err(_) => return None,
    };
    let mut out = Vec::new();
    resp.into_reader()
        .take(20 << 20)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Main-stream channel name and codec ("hevc"/"h264") from the camera
/// itself — the camera editor's Detect buttons (ISAPI.detectChannel port).
/// Both None when the camera can't be reached with these credentials.
pub fn detect_channel(host: &str, user: &str, password: &str) -> (Option<String>, Option<String>) {
    let Some(body) = get(host, user, password, "/ISAPI/Streaming/channels/101") else {
        return (None, None);
    };
    let xml = String::from_utf8_lossy(&body);
    let name = tag(&xml, "channelName")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let codec = tag(&xml, "videoCodecType").and_then(|t| {
        if t.starts_with("H.265") {
            Some("hevc".to_string())
        } else if t.starts_with("H.264") {
            Some("h264".to_string())
        } else {
            None
        }
    });
    (name, codec)
}

/// Text of the first `<name>…</name>` in `xml`.
pub fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{name}>"))?;
    Some(&xml[start..end])
}

/// The inner text of every `<name>…</name>` block, in document order.
pub fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        out.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    out
}
