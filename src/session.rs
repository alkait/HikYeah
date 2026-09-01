// session.rs — persisted "pick up where you left off" state: which view was
// open (grid or a camera). Port of SessionStore.swift; playback position and
// pane state join it when those features land. UI state only, no credentials,
// not part of export/import. Written eagerly on every transition — quitting
// is an instant exit(0), so there is no save-on-exit moment.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct SessionState {
    /// "grid" or "camera".
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub camera_host: Option<String>,
}

fn path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("state.json")
}

pub fn load() -> SessionState {
    std::fs::read(path())
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

pub fn save(location: &str, camera_host: Option<&str>) {
    let state = SessionState {
        location: location.to_string(),
        camera_host: camera_host.map(str::to_string),
    };
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(data) = serde_json::to_vec_pretty(&state) {
        let _ = std::fs::write(p, data);
    }
}
