// config.rs — camera models + on-disk config, format-compatible with the
// macOS HikViewer's file (~/Library/Application Support/hikviewer/config.json,
// which is also what its File > Export writes). Linux keeps it under
// ~/.config/hikviewer, Windows under %APPDATA%\hikviewer; on macOS it is the
// Mac app's own file, so both apps share one camera list.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct StoredCamera {
    pub host: String,
    #[serde(default)]
    pub name: String,
    pub user: String,
    pub port: u16,
    /// "hevc" / "h264". Decode here is codec-agnostic; kept (and editable)
    /// so a config written by HikYeah still tells the Mac app which raw
    /// muxer to use.
    #[serde(default = "default_codec")]
    pub codec: String,
    pub password: String,
}

fn default_codec() -> String {
    "hevc".into()
}

impl StoredCamera {
    pub fn codec_label(&self) -> &'static str {
        if self.codec == "h264" {
            "H.264"
        } else {
            "HEVC"
        }
    }
}

/// NVR credentials (recordings live there). Playback isn't ported yet —
/// preserved so a Mac export survives a round trip through the editor.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct StoredNvr {
    pub host: String,
    pub user: String,
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct StoredConfig {
    pub cameras: Vec<StoredCamera>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvr: Option<StoredNvr>,
}

pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} not set")))
}

pub fn config_path() -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"))
    };
    dir.join("hikviewer/config.json")
}

/// Current format ({cameras, nvr}) or the pre-playback bare camera array.
/// Cameras without a host are skipped (the Mac app does the same).
pub fn load() -> Option<StoredConfig> {
    let data = std::fs::read(config_path()).ok()?;
    let mut cfg = serde_json::from_slice::<StoredConfig>(&data)
        .ok()
        .or_else(|| {
            serde_json::from_slice::<Vec<StoredCamera>>(&data)
                .ok()
                .map(|cameras| StoredConfig { cameras, nvr: None })
        })?;
    cfg.cameras.retain(|c| !c.host.is_empty());
    Some(cfg)
}

/// Atomic write, owner-only: the file holds passwords in clear (see the Mac
/// app's Keychain rationale — a 0600 file survives rebuilds, the Keychain
/// re-prompts after each).
pub fn save(cfg: &StoredConfig) -> std::io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_vec_pretty(cfg).expect("config serializes");
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&tmp)?.write_all(&data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)
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
