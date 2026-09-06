//! Read-only fetch of per-camera event configuration through the NVR
//! (EventConfig.swift): motion + intrusion (field detection) enabled state,
//! their arming schedules, and the zone polygons. Feeds the nerd-stats
//! panel's motion/intrusion rows and the zone overlay. GETs only — nothing
//! here writes to the NVR or cameras.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::config::StoredNvr;
use crate::isapi::{blocks, tag};

/// A normalized 0–1 polygon with a bottom-left origin (y up) — the ISAPI
/// and AppKit convention; `paint` flips it for the screen.
pub type Poly = Vec<[f32; 2]>;

#[derive(Clone, Copy, PartialEq, Default)]
pub enum State {
    #[default]
    Loading,
    On,
    Off,
    Unknown,
}

/// What the panel shows for one camera. Composed on the fly from the
/// store's caches, so fields fill in as fetches land.
pub struct ChannelEvents {
    pub motion: State,
    /// None while the schedule list loads.
    pub motion_schedule: Option<String>,
    pub intrusion: State,
    pub intrusion_schedule: Option<String>,
    /// Intrusion zones come straight from the camera's VCA polygons; motion
    /// areas are the camera's detection grid with runs of enabled cells
    /// merged into rectangles.
    pub motion_regions: Vec<Poly>,
    pub intrusion_regions: Vec<Poly>,
    /// AcuSense target filters ("human", "vehicle", "human+vehicle"); None
    /// on cameras without target classification. Intrusion is the union
    /// across its active zones.
    pub motion_targets: Option<String>,
    pub intrusion_targets: Option<String>,
}

pub enum Info {
    /// No NVR configured in Settings.
    NoNvr,
    /// NVR channel map still loading.
    Connecting,
    /// Camera isn't a channel on this NVR.
    NotRecorded,
    Ready(ChannelEvents),
}

#[derive(Clone, Default)]
struct PerChannel {
    motion: State,
    intrusion: State,
    motion_regions: Vec<Poly>,
    regions: Vec<Poly>,
    motion_targets: Option<String>,
    intrusion_targets: Option<String>,
}

enum Msg {
    Channel(String, u32, PerChannel),
    Schedules(String, HashMap<u32, String>, HashMap<u32, String>),
}

/// Per-channel event config cache. `info` kicks the fetches it is missing
/// and returns whatever is cached so far. Everything expires together after
/// 5 minutes so config edits on the NVR eventually show up.
pub struct Store {
    host: String,
    per_channel: HashMap<u32, PerChannel>,
    fetching: HashSet<u32>,
    /// (motion, intrusion) per channel; None = not loaded yet.
    sched: Option<(HashMap<u32, String>, HashMap<u32, String>)>,
    sched_fetching: bool,
    stamp: Instant,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for Store {
    fn default() -> Store {
        let (tx, rx) = std::sync::mpsc::channel();
        Store {
            host: String::new(),
            per_channel: HashMap::new(),
            fetching: HashSet::new(),
            sched: None,
            sched_fetching: false,
            stamp: Instant::now(),
            tx,
            rx,
        }
    }
}

impl Store {
    pub fn info(&mut self, nvr: &StoredNvr, channel: u32, ctx: &egui::Context) -> ChannelEvents {
        if nvr.host != self.host || self.stamp.elapsed() > Duration::from_secs(300) {
            self.host = nvr.host.clone();
            self.per_channel.clear();
            self.fetching.clear();
            self.sched = None;
            self.sched_fetching = false;
            self.stamp = Instant::now();
        }
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                // A stale host's result lands after a switch: drop it.
                Msg::Channel(host, ch, pc) if host == self.host => {
                    self.fetching.remove(&ch);
                    self.per_channel.insert(ch, pc);
                }
                Msg::Schedules(host, motion, intrusion) if host == self.host => {
                    self.sched_fetching = false;
                    self.sched = Some((motion, intrusion));
                }
                _ => {}
            }
        }
        self.fetch_channel(nvr, channel, ctx);
        self.fetch_schedules(nvr, ctx);

