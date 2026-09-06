// timeline.rs — the playback bar overlaid on the focused view (PlaybackUI.swift
// port): play/pause, the date button with its calendar popover (recorded
// days only), a zoomable 24-hour timeline of recorded segments, a loading
// spinner, the time readout, and the zoom and speed buttons.

use crate::playback::{Playback, ZOOM_LEVELS};
use crate::tile;
use chrono::{DateTime, TimeDelta, Utc};
use eframe::egui;

pub const BAR_HEIGHT: f32 = 38.0;
const STRIP_HEIGHT: f32 = 26.0;
/// Recorded segments (same teal as the app icon) and the calendar chips.
const TEAL: egui::Color32 = egui::Color32::from_rgb(41, 189, 204);

/// Timeline scroll/pinch accumulators (a zoom step per ~25 px or 15%).
#[derive(Default)]
pub struct StripInput {
    scroll_acc: f32,
    magnify_acc: f32,
    /// Pointer-down time while scrubbing; the seek fires on release so
    /// scrubbing doesn't restart the pipe per pixel.
    scrub: Option<DateTime<Utc>>,
}

impl Playback {
    /// The bar along the bottom of `avail`. Returns true when the speed
    /// changed (the caller persists it).
    pub fn show_bar(&mut self, ui: &mut egui::Ui, avail: egui::Rect) -> bool {
        let bar =
            egui::Rect::from_min_max(egui::pos2(avail.min.x, avail.max.y - BAR_HEIGHT), avail.max);
        ui.painter()
            .rect_filled(bar, 0.0, egui::Color32::from_black_alpha(140));
        let inner = bar.shrink2(egui::vec2(10.0, 6.0));
        let mut speed_changed = false;

        // Right-hand group first so the strip can take what's left.
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        right.spacing_mut().item_spacing.x = 10.0;
        if bar_button(&mut right, &format!("{}×", self.speed))
            .on_hover_text("Playback speed — X")
            .clicked()
        {
            self.cycle_speed();
            speed_changed = true;
        }
        if bar_button(&mut right, ZOOM_LEVELS[self.zoom_index].1)
            .on_hover_text("Timeline zoom — scroll on the strip")
            .clicked()
        {
            self.cycle_zoom();
        }
        right.label(bar_text(&self.clock_label()));
        // The spinner's slot is always there: a widget that comes and goes
        // would shift the strip and the readout on every seek.
        let (slot, _) = right.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
        if self.loading {
            egui::Spinner::new().size(14.0).paint_at(&right, slot);
        }
        let right_edge = right.min_rect().min.x;

        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_max(
                    inner.min,
                    egui::pos2(right_edge - 10.0, inner.max.y),
                ))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        left.spacing_mut().item_spacing.x = 10.0;
        if bar_button(&mut left, if self.is_paused() { "▶" } else { "⏸" })
            .on_hover_text("Pause / resume — Space")
            .clicked()
        {
            self.toggle_pause();
        }
        let date = bar_button(&mut left, &format!("{}  ▾", self.day_label()))
            .on_hover_text("Calendar — C");
        if date.clicked() {
            self.toggle_calendar();
        }
        let date_button = date.rect;
        let strip = egui::Rect::from_center_size(
            egui::pos2(
                (left.cursor().min.x + right_edge - 10.0) / 2.0,
                inner.center().y,
            ),
            egui::vec2(
                (right_edge - 10.0 - left.cursor().min.x).max(40.0),
                STRIP_HEIGHT,
            ),
        );
        self.show_strip(ui, strip);

