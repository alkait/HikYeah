// media.rs — snapshots (JPEG) and clips (MP4) of the focused camera, saved
// under the Mac app's naming (MediaSaver.swift) and then offered for
// renaming. Live snapshots come off the camera's ISAPI picture endpoint at
// full resolution; playback snapshots and all clips go through ffmpeg over
// RTSP with stream copy (no transcode). Clips are fragmented MP4, so even a
// hard quit leaves a playable file.

use crate::config::StoredCamera;
use eframe::egui;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub enum Msg {
    Snapshot(Result<PathBuf, String>),
    /// A recording's ffmpeg exited; Ok carries the finished file.
    Clip(Result<PathBuf, String>),
}

/// Where captures go: the folder chosen in Settings (created on demand),
/// else the desktop.
pub fn save_dir(prefs: &crate::prefs::Prefs) -> Result<PathBuf, String> {
    let Some(custom) = prefs
        .save_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(default_save_dir());
    };
    let dir = expand_home(custom);
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't use {}: {e}", dir.display()))?;
    Ok(dir)
}

/// "~/Captures" → the home folder + "/Captures".
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => crate::config::home_dir().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(path),
    }
}

/// The desktop (the XDG user dir on Linux), falling back to home.
pub fn default_save_dir() -> PathBuf {
    let home = crate::config::home_dir();
    #[cfg(target_os = "linux")]
    {
        // ~/.config/user-dirs.dirs: XDG_DESKTOP_DIR="$HOME/Desktop"
        let dirs = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("user-dirs.dirs");
        if let Ok(text) = std::fs::read_to_string(dirs) {
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("XDG_DESKTOP_DIR=") {
                    let v = v
                        .trim_matches('"')
                        .replace("$HOME", &home.to_string_lossy());
                    let p = PathBuf::from(v);
                    if p.is_dir() {
                        return p;
                    }
                }
            }
        }
    }
    let desktop = home.join("Desktop");
    if desktop.is_dir() { desktop } else { home }
}

/// Move a finished capture to the name the user typed (same folder, same
/// extension); refuses to overwrite.
pub fn rename(path: &std::path::Path, stem: &str) -> Result<PathBuf, String> {
    let stem = stem.trim();
    if stem.is_empty() || stem.contains('/') || stem.contains('\\') {
        return Err("Enter a file name (no slashes)".into());
    }
    let ext = path.extension().map_or("", |e| e.to_str().unwrap_or(""));
    let target = path.with_file_name(format!("{stem}.{ext}"));
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        return Err("A file with that name already exists".into());
    }
    std::fs::rename(path, &target).map_err(|e| e.to_string())?;
    Ok(target)
}

/// "2026-07-20 14.32.05" — the timestamp is footage time for playback (NVR
/// timezone), wall clock for live (MediaSaver.defaultName).
pub fn stamp<Tz: chrono::TimeZone>(t: chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    t.format("%Y-%m-%d %H.%M.%S").to_string()
}

/// "Front Door 2026-07-20 14.32.05.jpg", never overwriting — collisions get
/// " (2)"….
pub fn unique_path(dir: &std::path::Path, camera: &str, stamp: &str, ext: &str) -> PathBuf {
    let name = camera.replace('/', "-").replace(':', ".");
    let mut path = dir.join(format!("{name} {stamp}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{name} {stamp} ({n}).{ext}"));
        n += 1;
    }
    path
}

/// Full-resolution JPEG of "now" from the camera itself, in the background.
pub fn spawn_snapshot(
    cam: StoredCamera,
    name: String,
    dir: PathBuf,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let result = (|| {
            let path = format!(
                "/ISAPI/Streaming/channels/{}/picture",
                crate::config::MAIN_CHANNEL
            );
            let jpeg = crate::isapi::get(&cam.host, &cam.user, &cam.password, &path)
                .ok_or("camera didn't answer")?;
            if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] {
                return Err("camera sent no picture".to_string());
            }
            let out = unique_path(&dir, &name, &stamp(chrono::Local::now()), "jpg");
            std::fs::write(&out, &jpeg).map_err(|e| e.to_string())?;
            Ok(out)
        })();
        let _ = tx.send(Msg::Snapshot(result));
        ctx.request_repaint();
    });
}

/// One decoded frame at the playback position, pulled from the NVR by
/// ffmpeg's own RTSP client (MediaSaver.capturePlaybackSnapshot). Takes a
/// few seconds — RTSP setup plus ffmpeg's initial buffering — so it works
/// while paused: scrub to the moment and press S.
pub fn spawn_playback_snapshot(
    url: String,
    name: String,
    stamp: String,
    dir: PathBuf,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let out = unique_path(&dir, &name, &stamp, "jpg");
        let result = (|| {
            let mut cmd = Command::new(crate::stream::ffmpeg_path());
            cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
                .args(["-rtsp_transport", "tcp", "-i", &url])
                .args(["-frames:v", "1", "-q:v", "2", "-f", "image2", "-y"])
                .arg(&out)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            pdeathsig(&mut cmd);
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("ffmpeg failed to launch: {e}"))?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while !matches!(child.try_wait(), Ok(Some(_))) {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) > 0 {
                Ok(out.clone())
            } else {
                let _ = std::fs::remove_file(&out);
                Err("the NVR sent no picture".to_string())
            }
        })();
        let _ = tx.send(Msg::Snapshot(result));
        ctx.request_repaint();
    });
}

