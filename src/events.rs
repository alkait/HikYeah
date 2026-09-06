//! Shift-E: the centralized intrusion review (EventListPane.swift). One pane
//! listing a whole day's intrusion events across every camera — from the
//! same all-channel alarm-log crawl the playback timeline uses — newest
//! first. Selector-style: type to filter, ↑↓ move the red cursor, ←→ step
//! days, Return jumps into that camera's playback at the event with the
//! intrusion band up. Events carry a seen marker — jumping to one marks it,
//! ⌫/⌦ toggles it in place — so reviewed events dim and new ones stand out.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use chrono::{DateTime, TimeDelta, Utc};
use eframe::egui;

use crate::nvr::{self, EventLog};
use crate::{App, NvrState, tile};

/// Which intrusion events have been reviewed. Keys are "channel|startEpoch"
/// — stable across fetches (span starts come from the NVR's
/// fieldDetectionStart log entries). Local UI state like bookmarks: no
/// credentials, not part of export/import.
pub struct SeenStore {
    keys: HashSet<String>,
}

fn seen_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("seen-events.json")
}

impl SeenStore {
    pub fn load() -> SeenStore {
        let list: Vec<String> = std::fs::read(seen_path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        // Prune events older than 90 days (long past NVR retention) so the
        // file doesn't grow forever — the key's tail is the event epoch.
        let cutoff = Utc::now().timestamp() - 90 * 86_400;
        let keys = list
            .into_iter()
            .filter(|k| {
                k.rsplit('|')
                    .next()
                    .and_then(|e| e.parse::<i64>().ok())
                    .is_some_and(|e| e > cutoff)
            })
            .collect();
        SeenStore { keys }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    pub fn mark_seen(&mut self, key: &str) {
        if self.keys.insert(key.to_string()) {
            self.persist();
        }
    }

    pub fn toggle(&mut self, key: &str) {
        if !self.keys.remove(key) {
            self.keys.insert(key.to_string());
        }
        self.persist();
    }

    fn persist(&self) {
        let p = seen_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let list: Vec<&String> = self.keys.iter().collect();
        if let Ok(data) = serde_json::to_vec(&list) {
            let _ = std::fs::write(p, data);
        }
    }
}

#[derive(Clone)]
pub struct Row {
    pub camera_name: String,
    /// None: the channel has no configured camera.
    pub host: Option<String>,
    pub channel: u32,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Row {
    pub fn key(&self) -> String {
        format!("{}|{}", self.channel, self.start.timestamp())
    }
}

/// What the pane remembers between opens: the day, the cursored event and
/// the filter, so a review resumes where it stopped.
#[derive(Default)]
pub struct Memo {
    pub day: Option<DateTime<Utc>>,
    pub key: Option<String>,
    pub filter: String,
}

pub struct Pane {
    /// Start of the displayed day (NVR zone); None until the NVR is ready.
    day: Option<DateTime<Utc>>,
    filter: String,
    sel: Option<usize>,
    /// Selection carried across reopen, resolved once the rows land.
    pending_key: Option<String>,
    rows: Vec<Row>,
    /// The rows after the filter, as drawn (cursor indices refer to it).
    visible: Vec<Row>,
    loading: bool,
    message: Option<String>,
    /// The current day's delivery; a new load replaces it, dropping stale
    /// ones. May deliver twice (cached, then fresh).
    rx: Option<Receiver<Arc<EventLog>>>,
    /// Neighbour warm-ups after the shown day lands (once per load).
    prefetch: bool,
    /// Keep the cursor row in view after a key move.
    scroll_to_sel: bool,
}

/// Viewport cap (~12 rows) — beyond it the list scrolls (arrows follow).
const MAX_LIST_HEIGHT: f32 = 300.0;

/// Filter → (include, exclude) terms. Space-separated terms must all match;
/// a "-" prefix excludes instead ("porch -outside"). Double quotes group
/// words into one exact phrase — "front door", -"outside right" — so an
/// exclusion is provably that term, not two loose words. A bare "-" is
/// inert, and an unclosed quote runs to the end of the filter (the phrase
/// just narrows live as it's typed).
fn parse_filter(filter: &str) -> (Vec<String>, Vec<String>) {
    let (mut include, mut exclude) = (Vec::new(), Vec::new());
    let chars: Vec<char> = filter.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            i += 1;
            continue;
        }
        let mut negated = false;
        if chars[i] == '-' {
            negated = true;
            i += 1;
        }
        let mut term = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                term.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1; // closing quote
            }
        } else {
            while i < chars.len() && chars[i] != ' ' {
                term.push(chars[i]);
                i += 1;
            }
        }
        if term.is_empty() {
            continue;
        }
        if negated {
            exclude.push(term);
        } else {
            include.push(term);
        }
    }
    (include, exclude)
}

