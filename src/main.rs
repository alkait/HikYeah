// main.rs — HikYeah: cross-platform HikViewer port (Rust + egui + wgpu).
//
// A grid of live substream tiles, one per configured camera (mirrors the
// macOS app: grid on channel 102). Double-clicking a tile focuses it
// full-window on the camera's main stream (101); Esc returns to the grid.
// Ctrl-, opens Settings (decode device / render adapter — explicit toggles,
// no auto-detection).
//
//   hikyeah                cameras from ~/.config/hikviewer/config.json
//                          (same JSON as the macOS app's File > Export)
//   hikyeah <rtsp-url>     single explicit URL (no focus view)
//   hikyeah --test         ffmpeg synthetic test pattern (no camera needed)

mod config;
mod prefs;
mod render;
mod session;
mod snapshot;
mod stream;

use eframe::egui;
use std::sync::Arc;
use std::time::Instant;

fn main() -> eframe::Result {
    let arg = std::env::args().nth(1);
    let source = match arg.as_deref() {
        Some("--test") => Source::Single("test pattern".into(), "--test".into()),
        Some(u) if u.starts_with("rtsp://") => Source::Single("camera".into(), u.into()),
        Some(other) => {
            eprintln!("usage: hikyeah [rtsp://… | --test]  (unrecognized: {other})");
            std::process::exit(2);
        }
        None => {
            let cams: Vec<config::StoredCamera> = config::load()
                .map(|c| {
                    c.cameras
                        .into_iter()
                        .filter(|c| !c.host.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if cams.is_empty() {
                eprintln!(
                    "no cameras: pass an rtsp:// URL or --test, or put a HikViewer\n\
                     config.json (File > Export on the Mac app) at {}",
                    config::config_path().display()
                );
                std::process::exit(2);
            }
            Source::Config(cams)
        }
    };

    let app_prefs = prefs::Prefs::load();
    stream::SMOOTH.store(app_prefs.smooth_live, std::sync::atomic::Ordering::Relaxed);
    prefs::start_probe();

    // Render adapter: pick the user's choice by name, else wgpu's first.
    // The selector also records what exists for the Settings dropdown.
    let want = app_prefs.render_adapter.clone();
    let selector: eframe::egui_wgpu::NativeAdapterSelectorMethod =
        Arc::new(move |adapters, surface| {
            let usable: Vec<&eframe::wgpu::Adapter> = adapters
                .iter()
                .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
                .collect();
            let mut names: Vec<String> = Vec::new();
            for a in &usable {
                let n = a.get_info().name;
                if !names.contains(&n) {
                    names.push(n);
                }
            }
            render::set_adapter_names(names);
            let pick = want
                .as_ref()
                .and_then(|w| usable.iter().find(|a| &a.get_info().name == w))
                .or_else(|| usable.first())
                .ok_or("no usable graphics adapter")?;
            Ok((*pick).clone())
        });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1440.0, 810.0]),
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(
                eframe::egui_wgpu::WgpuSetupCreateNew {
                    native_adapter_selector: Some(selector),
                    ..eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle()
                },
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "HikYeah",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, source, app_prefs)))),
    )
}

enum Source {
    /// Explicit URL or --test: one tile, no main-stream focus.
    Single(String, String),
    Config(Vec<config::StoredCamera>),
}

/// One camera in the grid: its running substream and how to reach the
/// main stream when focused.
struct Cam {
    name: String,
    /// Identifies the camera across launches and reorders (session state).
    host: String,
    sub_url: String,
    main_url: Option<String>,
    shared: Arc<stream::Shared>,
    /// JPEG shown until the first live frame: last-known from disk
    /// (true = "cached", dimmed + badged) or a fresh ISAPI snapshot (false).
    placeholder: Option<(egui::TextureHandle, bool)>,
}

