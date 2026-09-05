// session.rs — persisted "pick up where you left off" state: which view was
// open (grid or a camera) and, per camera, live-vs-playback and the last
// playback position. Port of SessionStore.swift, same file shape (camelCase
// keys, dates as seconds since 2001 the way Foundation's JSONEncoder writes
// them) so a Mac and this app can share state.json on macOS. UI state only,
// no credentials, not part of export/import. Written eagerly on every
// transition — quitting is an instant exit(0), so there is no save-on-exit
// moment beyond the one the close path makes explicitly.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    #[default]
    Grid,
    Camera,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Live,
    Playback,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct CameraViewState {
    pub mode: Mode,
    /// Kept even in live mode — P resumes playback from here. Seconds since
    /// 2001-01-01 UTC (Foundation's reference date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_position: Option<f64>,
    /// Supplementary panes (not ported yet) — carried through for the Mac.
    #[serde(default)]
    pub panes_visible: bool,
}

impl CameraViewState {
    pub fn position(&self) -> Option<DateTime<Utc>> {
        self.playback_position.map(from_apple)
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(default)]
    pub location: Location,
    #[serde(default)]
    pub camera_host: Option<String>,
    #[serde(default)]
    pub per_camera: HashMap<String, CameraViewState>,
}

/// Foundation reference date: 2001-01-01T00:00:00Z as Unix seconds.
const APPLE_EPOCH: f64 = 978_307_200.0;

pub fn to_apple(t: DateTime<Utc>) -> f64 {
    t.timestamp() as f64 + f64::from(t.timestamp_subsec_millis()) / 1000.0 - APPLE_EPOCH
}

fn from_apple(s: f64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(((s + APPLE_EPOCH) * 1000.0) as i64).unwrap_or_default()
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

/// Read-modify-write the file (SessionStore.update).
pub fn update(mutate: impl FnOnce(&mut SessionState)) {
    let mut state = load();
    mutate(&mut state);
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(data) = serde_json::to_vec_pretty(&state) {
        let _ = std::fs::write(p, data);
    }
}

pub fn save(location: Location, camera_host: Option<&str>) {
    update(|s| {
        s.location = location;
        s.camera_host = camera_host.map(str::to_string);
    });
}