fn duration_text(secs: i64) -> String {
    let sec = secs.max(1);
    if sec < 60 {
        return format!("{sec}s");
    }
    let m = sec / 60;
    if m < 60 {
        return format!("{m}m {:02}s", sec % 60);
    }
    format!("{}h {:02}m", m / 60, m % 60)
}

impl Pane {
    fn new() -> Pane {
        Pane {
            day: None,
            filter: String::new(),
            sel: None,
            pending_key: None,
            rows: Vec::new(),
            visible: Vec::new(),
            loading: false,
            message: Some("connecting to NVR…".into()),
            rx: None,
            prefetch: false,
            scroll_to_sel: false,
        }
    }

    /// The cursored event, read back at close so the next open re-selects it.
    fn selected_key(&self) -> Option<String> {
        match self.sel {
            Some(s) if s < self.visible.len() => Some(self.visible[s].key()),
            _ => self.pending_key.clone(),
        }
    }

    fn today(client: &nvr::Client) -> DateTime<Utc> {
        client.start_of_day(Utc::now())
    }

    /// The NVR is prepared: open on the remembered (or today's) day and
    /// restore the remembered selection and filter.
    fn ready(&mut self, client: &Arc<nvr::Client>, memo: &Memo) {
        self.filter = memo.filter.clone();
        let today = Self::today(client);
        self.day = Some(memo.day.unwrap_or(today).min(today));
        self.pending_key = memo.key.clone();
        self.load_day(client);
    }

    pub fn fail(&mut self, msg: &str) {
        self.loading = false;
        self.message = Some(msg.into());
    }

    fn load_day(&mut self, client: &Arc<nvr::Client>) {
        let Some(day) = self.day else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        client.event_log(day, day + TimeDelta::days(1), tx);
        self.rx = Some(rx);
        self.rows.clear();
        self.message = None;
        self.loading = true;
        self.prefetch = true;
    }

    /// Warm the adjacent days once the shown day has landed: all LAN-local,
    /// and the client caches per day (sharing any in-flight crawl), so ←/→
    /// stepping is instant and a continuous walk stays one day ahead.
    fn prefetch_neighbors(&self, client: &Arc<nvr::Client>) {
        let Some(day) = self.day else {
            return;
        };
        let today = Self::today(client);
        for delta in [-1, 1] {
            let d = day + TimeDelta::days(delta);
            if d <= today {
                let (tx, _rx) = std::sync::mpsc::channel();
                client.event_log(d, d + TimeDelta::days(1), tx);
            }
        }
    }

    fn show_day(&mut self, client: &Arc<nvr::Client>, target: DateTime<Utc>) {
        if self.day == Some(target) {
            return;
        }
        self.day = Some(target);
        self.sel = None;
        self.pending_key = None;
        self.load_day(client);
    }

    fn step_day(&mut self, client: &Arc<nvr::Client>, delta: i64) {
        let Some(cur) = self.day else {
            return;
        };
        // Never into the future.
        let next = (cur + TimeDelta::days(delta)).min(Self::today(client));
        self.show_day(client, next);
    }

