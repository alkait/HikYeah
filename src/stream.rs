// stream.rs — one camera's ffmpeg pipe (RTSP -> decoded frames).
//
// Unlike the macOS app (stream copy + VideoToolbox), ffmpeg decodes here and
// hands us raw yuv4mpegpipe frames on stdout — self-describing (the y4m header
// carries WxH), portable, and codec-agnostic. We convert I420 -> RGBA on the
// CPU and publish only the latest frame, so a slow UI never backs up the pipe
// (latency can't accumulate). Reconnects forever on any exit or stall, like
// CameraStream.swift.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Smooth live video: re-time frames onto the camera's steady beat behind a
/// ~0.2 s buffer, absorbing delivery jitter (port of the Mac app's smoothLive;
/// VideoStreamParser.swift). Checked per frame, so toggling applies to
/// running streams immediately.
pub static SMOOTH: AtomicBool = AtomicBool::new(true);

/// Scheduled headroom behind arrival — late deliveries still make their slot.
const SMOOTHING_DELAY: f64 = 0.2;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// I420: Y plane (w*h), then U and V (⌈w/2⌉*⌈h/2⌉ each). Uploaded to the
    /// GPU as-is; color conversion happens in the shader (render.rs).
    pub yuv: Vec<u8>,
    pub seq: u64,
    /// When to present (arrival time when smoothing is off).
    pub due: Instant,
}


#[derive(Default, Clone)]
pub struct Stats {
    pub status: String,
    pub frames: u64,
    pub reconnects: u32,
    /// Times smoothing gave up and restarted its schedule — each is one
    /// brief visible hiccup.
    pub reanchors: u32,
    pub fps: f32,
    pub first_frame_secs: Option<f32>,
}

