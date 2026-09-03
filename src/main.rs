// main.rs — HikYeah: cross-platform HikViewer port (Rust + egui + wgpu).
//
// App state and the frame loop live here; the views are their own modules
// (grid.rs, focused.rs, settings.rs, overlay.rs). A grid of live substream
// tiles, one per configured camera (mirrors the macOS app: grid on channel
// 102). Double-clicking a tile focuses it full-window on the camera's main
// stream (101); Esc returns to the grid. S saves a snapshot and R records a
// clip of the focused camera; I shows the nerd-stats panel; Ctrl-, opens
// Settings.
//
//   hikyeah                cameras from the config file (config.rs) — the
//                          Settings window opens when there are none yet
//   hikyeah <rtsp-url>     single explicit URL (no focus view, no editing)
//   hikyeah --test         ffmpeg synthetic test pattern (no camera needed)

mod config;
mod focused;
mod grid;
mod isapi;
mod media;
mod overlay;
mod prefs;
mod render;
mod session;
mod settings;
mod snapshot;
mod stats;
mod stream;
mod tile;
mod update;

use eframe::egui;
use std::sync::Arc;
use std::time::Instant;

fn main() -> eframe::Result {
    // winit's Wayland path can burn a full core just presenting at 60 Hz
    // (measured on Hyprland + NVIDIA: a blank window costs 98% of a core on
    // Wayland vs 7% via Xwayland — same binary). Prefer X11 when available;
    // HIK_WAYLAND=1 opts back in. Safe here: no threads exist yet.
    if std::env::var_os("HIK_WAYLAND").is_none() && std::env::var_os("DISPLAY").is_some() {
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }
    let arg = std::env::args().nth(1);
    let source = match arg.as_deref() {
        Some("--test") => Source::Single("test pattern".into(), "--test".into()),
        Some(u) if u.starts_with("rtsp://") => Source::Single("camera".into(), u.into()),
        Some(other) => {
            eprintln!("usage: hikyeah [rtsp://… | --test]  (unrecognized: {other})");
            std::process::exit(2);
        }
        None => Source::Config(config::load().unwrap_or_default()),
    };
    // Skip auto-fullscreen when unconfigured: the Settings window that
    // opens on first run would be buried behind it (AppDelegate.swift).
    let configured = match &source {
        Source::Single(..) => true,
        Source::Config(cfg) => !cfg.cameras.is_empty(),
    };

    #[cfg(target_os = "linux")]
    let instance_lock = single_instance_lock();
    #[cfg(not(target_os = "linux"))]
    let instance_lock: Option<std::fs::File> = None;

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
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 810.0])
            .with_fullscreen(app_prefs.start_fullscreen && configured),
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
        Box::new(move |cc| Ok(Box::new(App::new(cc, source, app_prefs, instance_lock)))),
    )
}

enum Source {
    /// Explicit URL or --test: one tile, no main-stream focus.
    Single(String, String),
    Config(config::StoredConfig),
}

/// One camera in the grid: its running substream and how to reach the
/// main stream when focused.
pub struct Cam {
    pub name: String,
    /// Identifies the camera across launches and reorders (session state).
    pub host: String,
    /// Stable id for GPU tile state, snapshot deliveries and animations —
    /// indices shift under reorders and Settings saves.
    pub id: u64,
    pub sub_url: String,
    pub main_url: Option<String>,
    pub shared: Arc<stream::Shared>,
    /// JPEG shown until the first live frame: last-known from disk
    /// (true = "cached", dimmed + badged) or a fresh ISAPI snapshot (false).
    pub placeholder: Option<(egui::TextureHandle, bool)>,
}

/// The full-window view: which camera, its main stream, and digital zoom
/// (scale 1–8 and the video point, 0..1, sitting at the view center).
pub struct Focused {
    pub idx: usize,
    pub main: Arc<stream::Shared>,
    pub zoom: f32,
    pub center: egui::Vec2,
}

