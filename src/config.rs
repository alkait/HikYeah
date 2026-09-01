// config.rs — camera models + on-disk config, format-compatible with the
// macOS HikViewer's export (~/Library/Application Support/hikviewer/config.json).
// On Linux it lives at ~/.config/hikviewer/config.json, so a File > Export
// from the Mac app can be dropped there unchanged.

use serde::Deserialize;

#[derive(Deserialize, Clone)]
pub struct StoredCamera {
    pub host: String,
    #[serde(default)]
    pub name: String,
    pub user: String,
    pub port: u16,
    #[serde(default)]
    #[allow(dead_code)] // kept for Mac-app config compatibility; decode is codec-agnostic
    pub codec: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct StoredConfig {
    pub cameras: Vec<StoredCamera>,
}

pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME not set");
            std::path::PathBuf::from(home).join(".config")
        });
    base.join("hikviewer/config.json")
}

/// Current format ({cameras, nvr}) or the pre-playback bare camera array.
pub fn load() -> Option<StoredConfig> {
    let data = std::fs::read(config_path()).ok()?;
    if let Ok(cfg) = serde_json::from_slice::<StoredConfig>(&data) {
        return Some(cfg);
    }
    serde_json::from_slice::<Vec<StoredCamera>>(&data)
        .ok()
        .map(|cameras| StoredConfig { cameras })
}

/// Percent-encode everything outside RFC 3986 unreserved (matches the Swift
/// urlEncode — credentials with @ : / etc. survive the URL).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[allow(dead_code)]
pub const SUB_CHANNEL: &str = "102"; // grid feed (substream)
pub const MAIN_CHANNEL: &str = "101"; // focused-tile feed (main stream)

pub fn rtsp_url(cam: &StoredCamera, channel: &str) -> String {
    format!(
        "rtsp://{}:{}@{}:{}/Streaming/Channels/{}",
        url_encode(&cam.user),
        url_encode(&cam.password),
        cam.host,
        cam.port,
        channel
    )
}
