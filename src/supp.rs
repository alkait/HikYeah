//! Supplementary panes (SupplementaryManager.swift + SupplementaryViews.swift):
//! up to four floating panes over the focused camera, each showing another
//! camera. Live panes tap the substreams already running under the focused
//! view, so they cost no extra sessions; in playback each pane runs its own
//! NVR stream synced to the main view's transport. `+` opens a selector,
//! `−` closes the last-added pane, a double-click promotes a pane to the
//! main view with a way back.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::{App, NvrState, config, nvr, render, rtsp, session, snapshot, stream, tile, timeline};

/// Texture id bit for a pane's own playback pipe (the live tap reuses the
/// camera's substream texture).
pub const PANE_BIT: u64 = 1 << 34;

/// One saved pane, normalized to the tile's bounds — Mac format: bottom-
/// left origin, y up.
#[derive(Serialize, Deserialize, Clone)]
pub struct PaneLayout {
    pub host: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Last-used pane set per main camera, on disk so it survives relaunches.
/// Deliberately separate from config.json: layout state, no credentials,
/// and export/import stays untouched.
fn layouts_path() -> std::path::PathBuf {
    config::config_path().with_file_name("layouts.json")
}

fn load_layouts() -> HashMap<String, Vec<PaneLayout>> {
    std::fs::read(layouts_path())
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

pub fn saved_layouts(main_host: &str) -> Vec<PaneLayout> {
    load_layouts().remove(main_host).unwrap_or_default()
}

fn save_layouts(main_host: &str, layouts: Vec<PaneLayout>) {
    let mut all = load_layouts();
    all.insert(main_host.to_string(), layouts);
    let p = layouts_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(data) = serde_json::to_vec(&all) {
        let _ = std::fs::write(p, data);
    }
}

/// A pane's own playback pipe. Dropping it ends the session; `session`
/// alone is dropped to freeze on the last frame (end of footage, or a pane
/// added while paused).
struct Feed {
    shared: Arc<stream::Shared>,
    session: Option<rtsp::Session>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            s.stop();
        }
        self.shared.stop();
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Edges {
    n: bool,
    s: bool,
    e: bool,
    w: bool,
}

impl Edges {
    const NONE: Edges = Edges {
        n: false,
        s: false,
        e: false,
        w: false,
    };
    fn any(self) -> bool {
        self.n || self.s || self.e || self.w
    }
}

struct Drag {
    edges: Edges,
    origin: egui::Pos2,
    start: egui::Rect,
}

pub struct Pane {
    pub host: String,
    /// Normalized to the tile, egui's top-left origin.
    norm: egui::Rect,
    feed: Option<Feed>,
    /// The last pipe's picture, kept up while a replacement connects (a
    /// seek, a speed change) — the Mac display layer's frame survives the
    /// swap the same way.
    frozen: Option<Arc<stream::Shared>>,
    note: Option<String>,
    /// A real frame has rendered — the keyframe nudge stops retrying, and a
    /// pane added while paused freezes here.
    has_video: bool,
    freeze_after_first: bool,
    /// Live-tap IDR nudges left and when the next is due.
    nudges: u8,
    next_nudge: Option<Instant>,
    drag: Option<Drag>,
}

/// The pane set on the focused camera.
#[derive(Default)]
pub struct Manager {
    pub panes: Vec<Pane>,
    pub attached_host: Option<String>,
    pub playback_active: bool,
}

const MAX_PANES: usize = 4;
/// Resize hit zones: a thin band along each edge, widened at the corners
/// so the diagonal grabs have a fatter target.
const EDGE_BAND: f32 = 10.0;
const CORNER_REACH: f32 = 18.0;
const CLOSE_SIZE: f32 = 16.0;

impl Manager {
    pub fn count(&self) -> usize {
        self.panes.len()
    }

    pub fn has(&self, host: &str) -> bool {
        self.panes.iter().any(|p| p.host == host)
    }

    /// Default slots: a column of four on the far right, top to bottom,
    /// skipping occupied ones.
    fn free_slot(&self) -> egui::Rect {
        let slots: Vec<egui::Rect> = (0..4)
            .map(|i| {
                egui::Rect::from_min_size(
                    egui::pos2(0.755, 0.025 + i as f32 * 0.233),
                    egui::vec2(0.235, 0.22),
                )
            })
            .collect();
        for slot in &slots {
            if !self.panes.iter().any(|p| p.norm.intersects(*slot)) {
                return *slot;
            }
        }
        slots[self.panes.len() % 4]
    }

    fn add(&mut self, host: &str, norm: Option<egui::Rect>) -> bool {
        if self.panes.len() >= MAX_PANES || self.attached_host.is_none() || self.has(host) {
            return false;
        }
        let norm = norm.unwrap_or_else(|| self.free_slot());
        self.panes.push(Pane {
            host: host.to_string(),
            norm,
            feed: None,
            frozen: None,
            note: None,
            has_video: false,
            freeze_after_first: false,
            nudges: 0,
            next_nudge: None,
            drag: None,
        });
        self.persist();
        true
    }

    /// "-" — close the most recently added pane. No persist: closing panes
    /// must not shrink the saved set, so closing all of them (in any order)
    /// leaves the full last set behind the selector's "↺ Restore last" row.
    /// The saved set tracks adds, drags/resizes, and teardown.
    pub fn remove_last(&mut self) -> bool {
        self.panes.pop().is_some()
    }

    fn remove(&mut self, i: usize) {
        self.panes.remove(i);
    }

    /// Remove every pane (saving the layout for restore). Safe to call twice.
    pub fn teardown(&mut self) {
        if self.attached_host.is_some() && !self.panes.is_empty() {
            self.persist();
        }
        self.panes.clear();
        self.attached_host = None;
        self.playback_active = false;
    }

    fn persist(&self) {
        let Some(host) = &self.attached_host else {
            return;
        };
        save_layouts(
            host,
            self.panes
                .iter()
                .map(|p| PaneLayout {
                    host: p.host.clone(),
                    x: f64::from(p.norm.min.x),
                    y: f64::from(1.0 - p.norm.max.y),
                    w: f64::from(p.norm.width()),
                    h: f64::from(p.norm.height()),
                })
                .collect(),
        );
    }

    /// Main view returned to live — panes follow the substream tap again.
    pub fn switch_to_live(&mut self) {
        self.playback_active = false;
        for p in &mut self.panes {
            p.feed = None;
            p.frozen = None;
            p.note = None;
            p.has_video = false;
            p.freeze_after_first = false;
        }
    }

    /// Promote due frames of the pane pipes (the frame loop's pump).
    pub fn advance(&self, now: Instant) -> Option<Instant> {
        self.panes
            .iter()
            .filter_map(|p| p.feed.as_ref().and_then(|f| f.shared.advance(now)))
            .min()
    }
}

/// Min/max pane size within the tile — shared by clamping and the resize
/// drag (which must clamp size before anchoring fixed edges).
fn clamp_size(s: egui::Vec2, tile: egui::Rect) -> egui::Vec2 {
    let min_w = (tile.width() * 0.15).max(120.0);
    egui::vec2(
        s.x.max(min_w).min(tile.width() * 0.5),
        s.y.max(70.0).min(tile.height() * 0.6),
    )
}

/// Clamp a frame into the tile, respecting the bar inset and sane sizes.
fn clamped(r: egui::Rect, tile: egui::Rect, bottom_inset: f32) -> egui::Rect {
    let size = clamp_size(r.size(), tile);
    let max_x = (tile.max.x - size.x - 2.0).max(tile.min.x + 2.0);
    let max_y = (tile.max.y - bottom_inset - size.y - 2.0).max(tile.min.y + 2.0);
    let x = r.min.x.clamp(tile.min.x + 2.0, max_x);
    let y = r.min.y.clamp(tile.min.y + 2.0, max_y);
    egui::Rect::from_min_size(egui::pos2(x, y), size)
}

fn to_pixels(norm: egui::Rect, tile: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        tile.min + norm.min.to_vec2() * tile.size(),
        norm.size() * tile.size(),
    )
}

fn to_norm(r: egui::Rect, tile: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        ((r.min - tile.min) / tile.size()).to_pos2(),
        r.size() / tile.size(),
    )
}