    fn jump_to_today(&mut self, client: &Arc<nvr::Client>) {
        if self.day.is_some() {
            self.show_day(client, Self::today(client));
        }
    }

    fn matching(&self, client: &nvr::Client) -> Vec<Row> {
        let (include, exclude) = parse_filter(&self.filter);
        if include.is_empty() && exclude.is_empty() {
            return self.rows.clone();
        }
        self.rows
            .iter()
            .filter(|r| {
                let hay =
                    format!("{} {}", r.camera_name, time_text(client, r.start)).to_lowercase();
                if exclude.iter().any(|t| hay.contains(t.as_str())) {
                    return false;
                }
                include.iter().all(|t| hay.contains(t.as_str()))
            })
            .cloned()
            .collect()
    }

    fn move_selection(&mut self, delta: i64) {
        if self.visible.is_empty() {
            return;
        }
        let next = self
            .sel
            .map_or(if delta > 0 { -1 } else { 0 }, |s| s as i64)
            + delta;
        self.sel = Some(next.clamp(0, self.visible.len() as i64 - 1) as usize);
        self.pending_key = None;
        self.scroll_to_sel = true;
    }

    fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.sel = None;
        self.pending_key = None;
    }
}

fn time_text(client: &nvr::Client, t: DateTime<Utc>) -> String {
    client.local(t).format("%-I:%M:%S %p").to_string()
}

impl App {
    /// Shift-E: the intrusion pane, from any context (plain E stays the
    /// band selector in playback).
    pub fn toggle_event_pane(&mut self) {
        if self.event_pane.is_some() {
            self.close_event_pane();
            return;
        }
        if self.bookmark_prompt.is_some() {
            return;
        }
        self.help_open = false;
        self.bookmark_pane = None;
        let mut pane = Pane::new();
        if self.config.as_ref().and_then(|c| c.nvr.as_ref()).is_none() {
            pane.fail("no NVR in Settings (Ctrl-,)");
            self.event_pane = Some(pane);
            return;
        }
        self.event_pane = Some(pane);
        self.ensure_nvr();
        self.poll_event_pane();
    }

    pub fn close_event_pane(&mut self) {
        let Some(pane) = self.event_pane.take() else {
            return;
        };
        self.event_memo = Memo {
            day: pane.day,
            key: pane.selected_key(),
            filter: pane.filter,
        };
    }