/// Belt-and-braces on Linux: the kernel kills ffmpeg the instant we die.
fn pdeathsig(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// One in-flight clip: an ffmpeg stream-copy mux independent of the viewing
/// pipeline (ClipRecorder.swift), so pausing/seeking never disturbs the
/// file. Input is either ffmpeg's own RTSP pull (live from the camera,
/// playback from the NVR at 1×) or — for fast playback, where ffmpeg can't
/// send the `Scale:` header — Annex B NALs pushed in through stdin from a
/// native RTSP session, stamped by arrival time so the clip plays back at
/// the watched speed.
pub struct Recorder {
    child: Child,
    pub path: PathBuf,
    pub started: Instant,
    /// The camera it belongs to — leaving that camera stops the clip.
    pub host: String,
    /// Piped mode: the session feeding stdin. Stopping it closes stdin —
    /// EOF is the clean shutdown there.
    feed: Option<crate::rtsp::Session>,
}

impl Recorder {
    /// ffmpeg pulls `url` itself.
    pub fn start(
        url: &str,
        hevc: bool,
        name: &str,
        stamp: &str,
        host: String,
        dir: &std::path::Path,
    ) -> Result<Recorder, String> {
        let path = unique_path(dir, name, stamp, "mp4");
        let mut cmd = Command::new(crate::stream::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-rtsp_transport", "tcp", "-i", url, "-an", "-c:v", "copy"]);
        let child = Self::spawn(cmd, hevc, &path, false)?;
        Ok(Recorder {
            child,
            path,
            started: Instant::now(),
            host,
            feed: None,
        })
    }

    /// Piped mode (fast playback): raw elementary stream on stdin from a
    /// native session at `scale`, wall-clock timestamps — the NVR paces
    /// delivery at scale×, so arrival time IS the intended playback pace.
    pub fn start_piped(
        req: crate::rtsp::Request,
        name: &str,
        stamp: &str,
        host: String,
        dir: &std::path::Path,
    ) -> Result<Recorder, String> {
        let path = unique_path(dir, name, stamp, "mp4");
        let hevc = req.codec == "hevc";
        let mut cmd = Command::new(crate::stream::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error"])
            .args(["-use_wallclock_as_timestamps", "1"])
            .args(["-f", req.codec, "-i", "pipe:0", "-an", "-c:v", "copy"]);
        let mut child = Self::spawn(cmd, hevc, &path, true)?;
        let stdin = child.stdin.take().unwrap();
        let feed = crate::rtsp::start(req, stdin, |_| {});
        Ok(Recorder {
            child,
            path,
            started: Instant::now(),
            host,
            feed: Some(feed),
        })
    }

    fn spawn(
        mut cmd: Command,
        hevc: bool,
        path: &std::path::Path,
        piped: bool,
    ) -> Result<Child, String> {
        if hevc {
            cmd.args(["-tag:v", "hvc1"]); // QuickTime-openable HEVC
        }
        cmd.args(["-f", "mp4", "-movflags", "frag_keyframe+empty_moov", "-y"])
            .arg(path)
            .stdin(if piped { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::null())
            .stderr(if std::env::var_os("HIK_DEBUG").is_some() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        pdeathsig(&mut cmd);
        cmd.spawn()
            .map_err(|e| format!("ffmpeg failed to launch: {e}"))
    }

    /// ffmpeg exited on its own (stream error): the finished file, if any.
    pub fn poll(&mut self) -> Option<Result<PathBuf, String>> {
        matches!(self.child.try_wait(), Ok(Some(_))).then(|| self.finish())
    }

    /// Ask ffmpeg to finish the file cleanly (SIGINT), give it 3 s, then
    /// report through `tx` from a background thread.
    pub fn stop(mut self, tx: Sender<Msg>, ctx: egui::Context) {
        self.interrupt();
        std::thread::spawn(move || {
            self.wait_up_to(Duration::from_secs(3));
            let _ = tx.send(Msg::Clip(self.finish()));
            ctx.request_repaint();
        });
    }

    /// Quit path: block briefly so the file is finalized before we exit.
    pub fn stop_and_wait(mut self) {
        self.interrupt();
        self.wait_up_to(Duration::from_millis(1500));
    }

    /// SIGINT lets ffmpeg finish the file cleanly. Piped mode instead
    /// stops the feed — its closed stdin is the EOF ffmpeg finalizes on.
    fn interrupt(&mut self) {
        if let Some(feed) = self.feed.take() {
            feed.stop();
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        // No SIGINT on Windows; the fragmented MP4 survives a hard stop.
        #[cfg(not(unix))]
        let _ = self.child.kill();
    }

    fn wait_up_to(&mut self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !matches!(self.child.try_wait(), Ok(Some(_))) {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn finish(&self) -> Result<PathBuf, String> {
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > 4096 {
            Ok(self.path.clone())
        } else {
            let _ = std::fs::remove_file(&self.path);
            Err("recording failed — no video arrived".into())
        }
    }
}
