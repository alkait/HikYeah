// stats.rs — the nerd-stats panel (NerdStats.swift port): live diagnostics
// for the focused camera or the grid's selected tile. The stream pushes one
// sample per frame; every window and system probe here runs only while the
// panel is open, twice a second. Bitrate and GOP rows are absent: ffmpeg
// hands us decoded frames, so the compressed stream never passes through.

use crate::{App, NvrState, stream, tile, zones};
use eframe::egui;
use std::time::{Duration, Instant};

const AMBER: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);
const RED: egui::Color32 = egui::Color32::from_rgb(255, 90, 80);
const REFRESH: Duration = Duration::from_millis(500);

/// One aggregation tick, kept for windowed deltas ("last 60 s").
struct Tick {
    t: Instant,
    frames: u64,
    late: u64,
    reanchors: u32,
    app_cpu: f64,
    ff_cpu: f64,
}

struct Seg {
    text: String,
    color: egui::Color32,
}

struct Row {
    key: &'static str,
    tip: &'static str,
    segs: Vec<Seg>,
}

#[derive(Default)]
pub struct NerdStats {
    title: String,
    rows: Vec<Row>,
    history: Vec<Tick>,
    /// Which stream the history belongs to; a switch resets it.
    target: Option<u64>,
    peak_fps: f32,
    last_refresh: Option<Instant>,
    /// Panel position seen last frame and whether it still needs saving.
    pos_dirty: bool,
    /// The target camera's configured areas, for the draw boxes + overlay.
    motion_regions: Vec<zones::Poly>,
    intrusion_regions: Vec<zones::Poly>,
}

fn seg(text: impl Into<String>, color: egui::Color32) -> Seg {
    Seg {
        text: text.into(),
        color,
    }
}

fn dash() -> Vec<Seg> {
    vec![seg("—", tile::DIM)]
}

/// "on · 24/7 · 2 zones", "off", "…" while loading, "n/a" when the camera
/// doesn't support the event or the config couldn't be read.
fn event_row(state: zones::State, schedule: Option<&str>, detail: Option<&str>) -> Vec<Seg> {
    match state {
        zones::State::Loading => vec![seg("…", tile::DIM)],
        zones::State::Unknown => vec![seg("n/a", tile::DIM)],
        zones::State::Off => vec![seg("off", tile::DIM)],
        zones::State::On => {
            let mut segs = vec![
                seg("on", tile::WHITE),
                seg(format!(" · {}", schedule.unwrap_or("…")), tile::DIM),
            ];
            if let Some(d) = detail {
                segs.push(seg(format!(" · {d}"), tile::DIM));
            }
            segs
        }
    }
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s ago")
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else {
        format!("{} h ago", s / 3600)
    }
}

