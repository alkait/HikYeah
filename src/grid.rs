// grid.rs — the tile grid: layout, keyboard cursor, double-click focus and
// long-press drag-to-reorder (GridView.swift port; the order is written
// back to the config file like the Mac app's persistOrder).

use crate::{App, config, render, tile};
use eframe::egui;
use std::time::{Duration, Instant};

/// A tile lifted by a long press: which one, where it started, the pointer's
/// offset from its center (so it doesn't jump under the cursor) and where it
/// was last drawn (the drop animation starts there).
pub struct Drag {
    pub idx: usize,
    pub orig: usize,
    pub grab: egui::Vec2,
    pub rect: egui::Rect,
}

/// Hold this long without moving to lift a tile (TileView.mouseDown).
const LONG_PRESS: f64 = 0.45;
/// Moving further than this before the hold fires is a stray drag, not a hold.
const PRESS_SLOP: f32 = 6.0;
/// Tiles glide to their slots over this long after a drop or cancel.
const SETTLE: f32 = 0.25;

struct Layout {
    rect: egui::Rect,
    n: usize,
    cols: usize,
    rows: usize,
    cw: f32,
    ch: f32,
}

impl Layout {
    /// The column count that gives the largest 16:9 tiles.
    fn new(rect: egui::Rect, n: usize) -> Self {
        let mut cols = 1;
        let mut best = 0.0f32;
        for c in 1..=n {
            let rows = n.div_ceil(c);
            let (cw, ch) = (rect.width() / c as f32, rect.height() / rows as f32);
            let scale = (cw / 16.0).min(ch / 9.0);
            if scale > best {
                best = scale;
                cols = c;
            }
        }
        let rows = n.div_ceil(cols);
        Layout {
            rect,
            n,
            cols,
            rows,
            cw: rect.width() / cols as f32,
            ch: rect.height() / rows as f32,
        }
    }

    fn slot(&self, i: usize) -> egui::Rect {
        egui::Rect::from_min_size(
            self.rect.left_top()
                + egui::vec2(
                    (i % self.cols) as f32 * self.cw,
                    (i / self.cols) as f32 * self.ch,
                ),
            egui::vec2(self.cw, self.ch),
        )
        .shrink(1.0)
    }

    /// Slot under a point, clamped to the grid.
    fn index_at(&self, p: egui::Pos2) -> usize {
        let col =
            (((p.x - self.rect.min.x) / self.cw).floor() as i64).clamp(0, self.cols as i64 - 1);
        let row =
            (((p.y - self.rect.min.y) / self.ch).floor() as i64).clamp(0, self.rows as i64 - 1);
        ((row * self.cols as i64 + col) as usize).min(self.n - 1)
    }
}

impl App {
    /// One arrow press: show the cursor on the last-used tile, or move a
    /// visible cursor by (dc, dr), clamped to the grid.
    fn move_key_cursor(&mut self, dc: i32, dr: i32, layout: &Layout) {
        let n = self.cams.len();
        let mut i = self.last_key_sel.min(n - 1);
        if let Some((cur, _)) = self.key_sel {
            let cols = layout.cols;
            let c = ((cur % cols) as i32 + dc).clamp(0, cols as i32 - 1) as usize;
            let r = ((cur / cols) as i32 + dr).clamp(0, layout.rows as i32 - 1) as usize;
            i = (r * cols + c).min(n - 1);
        }
        self.last_key_sel = i;
        self.key_sel = Some((i, Instant::now() + Duration::from_secs(2)));
    }

    /// Esc during a drag: put the tile back where it started.
    pub fn cancel_drag(&mut self) {
        if let Some(d) = self.drag.take() {
            if d.idx != d.orig {
                let cam = self.cams.remove(d.idx);
                self.cams.insert(d.orig, cam);
            }
            self.settle_until = Instant::now() + Duration::from_secs_f32(SETTLE);
        }
    }

