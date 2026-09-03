// media.rs — snapshots (JPEG) and clips (MP4) of the focused camera, saved
// straight to the desktop under the Mac app's naming (MediaSaver.swift).
// No save panel here (that needs a file-dialog dependency): files get a
// unique name and the HUD says where they went. Live snapshots come off the
// camera's ISAPI picture endpoint at full resolution; clips are an ffmpeg
// stream copy (no transcode) into fragmented MP4, so even a hard quit
// leaves a playable file.

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

/// The desktop (the XDG user dir on Linux), falling back to home.
pub fn save_dir() -> PathBuf {
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

/// "Front Door 2026-07-20 14.32.05.jpg" (wall clock), never overwriting —
/// collisions get " (2)"….
pub fn unique_path(camera: &str, ext: &str) -> PathBuf {
    let name = camera.replace('/', "-").replace(':', ".");
    let stamp = chrono::Local::now().format("%Y-%m-%d %H.%M.%S");
    let dir = save_dir();
    let mut path = dir.join(format!("{name} {stamp}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{name} {stamp} ({n}).{ext}"));
        n += 1;
    }
    path
}

/// Full-resolution JPEG of "now" from the camera itself, in the background.
pub fn spawn_snapshot(cam: StoredCamera, name: String, tx: Sender<Msg>, ctx: egui::Context) {
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
            let out = unique_path(&name, "jpg");
            std::fs::write(&out, &jpeg).map_err(|e| e.to_string())?;
            Ok(out)
        })();
        let _ = tx.send(Msg::Snapshot(result));
        ctx.request_repaint();
    });
}

/// One in-flight clip: an ffmpeg stream-copy mux independent of the viewing
/// pipeline (ClipRecorder.swift). Records the main stream from the moment
/// R is pressed.
pub struct Recorder {
    child: Child,
    pub path: PathBuf,
    pub started: Instant,
    /// The camera it belongs to — leaving that camera stops the clip.
    pub host: String,
}

impl Recorder {
    pub fn start(url: &str, hevc: bool, name: &str, host: String) -> Result<Recorder, String> {
        let path = unique_path(name, "mp4");
        let mut cmd = Command::new(crate::stream::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-rtsp_transport", "tcp", "-i", url, "-an", "-c:v", "copy"]);
        if hevc {
            cmd.args(["-tag:v", "hvc1"]); // QuickTime-openable HEVC
        }
        cmd.args(["-f", "mp4", "-movflags", "frag_keyframe+empty_moov", "-y"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if std::env::var_os("HIK_DEBUG").is_some() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("ffmpeg failed to launch: {e}"))?;
        Ok(Recorder {
            child,
            path,
            started: Instant::now(),
            host,
        })
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

    fn interrupt(&mut self) {
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