struct App {
    cams: Vec<Cam>,
    /// Focused camera (index) and its main stream.
    focused: Option<(usize, Arc<stream::Shared>)>,
    prefs: prefs::Prefs,
    settings_open: bool,
    /// Render adapter choice awaiting the restart/cancel confirmation.
    pending_render: Option<Option<String>>,
    /// Grid keyboard cursor (red border): tile index + when it fades.
    key_sel: Option<(usize, Instant)>,
    /// Arrows resume from here (last cursor position or last focused tile).
    last_key_sel: usize,
    /// Fresh snapshots arriving from the background ISAPI fetches.
    snap_rx: std::sync::mpsc::Receiver<(usize, egui::ColorImage)>,
}

/// Stable tile ids for per-tile GPU state: grid substream = index,
/// focused main stream = index | MAIN_BIT.
const MAIN_BIT: u64 = 1 << 32;

impl App {
    fn new(cc: &eframe::CreationContext<'_>, source: Source, app_prefs: prefs::Prefs) -> Self {
        let rs = cc.wgpu_render_state.as_ref().expect("wgpu render state");
        rs.renderer
            .write()
            .callback_resources
            .insert(render::VideoRenderer::new(&rs.device, rs.target_format));
        let (tx, snap_rx) = std::sync::mpsc::channel();
        let mut app = App {
            cams: Vec::new(),
            focused: None,
            prefs: app_prefs,
            settings_open: false,
            pending_render: None,
            key_sel: None,
            last_key_sel: 0,
            snap_rx,
        };
        match source {
            Source::Single(name, url) => {
                app.cams = vec![Cam {
                    name,
                    host: String::new(),
                    sub_url: url,
                    main_url: None,
                    shared: Default::default(),
                    placeholder: None,
                }];
            }
            Source::Config(stored) => {
                app.cams = stored
                    .iter()
                    .map(|c| Cam {
                        name: if c.name.is_empty() {
                            c.host.clone()
                        } else {
                            c.name.clone()
                        },
                        host: c.host.clone(),
                        sub_url: config::rtsp_url(c, config::SUB_CHANNEL),
                        main_url: Some(config::rtsp_url(c, config::MAIN_CHANNEL)),
                        shared: Default::default(),
                        placeholder: None,
                    })
                    .collect();
                for (i, c) in stored.iter().enumerate() {
                    // Instant: last-known cached frame (marked cached, possibly stale).
                    if let Some(img) = snapshot::load_cached(&c.host) {
                        let tex = cc.egui_ctx.load_texture(
                            format!("snap{i}"),
                            img,
                            egui::TextureOptions::LINEAR,
                        );
                        app.cams[i].placeholder = Some((tex, true));
                    }
                    // Fresh: live snapshot replaces it and refreshes the cache.
                    snapshot::spawn_fetch(
                        c.clone(),
                        i,
                        config::SUB_CHANNEL,
                        tx.clone(),
                        cc.egui_ctx.clone(),
                    );
                }
            }
        }
        app.start_streams(&cc.egui_ctx);
        // Reopen where the user left off (SessionStore port): a focused
        // camera comes straight back, snapshot/substream bridging the wait.
        let st = session::load();
        if st.location == "camera"
            && let Some(idx) = st.camera_host.and_then(|h| {
                app.cams
                    .iter()
                    .position(|c| !c.host.is_empty() && c.host == h)
            })
        {
            app.focus(idx, &cc.egui_ctx);
        }
        app
    }

    fn start_streams(&mut self, ctx: &egui::Context) {
        let hw = self.prefs.hwaccel();
        for cam in &mut self.cams {
            let c = ctx.clone();
            cam.shared = stream::start(cam.sub_url.clone(), hw, move || c.request_repaint());
        }
    }

    /// Decode setting changed: tear down and relaunch every stream
    /// (saving applies immediately, like the Mac app's Settings).
    fn restart_streams(&mut self, ctx: &egui::Context) {
        if let Some((_, main)) = self.focused.take() {
            main.stop();
        }
        for cam in &self.cams {
            cam.shared.stop();
        }
        self.start_streams(ctx);
    }

    fn focus(&mut self, idx: usize, ctx: &egui::Context) {
        let Some(url) = self.cams[idx].main_url.clone() else {
            return;
        };
        let c = ctx.clone();
        let main = stream::start(url, self.prefs.hwaccel(), move || c.request_repaint());
        self.focused = Some((idx, main));
        self.last_key_sel = idx; // arrows resume from here after unfocus
        self.key_sel = None;
        if !self.cams[idx].host.is_empty() {
            session::save("camera", Some(&self.cams[idx].host));
        }
    }

