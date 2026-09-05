// overlay.rs — chrome drawn over the video: the auto-hiding top bar, the
// transient centered message chip (HUD.swift) and the "?" shortcut sheet
// (HelpOverlay.swift), a translucent panel listing the shortcuts for where
// the user is right now.

use crate::{App, media, tile};
use eframe::egui;
use std::path::PathBuf;
use std::time::Instant;

/// A finished snapshot or clip, on disk under its default name, waiting
/// for the user to keep, rename or discard it.
pub struct SavePrompt {
    path: PathBuf,
    what: &'static str,
    /// The name being typed (no extension).
    stem: String,
    /// Select the whole name on the first frame so typing replaces it.
    fresh: bool,
    error: Option<String>,
}

impl SavePrompt {
    pub fn new(path: PathBuf, what: &'static str) -> Self {
        let stem = path
            .file_stem()
            .map_or(String::new(), |s| s.to_string_lossy().into_owned());
        SavePrompt {
            path,
            what,
            stem,
            fresh: true,
            error: None,
        }
    }
}

pub enum HelpContext {
    Grid,
    Camera,
    Playback,
}

impl HelpContext {
    fn title(&self) -> &'static str {
        match self {
            HelpContext::Grid => "Grid",
            HelpContext::Camera => "Camera view",
            HelpContext::Playback => "Playback",
        }
    }

    fn rows(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            HelpContext::Grid => &[
                ("← ↑ ↓ →", "select a tile"),
                ("Return", "open the selected camera"),
                ("2×click", "open a camera"),
                ("hold+drag", "reorder the grid"),
                ("I", "nerd stats (selected tile)"),
                ("F11", "full screen"),
                ("Ctrl-,", "settings"),
                ("Esc", "cancel selection / reorder"),
            ],
            HelpContext::Camera => &[
                ("P", "recorded playback"),
                ("wheel / pinch", "zoom toward the pointer"),
                ("2×click", "quick 2× there · again to reset"),
                ("drag", "pan while zoomed"),
                ("S", "snapshot → Desktop"),
                ("R", "record clip → Desktop"),
                ("I", "nerd stats"),
                ("F11", "full screen"),
                ("Ctrl-,", "settings"),
                ("Esc", "zoom out · back to the grid"),
            ],
            HelpContext::Playback => &[
                ("Space", "pause / resume"),
                ("← →", "seek 10 s · ⇧ 60 s · Ctrl 15 min"),
                ("0–9", "jump within visible footage"),
                ("X", "speed 1× → 2× → 4×"),
                ("C", "calendar · arrows + ↵ pick a day"),
                ("T", "jump to today"),
                ("S", "snapshot at this position"),
                ("R", "record clip from here"),
                ("I", "nerd stats"),
                ("scroll", "timeline zoom · pan"),
                ("P / Esc", "back to live"),
            ],
        }
    }
}

pub fn show_help(ctx: &egui::Context, context: HelpContext) {
    let screen = ctx.viewport_rect();
    ctx.layer_painter(egui::LayerId::background()).rect_filled(
        screen,
        0.0,
        egui::Color32::from_black_alpha(90),
    );
    egui::Window::new("help")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .frame(
            egui::Frame::NONE
                .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 235))
                .corner_radius(10.0)
                .inner_margin(egui::Margin {
                    left: 22,
                    right: 22,
                    top: 16,
                    bottom: 12,
                }),
        )
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(format!("Shortcuts — {}", context.title()))
                        .size(14.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.add_space(14.0);
                egui::Grid::new("help rows")
                    .spacing(egui::vec2(12.0, 6.0))
                    .show(ui, |ui| {
                        for (key, desc) in context.rows() {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(*key)
                                            .monospace()
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    );
                                },
                            );
                            ui.label(
                                egui::RichText::new(*desc).color(egui::Color32::from_gray(200)),
                            );
                            ui.end_row();
                        }
                    });
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new("any key or click closes")
                        .size(10.0)
                        .color(egui::Color32::from_gray(140)),
                );
            });
        });
}

