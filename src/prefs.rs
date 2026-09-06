// prefs.rs — app preferences: which device decodes and which GPU renders.
// Deliberately separate from the camera config (config.rs) so a config
// export never carries one machine's hardware choices to another.
// No auto-detection by design: the user picks from explicit toggles.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Serialize, Deserialize, Clone)]
pub struct Prefs {
    /// Decode option id ("cpu", "cuda", …); empty = cpu.
    #[serde(default)]
    pub decode: String,
    /// Render adapter name as reported by wgpu; None = wgpu's default pick.
    #[serde(default)]
    pub render_adapter: Option<String>,
    /// Smooth live video (~0.2 s buffer absorbing delivery jitter).
    #[serde(default = "default_true")]
    pub smooth_live: bool,
    /// Open full screen at launch (skipped while unconfigured, so the
    /// first-run Settings window isn't buried).
    #[serde(default = "default_true")]
    pub start_fullscreen: bool,
    /// Reopen where the user left off. Gates restoring only — state is
    /// always recorded, so switching it back on picks up the current
    /// session, not a stale one.
    #[serde(default = "default_true")]
    pub remember_last_view: bool,
    /// Nerd-stats panel visibility ("I" toggles it; survives relaunch).
    #[serde(default)]
    pub nerd_stats: bool,
    /// Where the panel was last dragged to (window points).
    #[serde(default)]
    pub nerd_pos: Option<[f32; 2]>,
    /// Nerd-stats overlay boxes: draw the camera's motion areas / intrusion
    /// zones on its video (display only).
    #[serde(default)]
    pub nerd_draw_motion: bool,
    #[serde(default)]
    pub nerd_draw_zones: bool,
    /// Folder for snapshots and clips; None = the desktop (media.rs).
    #[serde(default)]
    pub save_dir: Option<String>,
    /// Playback speed (1/2/4×) — one preference shared across cameras.
    #[serde(default = "default_speed")]
    pub playback_speed: u32,
    /// Which event band the playback timeline highlights: none, motion,
    /// intrusion (one global choice, like speed).
    #[serde(default = "default_band")]
    pub event_band: String,
    /// Motion refinement: any of "human", "vehicle" (both on by default →
    /// AcuSense search; neither → the alarm log).
    #[serde(default = "default_filter")]
    pub motion_filter: Vec<String>,
}

fn default_band() -> String {
    "motion".into()
}

fn default_filter() -> Vec<String> {
    vec!["human".into(), "vehicle".into()]
}

fn default_speed() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            decode: String::new(),
            render_adapter: None,
            smooth_live: true,
            start_fullscreen: true,
            remember_last_view: true,
            nerd_stats: false,
            nerd_pos: None,
            nerd_draw_motion: false,
            nerd_draw_zones: false,
            save_dir: None,
            playback_speed: 1,
            event_band: default_band(),
            motion_filter: default_filter(),
        }
    }
}

pub struct DecodeOption {
    pub id: &'static str,
    pub label: &'static str,
    /// Value for ffmpeg's -hwaccel; None = software decode.
    pub hwaccel: Option<&'static str>,
}

/// Platform-appropriate decode choices. Listed, not probed — a wrong pick
/// shows up as a stream error status and the user flips it back.
pub fn decode_options() -> &'static [DecodeOption] {
    #[cfg(target_os = "linux")]
    return &[
        DecodeOption {
            id: "cpu",
            label: "CPU (software)",
            hwaccel: None,
        },
        DecodeOption {
            id: "cuda",
            label: "NVDEC (NVIDIA)",
            hwaccel: Some("cuda"),
        },
        DecodeOption {
            id: "qsv",
            label: "Quick Sync (Intel)",
            hwaccel: Some("qsv"),
        },
        DecodeOption {
            id: "vaapi",
            label: "VAAPI (Intel/AMD)",
            hwaccel: Some("vaapi"),
        },
    ];
    #[cfg(target_os = "macos")]
    return &[
        DecodeOption {
            id: "cpu",
            label: "CPU (software)",
            hwaccel: None,
        },
        DecodeOption {
            id: "videotoolbox",
            label: "VideoToolbox",
            hwaccel: Some("videotoolbox"),
        },
    ];
    #[cfg(target_os = "windows")]
    return &[
        DecodeOption {
            id: "cpu",
            label: "CPU (software)",
            hwaccel: None,
        },
        DecodeOption {
            id: "d3d11va",
            label: "Direct3D 11 VA",
            hwaccel: Some("d3d11va"),
        },
        DecodeOption {
            id: "cuda",
            label: "NVDEC (NVIDIA)",
            hwaccel: Some("cuda"),
        },
        DecodeOption {
            id: "qsv",
            label: "Quick Sync (Intel)",
            hwaccel: Some("qsv"),
        },
    ];
}

