// bookmarks.rs — bookmarked playback moments (Bookmarks.swift port): the
// on-disk store, the naming prompt shown when B is pressed in playback, and
// the Shift-B list pane (selector-style: type to filter, arrows + red
// cursor, Return jumps, delete removes with confirmation).
//
// bookmarks.json sits next to the config in the Mac app's format (its
// Codable keys, dates as seconds since 2001), so both apps share it on
// macOS. UI state only, no credentials, not part of export/import.

use crate::{App, session, tile};
use chrono::{DateTime, Utc};
use eframe::egui;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    pub id: String,
    pub host: String,
    /// Kept so rows stay readable if the camera is renamed/removed.
    pub camera_name: String,
    /// Playback position (absolute; shown in the NVR's timezone).
    pub time: f64,
    pub label: String,
    pub created: f64,
}

impl Bookmark {
    pub fn at(&self) -> DateTime<Utc> {
        session::from_apple(self.time)
    }
}

pub struct Store {
    pub all: Vec<Bookmark>,
    /// Bumped on every change so open views refresh their pins.
    pub version: u64,
}

fn path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("bookmarks.json")
}

impl Store {
    pub fn load() -> Store {
        let all = std::fs::read(path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        Store { all, version: 0 }
    }

    pub fn add(&mut self, b: Bookmark) {
        self.all.push(b);
        self.persist();
    }

    pub fn remove(&mut self, id: &str) {
        self.all.retain(|b| b.id != id);
        self.persist();
    }

    fn persist(&mut self) {
        self.version += 1;
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(&self.all) {
            let _ = std::fs::write(p, data);
        }
    }

    /// This camera's bookmarks within [from, to), for the timeline pins.
    pub fn pins(&self, host: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        self.all
            .iter()
            .filter(|b| b.host == host)
            .map(Bookmark::at)
            .filter(|t| *t >= from && *t < to)
            .collect()
    }
}

/// A UUID-shaped id, like the Mac's.
fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let pid = u64::from(std::process::id());
    format!(
        "{:08X}-{:04X}-4{:03X}-{:04X}-{:012X}",
        (nanos >> 32) as u32,
        (nanos >> 16) as u16,
        (nanos & 0xFFF) as u16,
        (pid & 0xFFFF) as u16,
        nanos.rotate_left(17) & 0xFFFF_FFFF_FFFF
    )
}

/// "Sat 2026-09-05 8:10:05 AM" in the NVR's zone (the row formatter).
pub fn row_time(client: Option<&crate::nvr::Client>, t: DateTime<Utc>) -> String {
    match client {
        Some(c) => c.local(t).format("%a %Y-%m-%d %-I:%M:%S %p").to_string(),
        None => t
            .with_timezone(&chrono::Local)
            .format("%a %Y-%m-%d %-I:%M:%S %p")
            .to_string(),
    }
}

/// The B prompt: a small panel over the focused view asking for an optional
/// bookmark name. Return saves (empty is fine — the camera + timestamp
/// identify the moment), Esc or a click outside cancels.
pub struct Prompt {
    pub host: String,
    pub camera_name: String,
    pub time: DateTime<Utc>,
    pub name: String,
    fresh: bool,
}

/// The Shift-B pane: all bookmarks, newest first, selector-style.
pub struct Pane {
    pub filter: String,
    pub sel: Option<usize>,
    /// A delete awaiting confirmation (the bookmark's id).
    pub confirm: Option<String>,
}

const MAX_ROWS: usize = 12;
const AMBER: egui::Color32 = egui::Color32::from_rgb(255, 191, 51);

impl App {
    /// B in playback: pause (a bookmark is a precise moment), then ask for
    /// an optional name over the focused view.
    pub fn prompt_bookmark(&mut self) {
        if self.bookmark_prompt.is_some() || self.bookmark_pane.is_some() {
            return;
        }
        let Some(f) = &mut self.focused else {
            return;
        };
        let Some(pb) = &mut f.playback else {
            return;
        };
        if !pb.is_paused() {
            pb.toggle_pause();
        }
        let cam = &self.cams[f.idx];
        self.bookmark_prompt = Some(Prompt {
            host: cam.host.clone(),
            camera_name: cam.name.clone(),
            time: pb.position(),
            name: String::new(),
            fresh: true,
        });
    }

    pub fn toggle_bookmark_pane(&mut self) {
        if self.bookmark_pane.is_some() {
            self.bookmark_pane = None;
            return;
        }
        if self.bookmark_prompt.is_some() {
            return;
        }
        self.help_open = false;
        self.close_event_pane();
        self.bookmark_pane = Some(Pane {
            filter: String::new(),
            sel: None,
            confirm: None,
        });
    }

    fn nvr_client(&self) -> Option<&crate::nvr::Client> {
        match &self.nvr {
            crate::NvrState::Ready(c) => Some(c),
            _ => None,
        }
    }

