// isapi.rs — minimal Hikvision ISAPI client: one digest-authenticated GET
// (URLSession did the digest dance for free on the Mac; ureq needs the
// explicit 401 round trip) plus the read-only probes built on it.

use std::io::Read;

/// GET `path` on the camera with a 6 s timeout, like the Mac app. Answers
/// only 200 bodies; anything else (unreachable, bad credentials) is None.
pub fn get(host: &str, user: &str, password: &str, path: &str) -> Option<Vec<u8>> {
    let url = format!("http://{host}{path}");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(6))
        .build();
    let resp = match agent.get(&url).call() {
        Ok(r) => r, // camera without auth on this endpoint — take it
        Err(ureq::Error::Status(401, r)) => {
            let www = r.header("www-authenticate")?.to_string();
            let mut prompt = digest_auth::parse(&www).ok()?;
            let ctx = digest_auth::AuthContext::new(user, password, path);
            let answer = prompt.respond(&ctx).ok()?.to_header_string();
            agent.get(&url).set("Authorization", &answer).call().ok()?
        }
        Err(_) => return None,
    };
    let mut body = Vec::new();
    resp.into_reader()
        .take(20 << 20)
        .read_to_end(&mut body)
        .ok()?;
    Some(body)
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

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{name}>"))?;
    Some(&xml[start..end])
}
