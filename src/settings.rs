// settings.rs — the Settings window (camera list, prefs, decode/render,
// updates), the per-camera editor (SettingsUI.swift port), the update
// banner and the render-adapter restart prompt. Camera edits are staged and
// committed by Save, which rebuilds the grid; everything else applies the
// moment it's toggled.

use crate::config::StoredCamera;
use crate::{App, UpdateUi, config, isapi, prefs, relaunch, render, stream, tile, update};
use eframe::egui;
use std::sync::mpsc::{Receiver, channel};

#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    /// Render adapter choice awaiting the restart/cancel confirmation.
    pub pending_render: Option<Option<String>>,
    /// The camera list as edited; `config` keeps what's on disk until Save.
    pub staged: Vec<StoredCamera>,
    pub selected: Option<usize>,
    pub editor: Option<Editor>,
    /// Last update-check outcome ("up to date", "check failed…").
    pub update_note: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum DetectField {
    Name,
    Codec,
}

/// What a Detect probe answers: channel name and codec, each if readable.
type Detected = (Option<String>, Option<String>);

/// The add/edit camera form. Fields are text so a half-typed port doesn't
/// fight the user; validated on OK.
pub struct Editor {
    /// Staged row being edited; None = adding.
    row: Option<usize>,
    host: String,
    user: String,
    password: String,
    port: String,
    name: String,
    h264: bool,
    /// A Detect probe in flight: which button asked, and its result.
    detect: Option<(DetectField, Receiver<Detected>)>,
}

impl Editor {
    fn new(row: Option<usize>, cam: Option<&StoredCamera>) -> Self {
        Editor {
            row,
            host: cam.map_or(String::new(), |c| c.host.clone()),
            user: cam.map_or(String::new(), |c| c.user.clone()),
            password: cam.map_or(String::new(), |c| c.password.clone()),
            port: cam.map_or(554, |c| c.port).to_string(),
            name: cam.map_or(String::new(), |c| c.name.clone()),
            h264: cam.is_some_and(|c| c.codec == "h264"),
            detect: None,
        }
    }
}

/// Camera table columns: title and width.
const COLS: [(&str, f32); 5] = [
    ("Name", 150.0),
    ("Host / IP", 130.0),
    ("User", 90.0),
    ("Port", 48.0),
    ("Codec", 56.0),
];
const ROW_H: f32 = 22.0;

fn draw_row(painter: &egui::Painter, rect: egui::Rect, cells: [&str; 5], color: egui::Color32) {
    let mut x = rect.min.x + 6.0;
    for ((_, w), text) in COLS.iter().zip(cells) {
        let cell = egui::Rect::from_min_max(
            egui::pos2(x, rect.min.y),
            egui::pos2(x + w - 8.0, rect.max.y),
        );
        painter.with_clip_rect(cell).text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(13.0),
            color,
        );
        x += w;
    }
}

impl App {
    /// Open Settings with the camera list staged from what's on disk.
    pub fn open_settings(&mut self) {
        self.settings.staged = self
            .config
            .as_ref()
            .map(|c| c.cameras.clone())
            .unwrap_or_default();
        self.settings.selected = None;
        self.settings.editor = None;
        self.settings.open = true;
    }