        let pc = self.per_channel.get(&channel).cloned().unwrap_or_default();
        // Loaded schedule list with no entry for this channel = never armed.
        let sched =
            |m: &HashMap<u32, String>| m.get(&channel).cloned().unwrap_or_else(|| "never".into());
        ChannelEvents {
            motion: pc.motion,
            motion_schedule: self.sched.as_ref().map(|(m, _)| sched(m)),
            intrusion: pc.intrusion,
            intrusion_schedule: self.sched.as_ref().map(|(_, i)| sched(i)),
            motion_regions: pc.motion_regions,
            intrusion_regions: pc.regions,
            motion_targets: pc.motion_targets,
            intrusion_targets: pc.intrusion_targets,
        }
    }

    fn fetch_channel(&mut self, nvr: &StoredNvr, ch: u32, ctx: &egui::Context) {
        if self.per_channel.contains_key(&ch) || !self.fetching.insert(ch) {
            return;
        }
        let (nvr, tx, ctx) = (nvr.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut pc = PerChannel::default();
            match get(
                &nvr,
                &format!("/ISAPI/System/Video/inputs/channels/{ch}/motionDetection"),
            ) {
                Some(xml) => {
                    pc.motion = enabled_state(&xml);
                    let rows: usize = tag(&xml, "rowGranularity")
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let cols: usize = tag(&xml, "columnGranularity")
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let grid = tag(&xml, "gridMap").map(str::trim).unwrap_or("");
                    // Grid cameras carry both a gridMap and a (redundant)
                    // polygon list — the grid is the authoritative regionType.
                    pc.motion_regions = if !grid.is_empty() && rows > 0 && cols > 0 {
                        grid_polygons(grid, rows, cols)
                    } else {
                        blocks(&xml, "Region")
                            .into_iter()
                            .filter_map(polygon)
                            .collect()
                    };
                    pc.motion_targets = target_label(tag(&xml, "targetType"));
                }
                None => pc.motion = State::Unknown,
            }
            match get(&nvr, &format!("/ISAPI/Smart/FieldDetection/{ch}")) {
                Some(xml) => {
                    pc.intrusion = enabled_state(&xml);
                    let mut targets = Vec::new();
                    // Regions with fewer than 3 points are unused slots (the
                    // NVR always reports 4).
                    for region in blocks(&xml, "FieldDetectionRegion") {
                        if let Some(poly) = polygon(region) {
                            pc.regions.push(poly);
                            if let Some(t) = tag(region, "detectionTarget").map(str::trim)
                                && !t.is_empty()
                            {
                                targets.push(t.to_string());
                            }
                        }
                    }
                    pc.intrusion_targets = target_label(Some(&targets.join(",")));
                }
                None => pc.intrusion = State::Unknown,
            }
            let _ = tx.send(Msg::Channel(nvr.host, ch, pc));
            ctx.request_repaint();
        });
    }

    fn fetch_schedules(&mut self, nvr: &StoredNvr, ctx: &egui::Context) {
        if self.sched.is_some() || self.sched_fetching {
            return;
        }
        self.sched_fetching = true;
        let (nvr, tx, ctx) = (nvr.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let fetch = |path: &str| -> HashMap<u32, String> {
                get(&nvr, path)
                    .map(|xml| {
                        parse_schedules(&xml)
                            .into_iter()
                            .map(|(ch, blocks)| (ch, summarize(&blocks)))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let motion = fetch("/ISAPI/Event/schedules/motionDetections");
            let intrusion = fetch("/ISAPI/Event/schedules/fieldDetections");
            let _ = tx.send(Msg::Schedules(nvr.host, motion, intrusion));
            ctx.request_repaint();
        });
    }
}

fn get(nvr: &StoredNvr, path: &str) -> Option<String> {
    crate::isapi::request(
        &nvr.host,
        &nvr.user,
        &nvr.password,
        path,
        None,
        Duration::from_secs(8),
    )
    .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The first `<enabled>` is the top-level toggle (regions carry none on
/// FieldDetection; MotionDetection's nested ones come later). No `<enabled>`
/// at all = the camera doesn't support the event / the config couldn't be
/// read.
fn enabled_state(xml: &str) -> State {
    match tag(xml, "enabled").map(str::trim) {
        Some("true") => State::On,
        Some(_) => State::Off,
        None => State::Unknown,
    }
}

/// positionX/positionY pairs (0–1000) -> normalized polygon; fewer than 3
/// points is an unused slot.
fn polygon(region: &str) -> Option<Poly> {
    let xs = blocks(region, "positionX");
    let ys = blocks(region, "positionY");
    let pts: Poly = xs
        .iter()
        .zip(ys.iter())
        .filter_map(|(x, y)| {
            Some([
                x.trim().parse::<f32>().ok()? / 1000.0,
                y.trim().parse::<f32>().ok()? / 1000.0,
            ])
        })
        .collect();
    (pts.len() >= 3).then_some(pts)
}

/// "human,vehicle" (any order, duplicates) -> "human+vehicle"; empty/None
/// -> None (camera has no AcuSense target filter on this event).
fn target_label(raw: Option<&str>) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    for t in raw?.split(',').map(str::trim) {
        if !t.is_empty() && !seen.contains(&t) {
            seen.push(t);
        }
    }
    if seen.is_empty() {
        return None;
    }
    seen.sort_unstable();
    Some(seen.join("+"))
}

/// Motion grid -> polygons: decode the row-major hex bitmap (each row padded
/// to whole bytes, MSB = leftmost column, row 0 = top of frame), then merge
/// vertical runs of identical row-spans into rectangles so the overlay is a
/// few clean shapes instead of hundreds of cells.
fn grid_polygons(map: &str, rows: usize, cols: usize) -> Vec<Poly> {
    let chars_per_row = cols.div_ceil(8) * 2;
    let chars: Vec<char> = map.chars().collect();
    if rows == 0 || cols == 0 || chars.len() < rows * chars_per_row {
        return Vec::new();
    }
    let mut bits = vec![vec![false; cols]; rows];
    for (r, row) in bits.iter_mut().enumerate() {
        for b in 0..chars_per_row / 2 {
            let i = r * chars_per_row + b * 2;
            let Ok(v) = u8::from_str_radix(&chars[i..i + 2].iter().collect::<String>(), 16) else {
                continue;
            };
            for bit in 0..8 {
                if b * 8 + bit < cols && v & (0x80 >> bit) != 0 {
                    row[b * 8 + bit] = true;
                }
            }
        }
    }
    // (c0, c1, r0, r1) inclusive cell rectangles.
    let mut rects: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut open: Vec<(usize, usize, usize)> = Vec::new();
    for (r, row) in bits.iter().enumerate() {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut c = 0;
        while c < cols {
            if !row[c] {
                c += 1;
                continue;
            }
            let mut e = c;
            while e + 1 < cols && row[e + 1] {
                e += 1;
            }
            runs.push((c, e));
            c = e + 1;
        }
        let mut next = Vec::new();
        for run in runs {
            if let Some(i) = open.iter().position(|o| o.0 == run.0 && o.1 == run.1) {
                next.push(open.remove(i));
            } else {
                next.push((run.0, run.1, r));
            }
        }
        for o in open {
            rects.push((o.0, o.1, o.2, r - 1));
        }
        open = next;
    }
    for o in open {
        rects.push((o.0, o.1, o.2, rows - 1));
    }
    rects
        .into_iter()
        .map(|(c0, c1, r0, r1)| {
            let (x0, x1) = (c0 as f32 / cols as f32, (c1 + 1) as f32 / cols as f32);
            let (yt, yb) = (
                1.0 - r0 as f32 / rows as f32,
                1.0 - (r1 + 1) as f32 / rows as f32,
            );
            vec![[x0, yb], [x1, yb], [x1, yt], [x0, yt]]
        })
        .collect()
}

/// One of the /ISAPI/Event/schedules/* lists: every channel's TimeBlocks in
/// a single document -> channel -> [(day, begin, end)]. A TimeBlock without
/// <dayOfWeek> is the holiday row (stored as day 8).
fn parse_schedules(xml: &str) -> HashMap<u32, Vec<(u32, String, String)>> {
    let mut out: HashMap<u32, Vec<(u32, String, String)>> = HashMap::new();
    for sched in blocks(xml, "Schedule") {
        let channel = tag(sched, "videoInputChannelID")
            .or_else(|| tag(sched, "dynVideoInputChannelID"))
            .and_then(|s| s.trim().parse::<u32>().ok());
        let Some(ch) = channel else {
            continue;
        };
        for block in blocks(sched, "TimeBlock") {
            let day = tag(block, "dayOfWeek")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(8);
            let begin = tag(block, "beginTime").map(str::trim).unwrap_or("");
            let end = tag(block, "endTime").map(str::trim).unwrap_or("");
            if !begin.is_empty() && !end.is_empty() {
                out.entry(ch)
                    .or_default()
                    .push((day, begin.into(), end.into()));
            }
        }
    }
    out
}

/// Compact one-line arming summary: "24/7", "daily 01:30–05:30",
/// "Mon–Fri 08:00–17:00; Sat 10:00–12:00". dayOfWeek 1 = Monday; a block
/// with no dayOfWeek is the holiday row — appended only when it differs.
fn summarize(blocks: &[(u32, String, String)]) -> String {
    let hhmm = |s: &str| s.chars().take(5).collect::<String>();
    let ranges = |sel: &[&(u32, String, String)]| {
        let mut v: Vec<String> = sel
            .iter()
            .map(|b| format!("{}–{}", hhmm(&b.1), hhmm(&b.2)))
            .collect();
        v.sort();
        v.join(",")
    };
    // Zero-length = disabled.
    let live: Vec<&(u32, String, String)> = blocks.iter().filter(|b| b.1 != b.2).collect();
    let week: Vec<&(u32, String, String)> = live.iter().copied().filter(|b| b.0 <= 7).collect();
    let holiday_blocks: Vec<&(u32, String, String)> =
        live.iter().copied().filter(|b| b.0 > 7).collect();
    let holiday = ranges(&holiday_blocks);

    let mut summary = if week.is_empty() {
        "never".to_string()
    } else {
        let mut per_day: HashMap<u32, String> = HashMap::new();
        for d in 1..=7 {
            let sel: Vec<&(u32, String, String)> =
                week.iter().copied().filter(|b| b.0 == d).collect();
            if !sel.is_empty() {
                per_day.insert(d, ranges(&sel));
            }
        }
        let distinct: HashSet<&String> = per_day.values().collect();
        if per_day.len() == 7 && distinct.len() == 1 {
            let r = per_day[&1].as_str();
            if r == "00:00–24:00" {
                "24/7".to_string()
            } else {
                format!("daily {r}")
            }
        } else {
            let names = ["", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let mut parts = Vec::new();
            let mut d = 1;
            while d <= 7 {
                let Some(r) = per_day.get(&d) else {
                    d += 1;
                    continue;
                };
                let mut e = d;
                while e < 7 && per_day.get(&(e + 1)) == Some(r) {
                    e += 1;
                }
                let span = if d == e {
                    names[d as usize].to_string()
                } else {
                    format!("{}–{}", names[d as usize], names[e as usize])
                };
                parts.push(format!("{span} {r}"));
                d = e + 1;
            }
            parts.join("; ")
        }
    };
    if !holiday.is_empty() && (summary != "24/7" || holiday != "00:00–24:00") {
        summary.push_str(&format!(
            " · hol {}",
            if holiday == "00:00–24:00" {
                "24h"
            } else {
                &holiday
            }
        ));
    }
    summary
}

/// The overlay shown on one camera's video while the nerd-stats panel has a
/// draw box ticked: which host, and the polygons per event type.
pub struct Overlay {
    pub host: String,
    pub motion: Vec<Poly>,
    pub intrusion: Vec<Poly>,
}

/// Draw the polygons over `video` (the aspect-fitted, possibly zoomed rect
/// the pixels occupy), clipped to `clip`. Colors match the timeline's event
/// bands — one color per concept; motion sits under intrusion so both
/// outlines stay visible. Display only.
pub fn paint(painter: &egui::Painter, video: egui::Rect, clip: egui::Rect, overlay: &Overlay) {
    let painter = painter.with_clip_rect(clip);
    for (polys, color) in [
        (&overlay.motion, crate::timeline::MOTION),
        (&overlay.intrusion, crate::timeline::INTRUSION),
    ] {
        let fill = color.gamma_multiply(0.14);
        let stroke = egui::Stroke::new(2.0, color.gamma_multiply(0.85));
        for poly in polys.iter().filter(|p| p.len() >= 3) {
            let pts: Vec<egui::Pos2> = poly
                .iter()
                .map(|[x, y]| {
                    egui::pos2(
                        video.min.x + x * video.width(),
                        video.max.y - y * video.height(),
                    )
                })
                .collect();
            painter.add(egui::Shape::convex_polygon(
                pts.clone(),
                fill,
                egui::Stroke::NONE,
            ));
            painter.add(egui::Shape::closed_line(pts, stroke));
        }
    }
}