pub struct App {
    pub cams: Vec<Cam>,
    /// The on-disk config the grid was built from; None in single-URL mode
    /// (nothing to edit, no order to persist).
    pub config: Option<config::StoredConfig>,
    pub focused: Option<Focused>,
    pub prefs: prefs::Prefs,
    pub settings: settings::SettingsUi,
    /// Grid keyboard cursor (red border): tile index + when it fades.
    pub key_sel: Option<(usize, Instant)>,
    /// Arrows resume from here (last cursor position or last focused tile).
    pub last_key_sel: usize,
    /// Long-press drag-to-reorder in progress.
    pub drag: Option<grid::Drag>,
    /// The current press moved too far to become a long press.
    pub press_voided: bool,
    /// Tiles glide to their slots until then (after a drop or cancel).
    pub settle_until: Instant,
    pub help_open: bool,
    pub nerd: stats::NerdStats,
    /// In-flight clip recording (R); at most one, tied to its camera.
    pub recorder: Option<media::Recorder>,
    /// Shutter flash for snapshots: when it started.
    pub flash_at: Option<Instant>,
    /// Finished captures waiting to be named (front is on screen).
    pub save_prompts: std::collections::VecDeque<overlay::SavePrompt>,
    media_tx: std::sync::mpsc::Sender<media::Msg>,
    media_rx: std::sync::mpsc::Receiver<media::Msg>,
    /// For background work that needs to wake the UI (recorder shutdown).
    ctx: egui::Context,
    /// The auto-hiding top bar is out (pointer at the top edge or on it).
    pub top_bar: bool,
    /// Transient centered message and when it appeared (HUD.swift).
    pub hud: Option<(String, Instant)>,
    next_cam_id: u64,
    /// Fresh snapshots arriving from the background ISAPI fetches.
    snap_tx: std::sync::mpsc::Sender<(u64, egui::ColorImage)>,
    snap_rx: std::sync::mpsc::Receiver<(u64, egui::ColorImage)>,
    /// Held for our lifetime; handed to relaunch() so the successor can take it.
    pub instance_lock: Option<std::fs::File>,
    /// HIK_DEBUG UI-loop stats: updates + time inside ui() per report window.
    dbg_frames: u32,
    dbg_spent: std::time::Duration,
    dbg_win_start: Instant,
    pub update: UpdateUi,
    pub upd_tx: std::sync::mpsc::Sender<update::Msg>,
    upd_rx: std::sync::mpsc::Receiver<update::Msg>,
}

/// Update flow state: the banner shows Available/Installing/InstallFailed.
pub enum UpdateUi {
    Idle,
    Checking,
    Available(update::Release),
    Installing,
    InstallFailed(String),
}

/// Stable tile ids for per-tile GPU state: grid substream = camera id,
/// focused main stream = camera id | MAIN_BIT.
pub const MAIN_BIT: u64 = 1 << 32;