    pub fn show_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.settings.open;
        let mut decode_changed = false;
        let mut save_cams = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if self.config.is_some() {
                    ui.label("Cameras — each has its own username, password, port, and codec (double-click to edit)");
                    self.show_camera_list(ui);
                    ui.horizontal(|ui| {
                        if ui.button("+").clicked() {
                            self.settings.editor = Some(Editor::new(None, None));
                        }
                        if ui.button("−").clicked() {
                            match self.settings.selected {
                                Some(i) => {
                                    self.settings.staged.remove(i);
                                    self.settings.selected = None;
                                }
                                None => self.flash("Select a camera first"),
                            }
                        }
                        if ui.button("Edit…").clicked() {
                            match self.settings.selected {
                                Some(i) => {
                                    self.settings.editor =
                                        Some(Editor::new(Some(i), Some(&self.settings.staged[i])));
                                }
                                None => self.flash("Select a camera first"),
                            }
                        }
                        let dirty = self
                            .config
                            .as_ref()
                            .is_some_and(|c| c.cameras != self.settings.staged);
                        ui.add_space(12.0);
                        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                            save_cams = true;
                        }
                        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                            self.settings.staged = self.config.as_ref().unwrap().cameras.clone();
                            self.settings.selected = None;
                        }
                        if dirty {
                            ui.small("unsaved changes");
                        }
                    });
                    ui.separator();
                }

                let mut changed = false;
                changed |= ui
                    .checkbox(&mut self.prefs.start_fullscreen, "Always start in full screen")
                    .on_hover_text("F11 toggles full screen now")
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.prefs.remember_last_view,
                        "Remember where I left off (grid or open camera)",
                    )
                    .changed();
                if ui
                    .checkbox(
                        &mut self.prefs.smooth_live,
                        "Smooth live video (buffers ~0.2 s to absorb Wi-Fi jitter)",
                    )
                    .changed()
                {
                    stream::SMOOTH.store(
                        self.prefs.smooth_live,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    changed = true;
                }
                if changed {
                    self.prefs.save();
                }
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Decode");
                    let current = self.prefs.decode_label();
                    let mut options = prefs::available_decode_options();
                    // Keep the active choice visible even if the probe ruled it out.
                    if !options.iter().any(|o| o.label == current)
                        && let Some(cur) = prefs::decode_options().iter().find(|o| o.label == current)
                    {
                        options.push(cur);
                    }
                    egui::ComboBox::from_id_salt("decode")
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for opt in options {
                                let selected = self.prefs.decode_label() == opt.label;
                                if ui.selectable_label(selected, opt.label).clicked() && !selected {
                                    self.prefs.decode = opt.id.to_string();
                                    decode_changed = true;
                                }
                            }
                        });
                    ui.add_space(12.0);
                    ui.label("Render");
                    let adapters = render::adapter_names();
                    // Show the candidate while its restart prompt is up.
                    let shown: Option<String> = self
                        .settings
                        .pending_render
                        .clone()
                        .unwrap_or_else(|| self.prefs.render_adapter.clone());
                    let shown_label = shown.clone().unwrap_or_else(|| "Default".into());
                    egui::ComboBox::from_id_salt("render")
                        .selected_text(&shown_label)
                        .show_ui(ui, |ui| {
                            let mut pick = |val: Option<String>, label: &str, ui: &mut egui::Ui| {
                                let selected = shown == val;
                                if ui.selectable_label(selected, label).clicked()
                                    && val != self.prefs.render_adapter
                                {
                                    self.settings.pending_render = Some(val);
                                }
                            };
                            pick(None, "Default", ui);
                            for name in adapters {
                                pick(Some(name.clone()), name, ui);
                            }
                        });
                });
                ui.small("Render changes take effect after restart.");
                ui.separator();

                ui.horizontal(|ui| {
                    let checking = matches!(self.update, UpdateUi::Checking);
                    if ui
                        .add_enabled(!checking, egui::Button::new("Check for updates"))
                        .clicked()
                    {
                        if update::installed() {
                            self.settings.update_note = None;
                            self.update = UpdateUi::Checking;
                            update::check(self.upd_tx.clone(), ctx.clone());
                        } else {
                            // Updater.checkInteractive's dev-build message.
                            self.settings.update_note = Some(format!(
                                "Source build (v{}) — pull and rebuild to update.",
                                update::VERSION
                            ));
                        }
                    }
                    if checking {
                        ui.small("checking…");
                    }
                });
                if let Some(note) = &self.settings.update_note {
                    ui.small(note.clone());
                }
            });
        self.settings.open = open;
        if !open {
            self.settings.editor = None;
        }
        if decode_changed {
            self.prefs.save();
            self.restart_streams(ctx);
        }
        if save_cams {
            self.save_cameras(ctx);
        }
        self.show_editor(ctx);
        self.show_pending_render(ctx);
    }

    fn show_camera_list(&mut self, ui: &mut egui::Ui) {
        let width: f32 = COLS.iter().map(|c| c.1).sum();
        let (header, _) = ui.allocate_exact_size(egui::vec2(width, ROW_H), egui::Sense::hover());
        draw_row(ui.painter(), header, COLS.map(|c| c.0), tile::DIM);
        egui::ScrollArea::vertical()
            .max_height(ROW_H * 8.0)
            .show(ui, |ui| {
                if self.settings.staged.is_empty() {
                    ui.label(
                        egui::RichText::new("No cameras yet — press + to add one.")
                            .color(tile::DIM),
                    );
                }
                let mut edit = None;
                for (i, c) in self.settings.staged.iter().enumerate() {
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(width, ROW_H), egui::Sense::CLICK);
                    if self.settings.selected == Some(i) {
                        ui.painter()
                            .rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
                    } else if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, 2.0, ui.visuals().widgets.hovered.bg_fill);
                    }
                    let port = c.port.to_string();
                    draw_row(
                        ui.painter(),
                        rect,
                        [&c.name, &c.host, &c.user, &port, c.codec_label()],
                        ui.visuals().text_color(),
                    );
                    if resp.clicked() {
                        self.settings.selected = Some(i);
                    }
                    if resp.double_clicked() {
                        edit = Some(i);
                    }
                }
                if let Some(i) = edit {
                    self.settings.selected = Some(i);
                    self.settings.editor =
                        Some(Editor::new(Some(i), Some(&self.settings.staged[i])));
                }
            });
    }

    /// Commit the staged list: write the config and rebuild the grid.
    fn save_cameras(&mut self, ctx: &egui::Context) {
        if self.settings.staged.is_empty() {
            self.flash("Add at least one camera");
            return;
        }
        let Some(cfg) = &mut self.config else {
            return;
        };
        cfg.cameras = self.settings.staged.clone();
        match config::save(cfg) {
            Ok(()) => {
                self.rebuild(ctx);
                self.settings.open = false;
            }
            Err(e) => self.flash(&format!("Couldn't save the config: {e}")),
        }
    }

    fn show_editor(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.settings.editor else {
            return;
        };
        let mut msg: Option<String> = None;
        // A finished Detect fills only the field whose button was pressed —
        // pressing it is the consent to overwrite.
        if let Some((field, rx)) = &ed.detect
            && let Ok((name, codec)) = rx.try_recv()
        {
            match (*field, name, codec) {
                (DetectField::Name, Some(name), _) => {
                    msg = Some(format!("Detected: {name}"));
                    ed.name = name;
                }
                (DetectField::Name, None, _) => {
                    msg = Some("Couldn't detect name — check host/credentials".into());
                }
                (DetectField::Codec, _, Some(codec)) => {
                    ed.h264 = codec == "h264";
                    msg = Some(format!(
                        "Detected: {}",
                        if ed.h264 { "H.264" } else { "HEVC" }
                    ));
                }
                (DetectField::Codec, _, None) => {
                    msg = Some("Couldn't detect codec — check host/credentials".into());
                }
            }
            ed.detect = None;
        }

        let mut done: Option<bool> = None; // Some(true) = OK, Some(false) = Cancel
        let mut detect: Option<DetectField> = None;
        let pending = ed.detect.is_some();
        let title = if ed.row.is_some() {
            "Edit camera"
        } else {
            "Add camera"
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                let field = |ui: &mut egui::Ui, s: &mut String, password: bool| {
                    ui.add(
                        egui::TextEdit::singleline(s)
                            .password(password)
                            .desired_width(220.0),
                    );
                };
                // Connection fields first, then the two Detect can fill from
                // the camera itself.
                egui::Grid::new("camera form")
                    .num_columns(2)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Host / IP:");
                        field(ui, &mut ed.host, false);
                        ui.end_row();
                        ui.label("Username:");
                        field(ui, &mut ed.user, false);
                        ui.end_row();
                        ui.label("Password:");
                        field(ui, &mut ed.password, true);
                        ui.end_row();
                        ui.label("RTSP port:");
                        field(ui, &mut ed.port, false);
                        ui.end_row();
                    });
                ui.separator();
                egui::Grid::new("camera detect")
                    .num_columns(2)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name:");
                        ui.horizontal(|ui| {
                            field(ui, &mut ed.name, false);
                            if ui
                                .add_enabled(!pending, egui::Button::new("Detect"))
                                .clicked()
                            {
                                detect = Some(DetectField::Name);
                            }
                            if matches!(ed.detect, Some((DetectField::Name, _))) {
                                ui.spinner();
                            }
                        });
                        ui.end_row();
                        ui.label("Codec:");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("codec")
                                .selected_text(if ed.h264 { "H.264" } else { "HEVC" })
                                .width(100.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut ed.h264, false, "HEVC");
                                    ui.selectable_value(&mut ed.h264, true, "H.264");
                                });
                            if ui
                                .add_enabled(!pending, egui::Button::new("Detect"))
                                .clicked()
                            {
                                detect = Some(DetectField::Codec);
                            }
                            if matches!(ed.detect, Some((DetectField::Codec, _))) {
                                ui.spinner();
                            }
                        });
                        ui.end_row();
                    });
                ui.small("Detect reads the name and codec from the camera (read-only).");
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("OK").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
            });

        if let Some(field) = detect {
            let (host, user, pass) = (
                ed.host.trim().to_string(),
                ed.user.trim().to_string(),
                ed.password.clone(),
            );
            if host.is_empty() || user.is_empty() || pass.is_empty() {
                msg = Some("Enter host and credentials first".into());
            } else {
                let (tx, rx) = channel();
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(isapi::detect_channel(&host, &user, &pass));
                    c.request_repaint();
                });
                ed.detect = Some((field, rx));
            }
        }
        match done {
            Some(true) => {
                let host = ed.host.trim().to_string();
                let user = ed.user.trim().to_string();
                let port = ed.port.trim().parse::<u16>().unwrap_or(0);
                if host.is_empty() || user.is_empty() || ed.password.is_empty() || port == 0 {
                    msg = Some("Host, user, password and port are required".into());
                } else {
                    let mut name = ed.name.trim().to_string();
                    if name.is_empty() {
                        name = host.clone();
                    }
                    let cam = StoredCamera {
                        host,
                        name,
                        user,
                        port,
                        codec: if ed.h264 { "h264" } else { "hevc" }.into(),
                        password: ed.password.clone(),
                    };
                    let staged = &mut self.settings.staged;
                    match ed.row {
                        Some(i) => staged[i] = cam,
                        None => {
                            staged.push(cam);
                            self.settings.selected = Some(staged.len() - 1);
                        }
                    }
                    self.settings.editor = None;
                }
            }
            Some(false) => self.settings.editor = None,
            None => {}
        }
        if let Some(m) = msg {
            self.flash(&m);
        }
    }

    /// Update flow banner, top center: offer -> installing -> failure.
    pub fn show_update_banner(&mut self, ctx: &egui::Context) {
        let mut install = false;
        let mut dismiss = false;
        let mut open_notes: Option<String> = None;
        let win = |title_hint: &str| {
            egui::Window::new(title_hint)
                .id(egui::Id::new("update banner"))
                .title_bar(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 12.0))
        };
        match &self.update {
            UpdateUi::Available(rel) => {
                let (tag, notes) = (rel.tag.clone(), rel.notes_url.clone());
                win("update").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "HikYeah {tag} is available — you have v{}",
                            update::VERSION
                        ));
                        if ui.button("Install").clicked() {
                            install = true;
                        }
                        if !notes.is_empty() && ui.button("Notes").clicked() {
                            open_notes = Some(notes);
                        }
                        if ui.button("Later").clicked() {
                            dismiss = true;
                        }
                    });
                });
            }
            UpdateUi::Installing => {
                win("update").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Updating — HikYeah restarts when the installer finishes…");
                    });
                });
            }
            UpdateUi::InstallFailed(e) => {
                let e = e.clone();
                win("update").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("Update failed: {e}"));
                        if ui.button("Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                });
            }
            _ => {}
        }
        if let Some(url) = open_notes {
            update::open_url(&url);
        }
        if install {
            self.update = UpdateUi::Installing;
            update::apply(self.upd_tx.clone(), ctx.clone());
        } else if dismiss {
            self.update = UpdateUi::Idle;
        }
    }

    /// Render change: confirm before relaunching; Cancel reverts.
    fn show_pending_render(&mut self, ctx: &egui::Context) {
        if let Some(pending) = self.settings.pending_render.clone() {
            let name = pending.clone().unwrap_or_else(|| "Default".into());
            egui::Window::new("Restart required")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!("Switch rendering to \"{name}\"?"));
                    ui.small("HikYeah restarts to apply the change.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Restart now").clicked() {
                            self.prefs.render_adapter = pending.clone();
                            self.prefs.save();
                            self.settings.pending_render = None;
                            relaunch(self.instance_lock.take());
                        }
                        if ui.button("Cancel").clicked() {
                            self.settings.pending_render = None;
                        }
                    });
                });
        }
    }
}