/// Which resize zone (if any) a point falls in. None = interior = move.
/// The ✕ button owns the top-right corner — never offer resize where a
/// click would actually close the pane.
fn resize_edges(r: egui::Rect, p: egui::Pos2) -> Edges {
    if close_rect(r).expand(4.0).contains(p) {
        return Edges::NONE;
    }
    let (t, c) = (EDGE_BAND, CORNER_REACH);
    let (lx, rx, ty, by) = (p.x - r.min.x, r.max.x - p.x, p.y - r.min.y, r.max.y - p.y);
    let mut e = Edges {
        n: ty < t,
        s: by < t,
        e: rx < t,
        w: lx < t,
    };
    // Corner reach: within c of two edges counts as that corner even when
    // outside the thin band on one axis.
    if lx < c && by < c {
        e = Edges {
            w: true,
            s: true,
            n: false,
            e: false,
        };
    }
    if rx < c && by < c {
        e = Edges {
            e: true,
            s: true,
            n: false,
            w: false,
        };
    }
    if lx < c && ty < c {
        e = Edges {
            w: true,
            n: true,
            s: false,
            e: false,
        };
    }
    if rx < c && ty < c {
        e = Edges {
            e: true,
            n: true,
            s: false,
            w: false,
        };
    }
    e
}