/// Repaints are coalesced to ~60 Hz: a dozen cameras deliver 200+ frames/s
/// combined, and repainting per frame burns a core drawing pixels the display
/// never shows. Every wake asks for "a repaint within 16 ms" instead of "now",
/// so one redraw presents everything that arrived in the window.
pub const REPAINT_COALESCE: std::time::Duration = std::time::Duration::from_millis(16);

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        source: Source,
        app_prefs: prefs::Prefs,
        instance_lock: Option<std::fs::File>,
    ) -> Self {
        let rs = cc.wgpu_render_state.as_ref().expect("wgpu render state");
        rs.renderer
            .write()
            .callback_resources
            .insert(render::VideoRenderer::new(&rs.device, rs.target_format));
        let (snap_tx, snap_rx) = std::sync::mpsc::channel();
        let (upd_tx, upd_rx) = std::sync::mpsc::channel();
        let (media_tx, media_rx) = std::sync::mpsc::channel();
        // Launch-time update check (Updater.checkInBackground): installed
        // builds only — a source build shouldn't be nagged about releases.
        if update::installed() {
            update::check(upd_tx.clone(), cc.egui_ctx.clone());
        }
        let mut app = App {
            cams: Vec::new(),
            config: None,
            focused: None,
            prefs: app_prefs,
            settings: settings::SettingsUi::default(),
            key_sel: None,
            last_key_sel: 0,
            drag: None,
            press_voided: false,
            settle_until: Instant::now(),
            help_open: false,
            nerd: stats::NerdStats::default(),
            recorder: None,
            flash_at: None,
            save_prompts: Default::default(),
            media_tx,
            media_rx,
            ctx: cc.egui_ctx.clone(),
            top_bar: false,
            hud: None,
            next_cam_id: 0,
            snap_tx,
            snap_rx,
            instance_lock,
            dbg_frames: 0,
            dbg_spent: std::time::Duration::ZERO,
            dbg_win_start: Instant::now(),
            update: UpdateUi::Idle,
            upd_tx,
            upd_rx,
        };
        match source {
            Source::Single(name, url) => {
                app.cams = vec![Cam {
                    name,
                    host: String::new(),
                    id: app.next_cam_id(),
                    sub_url: url,
                    main_url: None,
                    shared: Default::default(),
                    placeholder: None,
                }];
                app.start_streams(&cc.egui_ctx);
            }
            Source::Config(cfg) => {
                let configured = !cfg.cameras.is_empty();
                app.config = Some(cfg);
                app.rebuild(&cc.egui_ctx);
                if !configured {
                    app.open_settings();
                }
            }
        }
        // Reopen where the user left off (SessionStore port): a focused
        // camera comes straight back, snapshot/substream bridging the wait.
        let st = session::load();
        if app.prefs.remember_last_view
            && st.location == session::Location::Camera
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

    fn next_cam_id(&mut self) -> u64 {
        self.next_cam_id += 1;
        self.next_cam_id
    }

    /// (Re)build the grid from `config`: stop everything running, then one
    /// tile per stored camera with its cached frame and a fresh snapshot on
    /// the way (AppDelegate.rebuildStreams). Programmatic — the remembered
    /// view isn't rewritten; quitting later records the grid anyway.
    pub fn rebuild(&mut self, ctx: &egui::Context) {
        self.stop_recording();
        if let Some(f) = self.focused.take() {
            f.main.stop();
        }
        for cam in &self.cams {
            cam.shared.stop();
        }
        self.key_sel = None;
        self.last_key_sel = 0;
        self.drag = None;
        let stored = self
            .config
            .as_ref()
            .map(|c| c.cameras.clone())
            .unwrap_or_default();
        self.cams = Vec::with_capacity(stored.len());
        for c in &stored {
            let id = self.next_cam_id();
            // Instant: last-known cached frame (marked cached, possibly stale).
            let placeholder = snapshot::load_cached(&c.host).map(|img| {
                let tex = ctx.load_texture(format!("snap{id}"), img, egui::TextureOptions::LINEAR);
                (tex, true)
            });
            // Fresh: live snapshot replaces it and refreshes the cache.
            snapshot::spawn_fetch(
                c.clone(),
                id,
                config::SUB_CHANNEL,
                self.snap_tx.clone(),
                ctx.clone(),
            );
            self.cams.push(Cam {
                name: if c.name.is_empty() {
                    c.host.clone()
                } else {
                    c.name.clone()
                },
                host: c.host.clone(),
                id,
                sub_url: config::rtsp_url(c, config::SUB_CHANNEL),
                main_url: Some(config::rtsp_url(c, config::MAIN_CHANNEL)),
                shared: Default::default(),
                placeholder,
            });
        }
        self.start_streams(ctx);
    }

    fn start_streams(&mut self, ctx: &egui::Context) {
        let hw = self.prefs.hwaccel();
        for cam in &mut self.cams {
            let c = ctx.clone();
            cam.shared = stream::start(cam.sub_url.clone(), hw, move || {
                c.request_repaint_after(REPAINT_COALESCE)
            });
        }
    }

    /// Decode setting changed: tear down and relaunch every stream
    /// (saving applies immediately, like the Mac app's Settings).
    pub fn restart_streams(&mut self, ctx: &egui::Context) {
        self.stop_recording();
        if let Some(f) = self.focused.take() {
            f.main.stop();
        }
        for cam in &self.cams {
            cam.shared.stop();
        }
        self.start_streams(ctx);
    }

    pub fn focus(&mut self, idx: usize, ctx: &egui::Context) {
        let Some(url) = self.cams[idx].main_url.clone() else {
            return;
        };
        let c = ctx.clone();
        let main = stream::start(url, self.prefs.hwaccel(), move || {
            c.request_repaint_after(REPAINT_COALESCE)
        });
        self.focused = Some(Focused {
            idx,
            main,
            zoom: 1.0,
            center: egui::vec2(0.5, 0.5),
        });
        self.last_key_sel = idx; // arrows resume from here after unfocus
        self.key_sel = None;
        if !self.cams[idx].host.is_empty() {
            session::save(session::Location::Camera, Some(&self.cams[idx].host));
        }
    }

    pub fn unfocus(&mut self) {
        if let Some(f) = self.focused.take() {
            self.stop_recording(); // a clip follows its camera, not the view
            f.main.stop();
            session::save(session::Location::Grid, None);
        }
    }

    /// The focused camera's stored entry (credentials, codec), if any.
    fn focused_stored(&self) -> Option<(usize, config::StoredCamera)> {
        let f = self.focused.as_ref()?;
        let cam = &self.cams[f.idx];
        let stored = self
            .config
            .as_ref()?
            .cameras
            .iter()
            .find(|c| c.host == cam.host)?;
        Some((f.idx, stored.clone()))
    }

    /// S: full-resolution snapshot of the focused camera to the desktop.
    /// The shutter flash marks the captured moment; the HUD names the file.
    fn save_snapshot(&mut self) {
        let Some((idx, stored)) = self.focused_stored() else {
            return;
        };
        self.flash_at = Some(Instant::now());
        media::spawn_snapshot(
            stored,
            self.cams[idx].name.clone(),
            self.media_tx.clone(),
            self.ctx.clone(),
        );
    }

    /// R: start or stop recording the focused camera's main stream.
    fn toggle_recording(&mut self) {
        if self.recorder.is_some() {
            self.stop_recording();
            return;
        }
        let Some((idx, stored)) = self.focused_stored() else {
            return;
        };
        let cam = &self.cams[idx];
        match media::Recorder::start(
            &config::rtsp_url(&stored, config::MAIN_CHANNEL),
            stored.codec != "h264",
            &cam.name,
            cam.host.clone(),
        ) {
            Ok(r) => self.recorder = Some(r),
            Err(e) => self.flash(&e),
        }
    }

    fn stop_recording(&mut self) {
        if let Some(r) = self.recorder.take() {
            r.stop(self.media_tx.clone(), self.ctx.clone());
        }
    }

    /// A capture landed on disk under its default name: offer to rename it
    /// (the Mac app's save panel, minus the folder picker).
    fn media_done(&mut self, result: Result<std::path::PathBuf, String>, what: &'static str) {
        match result {
            Ok(path) => self
                .save_prompts
                .push_back(overlay::SavePrompt::new(path, what)),
            Err(e) => self.flash(&format!("{what} failed: {e}")),
        }
    }

    /// Esc, in the Mac app's order: dialogs first, then reorder, cursor,
    /// zoom, and finally leaving the camera view.
    fn escape(&mut self) {
        if self.save_prompts.front().is_some() {
            self.finish_save_prompt(false);
        } else if self.settings.editor.is_some() {
            self.settings.editor = None;
        } else if self.settings.open {
            self.settings.open = false;
        } else if self.drag.is_some() {
            self.cancel_drag();
        } else if self.key_sel.is_some() {
            self.key_sel = None;
        } else if self.focused.as_ref().is_some_and(Focused::zoomed) {
            self.focused.as_mut().unwrap().reset_zoom();
        } else {
            self.unfocus();
        }
    }
}