    fn end_drag(&mut self, ctx: &egui::Context) {
        if let Some(d) = self.drag.take() {
            // Seed the glide from where the lifted tile was dropped.
            let id = egui::Id::new("slot").with(self.cams[d.idx].id);
            ctx.animate_value_with_time(id.with("x"), d.rect.min.x, 0.0);
            ctx.animate_value_with_time(id.with("y"), d.rect.min.y, 0.0);
            self.settle_until = Instant::now() + Duration::from_secs_f32(SETTLE);
            if d.idx != d.orig {
                self.persist_order();
            }
        }
    }

    /// Write the grid order back to the config: cameras follow the tiles,
    /// anything unmatched keeps its place at the end (persistOrder port).
    fn persist_order(&mut self) {
        let Some(cfg) = &mut self.config else {
            return;
        };
        let mut remaining = std::mem::take(&mut cfg.cameras);
        let mut ordered = Vec::with_capacity(remaining.len());
        for cam in &self.cams {
            if let Some(i) = remaining.iter().position(|c| c.host == cam.host) {
                ordered.push(remaining.remove(i));
            }
        }
        ordered.extend(remaining);
        cfg.cameras = ordered;
        if let Err(e) = config::save(cfg) {
            self.flash(&format!("Couldn't save the order: {e}"));
        }
    }

