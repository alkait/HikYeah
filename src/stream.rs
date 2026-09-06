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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// One frame's arrival, for the nerd-stats jitter/buffer windows.
#[derive(Clone, Copy)]
pub struct Sample {
    pub t: Instant,
    /// Seconds since the previous frame (0 for the first).
    pub gap: f32,
    /// Scheduled headroom in seconds; negative = smoothing off.
    pub lead: f32,
}

/// Frames scheduled with less headroom than this are "late" — near-misses
/// that show up before re-anchors do.
pub const LATE_LEAD: f32 = 0.03;

#[derive(Default, Clone)]
pub struct Stats {
    pub status: String,
    pub frames: u64,
    pub reconnects: u32,
    pub last_reconnect: Option<Instant>,
    /// Sessions that went silent and were killed by the watchdog.
    pub stalls: u32,
    /// Times smoothing gave up and restarted its schedule — each is one
    /// brief visible hiccup.
    pub reanchors: u32,
    pub late: u64,
    pub fps: f32,
    pub first_frame_secs: Option<f32>,
    /// Last ~25 s of arrivals at 20 fps (oldest first).
    pub samples: VecDeque<Sample>,
    /// Last frame (or launch) — the stall watchdog's clock.
    pub last_activity: Option<Instant>,
    pub pid: Option<u32>,
}