#[derive(Default)]
pub struct Shared {
    /// The frame currently on screen (renderer reads this).
    pub current: Mutex<Option<Frame>>,
    /// Frames scheduled for the future, front = next due.
    queue: Mutex<VecDeque<Frame>>,
    /// Recycled plane buffers (steady state allocates nothing).
    pool: Mutex<Vec<Vec<u8>>>,
    pub stats: Mutex<Stats>,
    stopped: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Shared {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn set_status(&self, s: &str) {
        self.stats.lock().unwrap().status = s.to_string();
    }

    /// Promote every frame whose time has come to `current`; returns the next
    /// due time so the UI can wake up exactly then.
    pub fn advance(&self, now: Instant) -> Option<Instant> {
        let mut q = self.queue.lock().unwrap();
        let mut cur = self.current.lock().unwrap();
        while q.front().is_some_and(|f| f.due <= now) {
            let f = q.pop_front().unwrap();
            if let Some(old) = cur.replace(f) {
                self.recycle(old.yuv);
            }
        }
        q.front().map(|f| f.due)
    }

    fn recycle(&self, buf: Vec<u8>) {
        let mut pool = self.pool.lock().unwrap();
        if pool.len() < 8 {
            pool.push(buf);
        }
    }

    fn take_buffer(&self, len: usize) -> Vec<u8> {
        let mut pool = self.pool.lock().unwrap();
        if let Some(i) = pool.iter().position(|b| b.len() == len) {
            return pool.swap_remove(i);
        }
        vec![0u8; len]
    }

    fn publish(&self, frame: Frame) {
        let mut overflow = Vec::new();
        {
            let mut q = self.queue.lock().unwrap();
            q.push_back(frame);
            // Bound the schedule (~1.5 s at 20 fps); a deeper backlog means
            // the UI isn't consuming — drop from the front.
            while q.len() > 30 {
                overflow.push(q.pop_front().unwrap());
            }
        }
        for f in overflow {
            self.recycle(f.yuv);
        }
    }
}

/// Spawn the supervisor thread: launch ffmpeg, pump frames, relaunch on exit.
/// `hwaccel` is the user's decode choice (ffmpeg -hwaccel value; None = CPU).
/// `wake` is called after each published frame (UI repaint).
pub fn start(
    url: String,
    hwaccel: Option<&'static str>,
    wake: impl Fn() + Send + 'static,
) -> Arc<Shared> {
    let shared = Arc::new(Shared::default());
    let sh = shared.clone();
    std::thread::spawn(move || {
        while !sh.stopped.load(Ordering::SeqCst) {
            sh.set_status("connecting…");
            match run_once(&sh, &url, hwaccel, &wake) {
                Ok(()) => {}
                Err(e) => {
                    if std::env::var_os("HIK_DEBUG").is_some() {
                        eprintln!("[stream] {e}");
                    }
                }
            }
            if sh.stopped.load(Ordering::SeqCst) {
                break;
            }
            {
                let mut st = sh.stats.lock().unwrap();
                st.reconnects += 1;
                st.status = "reconnecting…".into();
                st.fps = 0.0;
            }
            wake();
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    shared
}

/// One ffmpeg lifetime: spawn, parse the y4m header, stream frames until EOF.
fn run_once(
    sh: &Shared,
    url: &str,
    hwaccel: Option<&'static str>,
    wake: &(impl Fn() + Send),
) -> Result<(), String> {
    let launch = Instant::now();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if url == "--test" {
        // Synthetic source: full pipeline minus the camera (dev/demo).
        // -re paces lavfi at realtime, like a camera would.
        cmd.args(["-re", "-f", "lavfi", "-i", "testsrc2=size=704x576:rate=25"]);
    } else {
        cmd.args(["-rtsp_transport", "tcp", "-fflags", "nobuffer", "-flags", "low_delay"]);
        if let Some(hw) = hwaccel {
            cmd.args(["-hwaccel", hw]);
        }
        cmd.args(["-i", url]);
    }
    cmd.args(["-an", "-f", "yuv4mpegpipe", "-pix_fmt", "yuv420p", "pipe:1"]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(if std::env::var_os("HIK_DEBUG").is_some() {
        Stdio::inherit()
    } else {
        Stdio::null()
    });
    cmd.stdin(Stdio::null());

    // Belt-and-braces on Linux: the kernel kills ffmpeg the instant we die,
    // covering even a stalled one that never hits its broken stdout pipe.
    // (Windows will use a Job Object; macOS relies on the pipe alone.)
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg failed to launch: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    *sh.child.lock().unwrap() = Some(child);

    // y4m stream header, e.g. "YUV4MPEG2 W704 H576 F25:1 Ip A1:1 C420mpeg2\n".
    let header = read_line(&mut out)?;
    let (mut w, mut h) = (0usize, 0usize);
    for tok in header.split_whitespace().skip(1) {
        match tok.as_bytes()[0] {
            b'W' => w = tok[1..].parse().unwrap_or(0),
            b'H' => h = tok[1..].parse().unwrap_or(0),
            b'C' if !tok.starts_with("C420") => {
                return Err(format!("unexpected pixel format {tok}"));
            }
            _ => {}
        }
    }
    if w == 0 || h == 0 {
        return Err(format!("bad y4m header: {header}"));
    }

    let frame_len = w * h + 2 * (w.div_ceil(2) * h.div_ceil(2));
    let mut seq: u64 = 0;
    // fps over a ~1 s window — decoder output is bursty, so per-frame gaps lie.
    let mut win_start = Instant::now();
    let mut win_frames: u32 = 0;
    // Smoothing state (port of VideoStreamParser.swift): the camera emits
    // frames on a steady beat; the network delivers them jittered.
    // Reconstruct the beat by scheduling each frame at prev + gap (gap = EWMA
    // of arrival spacing ≈ 1/fps), anchored SMOOTHING_DELAY behind arrival so
    // late deliveries still make their slot.
    let epoch = Instant::now();
    let mut last_arrival = -1.0f64;
    let mut frame_gap = 0.04f64; // seconds; EWMA of arrival gaps
    let mut next_pts = -1.0f64;
    loop {
        if sh.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        read_line(&mut out)?; // "FRAME" (+ optional params)
        let mut yuv = sh.take_buffer(frame_len);
        out.read_exact(&mut yuv).map_err(|e| format!("pipe closed: {e}"))?;
        seq += 1;

        // Burst arrivals (gap ≈ 0) are real data — the mean of the gaps is
        // the true frame interval. Cap only stall outliers.
        let now = epoch.elapsed().as_secs_f64();
        let gap = if last_arrival >= 0.0 { now - last_arrival } else { 0.0 };
        if last_arrival >= 0.0 {
            frame_gap =
                (frame_gap + 0.03 * (gap.min(0.35) - frame_gap)).clamp(1.0 / 120.0, 0.35);
        }
        last_arrival = now;

        let smoothing = SMOOTH.load(Ordering::Relaxed);
        let mut reanchored = false;
        let due = if smoothing {
            let mut t =
                if next_pts < 0.0 { now + SMOOTHING_DELAY } else { next_pts + frame_gap };
            // Re-anchor when the schedule drains (frame would show late) or
            // runs ahead of the buffer bound — one brief hiccup, then smooth.
            if next_pts >= 0.0 && t < now + 0.005 {
                reanchored = true;
                t = now + SMOOTHING_DELAY;
            } else if next_pts >= 0.0 && t > now + SMOOTHING_DELAY + 0.3 {
                reanchored = true;
                t = now + SMOOTHING_DELAY;
            }
            next_pts = t;
            epoch + Duration::from_secs_f64(t)
        } else {
            next_pts = -1.0;
            Instant::now()
        };
        sh.publish(Frame { width: w, height: h, yuv, seq, due });

        win_frames += 1;
        {
            let mut st = sh.stats.lock().unwrap();
            if seq == 1 {
                st.status = format!("{w}×{h}");
                st.first_frame_secs = Some(launch.elapsed().as_secs_f32());
            }
            st.frames += 1;
            if reanchored {
                st.reanchors += 1;
            }
            let win = win_start.elapsed().as_secs_f32();
            if win >= 1.0 {
                st.fps = win_frames as f32 / win;
                win_start = Instant::now();
                win_frames = 0;
            }
        }
        wake();
    }
}

fn read_line(r: &mut impl Read) -> Result<String, String> {
    let mut line = Vec::with_capacity(80);
    let mut b = [0u8; 1];
    loop {
        r.read_exact(&mut b).map_err(|e| format!("pipe closed: {e}"))?;
        if b[0] == b'\n' {
            return String::from_utf8(line).map_err(|_| "non-utf8 y4m header".into());
        }
        line.push(b[0]);
        if line.len() > 512 {
            return Err("y4m header line too long".into());
        }
    }
}