fn close_rect(r: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(r.max.x - CLOSE_SIZE - 4.0, r.min.y + 4.0),
        egui::vec2(CLOSE_SIZE, CLOSE_SIZE),
    )
}

fn cursor_for(e: Edges) -> egui::CursorIcon {
    match (e.n || e.s, e.e || e.w) {
        (true, true) => {
            if (e.n && e.w) || (e.s && e.e) {
                egui::CursorIcon::ResizeNwSe
            } else {
                egui::CursorIcon::ResizeNeSw
            }
        }
        (true, false) => egui::CursorIcon::ResizeVertical,
        (false, true) => egui::CursorIcon::ResizeHorizontal,
        (false, false) => egui::CursorIcon::Grab,
    }
}

/// The `+` selector: a translucent panel over the focused view with a
/// thumbnail grid of the other cameras. Type to filter, Return picks the
/// top match, Esc clears the filter then closes, click outside closes. When
/// no panes are active and a saved layout exists, a restore row offers it.
pub struct Selector {
    pub filter: String,
    pub sel: Option<usize>,
}

struct Entry {
    idx: usize,
    enabled: bool,
    note: Option<&'static str>,
}

const SELECTOR_COLS: usize = 4;
const THUMB: egui::Vec2 = egui::vec2(108.0, 60.0);

impl App {
    fn main_host(&self) -> Option<String> {
        self.focused
            .as_ref()
            .map(|f| self.cams[f.idx].host.clone())
            .filter(|h| !h.is_empty())
    }

    fn cam_index(&self, host: &str) -> Option<usize> {
        self.cams.iter().position(|c| c.host == host)
    }

    fn stored_camera(&self, host: &str) -> Option<config::StoredCamera> {
        self.config
            .as_ref()?
            .cameras
            .iter()
            .find(|c| c.host == host)
            .cloned()
    }

    /// Attach the pane set to the focused camera (tearing down another
    /// camera's set first).
    fn attach_panes(&mut self, main_host: &str) {
        if self.supp.attached_host.as_deref() != Some(main_host) {
            self.supp.teardown();
        }
        self.supp.attached_host = Some(main_host.to_string());
    }

    /// Add `host` as a pane on the focused camera, with the same anti-black-
    /// screen tricks as the grid: the last-known snapshot paints instantly
    /// and a fresh one is fetched; live panes nudge the camera for an IDR.
    fn add_pane(&mut self, host: &str, norm: Option<egui::Rect>) {
        if !self.supp.add(host, norm) {
            return;
        }
        if let (Some(cam), Some(stored)) = (self.cam_index(host), self.stored_camera(host)) {
            snapshot::spawn_fetch(
                stored,
                self.cams[cam].id,
                config::SUB_CHANNEL,
                self.snap_tx.clone(),
                self.ctx.clone(),
            );
        }
        // The IDR nudge only helps the live substream tap — playback panes
        // get their own NVR stream, which starts on a keyframe anyway.
        if !self.supp.playback_active
            && let Some(p) = self.supp.panes.last_mut()
        {
            p.nudges = 3;
            p.next_nudge = Some(Instant::now());
        }
        self.replay_transport();
    }

    /// Bring back the last-used set for the attached main camera.
    fn restore_panes(&mut self) {
        let Some(host) = self.supp.attached_host.clone() else {
            return;
        };
        for l in saved_layouts(&host).into_iter().take(MAX_PANES) {
            if self.supp.count() >= MAX_PANES
                || self.supp.has(&l.host)
                || self.cam_index(&l.host).is_none()
            {
                continue;
            }
            let norm = egui::Rect::from_min_size(
                egui::pos2(l.x as f32, (1.0 - l.y - l.h) as f32),
                egui::vec2(l.w as f32, l.h as f32),
            );
            self.add_pane(&l.host, Some(norm));
        }
    }

    /// A camera view opened fresh with panes remembered: bring them back.
    pub fn restore_panes_if_remembered(&mut self, host: &str) {
        if session::load()
            .per_camera
            .get(host)
            .is_some_and(|st| st.panes_visible)
        {
            self.attach_panes(host);
            self.restore_panes();
        }
    }

