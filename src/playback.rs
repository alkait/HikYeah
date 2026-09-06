// playback.rs — recorded-footage playback for the focused camera
// (PlaybackController.swift port).
//
// The NVR paces playback RTSP (at `speed`×, via the Scale header), so frames
// flow through the same ffmpeg -> y4m -> renderer pipeline as live, fed by
// the native RTSP client (rtsp.rs). Everything time-shaped happens here:
// seek = kill the pipe, relaunch at the new starttime; pause = kill the
// pipe, keep the last frame; position = starttime + wall-clock-since-first-
// frame × speed. The Mac does this with completion closures on the main
// thread; here the fetches run on threads and `poll` (once per UI frame)
// collects their results and runs what was waiting on them.

use crate::nvr::{self, Client, EventLog, Segment};
use crate::{rtsp, stream};
use chrono::{DateTime, Datelike, Months, NaiveDate, TimeDelta, Utc};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Timeline zoom: preset windows into the day; resets to 24h per day.
pub const ZOOM_LEVELS: [(i64, &str); 4] =
    [(86400, "24h"), (21600, "6h"), (3600, "1h"), (600, "10m")];
const DAY: i64 = 86400;

/// One playback pipe: the native RTSP session feeding an ffmpeg decoder.
struct Live {
    shared: Arc<stream::Shared>,
    session: rtsp::Session,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.session.stop();
        self.shared.stop();
    }
}

/// What to do once a segment fetch lands (the Mac's completion closures).
enum After {
    Start { at: DateTime<Utc>, retried: bool },
    Ended { pos: DateTime<Utc> },
}

struct Fetch {
    day: DateTime<Utc>,
    rx: Receiver<Vec<Segment>>,
    after: After,
}

/// A month's recorded days on the way: (year, month) and the result.
type MonthFetch = ((i32, u32), Receiver<HashSet<u32>>);

/// Which event type the playback timeline highlights (the E selector).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Band {
    None,
    Motion,
    Intrusion,
}

impl Band {
    pub const ALL: [Band; 3] = [Band::None, Band::Motion, Band::Intrusion];

    pub fn name(self) -> &'static str {
        match self {
            Band::None => "none",
            Band::Motion => "motion",
            Band::Intrusion => "intrusion",
        }
    }

    pub fn from_name(s: &str) -> Band {
        match s {
            "none" => Band::None,
            "intrusion" => Band::Intrusion,
            _ => Band::Motion,
        }
    }
}

/// Visible-window event counts on the way: (token, motion, intrusion).
type CountFetch = (u32, Receiver<(Option<usize>, Option<usize>)>);

/// The band and its motion filters — one global choice shared across
/// cameras, like speed.
#[derive(Clone, Copy)]
pub struct EventChoice {
    pub band: Band,
    pub human: bool,
    pub vehicle: bool,
}

/// The E dialog: transactional — ←→ move, ↑↓ switch rows, Space flips a
/// toggle, Return applies everything and dismisses, Esc cancels. Clicking a
/// band cell applies immediately (with the pending toggles).
pub struct Selector {
    pub band_cursor: usize,
    pub on_filters: bool,
    pub filter_cursor: usize,
    pub human: bool,
    pub vehicle: bool,
    /// Counts of events intersecting the visible window; None while fetching.
    pub motion_count: Option<usize>,
    pub intrusion_count: Option<usize>,
    counts: Option<CountFetch>,
    counts_token: u32,
}

/// Calendar popover state (MonthCalendarView + the controller's month cache).
pub struct Calendar {
    pub open: bool,
    /// First of the displayed month (NVR-local date).
    pub anchor: NaiveDate,
    cache: HashMap<(i32, u32), HashSet<u32>>,
    pending: Option<MonthFetch>,
    /// Keyboard cursor over the days; None until the first arrow press.
    pub cursor: Option<u32>,
    /// Set before a month step: -1 = land on the last day, else that day.
    pending_cursor: Option<i32>,
}

/// One month as the popover shows it.
pub struct MonthView {
    pub title: String,
    pub leading_blanks: u32,
    pub days_in_month: u32,
    pub enabled: HashSet<u32>,
    pub selected: Option<u32>,
    pub cursor: Option<u32>,
}