/// One instance per user: a duplicate would double every camera's RTSP
/// connections (cameras cap those) and the decode load. The kernel releases
/// the flock with the process, so a crash never leaves it stale. macOS gets
/// this free from LaunchServices once we ship a .app; Windows will want a
/// named mutex.
#[cfg(target_os = "linux")]
fn single_instance_lock() -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("hikyeah-{}.lock", unsafe { libc::getuid() }));
    match std::fs::File::create(&path) {
        Ok(f) => {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                eprintln!("HikYeah is already running ({} is locked)", path.display());
                std::process::exit(1);
            }
            Some(f)
        }
        Err(e) => {
            eprintln!(
                "[warn] no single-instance lock ({}: {e}) — running anyway",
                path.display()
            );
            None
        }
    }
}

/// Spawn a fresh instance (same binary, same args) and exit this one on the
/// spot — our ffmpeg children die on their broken pipes. Takes the instance
/// lock so it's released before the successor checks it.
pub fn relaunch(lock: Option<std::fs::File>) -> ! {
    drop(lock);
    if let Ok(mut exe) = std::env::current_exe() {
        // After a self-update the old binary is unlinked and /proc/self/exe
        // reads "…/hikyeah (deleted)" — point back at the fresh file.
        if let Some(orig) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            exe = orig.into();
        }
        let _ = std::process::Command::new(exe)
            .args(std::env::args().skip(1))
            .spawn();
    }
    std::process::exit(0);
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(r) = self.recorder.take() {
            r.stop_and_wait();
        }
        self.unfocus();
        for cam in &self.cams {
            cam.shared.stop(); // don't orphan ffmpeg
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // HIK_DEBUG: report the UI loop rate and the time spent inside this
        // function — separates "repaint storm" from "cost outside our code".
        let dbg_start = std::env::var_os("HIK_DEBUG").map(|_| Instant::now());
        if let Some(now) = dbg_start {
            self.dbg_frames += 1;
            let win = now.duration_since(self.dbg_win_start);
            if win.as_secs_f32() >= 2.0 {
                eprintln!(
                    "[ui] {:.0} updates/s, {:.2} ms inside ui() per update",
                    self.dbg_frames as f32 / win.as_secs_f32(),
                    self.dbg_spent.as_secs_f64() * 1000.0 / self.dbg_frames as f64,
                );
                self.dbg_frames = 0;
                self.dbg_spent = std::time::Duration::ZERO;
                self.dbg_win_start = now;
            }
        }
        // Close instantly: every ffmpeg dies on its broken stdout pipe the
        // moment we're gone (PDEATHSIG covers stalled ones on Linux), and
        // prefs are saved when changed — nothing needs a graceful path.
        if ui.input(|i| i.viewport().close_requested()) {
            // Except a clip in progress: give ffmpeg a moment to finalize.
            if let Some(r) = self.recorder.take() {
                r.stop_and_wait();
            }
            std::process::exit(0);
        }
        let ctx = ui.ctx().clone();
        // Fresh ISAPI snapshots replace the cached placeholders, unbadged.
        while let Ok((id, img)) = self.snap_rx.try_recv() {
            if let Some(cam) = self.cams.iter_mut().find(|c| c.id == id) {
                let tex = ctx.load_texture(format!("snap{id}"), img, egui::TextureOptions::LINEAR);
                cam.placeholder = Some((tex, false));
            }
        }
        while let Ok(msg) = self.media_rx.try_recv() {
            match msg {
                media::Msg::Snapshot(r) => self.media_done(r, "Snapshot"),
                media::Msg::Clip(r) => self.media_done(r, "Recording"),
            }
        }
        // ffmpeg died on its own mid-clip (stream error): report the file.
        if let Some(result) = self.recorder.as_mut().and_then(media::Recorder::poll) {
            self.recorder = None;
            self.media_done(result, "Recording");
        }
        while let Ok(msg) = self.upd_rx.try_recv() {
            match msg {
                update::Msg::Available(r) => self.update = UpdateUi::Available(r),
                update::Msg::UpToDate(tag) => {
                    self.update = UpdateUi::Idle;
                    self.settings.update_note =
                        Some(format!("Up to date — {tag} is the latest release."));
                }
                update::Msg::CheckFailed(e) => {
                    self.update = UpdateUi::Idle;
                    self.settings.update_note = Some(format!("Update check failed: {e}"));
                }
                // Onto the new binary + ffmpeg the installer just swapped in.
                update::Msg::Installed => relaunch(self.instance_lock.take()),
                update::Msg::InstallFailed(e) => self.update = UpdateUi::InstallFailed(e),
            }
        }

        // The help sheet swallows the key or click that closes it; text
        // fields keep their keystrokes (a "?" in a password is a "?").
        let typing = ctx.egui_wants_keyboard_input();
        let help_was_open = self.help_open;
        if help_was_open {
            if ui.input(|i| {
                i.pointer.any_pressed()
                    || i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Key { pressed: true, .. }))
            }) {
                self.help_open = false;
            }
        } else if !typing
            && ui.input(|i| {
                i.key_pressed(egui::Key::Questionmark)
                    || i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Text(t) if t == "?"))
            })
        {
            self.help_open = true;
        }
        if !help_was_open {
            if ui.input(|i| {
                (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::Comma)
            }) {
                if self.settings.open {
                    self.settings.open = false;
                } else {
                    self.open_settings();
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::F11)) {
                let fs = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.escape();
            }
            if !typing {
                if ui.input(|i| i.key_pressed(egui::Key::S)) {
                    self.save_snapshot();
                }
                if ui.input(|i| i.key_pressed(egui::Key::R)) {
                    self.toggle_recording();
                }
                if ui.input(|i| i.key_pressed(egui::Key::I)) {
                    self.prefs.nerd_stats = !self.prefs.nerd_stats;
                    self.prefs.save();
                }
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
        if let Some(f) = &self.focused {
            bump(f.main.advance(now));
        }
        if let Some(d) = next_due {
            // Quantized to the coalescing window — a frame due in 2 ms must
            // not re-trigger the per-frame redraw rate this exists to cap.
            ctx.request_repaint_after(d.saturating_duration_since(now).max(REPAINT_COALESCE));
        }

        let avail = ui.max_rect();
        ui.painter().rect_filled(avail, 0.0, egui::Color32::BLACK);
        if self.focused.is_some() {
            self.show_focused(ui, avail);
        } else {
            self.show_grid(ui, avail);
        }
        self.show_nerd_stats(&ctx);
        self.show_save_prompt(&ctx);
        self.show_top_bar(&ctx);
        self.show_update_banner(&ctx);
        if self.settings.open {
            self.show_settings(&ctx);
        }
        if self.help_open {
            overlay::show_help(
                &ctx,
                if self.focused.is_some() {
                    overlay::HelpContext::Camera
                } else {
                    overlay::HelpContext::Grid
                },
            );
        }
        self.show_hud(&ctx);
        if let Some(start) = dbg_start {
            self.dbg_spent += start.elapsed();
        }
    }
}