/// Decode option ids that passed the startup probe. None until probing
/// finishes (or if it couldn't run — then the full list is shown).
static PROBED: OnceLock<Option<Vec<&'static str>>> = OnceLock::new();

/// Probe each hardware decode option in the background: init its device and
/// actually decode a tiny generated H.264 clip. Filters the Settings menu to
/// what this machine really has; a probe can't run → menu stays unfiltered.
pub fn start_probe() {
    std::thread::spawn(|| {
        let result = probe_sample().map(|sample| {
            let handles: Vec<_> = decode_options()
                .iter()
                .filter_map(|o| o.hwaccel)
                .map(|hw| {
                    let sample = sample.clone();
                    std::thread::spawn(move || {
                        // A broken driver can hang device init forever (seen
                        // with vaapi on hybrid NVIDIA setups) — kill after 5 s.
                        let ok = std::process::Command::new(crate::stream::ffmpeg_path())
                            .args(["-hide_banner", "-v", "error", "-nostdin"])
                            .args(["-init_hw_device", &format!("{hw}=probe")])
                            .args(["-hwaccel", hw, "-i"])
                            .arg(&sample)
                            .args(["-f", "null", "-"])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .ok()
                            .is_some_and(|mut child| {
                                let deadline =
                                    std::time::Instant::now() + std::time::Duration::from_secs(5);
                                loop {
                                    match child.try_wait() {
                                        Ok(Some(status)) => return status.success(),
                                        Ok(None) if std::time::Instant::now() < deadline => {
                                            std::thread::sleep(std::time::Duration::from_millis(
                                                100,
                                            ));
                                        }
                                        _ => {
                                            let _ = child.kill();
                                            let _ = child.wait();
                                            return false;
                                        }
                                    }
                                }
                            });
                        (hw, ok)
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .filter_map(|(hw, ok)| ok.then_some(hw))
                .collect::<Vec<_>>()
        });
        let _ = PROBED.set(result);
    });
}

/// One-second 320×240 H.264 clip for probing, cached next to the snapshots.
fn probe_sample() -> Option<std::path::PathBuf> {
    let dir = crate::snapshot::cache_dir();
    let path = dir.join("probe.mp4");
    if path.exists() {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let ok = std::process::Command::new(crate::stream::ffmpeg_path())
        .args(["-hide_banner", "-v", "error", "-nostdin"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25",
            "-t",
            "1",
        ])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-y"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(path)
}

/// Decode options to show in Settings: CPU always, hardware entries filtered
/// by the probe once it has finished.
pub fn available_decode_options() -> Vec<&'static DecodeOption> {
    let probed = PROBED.get().and_then(|p| p.as_ref());
    decode_options()
        .iter()
        .filter(|o| match (o.hwaccel, probed) {
            (None, _) => true,       // CPU
            (Some(_), None) => true, // probe pending/unavailable: show all
            (Some(hw), Some(ids)) => ids.contains(&hw),
        })
        .collect()
}

impl Prefs {
    pub fn hwaccel(&self) -> Option<&'static str> {
        decode_options()
            .iter()
            .find(|o| o.id == self.decode)
            .and_then(|o| o.hwaccel)
    }

    pub fn decode_label(&self) -> &'static str {
        decode_options()
            .iter()
            .find(|o| o.id == self.decode)
            .map_or("CPU (software)", |o| o.label)
    }

    fn path() -> std::path::PathBuf {
        crate::config::config_path().with_file_name("prefs.json")
    }

    pub fn load() -> Prefs {
        let mut p: Prefs = std::fs::read(Self::path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        if ![1, 2, 4].contains(&p.playback_speed) {
            p.playback_speed = 1;
        }
        p
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }
}