pub struct Playback {
    pub client: Arc<Client>,
    pub track: u32,
    codec: &'static str,
    decode: stream::Decode,
    ctx: egui::Context,
    /// What the view draws: the running pipe once it has a frame, else the
    /// previous one frozen on its last frame (the Mac's display layer keeps
    /// the last image across a seek or pause).
    pub shown: Arc<stream::Shared>,
    stream: Option<Live>,
    /// 1 / 2 / 4 ×, shared across cameras (prefs).
    pub speed: u32,
    /// Guards a fail-retry loop.
    last_start: Instant,
    pub zoom_index: usize,
    pub win_start: DateTime<Utc>,
    /// For the displayed day.
    pub segments: Vec<Segment>,
    /// Start of the displayed day (NVR tz).
    pub day: DateTime<Utc>,
    requested_start: DateTime<Utc>,
    /// Media time of the current pipe's first frame, and when it arrived.
    anchor: Option<(DateTime<Utc>, Instant)>,
    paused_at: Option<DateTime<Utc>>,
    /// A seek/pipe is spinning up (the bar's spinner).
    pub loading: bool,
    /// Tile status while no pipe is running ("no recording here"…).
    note: String,
    fetch: Option<Fetch>,
    pub cal: Calendar,
    /// Transport changed (position, paused) — the app persists it.
    pub transport: Option<(DateTime<Utc>, bool)>,
    /// A message for the HUD.
    pub hud: Option<String>,
    pub strip_input: crate::timeline::StripInput,
    /// Event highlights: the band and the motion filters (one global
    /// choice shared across cameras, like speed). For motion, neither
    /// filter on = all motion (alarm log); either on = only AcuSense-
    /// classified human/vehicle motion. Intrusion = the camera's
    /// fieldDetection events from the same alarm log.
    pub band: Band,
    pub human: bool,
    pub vehicle: bool,
    /// The band/filters changed — the app persists them.
    pub band_dirty: bool,
    /// What the timeline shows for the displayed day.
    pub event_spans: Vec<Segment>,
    pub events_loading: bool,
    event_token: u32,
    /// Spans on the way; may deliver twice (cached, then fresh).
    event_rx: Option<(u32, Receiver<Vec<Segment>>)>,
    pub selector: Option<Selector>,
    /// Bookmarked moments within the displayed day (timeline pins), and
    /// which (day, store version) they were computed for.
    pub pins: Vec<DateTime<Utc>>,
    pub pins_key: (i64, u64),
}