/// Seconds of CPU time a process has consumed (user + system).
fn cpu_seconds(pid: Option<u32>) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let path = match pid {
            Some(p) => format!("/proc/{p}/stat"),
            None => "/proc/self/stat".into(),
        };
        let stat = std::fs::read_to_string(path).ok()?;
        // Fields after the parenthesized comm: utime and stime are 14th/15th.
        let rest = &stat[stat.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        Some(ticks / hz)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Wi-Fi signal from /proc/net/wireless: (interface, dBm). None when wired.
fn wifi() -> Option<(String, i32)> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/net/wireless").ok()?;
        for line in text.lines().skip(2) {
            let (iface, rest) = line.trim().split_once(':')?;
            let f: Vec<&str> = rest.split_whitespace().collect();
            let level = f.get(2)?.trim_end_matches('.').parse::<f32>().ok()?;
            if level != 0.0 {
                return Some((iface.to_string(), level as i32));
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// What the panel describes: a camera and one of its streams.
pub struct Target<'a> {
    pub id: u64,
    pub name: &'a str,
    pub codec: &'a str,
    pub channel: &'static str,
    pub shared: &'a stream::Shared,
}

impl NerdStats {
    fn refresh(&mut self, target: &Target, decode: &str, smooth: bool, events: zones::Info) {
        let now = Instant::now();
        if self.target != Some(target.id) {
            self.target = Some(target.id);
            self.history.clear();
            self.peak_fps = 0.0;
        }
        let st = target.shared.stats.lock().unwrap().clone();
        let app_cpu = cpu_seconds(None).unwrap_or(0.0);
        let ff_cpu = st.tid.and_then(|t| cpu_seconds(Some(t))).unwrap_or(0.0);
        self.history.push(Tick {
            t: now,
            frames: st.frames,
            late: st.late,
            reanchors: st.reanchors,
            app_cpu,
            ff_cpu,
        });
        if self.history.len() > 150 {
            self.history.drain(..self.history.len() - 150);
        }
        // Newest tick at least `secs` old; a short history falls back to the
        // oldest (windows then mean "since the panel opened").
        let tick_before = |secs: f32| -> Option<&Tick> {
            let first = self.history.first()?;
            Some(
                self.history
                    .iter()
                    .rev()
                    .find(|t| now.duration_since(t.t).as_secs_f32() >= secs)
                    .unwrap_or(first),
            )
        };

        self.title = format!("NERD STATS — {}", target.name);
        let mut rows: Vec<Row> = Vec::new();
        let mut push = |key, tip, segs| rows.push(Row { key, tip, segs });

        // stream
        let dims = tile::frame_dims(target.shared);
        push(
            "stream",
            "What you're receiving and how it travels: codec, frame size, RTSP channel (101 = main stream, 102 = grid substream), transported over TCP through an ffmpeg pipe. The focused view should show the main stream's full resolution.",
            match dims {
                Some(d) => vec![
                    seg(format!("{} {}×{}", target.codec, d.x, d.y), tile::WHITE),
                    seg(
                        format!(" · ch {} · TCP · ffmpeg", target.channel),
                        tile::DIM,
                    ),
                ],
                None => vec![
                    seg(format!("{} ", target.codec), tile::WHITE),
                    seg(
                        format!("awaiting stream · ch {}", target.channel),
                        tile::DIM,
                    ),
                ],
            },
        );
        // decode
        let software = decode.starts_with("CPU");
        push(
            "decode",
            "The decode device chosen in Settings. \"CPU (software)\" means the processor does the decompression — heavy for high-resolution HEVC and the usual cause of stutter; expect high app-side CPU below. Pick a hardware option there if the probe found one.",
            vec![seg(
                format!("{decode}{}", if software { " ⚠" } else { "" }),
                if software { AMBER } else { tile::WHITE },
            )],
        );

        // fps (5 s window of samples)
        let recent: Vec<&stream::Sample> = st
            .samples
            .iter()
            .filter(|s| now.duration_since(s.t) <= Duration::from_secs(5))
            .collect();
        let mut fps = 0.0f32;
        if recent.len() >= 2 {
            let span = recent
                .last()
                .unwrap()
                .t
                .duration_since(recent[0].t)
                .as_secs_f32();
            if span > 0.0 {
                fps = (recent.len() - 1) as f32 / span;
            }
        }
        self.peak_fps = fps.max(self.peak_fps * 0.995);
        push(
            "fps",
            "Frames actually arriving per second — not the camera's configured rate. Steady fps with a stuttery picture → look at jitter and decode. Sagging fps → the network or camera isn't delivering. Cameras also lower fps at night on purpose (longer exposure).",
            if fps > 0.0 {
                let c = if fps < self.peak_fps * 0.6 {
                    RED
                } else if fps < self.peak_fps * 0.85 {
                    AMBER
                } else {
                    tile::WHITE
                };
                vec![seg(format!("{fps:.1}"), c)]
            } else {
                dash()
            },
        );

        // jitter (10 s window)
        let gaps: Vec<f32> = st
            .samples
            .iter()
            .filter(|s| now.duration_since(s.t) <= Duration::from_secs(10) && s.gap > 0.0)
            .map(|s| s.gap)
            .collect();
        push(
            "jitter",
            "How unevenly frames arrive (last 10 s). σ is the typical wobble — a few ms on Ethernet, 10–30 on healthy Wi-Fi. max is the single worst gap: if it exceeds the smoothing buffer (200 ms), that spike was visible. High σ = constant congestion; low σ with occasional huge max = intermittent interference.",
            if gaps.len() >= 5 {
                let mean = gaps.iter().sum::<f32>() / gaps.len() as f32;
                let var =
                    gaps.iter().map(|g| (g - mean) * (g - mean)).sum::<f32>() / gaps.len() as f32;
                let sigma = var.sqrt() * 1000.0;
                let max = gaps.iter().cloned().fold(0.0f32, f32::max) * 1000.0;
                let mc = if max > 200.0 {
                    RED
                } else if max > 150.0 {
                    AMBER
                } else {
                    tile::WHITE
                };
                vec![
                    seg(format!("σ {sigma:.0} ms · "), tile::WHITE),
                    seg(format!("max {max:.0} ms"), mc),
                ]
            } else {
                dash()
            },
        );

        // stalls / reconnects
        let mut segs = vec![seg(
            format!("{} stalls · {} reconnects", st.stalls, st.reconnects),
            tile::WHITE,
        )];
        if let Some(last) = st.last_reconnect {
            let d = now.duration_since(last);
            let c = if d.as_secs() < 60 {
                RED
            } else if d.as_secs() < 3600 {
                AMBER
            } else {
                tile::DIM
            };
            segs.push(seg(format!(" · last {}", ago(d)), c));
        }
        push(
            "stalls",
            "Times this stream died and was restarted: stalls are sessions that went silent (caught by the 12 s watchdog); reconnects count every restart. One camera reconnecting alone → that camera, its cable, or PoE port. All cameras together → shared network. No buffer absorbs a stall.",
            segs,
        );

        // smoothing trio
        let leads: Vec<f32> = st
            .samples
            .iter()
            .filter(|s| now.duration_since(s.t) <= Duration::from_secs(10) && s.lead >= 0.0)
            .map(|s| s.lead)
            .collect();
        let (buffer, reanchors, late) = if !smooth {
            let off = || vec![seg("smoothing off", tile::DIM)];
            (off(), off(), off())
        } else if let Some(&last) = leads.last() {
            let min = leads.iter().cloned().fold(f32::MAX, f32::min) * 1000.0;
            let mc = if min < 30.0 {
                RED
            } else if min < 80.0 {
                AMBER
            } else {
                tile::DIM
            };
            let buffer = vec![
                seg(format!("{:.0} / 200 ms", last * 1000.0), tile::WHITE),
                seg(format!(" · min {min:.0}"), mc),
            ];
            let base = tick_before(60.0);
            let re60 = st.reanchors - base.map_or(0, |b| b.reanchors);
            let rc = if re60 > 2 {
                RED
            } else if re60 > 0 {
                AMBER
            } else {
                tile::WHITE
            };
            let reanchors = vec![
                seg(format!("{re60}"), rc),
                seg(format!(" (60 s) · {} total", st.reanchors), tile::DIM),
            ];
            let d_frames = st.frames - base.map_or(0, |b| b.frames);
            let d_late = st.late - base.map_or(0, |b| b.late);
            let late = if d_frames > 0 {
                let pct = d_late as f32 / d_frames as f32 * 100.0;
                let lc = if pct > 5.0 {
                    RED
                } else if pct > 2.0 {
                    AMBER
                } else {
                    tile::WHITE
                };
                let text = if pct < 1.0 {
                    format!("{pct:.1}%")
                } else {
                    format!("{pct:.0}%")
                };
                vec![seg(text, lc), seg(" (60 s)", tile::DIM)]
            } else {
                dash()
            };
            (buffer, reanchors, late)
        } else {
            (dash(), dash(), dash())
        };
        push(
            "buffer",
            "Health of the smoothing buffer: how far ahead the next frame is scheduled versus the 200 ms target, with the lowest value of the last 10 s. Steady near target = coasting. Dips that recover = spikes being absorbed. Repeatedly scraping zero = delivery spikes nearly beat the buffer.",
            buffer,
        );
        push(
            "re-anchors",
            "Moments smoothing gave up and restarted its schedule — each is one brief visible hiccup: a delivery gap outlasted the whole buffer, or the schedule drifted ahead. Zero means every spike was absorbed.",
            reanchors,
        );
        push(
            "late",
            "Share of frames arriving with almost no headroom (<30 ms) before their display slot — near-misses. A rising late % is the early warning that the buffer is being squeezed, visible before re-anchors appear. With software decode it also rises when the CPU can't keep up.",
            late,
        );

        // cpu (% of one core over ~2 s)
        push(
            "cpu",
            "Processor cost of viewing, in % of one core: the app (scheduling, upload, rendering) plus this stream's ffmpeg (network, demux and — unless a hardware device is chosen — the decode itself, which is where the cost lands on this port).",
            match (tick_before(2.0), cpu_seconds(None)) {
                (Some(base), Some(_)) if now > base.t => {
                    let dt = now.duration_since(base.t).as_secs_f64();
                    let app = (app_cpu - base.app_cpu).max(0.0) / dt * 100.0;
                    let ff = (ff_cpu - base.ff_cpu).max(0.0) / dt * 100.0;
                    let capacity =
                        std::thread::available_parallelism().map_or(1, |n| n.get()) as f64 * 100.0;
                    let total = app + ff;
                    let c = if total > capacity * 0.6 {
                        RED
                    } else if total > capacity * 0.25 {
                        AMBER
                    } else {
                        tile::WHITE
                    };
                    vec![
                        seg(format!("{app:.0}% app"), c),
                        seg(format!(" · {ff:.0}% ffmpeg"), tile::DIM),
                    ]
                }
                (_, None) => vec![seg("not available on this OS", tile::DIM)],
                _ => dash(),
            },
        );

        // wifi
        push(
            "wifi",
            "The radio under this stream: signal (−50 great, −70 marginal, −80 desperate). If jitter spikes line up with RSSI sags here, the radio is the cause — weak signal means distance/walls. Shows \"no Wi-Fi\" when wired.",
            match wifi() {
                Some((iface, dbm)) => {
                    let c = if dbm < -78 {
                        RED
                    } else if dbm < -70 {
                        AMBER
                    } else {
                        tile::WHITE
                    };
                    vec![
                        seg(format!("{dbm} dBm"), c),
                        seg(format!(" · {iface}"), tile::DIM),
                    ]
                }
                None => vec![seg("no Wi-Fi (wired?)", tile::DIM)],
            },
        );

        // EVENTS: motion / intrusion config via the NVR.
        let (motion, intrusion) = match events {
            zones::Info::NoNvr => (
                vec![seg("no NVR in Settings (Ctrl-,)", tile::DIM)],
                vec![seg("no NVR in Settings (Ctrl-,)", tile::DIM)],
            ),
            zones::Info::Connecting => (
                vec![seg("contacting NVR…", tile::DIM)],
                vec![seg("contacting NVR…", tile::DIM)],
            ),
            zones::Info::NotRecorded => (
                vec![seg("not a channel on this NVR", tile::DIM)],
                vec![seg("not a channel on this NVR", tile::DIM)],
            ),
            zones::Info::Ready(ev) => {
                let zones = ev.intrusion_regions.len();
                let mut detail = None;
                if ev.intrusion == zones::State::On {
                    let mut d = match zones {
                        0 => "no zones".to_string(),
                        1 => "1 zone".to_string(),
                        n => format!("{n} zones"),
                    };
                    if let Some(t) = &ev.intrusion_targets {
                        d.push_str(&format!(" · {t}"));
                    }
                    detail = Some(d);
                }
                self.motion_regions = ev.motion_regions;
                self.intrusion_regions = ev.intrusion_regions;
                (
                    event_row(
                        ev.motion,
                        ev.motion_schedule.as_deref(),
                        ev.motion_targets.as_deref(),
                    ),
                    event_row(
                        ev.intrusion,
                        ev.intrusion_schedule.as_deref(),
                        detail.as_deref(),
                    ),
                )
            }
        };
        push(
            "motion",
            "Motion detection as configured on the camera (read through the NVR, read-only): whether it's enabled and its arming schedule. Outside the armed window the camera still records but flags no motion events, so nothing shows on the playback motion bands there.",
            motion,
        );
        push(
            "intrusion",
            "AcuSense intrusion (field detection) as configured on the camera: enabled state and arming schedule. The box below draws the configured zone polygons over this camera's video — display only, nothing is written to the camera or NVR.",
            intrusion,
        );

        self.rows = rows;
        self.last_refresh = Some(now);
    }

    fn plain_text(&self) -> String {
        let mut out = self.title.clone();
        for r in &self.rows {
            out.push('\n');
            out.push_str(r.key);
            out.push_str(": ");
            for s in &r.segs {
                out.push_str(&s.text);
            }
        }
        out
    }
}

impl App {
    /// Camera event config comes through the NVR (its channel map resolves
    /// camera host -> channel); reuse playback's client, creating it here
    /// when the panel is opened before any playback.
    fn event_info(&mut self, host: &str, ctx: &egui::Context) -> zones::Info {
        let Some(nvr) = self.config.as_ref().and_then(|c| c.nvr.clone()) else {
            return zones::Info::NoNvr;
        };
        self.ensure_nvr();
        let ch = match &self.nvr {
            NvrState::Ready(c) => c.channel_by_host.get(host).copied(),
            _ => return zones::Info::Connecting,
        };
        match ch {
            Some(ch) => zones::Info::Ready(self.zones.info(&nvr, ch, ctx)),
            None => zones::Info::NotRecorded,
        }
    }

    pub fn show_nerd_stats(&mut self, ctx: &egui::Context) {
        if !self.prefs.nerd_stats {
            self.overlay = None;
            return;
        }
        // Focused camera's main stream (its substream during playback, when
        // no main pipe runs — AppDelegate.nerdStatsTarget), else the grid's
        // cursor (or last cursor) tile's substream.
        let (idx, channel): (usize, &'static str) = match &self.focused {
            Some(f) if f.playback.is_some() => (f.idx, crate::config::SUB_CHANNEL),
            Some(f) => (f.idx, crate::config::MAIN_CHANNEL),
            None if !self.cams.is_empty() => {
                let i = self
                    .key_sel
                    .map_or(self.last_key_sel, |(i, _)| i)
                    .min(self.cams.len() - 1);
                (i, crate::config::SUB_CHANNEL)
            }
            None => return,
        };
        let id = self.cams[idx].id
            | if channel == crate::config::MAIN_CHANNEL {
                crate::MAIN_BIT
            } else {
                0
            };
        let due = self
            .nerd
            .last_refresh
            .is_none_or(|t| t.elapsed() >= REFRESH)
            || self.nerd.target != Some(id);
        // The event rows need `&mut self` (NVR client, config cache), so
        // they are fetched before the stream is borrowed for the target.
        let host = self.cams[idx].host.clone();
        let events = if due {
            Some(self.event_info(&host, ctx))
        } else {
            None
        };
        let shared: &stream::Shared = match &self.focused {
            Some(f) if f.playback.is_none() => &f.main,
            _ => &self.cams[idx].shared,
        };
        let cam = &self.cams[idx];
        let codec = self
            .config
            .as_ref()
            .and_then(|c| c.cameras.iter().find(|c| c.host == cam.host))
            .map_or("", |c| c.codec_label());
        let target = Target {
            id,
            name: &cam.name,
            codec,
            channel,
            shared,
        };
        if let Some(events) = events {
            let decode = if channel == crate::config::SUB_CHANNEL {
                "CPU (software)"
            } else {
                self.prefs.decode_label()
            };
            let smooth = self.prefs.smooth_live;
            self.nerd.refresh(&target, decode, smooth, events);
        }
        ctx.request_repaint_after(REFRESH);

        let default_pos = self
            .prefs
            .nerd_pos
            .map_or(egui::pos2(20.0, 60.0), |p| egui::pos2(p[0], p[1]));
        let mut copy = false;
        let (mut draw_motion, mut draw_zones) =
            (self.prefs.nerd_draw_motion, self.prefs.nerd_draw_zones);
        let (has_motion, has_zones) = (
            !self.nerd.motion_regions.is_empty(),
            !self.nerd.intrusion_regions.is_empty(),
        );
        let resp = egui::Window::new("nerd stats")
            .title_bar(false)
            .resizable(false)
            .default_pos(default_pos)
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_black_alpha(200))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&self.nerd.title)
                            .size(11.0)
                            .strong()
                            .color(tile::WHITE),
                    );
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("⧉").size(12.0).color(tile::DIM))
                                .frame(false),
                        )
                        .on_hover_text("Copy as text")
                        .clicked()
                    {
                        copy = true;
                    }
                });
                ui.add_space(4.0);
                egui::Grid::new("nerd rows")
                    .spacing(egui::vec2(10.0, 3.0))
                    .show(ui, |ui| {
                        for row in &self.nerd.rows {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(row.key).size(11.0).color(tile::DIM),
                                    )
                                    .on_hover_text(row.tip);
                                },
                            );
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                for s in &row.segs {
                                    ui.label(
                                        egui::RichText::new(&s.text)
                                            .size(11.0)
                                            .monospace()
                                            .color(s.color),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
                for (flag, enabled, text, tip) in [
                    (
                        &mut draw_motion,
                        has_motion,
                        "draw motion areas on video",
                        "Overlay this camera's configured motion detection areas on its video. Display only — nothing is sent to the camera.",
                    ),
                    (
                        &mut draw_zones,
                        has_zones,
                        "draw intrusion zones on video",
                        "Overlay this camera's configured intrusion zones on its video. Display only — nothing is sent to the camera.",
                    ),
                ] {
                    ui.add_enabled(
                        enabled,
                        egui::Checkbox::new(
                            flag,
                            egui::RichText::new(text).size(10.0).monospace().color(tile::DIM),
                        ),
                    )
                    .on_hover_text(tip);
                }
            });
        if draw_motion != self.prefs.nerd_draw_motion || draw_zones != self.prefs.nerd_draw_zones {
            self.prefs.nerd_draw_motion = draw_motion;
            self.prefs.nerd_draw_zones = draw_zones;
            self.prefs.save();
        }
        // The overlay follows the panel's target and boxes; drawn by the
        // grid tile / focused view whose camera it names.
        let motion = if draw_motion && has_motion {
            self.nerd.motion_regions.clone()
        } else {
            Vec::new()
        };
        let intrusion = if draw_zones && has_zones {
            self.nerd.intrusion_regions.clone()
        } else {
            Vec::new()
        };
        self.overlay = (!motion.is_empty() || !intrusion.is_empty()).then(|| zones::Overlay {
            host: host.clone(),
            motion,
            intrusion,
        });
        if copy {
            ctx.copy_text(self.nerd.plain_text());
        }
        // Remember where it was dragged, once the drag is over.
        if let Some(r) = resp {
            let pos = [r.response.rect.min.x, r.response.rect.min.y];
            if self.prefs.nerd_pos != Some(pos) {
                self.prefs.nerd_pos = Some(pos);
                self.nerd.pos_dirty = true;
            } else if self.nerd.pos_dirty && !ctx.egui_is_using_pointer() {
                self.nerd.pos_dirty = false;
                self.prefs.save();
            }
        }
    }
}