impl App {
    /// The rename dialog for the front capture: Return saves under the typed
    /// name, Esc (or Discard) deletes the file.
    pub fn show_save_prompt(&mut self, ctx: &egui::Context) {
        let Some(p) = self.save_prompts.front_mut() else {
            return;
        };
        let folder = p
            .path
            .parent()
            .map_or(String::new(), |d| d.display().to_string());
        let ext = p
            .path
            .extension()
            .map_or(String::new(), |e| format!(".{}", e.to_string_lossy()));
        let field_id = egui::Id::new("save prompt name");
        let mut done: Option<bool> = None;
        egui::Window::new(format!("Save {}", p.what.to_lowercase()))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut p.stem)
                            .id(field_id)
                            .desired_width(320.0),
                    );
                    ui.label(&ext);
                    if p.fresh {
                        p.fresh = false;
                        resp.request_focus();
                        let mut state =
                            egui::text_edit::TextEditState::load(ctx, field_id).unwrap_or_default();
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(p.stem.chars().count()),
                            )));
                        state.store(ctx, field_id);
                    }
                });
                ui.small(format!("in {folder}"));
                if let Some(e) = &p.error {
                    ui.colored_label(tile::CURSOR_RED, e);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Save").clicked() {
                            done = Some(true);
                        }
                        if ui.button("Discard").clicked() {
                            done = Some(false);
                        }
                    });
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
            });
        if let Some(keep) = done {
            self.finish_save_prompt(keep);
        }
    }

    /// Save (rename to the typed name) or discard (delete) the front capture.
    /// A rename that fails keeps the dialog up with the reason.
    pub fn finish_save_prompt(&mut self, keep: bool) {
        let Some(p) = self.save_prompts.front_mut() else {
            return;
        };
        if keep {
            match media::rename(&p.path, &p.stem) {
                Ok(path) => {
                    let name = path
                        .file_name()
                        .map_or(String::new(), |n| n.to_string_lossy().into_owned());
                    self.save_prompts.pop_front();
                    self.flash(&format!("Saved {name}"));
                }
                Err(e) => p.error = Some(e),
            }
        } else {
            let _ = std::fs::remove_file(&p.path);
            let what = p.what;
            self.save_prompts.pop_front();
            self.flash(&format!("{what} discarded"));
        }
    }

    /// Full-width bar parked above the window: slides down when the pointer
    /// touches the top edge, stays while the pointer is on it, and carries
    /// the Settings gear on the right.
    pub fn show_top_bar(&mut self, ctx: &egui::Context) {
        const HEIGHT: f32 = 36.0;
        const REVEAL_ZONE: f32 = 4.0;
        let screen = ctx.viewport_rect();
        let bar = egui::Rect::from_min_size(screen.min, egui::vec2(screen.width(), HEIGHT));
        let hover = ctx.input(|i| i.pointer.hover_pos());
        self.top_bar = hover.is_some_and(|p| {
            screen.contains(p)
                && (p.y < screen.min.y + REVEAL_ZONE || (self.top_bar && bar.contains(p)))
        });
        let t = ctx.animate_bool_with_time(egui::Id::new("top bar"), self.top_bar, 0.15);
        if t <= 0.0 {
            return;
        }
        let gear = egui::Area::new(egui::Id::new("top bar area"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min - egui::vec2(0.0, HEIGHT * (1.0 - t)))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_black_alpha((200.0 * t) as u8))
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.set_width(screen.width() - 16.0);
                        ui.set_height(HEIGHT - 8.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Button::new(
                                    egui::RichText::new("⚙").size(20.0).color(tile::WHITE),
                                )
                                .frame(false),
                            )
                            .on_hover_text("Settings — Ctrl-,")
                        })
                        .inner
                    })
                    .inner
            });
        if gear.inner.clicked() {
            self.open_settings();
        }
    }

    /// Flash a message in the middle of the window; a new one replaces the old.
    pub fn flash(&mut self, text: &str) {
        self.hud = Some((text.to_string(), Instant::now()));
    }

    /// Fade in over 0.15 s, hold ~1.1 s, fade out over 0.35 s. Never
    /// intercepts the pointer.
    pub fn show_hud(&mut self, ctx: &egui::Context) {
        let Some((text, since)) = &self.hud else {
            return;
        };
        let t = since.elapsed().as_secs_f32();
        let alpha = if t < 0.15 {
            t / 0.15
        } else if t < 1.25 {
            1.0
        } else if t < 1.6 {
            (1.6 - t) / 0.35
        } else {
            self.hud = None;
            return;
        };
        ctx.request_repaint_after(crate::REPAINT_COALESCE);
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("hud"),
        ));
        let color = tile::WHITE.gamma_multiply(alpha);
        let galley = painter.layout_no_wrap(text.clone(), egui::FontId::proportional(14.0), color);
        let rect = egui::Rect::from_center_size(
            ctx.viewport_rect().center(),
            galley.size() + egui::vec2(28.0, 18.0),
        );
        painter.rect_filled(
            rect,
            9.0,
            egui::Color32::from_black_alpha((178.0 * alpha) as u8),
        );
        painter.galley(rect.min + egui::vec2(14.0, 9.0), galley, color);
    }
}
