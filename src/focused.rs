// focused.rs — the full-window main-stream view with digital zoom
// (TileView.swift's zoom: 1–8× toward the pointer, drag to pan, a badge
// that resets). Zoom is a crop: the shader samples a sub-rectangle of the
// frame, because egui clamps a paint callback's viewport to the screen.

use crate::{App, Focused, MAIN_BIT, render, tile, timeline};
use eframe::egui;

const MAX_ZOOM: f32 = 8.0;
/// Tile id bit for the playback picture (one slot; the pipes behind it
/// come and go with every seek).
const PLAYBACK_BIT: u64 = 1 << 33;

impl Focused {
    pub fn zoomed(&self) -> bool {
        self.zoom > 1.001
    }

    pub fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.center = egui::vec2(0.5, 0.5);
    }

    /// The video's rect at the current zoom, clamped so no edge pans into
    /// view (an axis smaller than the view stays centered). `center` is
    /// re-derived from the clamped result so state and picture never
    /// disagree. `base` is the 1× aspect-fit rect.
    fn video_rect(&mut self, avail: egui::Rect, base: egui::Rect) -> egui::Rect {
        let size = base.size() * self.zoom;
        let c = avail.center();
        let mut min = egui::pos2(c.x - self.center.x * size.x, c.y - self.center.y * size.y);
        min.x = if size.x >= avail.width() {
            min.x.clamp(avail.max.x - size.x, avail.min.x)
        } else {
            c.x - size.x / 2.0
        };
        min.y = if size.y >= avail.height() {
            min.y.clamp(avail.max.y - size.y, avail.min.y)
        } else {
            c.y - size.y / 2.0
        };
        self.center = egui::vec2((c.x - min.x) / size.x, (c.y - min.y) / size.y);
        egui::Rect::from_min_size(min, size)
    }

    /// Zoom toward `at` (None = view center): the video point under it
    /// stays put while the scale changes.
    fn set_zoom(
        &mut self,
        avail: egui::Rect,
        base: egui::Rect,
        target: f32,
        at: Option<egui::Pos2>,
    ) {
        let new = target.clamp(1.0, MAX_ZOOM);
        let cur = self.video_rect(avail, base);
        let p = at.unwrap_or(avail.center());
        let q = egui::vec2(
            (p.x - cur.min.x) / cur.width(),
            (p.y - cur.min.y) / cur.height(),
        );
        let size = base.size() * new;
        let min = egui::pos2(p.x - q.x * size.x, p.y - q.y * size.y);
        self.zoom = new;
        let c = avail.center();
        self.center = egui::vec2((c.x - min.x) / size.x, (c.y - min.y) / size.y);
    }

    fn pan(&mut self, base: egui::Rect, delta: egui::Vec2) {
        let size = base.size() * self.zoom;
        self.center -= egui::vec2(delta.x / size.x, delta.y / size.y);
    }
}

impl App {
    pub fn show_focused(&mut self, ui: &mut egui::Ui, avail: egui::Rect) {
        let Some(f) = &mut self.focused else {
            return;
        };
        let cam = &self.cams[f.idx];
        let status = match (&f.playback, &f.note) {
            (Some(pb), _) => pb.status(),
            (None, Some(n)) => n.clone(),
            (None, None) => f.main.stats.lock().unwrap().status.clone(),
        };

        // Playback's picture (the last frame stays up across seeks), else
        // the main stream once it has a frame on screen; the substream's
        // picture as a stand-in while either connects (the Mac app's
        // cached-frame trick), and the snapshot placeholder before even that.
        let playback_pic = f
            .playback
            .as_ref()
            .map(|pb| pb.shown.clone())
            .filter(|s| s.current.lock().unwrap().is_some());
        let main_showing = f.playback.is_none() && f.main.current.lock().unwrap().is_some();
        let (id, shared) = if let Some(s) = playback_pic {
            (cam.id | PLAYBACK_BIT, s)
        } else if main_showing {
            (cam.id | MAIN_BIT, f.main.clone())
        } else {
            (cam.id, cam.shared.clone())
        };
        let resp = ui.interact(
            avail,
            egui::Id::new("focused"),
            egui::Sense::CLICK | egui::Sense::DRAG,
        );
        // The playback bar owns the pointer over its strip.
        let bar = egui::Rect::from_min_max(
            egui::pos2(avail.min.x, avail.max.y - timeline::BAR_HEIGHT),
            avail.max,
        );
        let over_bar = f.playback.is_some() && resp.hover_pos().is_some_and(|p| bar.contains(p));
        if let Some(dims) = tile::frame_dims(&shared) {
            let base = tile::fit(avail, Some(dims));
            // Pinch (or Ctrl+wheel) and the plain wheel both zoom toward
            // the pointer (wheel up = in, like a map); a double-click
            // toggles a quick 2× at that spot.
            let (pinch, wheel) = if over_bar {
                (1.0, 0.0)
            } else {
                ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta().y))
            };
            if pinch != 1.0 {
                f.set_zoom(avail, base, f.zoom * pinch, resp.hover_pos());
            } else if wheel != 0.0 {
                f.set_zoom(
                    avail,
                    base,
                    f.zoom * 2f32.powf(-wheel / 200.0),
                    resp.hover_pos(),
                );
            }
            if resp.double_clicked() {
                if f.zoomed() {
                    f.reset_zoom();
                } else {
                    f.set_zoom(avail, base, 2.0, resp.interact_pointer_pos());
                }
            }
            if f.zoomed() && resp.dragged() {
                f.pan(base, resp.drag_delta());
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            let video = f.video_rect(avail, base);
            let visible = video.intersect(avail);
            let uv = egui::Rect::from_min_max(
                egui::pos2(
                    (visible.min.x - video.min.x) / video.width(),
                    (visible.min.y - video.min.y) / video.height(),
                ),
                egui::pos2(
                    (visible.max.x - video.min.x) / video.width(),
                    (visible.max.y - video.min.y) / video.height(),
                ),
            );
            ui.painter()
                .add(eframe::egui_wgpu::Callback::new_paint_callback(
                    visible,
                    render::VideoCallback { id, shared, uv },
                ));
            if let Some(o) = &self.overlay
                && o.host == cam.host
            {
                crate::zones::paint(ui.painter(), video, avail, o);
            }
        } else if let Some((tex, cached)) = &cam.placeholder {
            tile::draw_placeholder(ui.painter(), avail, tex, *cached);
        }