impl Playback {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<Client>,
        track: u32,
        codec: &'static str,
        decode: stream::Decode,
        ctx: egui::Context,
        shown: Arc<stream::Shared>,
        speed: u32,
        events: EventChoice,
    ) -> Self {
        let now = Utc::now();
        Playback {
            cal: Calendar {
                open: false,
                anchor: client.local(now).date_naive().with_day(1).unwrap(),
                cache: HashMap::new(),
                pending: None,
                cursor: None,
                pending_cursor: None,
            },
            client,
            track,
            codec,
            decode,
            ctx,
            shown,
            stream: None,
            speed,
            last_start: Instant::now() - Duration::from_secs(60),
            zoom_index: 0,
            win_start: now,
            segments: Vec::new(),
            day: now,
            requested_start: now,
            anchor: None,
            paused_at: None,
            loading: false,
            note: "loading recordings…".into(),
            fetch: None,
            transport: None,
            hud: None,
            strip_input: Default::default(),
            band: events.band,
            human: events.human,
            vehicle: events.vehicle,
            band_dirty: false,
            event_spans: Vec::new(),
            events_loading: false,
            event_token: 0,
            event_rx: None,
            selector: None,
            pins: Vec::new(),
            pins_key: (0, u64::MAX),
        }
    }

    pub fn begin(&mut self, start: DateTime<Utc>) {
        self.day = self.client.start_of_day(start);
        self.requested_start = start;
        self.reset_zoom();
        self.loading = true;
        self.fetch_segments(After::Start {
            at: start,
            retried: false,
        });
    }

    /// Once per UI frame: collect finished fetches, notice the pipe's first
    /// frame or its end, keep a zoomed window following the playhead.
    pub fn poll(&mut self) {
        if let Some(f) = &self.fetch
            && let Ok(segs) = f.rx.try_recv()
        {
            let Fetch { day, after, .. } = self.fetch.take().unwrap();
            if day == self.day {
                self.segments = segs;
                if self.segments.is_empty() {
                    self.note = "no recordings this day".into();
                }
                match after {
                    After::Start { at, retried } => self.start_playback(at, retried),
                    After::Ended { pos } => self.continue_after_end(pos),
                }
            }
        }
        if let Some(s) = &self.stream {
            if self.anchor.is_none() && s.shared.current.lock().unwrap().is_some() {
                self.anchor = Some((self.requested_start, Instant::now()));
                self.loading = false;
                self.shown = s.shared.clone();
            }
            if s.shared.ended() {
                let s = self.stream.take().unwrap();
                self.note = s.shared.stats.lock().unwrap().status.clone();
                self.stream_ended();
            }
        }
        self.poll_calendar();
        self.poll_events();

        // A zoomed window slides forward to keep the playing cursor in view.
        let pos = self.position();
        let dur = self.win_duration();
        if self.paused_at.is_none()
            && self.zoom_index > 0
            && pos > self.win_start + TimeDelta::seconds(dur * 9 / 10)
            && pos < self.day_end()
        {
            let max_start = self.day_end() - TimeDelta::seconds(dur);
            self.win_start = (pos - TimeDelta::seconds(dur / 10))
                .max(self.day)
                .min(max_start);
        }
        if self.paused_at.is_none() {
            self.ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Promote due frames of the running pipe (the frame loop's pump).
    pub fn advance(&self, now: Instant) -> Option<Instant> {
        self.stream.as_ref().and_then(|s| s.shared.advance(now))
    }

    pub fn status(&self) -> String {
        match &self.stream {
            Some(s) => s.shared.stats.lock().unwrap().status.clone(),
            None => self.note.clone(),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn day_end(&self) -> DateTime<Utc> {
        self.day + TimeDelta::seconds(DAY)
    }

    pub fn win_duration(&self) -> i64 {
        ZOOM_LEVELS[self.zoom_index].0
    }

    pub fn win_end(&self) -> DateTime<Utc> {
        self.win_start + TimeDelta::seconds(self.win_duration())
    }

    // MARK: transport

    pub fn seek(&mut self, t: DateTime<Utc>) {
        self.paused_at = None;
        if self.client.start_of_day(t) != self.day {
            self.day = self.client.start_of_day(t);
            self.reset_zoom();
            self.fetch_segments(After::Start {
                at: t,
                retried: false,
            });
        } else {
            self.start_playback(t, false);
        }
    }

    pub fn step(&mut self, seconds: i64) {
        self.seek(self.position() + TimeDelta::seconds(seconds));
    }

    /// YouTube-style digit jump: 0–9 → that tenth of the recorded span the
    /// user is *looking at* — the recordings clipped to the visible window.
    /// Zoomed out that's the whole day's footage; zoomed in, just that slice.
    pub fn jump_to_fraction(&mut self, f: f64) {
        let (ws, we) = (self.win_start, self.win_end());
        let visible: Vec<&Segment> = self
            .segments
            .iter()
            .filter(|s| s.end > ws && s.start < we)
            .collect();
        let (Some(first), Some(last)) = (visible.first(), visible.last()) else {
            return;
        };
        let lo = ws.max(first.start);
        let hi = we.min(last.end);
        let span = (hi - lo).num_milliseconds() as f64;
        self.seek(lo + TimeDelta::milliseconds((f * span) as i64));
    }

    /// T: jump to today's live edge, calendar open or closed. A HUD confirms
    /// either way ("Today" / "Already on today").
    pub fn jump_to_today(&mut self) {
        let now = Utc::now();
        if self.client.start_of_day(now) == self.day {
            self.hud = Some("Already on today".into());
            return;
        }
        self.cal.open = false;
        self.seek(now - TimeDelta::seconds(60)); // same "a minute back" as entering playback
        self.hud = Some("Today".into());
    }

    pub fn toggle_pause(&mut self) {
        if let Some(p) = self.paused_at {
            self.seek(p); // resume (seek clears paused_at)
        } else {
            let p = self.position();
            self.paused_at = Some(p); // freeze: last frame stays on screen
            self.stop_stream();
            self.loading = false;
            self.transport = Some((p, true));
        }
    }

    /// 1× → 2× → 4× → 1× (NVR fast playback via the Scale header).
    /// Returns the new speed for the caller to persist.
    pub fn cycle_speed(&mut self) -> u32 {
        self.speed = match self.speed {
            1 => 2,
            2 => 4,
            _ => 1,
        };
        if self.paused_at.is_none() {
            self.start_playback(self.position(), false);
        }
        self.speed
    }

    // MARK: timeline zoom

    fn reset_zoom(&mut self) {
        self.zoom_index = 0;
        self.win_start = self.day;
    }

    pub fn set_zoom(&mut self, index: isize, center: DateTime<Utc>) {
        self.zoom_index = index.clamp(0, ZOOM_LEVELS.len() as isize - 1) as usize;
        self.recenter(center);
    }

    pub fn cycle_zoom(&mut self) {
        let next = (self.zoom_index + 1) % ZOOM_LEVELS.len();
        self.set_zoom(next as isize, self.position());
    }

    fn recenter(&mut self, center: DateTime<Utc>) {
        let dur = self.win_duration();
        let max_start = self.day_end() - TimeDelta::seconds(dur);
        self.win_start = (center - TimeDelta::seconds(dur / 2))
            .max(self.day)
            .min(max_start);
    }

    /// Shift the window by `secs` (zoomed in only).
    pub fn pan(&mut self, secs: f64) {
        if self.zoom_index == 0 {
            return;
        }
        let dur = self.win_duration();
        let max_start = self.day_end() - TimeDelta::seconds(dur);
        self.win_start = (self.win_start + TimeDelta::milliseconds((secs * 1000.0) as i64))
            .max(self.day)
            .min(max_start);
    }

    /// Where playback is right now, in recording time.
    pub fn position(&self) -> DateTime<Utc> {
        if let Some(p) = self.paused_at {
            return p;
        }
        if let Some((t, wall)) = self.anchor {
            let elapsed = wall.elapsed().as_secs_f64() * f64::from(self.speed);
            return t + TimeDelta::milliseconds((elapsed * 1000.0) as i64);
        }
        self.requested_start
    }

    fn start_playback(&mut self, t: DateTime<Utc>, retried: bool) {
        self.stop_stream();
        // Snap into the recordings: the segment containing t, or the next one.
        let Some(seg) = self.segments.iter().find(|s| t < s.end).copied() else {
            if !retried {
                // The cached segment list ends at fetch time — resuming past
                // the old live edge (e.g. after a long pause) needs a refresh.
                self.fetch_segments(After::Start {
                    at: t,
                    retried: true,
                });
                return;
            }
            self.note = "no recording here".into();
            self.loading = false;
            self.paused_at = Some(t); // park the cursor where they clicked
            self.transport = Some((t, true));
            return;
        };
        let start = t.max(seg.start);
        if std::env::var_os("HIK_DEBUG").is_some() {
            eprintln!(
                "[playback] start {} .. {} (asked {}, speed {})",
                self.client.local(start).format("%F %T"),
                self.client.local(seg.end).format("%F %T"),
                self.client.local(t).format("%F %T"),
                self.speed
            );
        }
        // A zoomed window follows the seek target if it landed off-screen.
        if self.zoom_index > 0 && (start < self.win_start || start > self.win_end()) {
            self.recenter(start);
        }
        self.requested_start = start;
        self.last_start = Instant::now();
        self.anchor = None;
        self.loading = true;

        let (path, start_clock) = self.client.playback_request(self.track, start, seg.end);
        let ctx = self.ctx.clone();
        let (shared, sink) = stream::start_pipe(
            self.codec,
            self.decode.clone(),
            crate::REPAINT_COALESCE,
            move |d| ctx.request_repaint_after(d),
        );
        let status = shared.clone();
        let ctx = self.ctx.clone();
        let session = rtsp::start(
            rtsp::Request {
                host: self.client.nvr.host.clone(),
                port: self.client.nvr.rtsp_port(),
                user: self.client.nvr.user.clone(),
                password: self.client.nvr.password.clone(),
                path,
                start_clock,
                scale: self.speed,
                codec: self.codec,
            },
            sink,
            move |s| {
                status.set_status(s);
                ctx.request_repaint();
            },
        );
        self.stream = Some(Live { shared, session });
        self.transport = Some((start, false));
    }

    fn stop_stream(&mut self) {
        self.stream = None;
    }

    /// The pipe EOF'd: the segment played out, or we caught up with "now".
    /// Re-search (recordings grow), then continue past the gap — or stay
    /// paused at the end of what exists.
    fn stream_ended(&mut self) {
        if std::env::var_os("HIK_DEBUG").is_some() {
            eprintln!(
                "[playback] stream ended at {} (anchor {}, paused {}, since start {:?})",
                self.client.local(self.position()).format("%F %T"),
                self.anchor.is_some(),
                self.paused_at.is_some(),
                self.last_start.elapsed()
            );
        }
        if self.paused_at.is_some() {
            return;
        }
        // A pipe that died young without a single frame is a failure, not a
        // played-out segment — pause instead of retry-looping against it.
        if self.anchor.is_none() && self.last_start.elapsed() < Duration::from_secs(3) {
            let p = self.position();
            self.paused_at = Some(p);
            self.loading = false;
            self.transport = Some((p, true));
            return;
        }
        let pos = self.position();
        self.paused_at = Some(pos);
        self.fetch_segments(After::Ended { pos });
    }

    fn continue_after_end(&mut self, pos: DateTime<Utc>) {
        if self.paused_at.is_none() {
            return;
        }
        if let Some(next) = self
            .segments
            .iter()
            .find(|s| s.start > pos + TimeDelta::seconds(1))
        {
            self.seek(next.start);
        } else if self
            .segments
            .last()
            .is_some_and(|l| (l.end - pos).num_seconds() > 10)
        {
            self.seek(pos); // recording grew while we played — keep going
        } else {
            self.loading = false; // live edge / end of recorded video
            self.transport = Some((pos, true));
        }
    }

    fn fetch_segments(&mut self, after: After) {
        let (tx, rx) = channel();
        let client = self.client.clone();
        let (track, day, end) = (self.track, self.day, self.day_end());
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(client.search_segments(track, day, end));
            ctx.request_repaint();
        });
        self.fetch = Some(Fetch { day, rx, after });
        self.refresh_events();
        // Warm the day's event log now: the stitched crawl takes many
        // seconds on a busy day, and starting it at playback-open (instead
        // of when the selector or an intrusion switch first needs it) hides
        // that latency. Concurrent calls share one crawl.
        let (warm_tx, _) = channel();
        self.client.event_log(day, end, warm_tx);
    }

    // MARK: event highlights (E selector: none / motion / intrusion)

    fn channel(&self) -> u32 {
        self.track / 100
    }

    /// Fetch the displayed day's spans for the current band on a thread.
    /// The alarm log may answer twice (cached, then fresh); each answer is
    /// forwarded, and a token drops results from a superseded request.
    fn refresh_events(&mut self) {
        self.event_token += 1;
        let token = self.event_token;
        let (tx, rx) = channel();
        let client = self.client.clone();
        let (day, end, ch) = (self.day, self.day_end(), self.channel());
        let (band, human, vehicle) = (self.band, self.human, self.vehicle);
        let ctx = self.ctx.clone();
        self.events_loading = band != Band::None;
        if band == Band::None {
            self.event_spans.clear();
            self.event_rx = None;
            return;
        }
        std::thread::spawn(move || {
            if band == Band::Motion && (human || vehicle) {
                let mut union = Vec::new();
                for t in [(human, "human"), (vehicle, "vehicle")] {
                    if t.0 {
                        union.extend(client.classified_spans(ch, day, end, t.1).iter().copied());
                    }
                }
                let _ = tx.send(nvr::merge(union));
                ctx.request_repaint();
                return;
            }
            let (ltx, lrx) = channel();
            client.event_log(day, end, ltx);
            for log in lrx {
                let spans = match band {
                    Band::Intrusion => log.intrusion.get(&ch).cloned(),
                    _ => log.motion.get(&ch).cloned(),
                }
                .unwrap_or_default();
                if tx.send(spans).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        self.event_rx = Some((token, rx));
    }

    fn poll_events(&mut self) {
        if let Some((token, rx)) = &self.event_rx {
            let mut latest = None;
            let mut gone = false;
            loop {
                match rx.try_recv() {
                    Ok(spans) => latest = Some(spans),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
            if *token == self.event_token
                && let Some(spans) = latest
            {
                self.event_spans = spans;
                self.events_loading = false;
            }
            if gone {
                self.event_rx = None;
                self.events_loading = false;
            }
        }
        if let Some(sel) = &mut self.selector
            && let Some((token, rx)) = &sel.counts
            && let Ok((motion, intrusion)) = rx.try_recv()
        {
            if *token == sel.counts_token {
                if let Some(m) = motion {
                    sel.motion_count = Some(m);
                }
                if let Some(i) = intrusion {
                    sel.intrusion_count = Some(i);
                }
            }
            sel.counts = None;
        }
    }

    /// N: jump to the next event block after the current position — within
    /// the displayed day only; past the day's last block a HUD says so.
    pub fn jump_to_next_event(&mut self) {
        if self.band == Band::None {
            self.hud = Some("No events shown".into());
            return;
        }
        let pos = self.position();
        match self
            .event_spans
            .iter()
            .find(|s| s.start > pos + TimeDelta::seconds(1))
            .map(|s| s.start)
        {
            Some(t) => self.seek(t),
            None => {
                self.hud = Some(if self.events_loading {
                    "Loading events…".into()
                } else {
                    format!("No more {}", self.band.name())
                })
            }
        }
    }

    /// Shift-N: back to the previous event block (same day-bounded rules).
    pub fn jump_to_previous_event(&mut self) {
        if self.band == Band::None {
            self.hud = Some("No events shown".into());
            return;
        }
        let pos = self.position();
        match self
            .event_spans
            .iter()
            .rev()
            .find(|s| s.start < pos - TimeDelta::seconds(1))
            .map(|s| s.start)
        {
            Some(t) => self.seek(t),
            None => {
                self.hud = Some(if self.events_loading {
                    "Loading events…".into()
                } else {
                    format!("No earlier {}", self.band.name())
                })
            }
        }
    }

    pub fn toggle_selector(&mut self) {
        if self.selector.is_some() {
            self.selector = None;
            return;
        }
        self.selector = Some(Selector {
            band_cursor: Band::ALL.iter().position(|b| *b == self.band).unwrap_or(1),
            on_filters: false,
            filter_cursor: 0,
            human: self.human,
            vehicle: self.vehicle,
            motion_count: None,
            intrusion_count: None,
            counts: None,
            counts_token: 0,
        });
        self.fetch_counts();
    }

    /// Counts for the selector's cells: events intersecting the *visible*
    /// timeline window. The motion count follows the dialog's pending
    /// filter state.
    pub fn fetch_counts(&mut self) {
        let Some(sel) = &mut self.selector else {
            return;
        };
        sel.counts_token += 1;
        sel.motion_count = None;
        sel.intrusion_count = None;
        let token = sel.counts_token;
        let (human, vehicle) = (sel.human, sel.vehicle);
        let (tx, rx) = channel();
        let client = self.client.clone();
        let (day, end, ch) = (self.day, self.day_end(), self.channel());
        let (lo, hi) = (self.win_start, self.win_end());
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let clipped =
                |spans: &[Segment]| spans.iter().filter(|s| s.end > lo && s.start < hi).count();
            let motion = if human || vehicle {
                let mut union = Vec::new();
                for t in [(human, "human"), (vehicle, "vehicle")] {
                    if t.0 {
                        union.extend(client.classified_spans(ch, day, end, t.1).iter().copied());
                    }
                }
                Some(clipped(&nvr::merge(union)))
            } else {
                None
            };
            let (ltx, lrx) = channel();
            client.event_log(day, end, ltx);
            let mut last: Option<Arc<EventLog>> = None;
            for log in lrx {
                last = Some(log);
            }
            let (m, i) = match last {
                Some(log) => (
                    motion.or_else(|| Some(clipped(log.motion.get(&ch).map_or(&[][..], |v| v)))),
                    Some(clipped(log.intrusion.get(&ch).map_or(&[][..], |v| v))),
                ),
                None => (motion, None),
            };
            let _ = tx.send((m, i));
            ctx.request_repaint();
        });
        if let Some(sel) = &mut self.selector {
            sel.counts = Some((token, rx));
        }
    }

    /// Apply a band + filter choice (from the selector or a programmatic
    /// switch); only a real change clears and refetches.
    pub fn apply_events(&mut self, band: Band, human: bool, vehicle: bool) {
        let changed = band != self.band || human != self.human || vehicle != self.vehicle;
        self.band = band;
        self.human = human;
        self.vehicle = vehicle;
        self.band_dirty = true;
        self.selector = None;
        if changed {
            self.event_spans.clear();
            self.refresh_events();
        }
    }

    /// Keyboard on the open selector; true when the key was consumed.
    pub fn selector_key(&mut self, key: egui::Key, shift: bool) -> bool {
        let Some(sel) = &mut self.selector else {
            return false;
        };
        let _ = shift;
        match key {
            egui::Key::ArrowLeft | egui::Key::ArrowRight => {
                let d: isize = if key == egui::Key::ArrowLeft { -1 } else { 1 };
                if sel.on_filters {
                    sel.filter_cursor = (sel.filter_cursor as isize + d).clamp(0, 1) as usize;
                } else {
                    sel.band_cursor = (sel.band_cursor as isize + d).clamp(0, 2) as usize;
                }
            }
            egui::Key::ArrowDown => {
                if !sel.on_filters && Band::ALL[sel.band_cursor] == Band::Motion {
                    sel.on_filters = true;
                }
            }
            egui::Key::ArrowUp => sel.on_filters = false,
            egui::Key::Space => {
                if sel.on_filters {
                    if sel.filter_cursor == 0 {
                        sel.human = !sel.human;
                    } else {
                        sel.vehicle = !sel.vehicle;
                    }
                    self.fetch_counts();
                }
            }
            egui::Key::Enter => {
                let (band, human, vehicle) = (Band::ALL[sel.band_cursor], sel.human, sel.vehicle);
                self.apply_events(band, human, vehicle);
            }
            egui::Key::Escape | egui::Key::E => self.selector = None,
            _ => return false,
        }
        true
    }

    // MARK: calendar (recorded days per month via dailyDistribution)

    pub fn toggle_calendar(&mut self) {
        if self.cal.open {
            self.cal.open = false;
        } else {
            self.calendar_opened();
        }
    }

    fn calendar_opened(&mut self) {
        self.cal.open = true;
        self.cal.cursor = None;
        self.cal.pending_cursor = None;
        self.cal.anchor = self
            .client
            .local(self.day)
            .date_naive()
            .with_day(1)
            .unwrap();
        self.push_month();
    }

    pub fn step_month(&mut self, delta: i32) {
        let months = Months::new(delta.unsigned_abs());
        let Some(d) = (if delta < 0 {
            self.cal.anchor.checked_sub_months(months)
        } else {
            self.cal.anchor.checked_add_months(months)
        }) else {
            return;
        };
        self.cal.anchor = d;
        self.push_month();
    }

    /// Start of that day, snapping into its first recording.
    pub fn pick_day(&mut self, day_of_month: u32) {
        self.cal.open = false;
        if let Some(date) = self.cal.anchor.with_day(day_of_month) {
            self.seek(self.client.at_midnight(date));
        }
    }

    fn push_month(&mut self) {
        let key = (self.cal.anchor.year(), self.cal.anchor.month());
        if self.cal.cache.contains_key(&key)
            || self.cal.pending.as_ref().is_some_and(|p| p.0 == key)
        {
            return;
        }
        let (tx, rx) = channel();
        let client = self.client.clone();
        let track = self.track;
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(client.recorded_days(track, key.0, key.1));
            ctx.request_repaint();
        });
        self.cal.pending = Some((key, rx));
    }

    fn poll_calendar(&mut self) {
        if let Some((key, rx)) = &self.cal.pending
            && let Ok(days) = rx.try_recv()
        {
            self.cal.cache.insert(*key, days);
            self.cal.pending = None;
        }
    }

    /// The displayed month for the popover. Future days can't have
    /// recordings even if the NVR claims otherwise. Lands a pending keyboard
    /// cursor after a month step (MonthCalendarView.show).
    pub fn month_view(&mut self) -> MonthView {
        let anchor = self.cal.anchor;
        let days_in_month = anchor
            .checked_add_months(Months::new(1))
            .map_or(31, |n| (n - anchor).num_days() as u32);
        if let Some(p) = self.cal.pending_cursor.take() {
            self.cal.cursor = Some(if p == -1 {
                days_in_month
            } else {
                (p as u32).min(days_in_month)
            });
        } else if let Some(c) = self.cal.cursor
            && c > days_in_month
        {
            self.cal.cursor = Some(days_in_month);
        }
        let today = self.client.local(Utc::now()).date_naive();
        let mut enabled = self
            .cal
            .cache
            .get(&(anchor.year(), anchor.month()))
            .cloned()
            .unwrap_or_default();
        enabled.retain(|&d| anchor.with_day(d).is_some_and(|date| date <= today));
        let shown_day = self.client.local(self.day).date_naive();
        let same_month = shown_day.year() == anchor.year() && shown_day.month() == anchor.month();
        MonthView {
            title: anchor.format("%B %Y").to_string(),
            leading_blanks: anchor.weekday().num_days_from_sunday(),
            days_in_month,
            enabled,
            selected: same_month.then_some(shown_day.day()),
            cursor: self.cal.cursor,
        }
    }

    /// Arrows over the calendar: ±1 / ±7, crossing into the neighbouring
    /// month at the edges.
    pub fn move_calendar_cursor(&mut self, delta: i32) {
        let view = self.month_view();
        let next = view.cursor.or(view.selected).unwrap_or(1) as i32 + delta;
        if next < 1 {
            self.cal.pending_cursor = Some(-1);
            self.step_month(-1);
        } else if next > view.days_in_month as i32 {
            self.cal.pending_cursor = Some(1);
            self.step_month(1);
        } else {
            self.cal.cursor = Some(next as u32);
        }
    }

    /// Return over the calendar: pick the cursored day if it has recordings.
    pub fn calendar_return(&mut self) {
        let view = self.month_view();
        match view.cursor {
            Some(c) if view.enabled.contains(&c) => self.pick_day(c),
            _ => self.hud = Some("No recordings that day".into()),
        }
    }

    // MARK: labels

    /// "Sat 2026-09-05"
    pub fn day_label(&self) -> String {
        self.client
            .local(self.day)
            .format("%a %Y-%m-%d")
            .to_string()
    }

    /// "8:10:05 AM", prefixed "⏸ " while paused.
    pub fn clock_label(&self) -> String {
        let t = self.client.local(self.position()).format("%-I:%M:%S %p");
        if self.paused_at.is_some() {
            format!("⏸ {t}")
        } else {
            t.to_string()
        }
    }

    /// The playhead, when it falls inside the displayed day.
    pub fn cursor(&self) -> Option<DateTime<Utc>> {
        let pos = self.position();
        (pos >= self.day && pos < self.day_end()).then_some(pos)
    }
}

impl nvr::Client {
    /// Midnight of a NVR-local calendar date.
    pub fn at_midnight(&self, date: NaiveDate) -> DateTime<Utc> {
        use chrono::TimeZone;
        self.tz
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }
}