    fn unfocus(&mut self) {
        if let Some((_, main)) = self.focused.take() {
            main.stop();
            session::save("grid", None);
        }
    }

    /// Aspect-fit `dims` (or 16:9 if unknown) inside `cell`.
    fn fit(cell: egui::Rect, dims: Option<egui::Vec2>) -> egui::Rect {
        let ts = dims.unwrap_or(egui::vec2(16.0, 9.0));
        let scale = (cell.width() / ts.x).min(cell.height() / ts.y);
        egui::Rect::from_center_size(cell.center(), ts * scale)
    }

    fn frame_dims(shared: &stream::Shared) -> Option<egui::Vec2> {
        shared
            .current
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| egui::vec2(f.width as f32, f.height as f32))
    }

    /// White text on a translucent black pill — the Mac app's overlay style
    /// (TileView label: white on 55% black; nerd stats: white / dim white).
    fn label(
        painter: &egui::Painter,
        pos: egui::Pos2,
        align: egui::Align2,
        text: &str,
        font: egui::FontId,
        color: egui::Color32,
    ) {
        let galley = painter.layout(text.to_string(), font, color, f32::INFINITY);
        let rect = align.anchor_size(pos, galley.size());
        painter.rect_filled(
            rect.expand2(egui::vec2(5.0, 3.0)),
            3.0,
            egui::Color32::from_black_alpha(140),
        );
        painter.galley(rect.min, galley, color);
    }

    /// Placeholder JPEG, aspect-fit in `cell`; a cached one is dimmed to 75%
    /// and badged so it's never mistaken for live (TileView.setPlaceholder).
    fn draw_placeholder(
        painter: &egui::Painter,
        cell: egui::Rect,
        tex: &egui::TextureHandle,
        cached: bool,
    ) -> egui::Rect {
        let rect = Self::fit(cell, Some(tex.size_vec2()));
        let tint = if cached {
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 191)
        } else {
            egui::Color32::WHITE
        };
        painter.image(
            tex.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            tint,
        );
        if cached {
            Self::label(
                painter,
                rect.right_top() + egui::vec2(-6.0, 6.0),
                egui::Align2::RIGHT_TOP,
                "cached",
                egui::FontId::proportional(10.0),
                Self::WHITE,
            );
        }
        rect
    }

    const WHITE: egui::Color32 = egui::Color32::from_rgba_premultiplied(240, 240, 240, 255);
    const DIM: egui::Color32 = egui::Color32::from_rgba_premultiplied(150, 150, 150, 255);
    /// NSColor.systemRed — the grid keyboard cursor.
    const CURSOR_RED: egui::Color32 = egui::Color32::from_rgb(255, 59, 48);

    fn show_focused(&mut self, ui: &mut egui::Ui, avail: egui::Rect) {
        let Some((idx, main)) = &self.focused else {
            return;
        };
        let (idx, main) = (*idx, main.clone());
        let cam = &self.cams[idx];
        let main_stats = main.stats.lock().unwrap().clone();

        // Main stream once it has a frame on screen; the substream's picture
        // as a stand-in while it connects (the Mac app's cached-frame trick),
        // and the snapshot placeholder before even that.
        let main_showing = main.current.lock().unwrap().is_some();
        let (id, shared) = if main_showing {
            (idx as u64 | MAIN_BIT, main.clone())
        } else {
            (idx as u64, self.cams[idx].shared.clone())
        };
        let dims = Self::frame_dims(&shared);
        if dims.is_some() {
            let rect = Self::fit(avail, dims);
            ui.painter()
                .add(eframe::egui_wgpu::Callback::new_paint_callback(
                    rect,
                    render::VideoCallback { id, shared },
                ));
        } else if let Some((tex, cached)) = &cam.placeholder {
            Self::draw_placeholder(ui.painter(), avail, tex, *cached);
        }

        Self::label(
            ui.painter(),
            avail.left_top() + egui::vec2(10.0, 8.0),
            egui::Align2::LEFT_TOP,
            &format!("{} — {}", cam.name, main_stats.status),
            egui::FontId::proportional(12.0),
            Self::WHITE,
        );
    }

    /// One arrow press: show the cursor on the last-used tile, or move a
    /// visible cursor by (dc, dr), clamped to the grid (GridView.swift port).
    fn move_key_cursor(&mut self, dc: i32, dr: i32, cols: usize) {
        let n = self.cams.len();
        let mut i = self.last_key_sel.min(n - 1);
        if let Some((cur, _)) = self.key_sel {
            let rows = n.div_ceil(cols);
            let c = ((cur % cols) as i32 + dc).clamp(0, cols as i32 - 1) as usize;
            let r = ((cur / cols) as i32 + dr).clamp(0, rows as i32 - 1) as usize;
            i = (r * cols + c).min(n - 1);
        }
        self.last_key_sel = i;
        self.key_sel = Some((i, Instant::now() + std::time::Duration::from_secs(2)));
    }

    fn show_grid(&mut self, ui: &mut egui::Ui, avail: egui::Rect) {
        let n = self.cams.len();
        // Pick the column count that gives the largest 16:9 tiles.
        let mut cols = 1;
        let mut best = 0.0f32;
        for c in 1..=n {
            let rows = n.div_ceil(c);
            let (cw, ch) = (avail.width() / c as f32, avail.height() / rows as f32);
            let scale = (cw / 16.0).min(ch / 9.0);
            if scale > best {
                best = scale;
                cols = c;
            }
        }
        let rows = n.div_ceil(cols);
        let (cw, ch) = (avail.width() / cols as f32, avail.height() / rows as f32);

        // Keyboard navigation: arrows drive the red cursor, Return focuses it,
        // 5 s of inactivity clears it.
        let mut focus: Option<usize> = None;
        if !self.settings_open {
            ui.input(|i| {
                if i.key_pressed(egui::Key::ArrowLeft) {
                    self.move_key_cursor(-1, 0, cols);
                }
                if i.key_pressed(egui::Key::ArrowRight) {
                    self.move_key_cursor(1, 0, cols);
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    self.move_key_cursor(0, -1, cols);
                }
                if i.key_pressed(egui::Key::ArrowDown) {
                    self.move_key_cursor(0, 1, cols);
                }
            });
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

        for (i, cam) in self.cams.iter().enumerate() {
            let cell = egui::Rect::from_min_size(
                avail.left_top() + egui::vec2((i % cols) as f32 * cw, (i / cols) as f32 * ch),
                egui::vec2(cw, ch),
            )
            .shrink(1.0);
            let dims = Self::frame_dims(&cam.shared);
            let rect = match (dims, &cam.placeholder) {
                (Some(d), _) => {
                    let rect = Self::fit(cell, Some(d));
                    ui.painter()
                        .add(eframe::egui_wgpu::Callback::new_paint_callback(
                            rect,
                            render::VideoCallback {
                                id: i as u64,
                                shared: cam.shared.clone(),
                            },
                        ));
                    rect
                }
                (None, Some((tex, cached))) => {
                    Self::draw_placeholder(ui.painter(), cell, tex, *cached)
                }
                (None, None) => Self::fit(cell, None),
            };

            let status = cam.shared.stats.lock().unwrap().status.clone();
            Self::label(
                ui.painter(),
                rect.left_bottom() + egui::vec2(6.0, -6.0),
                egui::Align2::LEFT_BOTTOM,
                &format!("{} — {}", cam.name, status),
                egui::FontId::proportional(12.0),
                Self::WHITE,
            );

            if self.key_sel.is_some_and(|(sel, _)| sel == i) {
                ui.painter().rect_stroke(
                    rect.shrink(1.5),
                    0.0,
                    egui::Stroke::new(3.0, Self::CURSOR_RED),
                    egui::StrokeKind::Inside,
                );
            }

            if cam.main_url.is_some() {
                let resp = ui.interact(cell, egui::Id::new("tile").with(i), egui::Sense::click());
                if resp.double_clicked() {
                    focus = Some(i);
                }
            }
        }
        if let Some(i) = focus {
            let ctx = ui.ctx().clone();
            self.focus(i, &ctx);
        }

        Self::label(
            ui.painter(),
            avail.left_top() + egui::vec2(10.0, 8.0),
            egui::Align2::LEFT_TOP,
            &format!(
                "{} cameras · arrows + Return or double-click: main stream · Ctrl-,: settings",
                n
            ),
            egui::FontId::proportional(11.0),
            Self::DIM,
        );
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let mut decode_changed = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
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
                ui.add_space(8.0);
                ui.label("Render");
                let adapters = render::adapter_names();
                // Show the candidate while its restart prompt is up.
                let shown: Option<String> = self
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
                                self.pending_render = Some(val);
                            }
                        };
                        pick(None, "Default", ui);
                        for name in adapters {
                            pick(Some(name.clone()), name, ui);
                        }
                    });
                ui.small("Render changes take effect after restart.");
                ui.add_space(8.0);
                let mut smooth = self.prefs.smooth_live;
                if ui
                    .checkbox(
                        &mut smooth,
                        "Smooth live video (buffers ~0.2 s to absorb Wi-Fi jitter)",
                    )
                    .changed()
                {
                    self.prefs.smooth_live = smooth;
                    stream::SMOOTH.store(smooth, std::sync::atomic::Ordering::Relaxed);
                    self.prefs.save();
                }
            });
        self.settings_open = open;
        if decode_changed {
            self.prefs.save();
            self.restart_streams(ctx);
        }

        // Render change: confirm before relaunching; Cancel reverts.
        if let Some(pending) = self.pending_render.clone() {
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
                            self.pending_render = None;
                            relaunch(ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            self.pending_render = None;
                        }
                    });
                });
        }
    }
}