    pub fn show_bookmark_prompt(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.bookmark_prompt else {
            return;
        };
        let subtitle = format!(
            "{} · {}",
            p.camera_name,
            row_time(
                match &self.nvr {
                    crate::NvrState::Ready(c) => Some(c.as_ref()),
                    _ => None,
                },
                p.time
            )
        );
        let field_id = egui::Id::new("bookmark name");
        let mut done: Option<bool> = None;
        let resp = egui::Window::new("New Bookmark")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(&subtitle)
                            .monospace()
                            .size(11.0)
                            .color(tile::DIM),
                    );
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut p.name)
                            .id(field_id)
                            .hint_text("name (optional)")
                            .desired_width(260.0),
                    );
                    if p.fresh {
                        p.fresh = false;
                        field.request_focus();
                    }
                    ui.small("Return saves · Esc cancels");
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
            });
        // A click outside the panel cancels.
        if let Some(r) = &resp
            && ctx.input(|i| i.pointer.any_pressed())
            && let Some(pos) = ctx.input(|i| i.pointer.interact_pos())
            && !r.response.rect.contains(pos)
        {
            done = Some(false);
        }
        match done {
            Some(true) => {
                let p = self.bookmark_prompt.take().unwrap();
                self.bookmarks.add(Bookmark {
                    id: new_id(),
                    host: p.host,
                    camera_name: p.camera_name,
                    time: session::to_apple(p.time),
                    label: p.name.trim().to_string(),
                    created: session::to_apple(Utc::now()),
                });
                self.flash("Bookmarked");
            }
            Some(false) => self.bookmark_prompt = None,
            None => {}
        }
    }

    /// Rows matching the pane's filter, newest first.
    fn bookmark_rows(&self, filter: &str) -> Vec<Bookmark> {
        let client = self.nvr_client();
        let mut rows: Vec<Bookmark> = self.bookmarks.all.clone();
        rows.sort_by(|a, b| {
            b.created
                .partial_cmp(&a.created)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if filter.is_empty() {
            return rows;
        }
        rows.retain(|b| {
            format!("{} {} {}", b.camera_name, b.label, row_time(client, b.at()))
                .to_lowercase()
                .contains(filter)
        });
        rows
    }

    /// Keys while the pane is up: arrows move, Return jumps, Backspace edits
    /// the filter (or deletes with an empty one), Delete always deletes,
    /// printable characters filter. Esc goes through the app's escape chain.
    pub fn bookmark_pane_key(&mut self, ev: &egui::Event) {
        let Some(filter) = self.bookmark_pane.as_ref().map(|p| p.filter.clone()) else {
            return;
        };
        let rows = self.bookmark_rows(&filter);
        let visible = rows.len().min(MAX_ROWS);
        let Some(pane) = &mut self.bookmark_pane else {
            return;
        };
        if let Some(id) = pane.confirm.clone() {
            // The confirmation sheet: Return deletes, Esc (chain) cancels.
            if let egui::Event::Key {
                key: egui::Key::Enter,
                pressed: true,
                ..
            } = ev
            {
                pane.confirm = None;
                self.bookmarks.remove(&id);
            }
            return;
        }
        match ev {
            egui::Event::Key {
                key, pressed: true, ..
            } => match key {
                egui::Key::ArrowUp | egui::Key::ArrowLeft => {
                    if visible > 0 {
                        pane.sel =
                            Some(pane.sel.map_or(0, |s| s.saturating_sub(1)).min(visible - 1));
                    }
                }
                egui::Key::ArrowDown | egui::Key::ArrowRight => {
                    if visible > 0 {
                        pane.sel = Some(pane.sel.map_or(0, |s| s + 1).min(visible - 1));
                    }
                }
                egui::Key::Enter => {
                    let pick = pane.sel.or(if visible > 0 { Some(0) } else { None });
                    if let Some(i) = pick
                        && let Some(b) = rows.get(i).cloned()
                    {
                        self.bookmark_pane = None;
                        self.jump_to_bookmark(&b);
                    }
                }
                egui::Key::Delete => {
                    if let Some(i) = pane.sel
                        && let Some(b) = rows.get(i)
                    {
                        pane.confirm = Some(b.id.clone());
                    }
                }
                egui::Key::Backspace => {
                    if !pane.filter.is_empty() {
                        pane.filter.pop();
                        pane.sel = None;
                    } else if let Some(i) = pane.sel
                        && let Some(b) = rows.get(i)
                    {
                        pane.confirm = Some(b.id.clone());
                    }
                }
                _ => {}
            },
            egui::Event::Text(t) => {
                for c in t.chars() {
                    let c = c.to_ascii_lowercase();
                    if c.is_alphanumeric() || matches!(c, ' ' | '-' | ':' | '.') {
                        pane.filter.push(c);
                        pane.sel = None;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn show_bookmark_pane(&mut self, ctx: &egui::Context) {
        if self.bookmark_pane.is_none() {
            return;
        }
        let (filter, sel, confirm) = {
            let p = self.bookmark_pane.as_ref().unwrap();
            (p.filter.clone(), p.sel, p.confirm.clone())
        };
        let rows = self.bookmark_rows(&filter);
        let client = self.nvr_client();
        let visible: Vec<&Bookmark> = rows.iter().take(MAX_ROWS).collect();
        let screen = ctx.viewport_rect();
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("bookmark backdrop"),
        ))
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(90));
        let mut picked: Option<Bookmark> = None;
        let area = egui::Area::new(egui::Id::new("bookmark pane"))
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
                        bottom: 14,
                    })
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("Bookmarks")
                                    .size(13.0)
                                    .strong()
                                    .color(tile::WHITE),
                            );
                            ui.label(
                                egui::RichText::new(if filter.is_empty() {
                                    "type to filter · Return jumps · ⌫ deletes · Esc closes"
                                        .to_string()
                                } else {
                                    format!("filter: {filter}")
                                })
                                .monospace()
                                .size(11.0)
                                .color(tile::DIM),
                            );
                            ui.add_space(6.0);
                        });
                        ui.spacing_mut().item_spacing.y = 4.0;
                        for (i, b) in visible.iter().enumerate() {
                            let mut job = egui::text::LayoutJob::default();
                            job.append(
                                &format!(" {}  ", b.camera_name),
                                0.0,
                                egui::TextFormat {
                                    font_id: egui::FontId::proportional(12.0),
                                    color: tile::WHITE,
                                    ..Default::default()
                                },
                            );
                            job.append(
                                &row_time(client, b.at()),
                                0.0,
                                egui::TextFormat {
                                    font_id: egui::FontId::monospace(12.0),
                                    color: egui::Color32::from_gray(180),
                                    ..Default::default()
                                },
                            );
                            if !b.label.is_empty() {
                                job.append(
                                    &format!("  {}", b.label),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::proportional(12.0),
                                        color: AMBER,
                                        ..Default::default()
                                    },
                                );
                            }
                            job.append(" ", 0.0, egui::TextFormat::default());
                            let mut button = egui::Button::new(job).frame(false).corner_radius(5.0);
                            // Same red-border cursor as the grid and the selector.
                            if sel == Some(i) {
                                button = button.stroke(egui::Stroke::new(2.0, tile::CURSOR_RED));
                            }
                            if ui.add(button).clicked() {
                                picked = Some((*b).clone());
                            }
                        }
                        if rows.is_empty() {
                            ui.label(
                                egui::RichText::new(if filter.is_empty() {
                                    "no bookmarks yet — press B in playback"
                                } else {
                                    "no match"
                                })
                                .color(tile::DIM),
                            );
                        } else if rows.len() > visible.len() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "… {} more — type to narrow",
                                    rows.len() - visible.len()
                                ))
                                .size(10.0)
                                .color(tile::DIM),
                            );
                        }
                    });
            });
        if let Some(id) = confirm {
            let detail = rows
                .iter()
                .find(|b| b.id == id)
                .map(|b| {
                    let mut d = format!("{} · {}", b.camera_name, row_time(client, b.at()));
                    if !b.label.is_empty() {
                        d.push_str(&format!(" · {}", b.label));
                    }
                    d
                })
                .unwrap_or_default();
            let mut decision: Option<bool> = None;
            // Above the pane's Foreground area, or the sheet hides behind it.
            egui::Window::new("Delete bookmark?")
                .order(egui::Order::Tooltip)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(detail);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Delete").clicked() {
                                decision = Some(true);
                            }
                            if ui.button("Cancel").clicked() {
                                decision = Some(false);
                            }
                        });
                    });
                });
            match decision {
                Some(true) => {
                    self.bookmarks.remove(&id);
                    if let Some(p) = &mut self.bookmark_pane {
                        p.confirm = None;
                    }
                }
                Some(false) => {
                    if let Some(p) = &mut self.bookmark_pane {
                        p.confirm = None;
                    }
                }
                None => {}
            }
            return;
        }
        if let Some(b) = picked {
            self.bookmark_pane = None;
            self.jump_to_bookmark(&b);
            return;
        }
        // A click on the dimmed backdrop (outside the panel) closes.
        if ctx.input(|i| i.pointer.any_pressed())
            && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
            && !area.response.rect.contains(p)
        {
            self.bookmark_pane = None;
        }
    }

    /// Open the bookmark's camera in playback at its moment — the same
    /// programmatic navigation the Mac's promote/back flows use.
    pub fn jump_to_bookmark(&mut self, b: &Bookmark) {
        let Some(target) = self.cams.iter().position(|c| c.host == b.host) else {
            self.flash("Camera no longer configured");
            return;
        };
        let at = b.at();
        if self.focused.as_ref().is_some_and(|f| f.idx == target) {
            match self.focused.as_mut().and_then(|f| f.playback.as_mut()) {
                Some(pb) => pb.seek(at),
                None => self.enter_playback(Some(at)),
            }
            return;
        }
        let ctx = self.ctx.clone();
        self.unfocus();
        self.focus_with(target, &ctx, false);
        self.enter_playback(Some(at));
    }
}