const SAMPLE_CAP: usize = 512;
/// A session silent this long is dead: kill ffmpeg and reconnect
/// (CameraStream.swift's watchdog).
const STALL_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Default)]
pub struct Shared {
    /// Whether this stream's frames are on screen: a hidden stream (a grid
    /// substream while a camera is focused) keeps decoding so the grid is
    /// instant on Esc, but must not wake the UI for every frame.
    pub visible: AtomicBool,
    /// Repaint coalescing window for this stream's wakes, in ms.
    pub coalesce_ms: AtomicU32,
    /// The frame currently on screen (renderer reads this).
    pub current: Mutex<Option<Frame>>,
    /// Frames scheduled for the future, front = next due.
    queue: Mutex<VecDeque<Frame>>,
    /// Recycled plane buffers (steady state allocates nothing).
    pool: Mutex<Vec<Vec<u8>>>,
    pub stats: Mutex<Stats>,
    stopped: AtomicBool,
    /// A one-shot (pipe-fed) stream's ffmpeg exited: the footage ran out,
    /// the feed failed, or it was stopped. Never set for live streams,
    /// which reconnect instead.
    ended: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Shared {
    fn new(visible: bool, coalesce: Duration) -> Arc<Shared> {
        let sh = Shared::default();
        sh.visible.store(visible, Ordering::Relaxed);
        sh.coalesce_ms
            .store(coalesce.as_millis() as u32, Ordering::Relaxed);
        Arc::new(sh)
    }

    pub fn set_visible(&self, visible: bool, coalesce: Duration) {
        self.visible.store(visible, Ordering::Relaxed);
        self.coalesce_ms
            .store(coalesce.as_millis() as u32, Ordering::Relaxed);
    }

    /// A frame landed: ask for the screen, at this stream's urgency.
    fn wake(&self, wake: &(impl Fn(Duration) + Send)) {
        if self.visible.load(Ordering::Relaxed) {
            wake(Duration::from_millis(u64::from(
                self.coalesce_ms.load(Ordering::Relaxed),
            )));
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    pub fn set_status(&self, s: &str) {
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

/// The ffmpeg to launch: a bundled one next to our executable (release
/// archives ship it) wins over whatever is on PATH.
pub fn ffmpeg_path() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| name.into())
}

/// Which decoder a live stream gets.
#[derive(Clone, Copy)]
pub enum Decode {
    /// Single-thread software decode: the grid's small substreams. Measured
    /// against NVDEC with sixteen 704×576 streams on a hybrid laptop:
    /// hardware decode cost *more* CPU (the per-frame GPU→CPU download),
    /// kept the discrete GPU clocked up (+4 W, its fan on), and frame
    /// threads only add latency at this size.
    Substream,
    /// The user's decode choice (Settings): main stream and playback, where
    /// a 4K stream is real work.
    Preferred(Option<&'static str>),
}

/// What ffmpeg reads.
pub enum Input {
    /// ffmpeg's own RTSP client (live cameras).
    Rtsp(String, Decode),
    /// Synthetic test pattern (dev/demo).
    Test,
    /// Annex B elementary stream ("hevc" / "h264") pushed into ffmpeg's
    /// stdin by the caller — the native RTSP playback client (rtsp.rs) —
    /// decoded with the user's choice.
    Pipe(&'static str, Option<&'static str>),
}

/// Spawn the supervisor thread: launch ffmpeg, pump frames, relaunch on exit.
/// `wake(window)` is called after each published frame while the stream is
/// visible — the UI repaints within that window.
pub fn start(
    url: String,
    decode: Decode,
    visible: bool,
    coalesce: Duration,
    wake: impl Fn(Duration) + Send + 'static,
) -> Arc<Shared> {
    let shared = Shared::new(visible, coalesce);
    let sh = shared.clone();
    std::thread::spawn(move || {
        let input = if url == "--test" {
            Input::Test
        } else {
            Input::Rtsp(url, decode)
        };
        while !sh.stopped.load(Ordering::SeqCst) {
            sh.set_status("connecting…");
            match run_once(&sh, &input, &wake) {
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
                st.last_reconnect = Some(Instant::now());
                st.status = "reconnecting…".into();
                st.fps = 0.0;
                st.pid = None;
            }
            sh.wake(&wake);
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    // Stall watchdog: a TCP session can stay open while the camera stops
    // sending, and the reader would block forever. Kill it; the supervisor
    // reconnects.
    let sh = shared.clone();
    std::thread::spawn(move || {
        while !sh.stopped.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
            let stalled = {
                let mut st = sh.stats.lock().unwrap();
                let stalled = st
                    .last_activity
                    .is_some_and(|t| t.elapsed() > STALL_TIMEOUT);
                if stalled {
                    st.stalls += 1;
                    st.last_activity = None; // once per session
                    st.status = "stalled — reconnecting…".into();
                }
                stalled
            };
            if stalled && let Some(c) = sh.child.lock().unwrap().as_mut() {
                let _ = c.kill();
            }
        }
    });
    shared
}

/// One ffmpeg fed on stdin, no supervisor: the caller writes the
/// elementary stream into the returned handle and closes it to end the
/// stream; `Shared::ended` reports when ffmpeg is gone. Never smoothed —
/// the NVR paces playback (VideoStreamParser: "playback never smooths").
pub fn start_pipe(
    codec: &'static str,
    hwaccel: Option<&'static str>,
    coalesce: Duration,
    wake: impl Fn(Duration) + Send + 'static,
) -> Result<(Arc<Shared>, std::process::ChildStdin), String> {
    let shared = Shared::new(true, coalesce);
    shared.set_status("connecting…");
    let mut child = spawn_ffmpeg(&Input::Pipe(codec, hwaccel))?;
    let stdin = child.stdin.take().unwrap();
    let mut out = child.stdout.take().unwrap();
    {
        let mut st = shared.stats.lock().unwrap();
        st.pid = Some(child.id());
        st.last_activity = Some(Instant::now());
    }
    *shared.child.lock().unwrap() = Some(child);
    let sh = shared.clone();
    std::thread::spawn(move || {
        let launch = Instant::now();
        if let Err(e) = pump(&sh, &mut out, false, launch, &wake)
            && !sh.stopped.load(Ordering::SeqCst)
            && std::env::var_os("HIK_DEBUG").is_some()
        {
            eprintln!("[playback] {e}");
        }
        sh.stats.lock().unwrap().pid = None;
        sh.ended.store(true, Ordering::SeqCst);
        sh.wake(&wake);
    });
    Ok((shared, stdin))
}

/// One ffmpeg lifetime: spawn, parse the y4m header, stream frames until EOF.
fn run_once(sh: &Shared, input: &Input, wake: &(impl Fn(Duration) + Send)) -> Result<(), String> {
    let launch = Instant::now();
    let mut child = spawn_ffmpeg(input)?;
    let mut out = child.stdout.take().unwrap();
    {
        let mut st = sh.stats.lock().unwrap();
        st.pid = Some(child.id());
        st.last_activity = Some(Instant::now());
    }
    *sh.child.lock().unwrap() = Some(child);
    let result = pump(sh, &mut out, true, launch, wake);
    // Reap the child whatever ended the pump (its exit, a stall kill, a
    // header we couldn't parse) — an un-waited ffmpeg lingers as a zombie
    // on every reconnect.
    if let Some(mut c) = sh.child.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    result
}

fn spawn_ffmpeg(input: &Input) -> Result<Child, String> {
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    match input {
        Input::Test => {
            // Synthetic source: full pipeline minus the camera (dev/demo).
            // -re paces lavfi at realtime, like a camera would.
            cmd.args(["-re", "-f", "lavfi", "-i", "testsrc2=size=704x576:rate=25"]);
        }
        Input::Rtsp(url, decode) => {
            // No "-fflags nobuffer": with HEVC over RTSP it makes the parser
            // hand the decoder split NALs — measured on a 4K 20 fps camera:
            // half the frames lost, constant RPS errors costing half a core,
            // and the first frame at 4.3 s instead of 1.5 s.
            cmd.args(["-rtsp_transport", "tcp", "-flags", "low_delay"]);
            match decode {
                Decode::Substream => {
                    cmd.args(["-threads", "1"]);
                }
                Decode::Preferred(Some(hw)) => {
                    cmd.args(["-hwaccel", hw]);
                }
                Decode::Preferred(None) => {}
            }
            cmd.args(["-i", url]);
        }
        Input::Pipe(codec, hwaccel) => {
            // No "-fflags nobuffer" here: on a raw elementary stream read
            // from a pipe it makes the parser hand the decoder split NALs
            // (measured: every frame errors, first picture lands a GOP late).
            cmd.args(["-flags", "low_delay"]);
            // Zero-probe fast start (CameraStream.swift): the parameter sets
            // lead the stream, so skip ffmpeg's input analysis. HEVC only —
            // the H.264 path needs the SPS dimensions before it commits.
            if *codec == "hevc" {
                cmd.args(["-probesize", "32", "-analyzeduration", "0"]);
            }
            if let Some(hw) = hwaccel {
                cmd.args(["-hwaccel", hw]);
            }
            cmd.args(["-f", codec, "-i", "pipe:0"]);
        }
    }
    cmd.args(["-an", "-f", "yuv4mpegpipe", "-pix_fmt", "yuv420p", "pipe:1"]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(if std::env::var_os("HIK_DEBUG").is_some() {
        Stdio::inherit()
    } else {
        Stdio::null()
    });
    cmd.stdin(if matches!(input, Input::Pipe(..)) {
        Stdio::piped()
    } else {
        Stdio::null()
    });

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

    cmd.spawn()
        .map_err(|e| format!("ffmpeg failed to launch: {e}"))
}

/// Read the y4m header, then frames until EOF, publishing each. `live`
/// streams smooth when the setting is on; pipe-fed playback never does.
fn pump(
    sh: &Shared,
    out: &mut std::process::ChildStdout,
    live: bool,
    launch: Instant,
    wake: &(impl Fn(Duration) + Send),
) -> Result<(), String> {
    // y4m stream header, e.g. "YUV4MPEG2 W704 H576 F25:1 Ip A1:1 C420mpeg2\n".
    let header = read_line(out)?;
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
        read_line(out)?; // "FRAME" (+ optional params)
        let mut yuv = sh.take_buffer(frame_len);
        out.read_exact(&mut yuv)
            .map_err(|e| format!("pipe closed: {e}"))?;
        seq += 1;

        // Burst arrivals (gap ≈ 0) are real data — the mean of the gaps is
        // the true frame interval. Cap only stall outliers.
        let now = epoch.elapsed().as_secs_f64();
        let gap = if last_arrival >= 0.0 {
            now - last_arrival
        } else {
            0.0
        };
        if last_arrival >= 0.0 {
            frame_gap = (frame_gap + 0.03 * (gap.min(0.35) - frame_gap)).clamp(1.0 / 120.0, 0.35);
        }
        last_arrival = now;

        let smoothing = live && SMOOTH.load(Ordering::Relaxed);
        let mut reanchored = false;
        let mut lead = -1.0f32;
        let due = if smoothing {
            let mut t = if next_pts < 0.0 {
                now + SMOOTHING_DELAY
            } else {
                next_pts + frame_gap
            };
            // Re-anchor when the schedule drains (frame would show late) or
            // runs ahead of the buffer bound — one brief hiccup, then smooth.
            if next_pts >= 0.0 && (t < now + 0.005 || t > now + SMOOTHING_DELAY + 0.3) {
                reanchored = true;
                t = now + SMOOTHING_DELAY;
            }
            next_pts = t;
            lead = (t - now) as f32;
            epoch + Duration::from_secs_f64(t)
        } else {
            next_pts = -1.0;
            Instant::now()
        };
        sh.publish(Frame {
            width: w,
            height: h,
            yuv,
            seq,
            due,
        });

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
            if (0.0..LATE_LEAD).contains(&lead) {
                st.late += 1;
            }
            let arrived = Instant::now();
            st.last_activity = Some(arrived);
            if st.samples.len() >= SAMPLE_CAP {
                st.samples.pop_front();
            }
            st.samples.push_back(Sample {
                t: arrived,
                gap: gap as f32,
                lead,
            });
            let win = win_start.elapsed().as_secs_f32();
            if win >= 1.0 {
                st.fps = win_frames as f32 / win;
                win_start = Instant::now();
                win_frames = 0;
            }
        }
        sh.wake(wake);
    }
}

fn read_line(r: &mut impl Read) -> Result<String, String> {
    let mut line = Vec::with_capacity(80);
    let mut b = [0u8; 1];
    loop {
        r.read_exact(&mut b)
            .map_err(|e| format!("pipe closed: {e}"))?;
        if b[0] == b'\n' {
            return String::from_utf8(line).map_err(|_| "non-utf8 y4m header".into());
        }
        line.push(b[0]);
        if line.len() > 512 {
            return Err("y4m header line too long".into());
        }
    }
}