    /// Adopt the NVR client once it lands, pull delivered rows, and warm the
    /// neighbours. Runs every frame while the pane is up (cheap: two
    /// try_recv-class checks).
    pub fn poll_event_pane(&mut self) {
        let NvrState::Ready(client) = &self.nvr else {
            return;
        };
        let client = client.clone();
        let Some(pane) = &mut self.event_pane else {
            return;
        };
        if pane.day.is_none() && pane.message.is_some() && !pane.loading {
            // First frame with a client: "connecting" → open the day.
            let memo = std::mem::take(&mut self.event_memo);
            pane.ready(&client, &memo);
            self.event_memo = memo;
        }
        let Some(rx) = &pane.rx else {
            return;
        };
        let mut delivered = None;
        loop {
            match rx.try_recv() {
                Ok(log) => delivered = Some(log),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    pane.rx = None;
                    break;
                }
            }
        }
        let Some(log) = delivered else {
            return;
        };
        // One eventLog crawl covers every channel — the same (cached) crawl
        // the playback timeline uses, so pane and timeline never duplicate
        // work. Unmatched channels still show (as "channel N") rather than
        // silently vanishing.
        let mut rows: Vec<Row> = log
            .intrusion
            .iter()
            .flat_map(|(&ch, spans)| {
                let host = client
                    .channel_by_host
                    .iter()
                    .find(|(_, c)| **c == ch)
                    .map(|(h, _)| h.clone());
                let name = host
                    .as_ref()
                    .map(|h| {
                        self.cams
                            .iter()
                            .find(|c| &c.host == h)
                            .map_or_else(|| h.clone(), |c| c.name.clone())
                    })
                    .unwrap_or_else(|| format!("channel {ch}"));
                spans.iter().map(move |s| Row {
                    camera_name: name.clone(),
                    host: host.clone(),
                    channel: ch,
                    start: s.start,
                    end: s.end,
                })
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.start));
        pane.loading = false;
        pane.rows = rows;
        if std::mem::take(&mut pane.prefetch) {
            pane.prefetch_neighbors(&client);
        }
    }

    /// The pane owns the keyboard while up.
    pub fn event_pane_key(&mut self, ev: &egui::Event) {
        let NvrState::Ready(client) = &self.nvr else {
            // Not ready: only the close paths work (Esc via the chain).
            if let (
                Some(pane),
                egui::Event::Key {
                    key: egui::Key::E,
                    pressed: true,
                    modifiers,
                    ..
                },
            ) = (&self.event_pane, ev)
                && modifiers.shift
                && pane.day.is_none()
            {
                self.close_event_pane();
            }
            return;
        };
        let client = client.clone();
        let Some(pane) = &mut self.event_pane else {
            return;
        };
        match ev {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => match key {
                egui::Key::ArrowUp => pane.move_selection(-1),
                egui::Key::ArrowDown => pane.move_selection(1),
                egui::Key::ArrowLeft => pane.step_day(&client, -1),
                egui::Key::ArrowRight => pane.step_day(&client, 1),
                // ⌦ always toggles seen; ⌫: filter editing wins, ⇧⌫
                // toggles seen even mid-filter.
                egui::Key::Delete => self.toggle_seen(),
                egui::Key::Backspace => {
                    if modifiers.shift || pane.filter.is_empty() {
                        self.toggle_seen();
                    } else {
                        let mut f = pane.filter.clone();
                        f.pop();
                        pane.set_filter(f);
                    }
                }
                egui::Key::Enter => {
                    let pick = pane.sel.filter(|&s| s < pane.visible.len()).or(
                        if pane.visible.is_empty() {
                            None
                        } else {
                            Some(0)
                        },
                    );
                    if let Some(i) = pick {
                        let row = pane.visible[i].clone();
                        self.jump_to_event(&row);
                    }
                }
                // Shifted letters never reach the filter (it matches
                // lowercased), so they are free to be commands: ⇧T = today,
                // ⇧E = the toggle that opened the pane closes it.
                egui::Key::T if modifiers.shift => pane.jump_to_today(&client),
                egui::Key::E if modifiers.shift => self.close_event_pane(),
                _ => {}
            },
            egui::Event::Text(t) => {
                let mut f = pane.filter.clone();
                let mut changed = false;
                for c in t.chars() {
                    // Shifted punctuation (the quote is ⇧') falls through.
                    if c.is_ascii_uppercase() {
                        continue;
                    }
                    let c = c.to_ascii_lowercase();
                    if c.is_alphanumeric() || matches!(c, ' ' | '-' | ':' | '.' | '"') {
                        f.push(c);
                        changed = true;
                    }
                }
                if changed {
                    pane.set_filter(f);
                }
            }
            _ => {}
        }
    }

    /// ⌫/⌦: flip the cursored event's seen mark without watching it —
    /// dismissing the trivial ones keeps the unseen count honest.
    fn toggle_seen(&mut self) {
        let Some(pane) = &self.event_pane else {
            return;
        };
        if let Some(s) = pane.sel
            && let Some(r) = pane.visible.get(s)
        {
            self.seen.toggle(&r.key());
        }
    }

    /// Esc: clear the filter, then close.
    pub fn event_pane_escape(&mut self) {
        if let Some(pane) = &mut self.event_pane
            && !pane.filter.is_empty()
        {
            pane.set_filter(String::new());
        } else {
            self.close_event_pane();
        }
    }

    /// Open the event's camera in playback at the event start with the
    /// intrusion band up, so N/Shift-N continue through that camera's
    /// events. Same programmatic navigation as jumpToBookmark; jumping marks
    /// it seen.
    fn jump_to_event(&mut self, row: &Row) {
        self.seen.mark_seen(&row.key());
        self.close_event_pane();
        let Some(target) = row
            .host
            .as_ref()
            .and_then(|h| self.cams.iter().position(|c| &c.host == h))
        else {
            self.flash("Camera not configured in HikYeah");
            return;
        };
        // A fresh playback reads the band from the pref in enter_playback;
        // an already-open one switches directly.
        self.prefs.event_band = crate::playback::Band::Intrusion.name().into();
        self.prefs.save();
        let at = row.start;
        if self.focused.as_ref().is_some_and(|f| f.idx == target) {
            match self.focused.as_mut().and_then(|f| f.playback.as_mut()) {
                Some(pb) => {
                    let (human, vehicle) = (pb.human, pb.vehicle);
                    pb.apply_events(crate::playback::Band::Intrusion, human, vehicle);
                    pb.seek(at);
                }
                None => self.enter_playback(Some(at)),
            }
            return;
        }
        let ctx = self.ctx.clone();
        self.unfocus();
        self.focus_with(target, &ctx, false);
        self.enter_playback(Some(at));
    }

    pub fn show_event_pane(&mut self, ctx: &egui::Context) {
        if self.event_pane.is_none() {
            return;
        }
        let client = match &self.nvr {
            NvrState::Ready(c) => Some(c.clone()),
            _ => None,
        };
        let pane = self.event_pane.as_mut().unwrap();
        pane.visible = client
            .as_deref()
            .map(|c| pane.matching(c))
            .unwrap_or_default();
        if pane.sel.is_none()
            && let Some(k) = &pane.pending_key
            && let Some(i) = pane.visible.iter().position(|r| &r.key() == k)
        {
            pane.sel = Some(i);
            pane.scroll_to_sel = true;
        }
        if let Some(s) = pane.sel
            && s >= pane.visible.len()
        {
            pane.sel = if pane.visible.is_empty() {
                None
            } else {
                Some(pane.visible.len() - 1)
            };
        }
        let title = match (&client, pane.day) {
            (Some(c), Some(d)) => format!("Intrusion — {}", c.local(d).format("%a %Y-%m-%d")),
            _ => "Intrusion".to_string(),
        };
        let info = if !pane.filter.is_empty() {
            format!("filter: {}", pane.filter)
        } else if pane.loading {
            "loading events…".to_string()
        } else if pane.message.is_some() || pane.rows.is_empty() {
            String::new()
        } else {
            let unseen = pane
                .rows
                .iter()
                .filter(|r| !self.seen.contains(&r.key()))
                .count();
            let n = pane.rows.len();
            let events = format!("{n} event{}", if n == 1 { "" } else { "s" });
            if unseen > 0 {
                format!("{events} · {unseen} unseen")
            } else {
                format!("{events} · all seen")
            }
        };

        let screen = ctx.viewport_rect();
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("event backdrop"),
        ))
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(90));
        let mut picked: Option<Row> = None;
        let scroll_to = std::mem::take(&mut pane.scroll_to_sel)
            .then_some(pane.sel)
            .flatten();
        let area = egui::Area::new(egui::Id::new("event pane"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 235))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 14,
                        bottom: 12,
                    })
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new(&title).size(13.0).strong().color(tile::WHITE));
                            if !info.is_empty() {
                                ui.label(
                                    egui::RichText::new(&info)
                                        .monospace()
                                        .size(11.0)
                                        .color(egui::Color32::from_gray(180)),
                                );
                            }
                            ui.add_space(6.0);
                        });
                        ui.spacing_mut().item_spacing.y = 4.0;
                        egui::ScrollArea::vertical()
                            .max_height(MAX_LIST_HEIGHT)
                            .show(ui, |ui| {
                                for (i, r) in pane.visible.iter().enumerate() {
                                    let seen = self.seen.contains(&r.key());
                                    let mut job = egui::text::LayoutJob::default();
                                    // Unseen: intrusion-orange dot, full
                                    // brightness. Seen: the dot fades to a
                                    // placeholder (alignment holds), text dims.
                                    // Monospace (Hack) has the symbols the
                                    // proportional face lacks.
                                    job.append(
                                        " ● ",
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::monospace(12.0),
                                            color: if seen {
                                                egui::Color32::from_white_alpha(30)
                                            } else {
                                                crate::timeline::INTRUSION
                                            },
                                            ..Default::default()
                                        },
                                    );
                                    job.append(
                                        &format!("{}  ", r.camera_name),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::proportional(12.0),
                                            color: if seen {
                                                egui::Color32::from_white_alpha(115)
                                            } else {
                                                tile::WHITE
                                            },
                                            ..Default::default()
                                        },
                                    );
                                    job.append(
                                        &client.as_deref().map(|c| time_text(c, r.start)).unwrap_or_default(),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::monospace(12.0),
                                            color: egui::Color32::from_white_alpha(if seen { 90 } else { 180 }),
                                            ..Default::default()
                                        },
                                    );
                                    job.append(
                                        &format!("  {} ", duration_text((r.end - r.start).num_seconds())),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::monospace(11.0),
                                            color: egui::Color32::from_white_alpha(if seen { 75 } else { 140 }),
                                            ..Default::default()
                                        },
                                    );
                                    let mut button = egui::Button::new(job).frame(false).corner_radius(5.0);
                                    // Same red-border cursor as the grid and the bookmark list.
                                    if pane.sel == Some(i) {
                                        button = button.stroke(egui::Stroke::new(2.0, tile::CURSOR_RED));
                                    }
                                    let resp = ui.add(button);
                                    if scroll_to == Some(i) {
                                        resp.scroll_to_me(None);
                                    }
                                    if resp.clicked() {
                                        picked = Some(r.clone());
                                    }
                                }
                                let empty = if let Some(msg) = &pane.message {
                                    Some(msg.as_str())
                                } else if pane.visible.is_empty() && !pane.loading {
                                    Some(if pane.filter.is_empty() {
                                        "no intrusion events this day"
                                    } else {
                                        "no match"
                                    })
                                } else {
                                    None
                                };
                                if let Some(text) = empty {
                                    ui.vertical_centered(|ui| {
                                        ui.label(egui::RichText::new(text).color(tile::DIM));
                                    });
                                }
                            });
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    "↵ open · ⇧⌫ seen · ←→ day · ⇧T today · type to filter (-x, -\"a b\" exclude) · esc",
                                )
                                .monospace()
                                .size(10.0)
                                .color(tile::DIM),
                            );
                        });
                    });
            });
        if let Some(r) = picked {
            self.jump_to_event(&r);
            return;
        }
        // A click on the dimmed backdrop (outside the panel) closes.
        if ctx.input(|i| i.pointer.any_pressed())
            && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
            && !area.response.rect.contains(p)
        {
            self.close_event_pane();
        }
    }

    /// Warm today's (and yesterday's) all-channel event log in the
    /// background as soon as the app has an NVR: the Shift-E pane's first
    /// open and the playback timeline then start from cache instead of a
    /// cold multi-second crawl. Failures are silent — the pane's own ladder
    /// reports problems. Runs on a worker thread; the UI never waits on it.
    pub fn warm_event_log(&mut self) {
        let NvrState::Ready(client) = &self.nvr else {
            return;
        };
        let today = client.start_of_day(Utc::now());
        for day in [today, today - TimeDelta::days(1)] {
            let (tx, _rx): (Sender<Arc<EventLog>>, _) = std::sync::mpsc::channel();
            client.event_log(day, day + TimeDelta::days(1), tx);
        }
    }
}