        // Camera-shutter flash for snapshots: 0.35 s fade from 70% white.
        if let Some(at) = self.flash_at {
            let t = at.elapsed().as_secs_f32() / 0.35;
            if t < 1.0 {
                ui.painter().rect_filled(
                    avail,
                    0.0,
                    egui::Color32::from_white_alpha((178.0 * (1.0 - t)) as u8),
                );
                ui.ctx().request_repaint_after(crate::REPAINT_COALESCE);
            } else {
                self.flash_at = None;
            }
        }

        tile::label(
            ui.painter(),
            avail.left_top() + egui::vec2(10.0, 8.0),
            egui::Align2::LEFT_TOP,
            &format!("{} — {}", cam.name, status),
            egui::FontId::proportional(12.0),
            tile::WHITE,
        );

        // Supplementary panes sit over the video, under the bar and badges.
        let zoomed = f.zoomed();
        let in_playback = f.playback.is_some();
        self.show_panes(ui, avail);
        // The translucent back arrow shows only when Esc's next action would
        // leave the promoted view (not zoomed, not in playback).
        if self.promoted_origin.is_some() && !in_playback && !zoomed {
            let r = egui::Rect::from_min_size(
                avail.left_top() + egui::vec2(6.0, 30.0),
                egui::vec2(28.0, 28.0),
            );
            let resp = ui.interact(r, egui::Id::new("promoted back"), egui::Sense::CLICK);
            let alpha = if resp.hovered() { 230 } else { 165 };
            ui.painter()
                .circle_filled(r.center(), 14.0, egui::Color32::from_white_alpha(alpha));
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                "←",
                egui::FontId::monospace(16.0),
                egui::Color32::BLACK,
            );
            if resp.on_hover_text("Back — Esc").clicked() {
                self.go_back_from_promoted();
                return;
            }
        }
        let Some(f) = &mut self.focused else {
            return;
        };

        if let Some(pb) = &mut f.playback
            && pb.show_bar(ui, avail)
        {
            self.prefs.playback_speed = pb.speed;
            self.prefs.save();
        }

        // Top-right badges: REC with elapsed time, then the zoom level.
        let rec = self
            .recorder
            .as_ref()
            .map(|r| r.started.elapsed().as_secs());
        if rec.is_some() || f.zoomed() {
            let mut reset = false;
            egui::Area::new(egui::Id::new("focused badges"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-6.0, 6.0))
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        if let Some(s) = rec {
                            egui::Frame::NONE
                                .fill(egui::Color32::from_black_alpha(140))
                                .corner_radius(3.0)
                                .inner_margin(egui::Margin::symmetric(5, 2))
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    ui.label(
                                        egui::RichText::new("●").size(11.0).color(tile::CURSOR_RED),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "REC {}:{:02}",
                                            s / 60,
                                            s % 60
                                        ))
                                        .size(11.0)
                                        .strong()
                                        .monospace()
                                        .color(tile::WHITE),
                                    );
                                });
                            ui.ctx()
                                .request_repaint_after(std::time::Duration::from_secs(1));
                        }
                        if f.zoomed()
                            && ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(format!(" {:.1}× ✕ ", f.zoom))
                                            .size(10.0)
                                            .strong()
                                            .color(tile::WHITE),
                                    )
                                    .fill(egui::Color32::from_black_alpha(140))
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(3.0),
                                )
                                .on_hover_text("Reset zoom — Esc")
                                .clicked()
                        {
                            reset = true;
                        }
                    });
                });
            if reset {
                f.reset_zoom();
            }
        }
    }
}
