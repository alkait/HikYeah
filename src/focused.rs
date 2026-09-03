// focused.rs — the full-window main-stream view with digital zoom
// (TileView.swift's zoom: 1–8× toward the pointer, drag to pan, a badge
// that resets). Zoom is a crop: the shader samples a sub-rectangle of the
// frame, because egui clamps a paint callback's viewport to the screen.

use crate::{App, Focused, MAIN_BIT, render, tile};
use eframe::egui;

const MAX_ZOOM: f32 = 8.0;

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
        let main_stats = f.main.stats.lock().unwrap().clone();

        // Main stream once it has a frame on screen; the substream's picture
        // as a stand-in while it connects (the Mac app's cached-frame trick),
        // and the snapshot placeholder before even that.
        let main_showing = f.main.current.lock().unwrap().is_some();
        let (id, shared) = if main_showing {
            (cam.id | MAIN_BIT, f.main.clone())
        } else {
            (cam.id, cam.shared.clone())
        };
        let resp = ui.interact(
            avail,
            egui::Id::new("focused"),
            egui::Sense::CLICK | egui::Sense::DRAG,
        );
        if let Some(dims) = tile::frame_dims(&shared) {
            let base = tile::fit(avail, Some(dims));
            // Pinch (or Ctrl+wheel) and the plain wheel both zoom toward
            // the pointer (wheel up = in, like a map); a double-click
            // toggles a quick 2× at that spot.
            let (pinch, wheel) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta().y));
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
        } else if let Some((tex, cached)) = &cam.placeholder {
            tile::draw_placeholder(ui.painter(), avail, tex, *cached);
        }

        tile::label(
            ui.painter(),
            avail.left_top() + egui::vec2(10.0, 8.0),
            egui::Align2::LEFT_TOP,
            &format!("{} — {}", cam.name, main_stats.status),
            egui::FontId::proportional(12.0),
            tile::WHITE,
        );

        if f.zoomed() {
            let badge = egui::Area::new(egui::Id::new("zoom badge"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-6.0, 6.0))
                .show(ui.ctx(), |ui| {
                    ui.add(
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
                });
            if badge.inner.clicked() {
                f.reset_zoom();
            }
        }
    }
}