    /// Long-press detection and the drag itself, from raw pointer state:
    /// a press that stays put for LONG_PRESS lifts the tile under it; while
    /// lifted, the tile's center picks the slot it swaps into.
    fn update_drag(&mut self, ui: &egui::Ui, layout: &Layout) {
        let (down, origin, start, pos, time) = ui.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.press_origin(),
                i.pointer.press_start_time(),
                i.pointer.latest_pos(),
                i.time,
            )
        });
        if !down {
            self.press_voided = false;
            if self.drag.is_some() {
                self.end_drag(ui.ctx());
            }
            return;
        }
        let (Some(origin), Some(start), Some(pos)) = (origin, start, pos) else {
            return;
        };
        match &mut self.drag {
            Some(d) => {
                let target = layout.index_at(pos + d.grab);
                if target != d.idx {
                    let cam = self.cams.remove(d.idx);
                    self.cams.insert(target, cam);
                    d.idx = target;
                }
            }
            None => {
                if self.press_voided
                    || self.settings.open
                    || layout.n < 2
                    || !layout.rect.contains(origin)
                    || ui.ctx().is_pointer_over_egui()
                {
                    return;
                }
                if (pos - origin).length() > PRESS_SLOP {
                    self.press_voided = true;
                    return;
                }
                let held = time - start;
                if held >= LONG_PRESS {
                    let idx = layout.index_at(origin);
                    self.key_sel = None;
                    self.drag = Some(Drag {
                        idx,
                        orig: idx,
                        grab: layout.slot(idx).center() - origin,
                        rect: layout.slot(idx),
                    });
                } else {
                    ui.ctx()
                        .request_repaint_after(Duration::from_secs_f64(LONG_PRESS - held));
                }
            }
        }
    }

    pub fn show_grid(&mut self, ui: &mut egui::Ui, avail: egui::Rect) {
        let n = self.cams.len();
        if n == 0 {
            ui.painter().text(
                avail.center(),
                egui::Align2::CENTER_CENTER,
                "No cameras yet — add one in Settings (⚙ at the top edge, or Ctrl-,)",
                egui::FontId::proportional(16.0),
                tile::DIM,
            );
            return;
        }
        let layout = Layout::new(avail, n);

        // Keyboard navigation: arrows drive the red cursor, Return focuses it,
        // inactivity clears it.
        let mut focus: Option<usize> = None;
        if !self.settings.open
            && self.save_prompts.is_empty()
            && self.drag.is_none()
            && self.bookmark_pane.is_none()
            && self.bookmark_prompt.is_none()
        {
            let arrows = ui.input(|i| {
                [
                    (i.key_pressed(egui::Key::ArrowLeft), -1, 0),
                    (i.key_pressed(egui::Key::ArrowRight), 1, 0),
                    (i.key_pressed(egui::Key::ArrowUp), 0, -1),
                    (i.key_pressed(egui::Key::ArrowDown), 0, 1),
                ]
            });
            for (pressed, dc, dr) in arrows {
                if pressed {
                    self.move_key_cursor(dc, dr, &layout);
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter))
                && let Some((i, _)) = self.key_sel
            {
                focus = Some(i);
            }
        }
        if let Some((_, deadline)) = self.key_sel {
            let now = Instant::now();
            if now >= deadline {
                self.key_sel = None;
            } else {
                ui.ctx().request_repaint_after(deadline - now);
            }
        }

        self.update_drag(ui, &layout);
        let animating = self.drag.is_some() || Instant::now() < self.settle_until;
        if animating {
            ui.ctx().request_repaint_after(crate::REPAINT_COALESCE);
        }
        let pointer = ui.input(|i| i.pointer.latest_pos());
        let lifted_idx = self.drag.as_ref().map(|d| d.idx);
        // The lifted tile paints last so it floats above its siblings.
        let order = (0..n).filter(|&i| Some(i) != lifted_idx).chain(lifted_idx);
        for i in order {
            let cam = &self.cams[i];
            let slot = layout.slot(i);
            let lifted = Some(i) == lifted_idx;
            let cell = if lifted {
                let d = self.drag.as_ref().unwrap();
                let center = pointer.map_or(slot.center(), |p| p + d.grab);
                egui::Rect::from_center_size(center, slot.size() * 1.03)
            } else {
                // Slots glide while reordering; otherwise snap (a window
                // resize must not animate).
                let t = if animating { SETTLE } else { 0.0 };
                let id = egui::Id::new("slot").with(cam.id);
                let x = ui
                    .ctx()
                    .animate_value_with_time(id.with("x"), slot.min.x, t);
                let y = ui
                    .ctx()
                    .animate_value_with_time(id.with("y"), slot.min.y, t);
                egui::Rect::from_min_size(egui::pos2(x, y), slot.size())
            };
            if lifted {
                ui.painter().rect_filled(
                    cell.translate(egui::vec2(0.0, 6.0)).expand(8.0),
                    6.0,
                    egui::Color32::from_black_alpha(150),
                );
            }
            let dims = tile::frame_dims(&cam.shared);
            let rect = match (dims, &cam.placeholder) {
                (Some(d), _) => {
                    let rect = tile::fit(cell, Some(d));
                    ui.painter()
                        .add(eframe::egui_wgpu::Callback::new_paint_callback(
                            rect,
                            render::VideoCallback {
                                id: cam.id,
                                shared: cam.shared.clone(),
                                uv: render::VideoCallback::FULL,
                            },
                        ));
                    rect
                }
                (None, Some((tex, cached))) => {
                    tile::draw_placeholder(ui.painter(), cell, tex, *cached)
                }
                (None, None) => tile::fit(cell, None),
            };

            let status = cam.shared.stats.lock().unwrap().status.clone();
            tile::label(
                ui.painter(),
                rect.left_bottom() + egui::vec2(6.0, -6.0),
                egui::Align2::LEFT_BOTTOM,
                &format!("{} — {}", cam.name, status),
                egui::FontId::proportional(12.0),
                tile::WHITE,
            );

            if lifted_idx.is_some() && !lifted {
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(64));
            }
            if lifted {
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Inside,
                );
            }
            if self.key_sel.is_some_and(|(sel, _)| sel == i) {
                ui.painter().rect_stroke(
                    rect.shrink(1.5),
                    0.0,
                    egui::Stroke::new(3.0, tile::CURSOR_RED),
                    egui::StrokeKind::Inside,
                );
            }

            if cam.main_url.is_some() && lifted_idx.is_none() {
                let resp =
                    ui.interact(cell, egui::Id::new("tile").with(cam.id), egui::Sense::CLICK);
                if resp.double_clicked() {
                    focus = Some(i);
                }
            }
        }
        if let Some(d) = &mut self.drag {
            d.rect = egui::Rect::from_center_size(
                pointer.map_or(layout.slot(d.idx).center(), |p| p + d.grab),
                layout.slot(d.idx).size(),
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if let Some(i) = focus {
            let ctx = ui.ctx().clone();
            self.focus(i, &ctx);
        }
    }
}