    /// The main view's playback transport changed — re-align every pane.
    /// Paused = stop pipes and freeze on the last frame.
    pub fn panes_transport(&mut self, position: DateTime<Utc>, speed: u32, paused: bool) {
        if self.supp.panes.is_empty() {
            return;
        }
        self.supp.playback_active = true;
        let NvrState::Ready(client) = &self.nvr else {
            return;
        };
        let client = client.clone();
        let decode = self.decode_for(false);
        for i in 0..self.supp.panes.len() {
            // Stop the pipe; its last frame stays up (pause = kill the pipe
            // and keep the frame, like the main view).
            let pane = &mut self.supp.panes[i];
            if let Some(f) = &mut pane.feed
                && let Some(s) = f.session.take()
            {
                s.stop();
            }
            if paused {
                // Frozen panes keep their last frame — but a pane added while
                // paused has none yet and would sit black. Run its stream
                // just long enough to render the frame at the paused
                // position, then stop: the pane freezes in sync.
                if !pane.has_video {
                    self.start_feed(i, position, 1, &client, &decode, true);
                }
            } else {
                self.start_feed(i, position, speed, &client, &decode, false);
            }
        }
    }

    fn start_feed(
        &mut self,
        i: usize,
        position: DateTime<Utc>,
        speed: u32,
        client: &Arc<nvr::Client>,
        decode: &stream::Decode,
        freeze_after_first: bool,
    ) {
        let host = self.supp.panes[i].host.clone();
        let Some(&ch) = client.channel_by_host.get(&host) else {
            self.supp.panes[i].note = Some("no recording".into());
            return;
        };
        let codec: &'static str = match self.stored_camera(&host) {
            Some(c) if c.codec == "h264" => "h264",
            _ => "hevc",
        };
        let (path, start_clock) =
            client.playback_request(nvr::track(ch), position, position + TimeDelta::hours(6));
        let ctx = self.ctx.clone();
        // Panes never drive the redraw rate: their frames are promoted on
        // the main view's repaints (measured: three live panes waking the
        // UI on their own pushed the 4K view from 82% to 104% of a core).
        // The grid's window only matters while the main view is still,
        // e.g. a pane's freeze frame landing during a pause.
        let (shared, sink) =
            stream::start_pipe(codec, decode.clone(), crate::GRID_COALESCE, move |d| {
                crate::repaint_after(&ctx, d)
            });
        let session = rtsp::start(
            rtsp::Request {
                host: client.nvr.host.clone(),
                port: client.nvr.rtsp_port(),
                user: client.nvr.user.clone(),
                password: client.nvr.password.clone(),
                path,
                start_clock,
                scale: speed,
                codec,
            },
            sink,
            |_| {},
        );
        let pane = &mut self.supp.panes[i];
        pane.note = None;
        pane.has_video = false;
        pane.freeze_after_first = freeze_after_first;
        // The outgoing pipe's picture bridges the gap to the new one.
        if let Some(old) = pane.feed.take()
            && old.shared.current.lock().unwrap().is_some()
        {
            pane.frozen = Some(old.shared.clone());
        }
        pane.feed = Some(Feed {
            shared,
            session: Some(session),
        });
    }

    /// A pane in playback mode was just added/restored — align it to the
    /// main view's current transport.
    fn replay_transport(&mut self) {
        let Some(pb) = self.focused.as_ref().and_then(|f| f.playback.as_ref()) else {
            return;
        };
        let (pos, speed, paused) = (pb.position(), pb.speed, pb.is_paused());
        self.panes_transport(pos, speed, paused);
    }

    /// Per-frame: freeze-after-first, frozen ends, keyframe nudges.
    pub fn poll_panes(&mut self) {
        let now = Instant::now();
        for i in 0..self.supp.panes.len() {
            let host = self.supp.panes[i].host.clone();
            let live_frame = !self.supp.playback_active
                && self
                    .cam_index(&host)
                    .is_some_and(|idx| self.cams[idx].shared.current.lock().unwrap().is_some());
            let stored = self.stored_camera(&host);
            let p = &mut self.supp.panes[i];
            if let Some(feed) = &mut p.feed {
                if feed.shared.current.lock().unwrap().is_some() {
                    p.has_video = true;
                    p.frozen = None;
                    if p.freeze_after_first
                        && let Some(s) = feed.session.take()
                    {
                        p.freeze_after_first = false;
                        s.stop();
                    }
                }
                // Freeze at the end; the main view leads.
                if feed.shared.ended()
                    && let Some(s) = feed.session.take()
                {
                    s.stop();
                }
            } else if !self.supp.playback_active {
                if live_frame {
                    p.has_video = true;
                }
                // Some firmware ignores a single requestKeyFrame — retry
                // once a second until the pane renders its first frame.
                if !p.has_video && p.nudges > 0 && p.next_nudge.is_some_and(|t| now >= t) {
                    p.nudges -= 1;
                    p.next_nudge = Some(now + Duration::from_secs(1));
                    if let Some(c) = stored {
                        std::thread::spawn(move || {
                            crate::isapi::request_key_frame(
                                &c.host,
                                &c.user,
                                &c.password,
                                config::SUB_CHANNEL,
                            );
                        });
                    }
                    self.ctx.request_repaint_after(Duration::from_secs(1));
                }
            }
        }
    }

    // MARK: keys

    /// `+`: the selector. `−`: close the last-added pane.
    pub fn open_pane_selector(&mut self) {
        if self.pane_selector.is_some() {
            return;
        }
        if self.promoted_origin.is_some() {
            self.flash("No panes on a promoted view");
            return;
        }
        if self.supp.count() >= MAX_PANES {
            self.flash("Pane limit reached");
            return;
        }
        if self.main_host().is_none() {
            return;
        }
        self.pane_selector = Some(Selector {
            filter: String::new(),
            sel: None,
        });
    }

    pub fn close_last_pane(&mut self) {
        if !self.supp.remove_last() {
            self.flash("No panes to close");
        }
    }

    fn selector_entries(&self) -> Vec<Entry> {
        let Some(main) = self.main_host() else {
            return Vec::new();
        };
        let in_playback = self.focused.as_ref().is_some_and(|f| f.playback.is_some());
        let channels = match &self.nvr {
            NvrState::Ready(c) => Some(&c.channel_by_host),
            _ => None,
        };
        self.cams
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.host.is_empty() && c.host != main)
            .map(|(idx, c)| {
                if self.supp.has(&c.host) {
                    Entry {
                        idx,
                        enabled: false,
                        note: Some("added"),
                    }
                } else if in_playback && channels.is_none_or(|m| !m.contains_key(&c.host)) {
                    Entry {
                        idx,
                        enabled: false,
                        note: Some("no recording"),
                    }
                } else {
                    Entry {
                        idx,
                        enabled: true,
                        note: None,
                    }
                }
            })
            .collect()
    }

    fn visible_entries(&self, filter: &str) -> Vec<Entry> {
        self.selector_entries()
            .into_iter()
            .filter(|e| filter.is_empty() || self.cams[e.idx].name.to_lowercase().contains(filter))
            .collect()
    }

    /// Names of the saved set for the "↺ Restore last" row, when no pane is
    /// up and the layout still names configured cameras.
    fn restore_names(&self) -> Option<String> {
        if self.supp.count() > 0 {
            return None;
        }
        let main = self.main_host()?;
        let names: Vec<String> = saved_layouts(&main)
            .iter()
            .filter_map(|l| self.cam_index(&l.host).map(|i| self.cams[i].name.clone()))
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    }

    fn pick_pane(&mut self, idx: usize) {
        let Some(main) = self.main_host() else {
            return;
        };
        self.pane_selector = None;
        self.attach_panes(&main);
        let host = self.cams[idx].host.clone();
        self.add_pane(&host, None);
    }

    fn pick_restore(&mut self) {
        let Some(main) = self.main_host() else {
            return;
        };
        self.pane_selector = None;
        self.attach_panes(&main);
        self.restore_panes();
    }

    /// The selector owns the keyboard while up.
    pub fn pane_selector_key(&mut self, ev: &egui::Event) {
        let Some(sel) = &self.pane_selector else {
            return;
        };
        let filter = sel.filter.clone();
        let pool = self.visible_entries(&filter);
        let sel = self.pane_selector.as_mut().unwrap();
        let mut move_sel = |delta: i64| {
            if pool.is_empty() {
                return;
            }
            let next = sel
                .sel
                .map_or(if delta > 0 { -delta } else { 0 }, |s| s as i64)
                + delta;
            sel.sel = Some(next.clamp(0, pool.len() as i64 - 1) as usize);
        };
        match ev {
            egui::Event::Key {
                key, pressed: true, ..
            } => match key {
                egui::Key::ArrowLeft => move_sel(-1),
                egui::Key::ArrowRight => move_sel(1),
                egui::Key::ArrowUp => move_sel(-(SELECTOR_COLS as i64)),
                egui::Key::ArrowDown => move_sel(SELECTOR_COLS as i64),
                egui::Key::Backspace => {
                    if !sel.filter.is_empty() {
                        sel.filter.pop();
                        sel.sel = None;
                    }
                }
                egui::Key::Enter => {
                    // Return: the selection, else the top enabled match.
                    match sel.sel.filter(|&s| s < pool.len()) {
                        Some(s) if pool[s].enabled => {
                            let idx = pool[s].idx;
                            self.pick_pane(idx);
                        }
                        Some(s) => {
                            let msg = if pool[s].note == Some("added") {
                                "Already added"
                            } else {
                                "No recording"
                            };
                            self.flash(msg);
                        }
                        None => {
                            if let Some(e) = pool.iter().find(|e| e.enabled) {
                                let idx = e.idx;
                                self.pick_pane(idx);
                            }
                        }
                    }
                }
                _ => {}
            },
            egui::Event::Text(t) => {
                let mut changed = false;
                for c in t.chars() {
                    let c = c.to_ascii_lowercase();
                    if c.is_alphanumeric() || c == ' ' || c == '-' {
                        sel.filter.push(c);
                        changed = true;
                    }
                }
                if changed {
                    sel.sel = None;
                }
            }
            _ => {}
        }
    }

    /// Esc: clear the filter, then close.
    pub fn pane_selector_escape(&mut self) {
        if let Some(sel) = &mut self.pane_selector
            && !sel.filter.is_empty()
        {
            sel.filter.clear();
            sel.sel = None;
        } else {
            self.pane_selector = None;
        }
    }

    pub fn show_pane_selector(&mut self, ctx: &egui::Context) {
        let Some(sel) = &self.pane_selector else {
            return;
        };
        let (filter, cursor) = (sel.filter.clone(), sel.sel);
        let pool = self.visible_entries(&filter);
        let restore = if filter.is_empty() {
            self.restore_names()
        } else {
            None
        };
        let screen = ctx.viewport_rect();
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("pane selector backdrop"),
        ))
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(90));
        let mut picked: Option<usize> = None;
        let mut restore_picked = false;
        let area = egui::Area::new(egui::Id::new("pane selector"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 235))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(if filter.is_empty() {
                                    "type to filter · Esc closes".to_string()
                                } else {
                                    format!("filter: {filter}")
                                })
                                .monospace()
                                .size(11.0)
                                .color(egui::Color32::from_gray(180)),
                            );
                            ui.add_space(4.0);
                            if let Some(names) = &restore
                                && ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(format!("↺ Restore last: {names}"))
                                                .size(12.0)
                                                .strong()
                                                .color(egui::Color32::from_rgb(41, 189, 204)),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                            {
                                restore_picked = true;
                            }
                        });
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        for (row_i, row) in pool.chunks(SELECTOR_COLS).enumerate() {
                            ui.horizontal(|ui| {
                                for (col_i, e) in row.iter().enumerate() {
                                    let i = row_i * SELECTOR_COLS + col_i;
                                    let cam = &self.cams[e.idx];
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(116.0, THUMB.y + 20.0),
                                        egui::Sense::click(),
                                    );
                                    let alpha = if e.enabled { 255 } else { 90 };
                                    let painter = ui.painter();
                                    let thumb = egui::Rect::from_min_size(
                                        egui::pos2(rect.center().x - THUMB.x / 2.0, rect.min.y),
                                        THUMB,
                                    );
                                    painter.rect_filled(thumb, 4.0, egui::Color32::from_gray(36));
                                    if let Some((tex, _)) = &cam.placeholder {
                                        // Cover-fit crop of the snapshot.
                                        let s = tex.size_vec2();
                                        let scale = (THUMB.x / s.x).max(THUMB.y / s.y);
                                        let shown = THUMB / (s * scale);
                                        let uv = egui::Rect::from_center_size(
                                            egui::pos2(0.5, 0.5),
                                            shown,
                                        );
                                        painter.image(
                                            tex.id(),
                                            thumb,
                                            uv,
                                            egui::Color32::from_white_alpha(alpha),
                                        );
                                    }
                                    let mut title = cam.name.clone();
                                    if let Some(n) = e.note {
                                        title.push_str(&format!(" · {n}"));
                                    }
                                    painter.text(
                                        egui::pos2(rect.center().x, thumb.max.y + 4.0),
                                        egui::Align2::CENTER_TOP,
                                        title,
                                        egui::FontId::proportional(10.0),
                                        egui::Color32::from_white_alpha(alpha),
                                    );
                                    // Arrow-key cursor: a red border, grid-cursor style.
                                    if cursor == Some(i) {
                                        painter.rect_stroke(
                                            rect.expand(2.0),
                                            6.0,
                                            egui::Stroke::new(2.0, tile::CURSOR_RED),
                                            egui::StrokeKind::Outside,
                                        );
                                    }
                                    if resp.clicked() && e.enabled {
                                        picked = Some(e.idx);
                                    }
                                }
                            });
                        }
                        if pool.is_empty() {
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new("no match").color(tile::DIM));
                            });
                        }
                    });
            });
        if let Some(idx) = picked {
            self.pick_pane(idx);
            return;
        }
        if restore_picked {
            self.pick_restore();
            return;
        }
        // A click on the dimmed backdrop (outside the panel) closes.
        if ctx.input(|i| i.pointer.any_pressed())
            && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
            && !area.response.rect.contains(p)
        {
            self.pane_selector = None;
        }
    }

    // MARK: promote / back

    /// Double-click on a pane: open that camera as a plain standard view at
    /// the same moment (no panes there, adding disabled), with a way back.
    fn promote_pane(&mut self, host: &str) {
        if self.promoted_origin.is_some() {
            return;
        }
        let Some(cur) = self.focused.as_ref().map(|f| f.idx) else {
            return;
        };
        let Some(target) = self.cam_index(host) else {
            return;
        };
        if target == cur {
            return;
        }
        let was_playback = self.focused.as_ref().is_some_and(|f| f.playback.is_some());
        let position = self
            .focused
            .as_ref()
            .and_then(|f| f.playback.as_ref())
            .map(|pb| pb.position());
        // The promoted view counts as its origin for "where you left off":
        // record the origin's state (playback and panes still live here) so
        // a quit from the promoted view reopens the origin, never this view.
        let origin_host = self.cams[cur].host.clone();
        self.save_view_state(&origin_host, position, was_playback);
        session::save(session::Location::Camera, Some(&origin_host));
        self.promoted_origin = Some((cur, was_playback, position));
        self.supp.teardown(); // saves the layout for the way back
        let ctx = self.ctx.clone();
        self.unfocus_quiet();
        self.focus_with(target, &ctx, false);
        // focus_with recorded the promoted view; the origin stays on record.
        session::save(session::Location::Camera, Some(&origin_host));
        if was_playback {
            self.enter_playback(position);
        }
    }

    /// Esc / the back arrow on a promoted view: return to the origin with
    /// its panes and playback.
    pub fn go_back_from_promoted(&mut self) {
        let Some((idx, was_playback, position)) = self.promoted_origin.take() else {
            return;
        };
        let ctx = self.ctx.clone();
        self.unfocus_quiet();
        if idx >= self.cams.len() {
            return;
        }
        self.focus_with(idx, &ctx, false);
        let host = self.cams[idx].host.clone();
        self.attach_panes(&host);
        self.restore_panes();
        if was_playback {
            self.enter_playback(position);
        }
    }

    // MARK: drawing

    /// Draw the panes over the focused view (called after the video and its
    /// label, before the playback bar, which must stay clear).
    pub fn show_panes(&mut self, ui: &mut egui::Ui, tile: egui::Rect) {
        if self.supp.panes.is_empty() {
            return;
        }
        let bottom_inset = if self.focused.as_ref().is_some_and(|f| f.playback.is_some()) {
            timeline::BAR_HEIGHT + 6.0
        } else {
            0.0
        };
        let playback_active = self.supp.playback_active;
        let mut close: Option<usize> = None;
        let mut promote: Option<String> = None;
        let mut raise: Option<usize> = None;
        let mut persist = false;
        for i in 0..self.supp.panes.len() {
            let host = self.supp.panes[i].host.clone();
            let cam_idx = self.cam_index(&host);
            let norm = self.supp.panes[i].norm;
            let rect = clamped(to_pixels(norm, tile), tile, bottom_inset);
            let id = egui::Id::new(("pane", &host));
            let resp = ui.interact(rect, id, egui::Sense::CLICK | egui::Sense::DRAG);
            let close_r = close_rect(rect);
            let close_resp = ui.interact(close_r, id.with("close"), egui::Sense::CLICK);

            // Drag: move from the interior, resize from the edges/corners.
            let pane = &mut self.supp.panes[i];
            if resp.drag_started()
                && let Some(p) = resp.interact_pointer_pos()
            {
                pane.drag = Some(Drag {
                    edges: resize_edges(rect, p),
                    origin: p,
                    start: rect,
                });
                raise = Some(i);
            }
            let mut shown = rect;
            if let Some(d) = &pane.drag
                && let Some(p) = ui.input(|i| i.pointer.interact_pos())
            {
                let delta = p - d.origin;
                let r = if d.edges.any() {
                    let mut r = d.start;
                    if d.edges.e {
                        r.max.x += delta.x;
                    }
                    if d.edges.w {
                        r.min.x += delta.x;
                    }
                    if d.edges.s {
                        r.max.y += delta.y;
                    }
                    if d.edges.n {
                        r.min.y += delta.y;
                    }
                    // Clamp size first, then re-anchor: dragging one edge
                    // must keep the opposite edge fixed even when the
                    // min/max clamp kicks in.
                    let size = clamp_size(r.size(), tile);
                    let min = egui::pos2(
                        if d.edges.w {
                            d.start.max.x - size.x
                        } else {
                            r.min.x
                        },
                        if d.edges.n {
                            d.start.max.y - size.y
                        } else {
                            r.min.y
                        },
                    );
                    egui::Rect::from_min_size(min, size)
                } else {
                    d.start.translate(delta)
                };
                shown = clamped(r, tile, bottom_inset);
                pane.norm = to_norm(shown, tile);
                if resp.drag_stopped() {
                    pane.drag = None;
                    persist = true;
                }
            }
            if resp.dragged() {
                ui.ctx()
                    .set_cursor_icon(if pane.drag.as_ref().is_some_and(|d| d.edges.any()) {
                        cursor_for(pane.drag.as_ref().unwrap().edges)
                    } else {
                        egui::CursorIcon::Grabbing
                    });
            } else if let Some(p) = resp.hover_pos()
                && !close_r.expand(4.0).contains(p)
            {
                ui.ctx().set_cursor_icon(cursor_for(resize_edges(rect, p)));
            }
            if resp.double_clicked() {
                promote = Some(host.clone());
            }
            if close_resp.clicked() {
                close = Some(i);
            }
            let rect = shown;

            // Picture: the pane's own pipe in playback, else the live
            // substream tap; the snapshot until either has a frame (dimmed
            // in playback, where it shows "now" rather than the moment).
            let painter = ui.painter().with_clip_rect(rect);
            painter.rect_filled(rect, 6.0, egui::Color32::BLACK);
            let inner = rect.shrink(2.0);
            let pic: Option<(u64, Arc<stream::Shared>)> = match (&pane.feed, cam_idx) {
                (Some(f), Some(ci)) if f.shared.current.lock().unwrap().is_some() => {
                    Some((self.cams[ci].id | PANE_BIT, f.shared.clone()))
                }
                (Some(_), Some(ci)) if pane.frozen.is_some() => {
                    Some((self.cams[ci].id | PANE_BIT, pane.frozen.clone().unwrap()))
                }
                (None, Some(ci))
                    if !playback_active
                        && self.cams[ci].shared.current.lock().unwrap().is_some() =>
                {
                    Some((self.cams[ci].id, self.cams[ci].shared.clone()))
                }
                _ => None,
            };
            if let Some((id, shared)) = pic {
                let r = tile::fit(inner, tile::frame_dims(&shared));
                painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
                    r,
                    render::VideoCallback {
                        id,
                        shared,
                        uv: render::VideoCallback::FULL,
                    },
                ));
            } else if let Some((tex, cached)) =
                cam_idx.and_then(|ci| self.cams[ci].placeholder.as_ref())
            {
                tile::draw_placeholder(&painter, inner, tex, *cached || playback_active);
            }
            // Chrome: a clearly visible boundary that doubles as the grab
            // target for moving/resizing.
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(2.0, egui::Color32::from_white_alpha(180)),
                egui::StrokeKind::Inside,
            );
            let name = cam_idx.map_or(host.clone(), |ci| self.cams[ci].name.clone());
            let text = match &pane.note {
                Some(n) => format!("{name} · {n}"),
                None => name,
            };
            tile::label(
                &painter,
                rect.min + egui::vec2(6.0, 6.0),
                egui::Align2::LEFT_TOP,
                &text,
                egui::FontId::proportional(10.0),
                tile::WHITE,
            );
            // ✕ on a dark disc — a bare white ✕ vanishes over white-ish video.
            painter.circle_filled(
                close_r.center(),
                CLOSE_SIZE / 2.0,
                egui::Color32::from_black_alpha(140),
            );
            painter.text(
                close_r.center(),
                egui::Align2::CENTER_CENTER,
                "✕",
                egui::FontId::proportional(11.0),
                egui::Color32::from_white_alpha(230),
            );
        }
        if let Some(i) = close {
            self.supp.remove(i);
        } else if let Some(i) = raise
            && i + 1 < self.supp.panes.len()
        {
            let p = self.supp.panes.remove(i);
            self.supp.panes.push(p);
        }
        if persist {
            self.supp.persist();
        }
        if let Some(host) = promote {
            self.promote_pane(&host);
        }
    }
}