/// Spawn a fresh instance (same binary, same args) and exit this one on the
/// spot — our ffmpeg children die on their broken pipes.
fn relaunch(_ctx: &egui::Context) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(std::env::args().skip(1))
            .spawn();
    }
    std::process::exit(0);
}

impl Drop for App {
    fn drop(&mut self) {
        self.unfocus();
        for cam in &self.cams {
            cam.shared.stop(); // don't orphan ffmpeg
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Close instantly: every ffmpeg dies on its broken stdout pipe the
        // moment we're gone (PDEATHSIG covers stalled ones on Linux), and
        // prefs are saved when changed — nothing needs a graceful path.
        if ui.input(|i| i.viewport().close_requested()) {
            std::process::exit(0);
        }
        let ctx = ui.ctx().clone();
        // Fresh ISAPI snapshots replace the cached placeholders, unbadged.
        while let Ok((idx, img)) = self.snap_rx.try_recv() {
            let tex = ctx.load_texture(format!("snap{idx}"), img, egui::TextureOptions::LINEAR);
            if let Some(cam) = self.cams.get_mut(idx) {
                cam.placeholder = Some((tex, false));
            }
        }
        if ui
            .input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::Comma))
        {
            self.settings_open = !self.settings_open;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.settings_open {
                self.settings_open = false;
            } else {
                self.unfocus();
            }
        }

        // Promote every due frame to the screen and wake up exactly when the
        // next scheduled frame is due (smoothing's presentation pump).
        let now = std::time::Instant::now();
        let mut next_due: Option<std::time::Instant> = None;
        let mut bump = |d: Option<std::time::Instant>| {
            if let Some(d) = d {
                next_due = Some(next_due.map_or(d, |n| n.min(d)));
            }
        };
        for cam in &self.cams {
            bump(cam.shared.advance(now));
        }
        if let Some((_, main)) = &self.focused {
            bump(main.advance(now));
        }
        if let Some(d) = next_due {
            ctx.request_repaint_after(d.saturating_duration_since(now));
        }

        let avail = ui.max_rect();
        ui.painter().rect_filled(avail, 0.0, egui::Color32::BLACK);
        if self.focused.is_some() {
            self.show_focused(ui, avail);
        } else {
            self.show_grid(ui, avail);
        }
        if self.settings_open {
            self.show_settings(&ctx);
        }
    }
}