        if self.cal.open {
            self.show_calendar(ui.ctx(), date_button);
        }
        speed_changed
    }

    // MARK: the strip

    fn x_for(&self, rect: egui::Rect, t: DateTime<Utc>) -> f32 {
        let frac =
            (t - self.win_start).num_milliseconds() as f32 / (self.win_duration() as f32 * 1000.0);
        rect.min.x + frac * rect.width()
    }

    fn date_at(&self, rect: egui::Rect, x: f32) -> DateTime<Utc> {
        let frac = ((x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0);
        self.win_start
            + TimeDelta::milliseconds((frac as f64 * self.win_duration() as f64 * 1000.0) as i64)
    }

    /// Tick spacing, label spacing, and label style per zoom level.
    fn tick_plan(&self) -> (i64, i64, &'static str) {
        let dur = self.win_duration();
        if dur >= 86000 {
            (3600, 10800, "%-I%p") // 24h: 1h / 3h, "3AM"
        } else if dur >= 21000 {
            (1800, 3600, "%-I%p") // 6h: 30m / 1h
        } else if dur >= 3500 {
            (300, 600, "%-I:%M") // 1h: 5m / 10m, "8:10" — am/pm is on the clock readout
        } else {
            (60, 120, "%-I:%M") // 10m: 1m / 2m
        }
    }

    /// Time labels along the top, recorded segments as a teal band, and a
    /// white cursor for the playback position. Tick/label density adapts to
    /// the zoom level, and lines are drawn light over the dark background
    /// and dark over the teal band so they read everywhere. Click or drag
    /// anywhere to seek (fires on release); scroll or pinch to zoom through
    /// the presets, horizontal scroll to pan when zoomed.
    fn show_strip(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 3.0, egui::Color32::from_gray(41));
        let label_h = 12.0;
        let band = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, rect.min.y + label_h + 1.0),
            egui::pos2(rect.max.x, rect.max.y - 2.0),
        );
        let win_end = self.win_end();

        let mut seg_rects = Vec::new();
        for s in &self.segments {
            let x0 = self.x_for(rect, s.start).max(rect.min.x);
            let x1 = self.x_for(rect, s.end).min(rect.max.x);
            if x1 <= rect.min.x || x0 >= rect.max.x {
                continue;
            }
            let r = egui::Rect::from_min_max(
                egui::pos2(x0, band.min.y),
                egui::pos2(x1.max(x0 + 1.0), band.max.y),
            );
            painter.rect_filled(r, 0.0, TEAL);
            seg_rects.push(r);
        }

        // Tick times aligned to wall-clock boundaries in the NVR's timezone.
        let (tick, label_every, fmt) = self.tick_plan();
        let tz_off = i64::from(self.client.tz.local_minus_utc());
        let start_wall = self.win_start.timestamp() + tz_off;
        let end_wall = win_end.timestamp() + tz_off;
        let first = (start_wall + tick - 1) / tick * tick;
        // The window's end boundary is included — at 24h that puts 12AM at
        // both edges of the strip.
        let ticks: Vec<(f32, bool, DateTime<Utc>)> = (first..=end_wall)
            .step_by(tick as usize)
            .map(|wall| {
                let t = DateTime::from_timestamp(wall - tz_off, 0).unwrap();
                (self.x_for(rect, t), wall % label_every == 0, t)
            })
            .collect();
        let interior: Vec<_> = ticks
            .iter()
            .filter(|t| t.0 > rect.min.x + 1.0 && t.0 < rect.max.x - 1.0)
            .collect();
        // Tick lines: light on the dark background…
        for t in &interior {
            let a = if t.1 { 97 } else { 46 };
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(t.0 - 0.5, band.min.y),
                    egui::pos2(t.0 + 0.5, band.max.y),
                ),
                0.0,
                egui::Color32::from_white_alpha(a),
            );
        }
        // …and re-drawn dark where they cross the teal band.
        for r in &seg_rects {
            let p = painter.with_clip_rect(*r);
            for t in &interior {
                let a = if t.1 { 140 } else { 77 };
                p.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(t.0 - 0.5, band.min.y),
                        egui::pos2(t.0 + 0.5, band.max.y),
                    ),
                    0.0,
                    egui::Color32::from_black_alpha(a),
                );
            }
        }
        // Labels on the labelled ticks; edge labels clamp inward instead of
        // vanishing, so the window's boundary times are always readable.
        let font = egui::FontId::monospace(9.0);
        for (x, is_label, t) in &ticks {
            if !is_label {
                continue;
            }
            let text = self.client.local(*t).format(fmt).to_string();
            let galley = painter.layout_no_wrap(text, font.clone(), egui::Color32::from_gray(230));
            let lx = (x - galley.size().x / 2.0)
                .clamp(rect.min.x + 1.0, rect.max.x - galley.size().x - 1.0);
            painter.galley(
                egui::pos2(lx, rect.min.y + 1.0),
                galley,
                egui::Color32::from_gray(230),
            );
        }

        // Interaction: scrubbing moves the cursor, release seeks.
        let resp = ui.interact(
            rect,
            egui::Id::new("timeline strip"),
            egui::Sense::CLICK | egui::Sense::DRAG,
        );
        let mut cursor = self.cursor();
        if let Some(p) = resp.interact_pointer_pos()
            && (resp.is_pointer_button_down_on() || resp.dragged())
        {
            let t = self.date_at(rect, p.x);
            self.strip_input.scrub = Some(t);
            cursor = Some(t);
        }
        if resp.drag_stopped() || resp.clicked() {
            let t = resp.interact_pointer_pos().map_or_else(
                || self.strip_input.scrub.unwrap_or(self.win_start),
                |p| self.date_at(rect, p.x),
            );
            self.strip_input.scrub = None;
            self.seek(t);
        }
        if resp.hovered() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            let at = resp
                .hover_pos()
                .map_or(self.position(), |p| self.date_at(rect, p.x));
            if scroll.y.abs() > scroll.x.abs() {
                // Same wheel direction as the video zoom (focused.rs).
                self.strip_input.scroll_acc -= scroll.y;
                if self.strip_input.scroll_acc.abs() > 25.0 {
                    let dir = if self.strip_input.scroll_acc > 0.0 {
                        1
                    } else {
                        -1
                    };
                    self.strip_input.scroll_acc = 0.0;
                    self.set_zoom(self.zoom_index as isize + dir, at);
                }
            } else if scroll.x != 0.0 {
                let secs =
                    -f64::from(scroll.x / rect.width().max(1.0)) * self.win_duration() as f64;
                self.pan(secs);
            }
            if pinch != 1.0 {
                self.strip_input.magnify_acc += pinch - 1.0;
                if self.strip_input.magnify_acc.abs() > 0.15 {
                    let dir = if self.strip_input.magnify_acc > 0.0 {
                        1
                    } else {
                        -1
                    };
                    self.strip_input.magnify_acc = 0.0;
                    self.set_zoom(self.zoom_index as isize + dir, at);
                }
            }
        }

        if let Some(c) = cursor
            && c >= self.win_start
            && c <= win_end
        {
            let cx = self
                .x_for(rect, c)
                .clamp(rect.min.x + 1.0, rect.max.x - 1.0);
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(cx - 0.75, rect.min.y),
                    egui::pos2(cx + 0.75, rect.max.y),
                ),
                0.0,
                egui::Color32::WHITE,
            );
        }
    }

    // MARK: calendar popover

    /// Minimal month calendar above the date button: only days with
    /// recordings are clickable, everything else (no recording / future) is
    /// dimmed. Transient — a click outside closes it.
    fn show_calendar(&mut self, ctx: &egui::Context, anchor: egui::Rect) {
        const CELL: egui::Vec2 = egui::vec2(28.0, 22.0);
        let view = self.month_view();
        let mut step: Option<i32> = None;
        let mut pick: Option<u32> = None;
        let area = egui::Area::new(egui::Id::new("calendar popover"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(egui::pos2(anchor.min.x, anchor.min.y - 6.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgba_unmultiplied(30, 30, 30, 245))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(70)))
                    .corner_radius(8.0)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::new("◀").frame(false)).clicked() {
                                step = Some(-1);
                            }
                            ui.add_sized(
                                egui::vec2(7.0 * CELL.x + 6.0 * 2.0 - 40.0, 18.0),
                                egui::Label::new(
                                    egui::RichText::new(&view.title)
                                        .size(12.0)
                                        .strong()
                                        .color(tile::WHITE),
                                ),
                            );
                            if ui.add(egui::Button::new("▶").frame(false)).clicked() {
                                step = Some(1);
                            }
                        });
                        ui.horizontal(|ui| {
                            for ch in ["S", "M", "T", "W", "T", "F", "S"] {
                                ui.add_sized(
                                    egui::vec2(CELL.x, 14.0),
                                    egui::Label::new(
                                        egui::RichText::new(ch).size(10.0).color(tile::DIM),
                                    ),
                                );
                            }
                        });
                        let mut day: i64 = 1 - i64::from(view.leading_blanks);
                        while day <= i64::from(view.days_in_month) {
                            ui.horizontal(|ui| {
                                for _ in 0..7 {
                                    let (rect, resp) =
                                        ui.allocate_exact_size(CELL, egui::Sense::CLICK);
                                    if day >= 1 && day <= i64::from(view.days_in_month) {
                                        let d = day as u32;
                                        let on = view.enabled.contains(&d);
                                        let selected = view.selected == Some(d);
                                        // Recorded days get a teal chip so they're obvious
                                        // at a glance; the selected day a stronger one.
                                        if on {
                                            ui.painter().rect_filled(
                                                rect,
                                                5.0,
                                                TEAL.gamma_multiply(if selected {
                                                    0.55
                                                } else {
                                                    0.22
                                                }),
                                            );
                                            if selected {
                                                ui.painter().rect_stroke(
                                                    rect,
                                                    5.0,
                                                    egui::Stroke::new(1.5, TEAL),
                                                    egui::StrokeKind::Inside,
                                                );
                                            }
                                        }
                                        // The keyboard cursor ring sits on top of everything,
                                        // dim days too — red, matching the grid's cursor.
                                        if view.cursor == Some(d) {
                                            ui.painter().rect_stroke(
                                                rect,
                                                5.0,
                                                egui::Stroke::new(2.0, tile::CURSOR_RED),
                                                egui::StrokeKind::Inside,
                                            );
                                        }
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            d.to_string(),
                                            egui::FontId::monospace(11.0),
                                            if on {
                                                tile::WHITE
                                            } else {
                                                egui::Color32::from_gray(110)
                                            },
                                        );
                                        if on && resp.clicked() {
                                            pick = Some(d);
                                        }
                                    }
                                    day += 1;
                                }
                            });
                        }
                    });
            });
        if let Some(d) = step {
            self.step_month(d);
        }
        if let Some(d) = pick {
            self.pick_day(d);
        }
        // Transient popover: a click anywhere else closes it (the date
        // button's own click toggles it separately).
        if ctx.input(|i| i.pointer.any_pressed())
            && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
            && !area.response.rect.contains(p)
            && !anchor.contains(p)
        {
            self.cal.open = false;
        }
    }
}

fn bar_text(s: &str) -> egui::RichText {
    egui::RichText::new(s)
        .monospace()
        .size(11.0)
        .strong()
        .color(tile::WHITE)
}

fn bar_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(bar_text(label)).frame(false))
}
