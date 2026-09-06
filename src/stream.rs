// stream.rs — one camera's live stream: a decoder thread (decode.rs) hands
// us pictures, we time them and publish only the latest, so a slow UI never
// backs anything up (latency can't accumulate). Reconnects forever on any
// exit or stall, like CameraStream.swift. Live streams supervise themselves;
// a pipe-fed playback stream runs once and reports when it ends.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Smooth live video: re-time frames onto the camera's steady beat behind a
/// ~0.2 s buffer, absorbing delivery jitter (port of the Mac app's smoothLive;
/// VideoStreamParser.swift). Checked per frame, so toggling applies to
/// running streams immediately.
pub static SMOOTH: AtomicBool = AtomicBool::new(true);

/// Scheduled headroom behind arrival — late deliveries still make their slot.
const SMOOTHING_DELAY: f64 = 0.2;

/// How a frame's planes are laid out in `Frame::data`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PixFmt {
    /// Y (w×h), U, V (⌈w/2⌉×⌈h/2⌉ each) — software decoders.
    I420,
    /// Y (w×h), then interleaved UV (⌈w/2⌉×⌈h/2⌉ pairs) — what every
    /// hardware decoder downloads. Sampled as-is by the shader.
    Nv12,
}

impl PixFmt {
    pub fn len(self, w: usize, h: usize) -> usize {
        let chroma = w.div_ceil(2) * h.div_ceil(2);
        w * h + 2 * chroma
    }

    /// Bytes per row of plane `i`.
    pub fn row_bytes(self, i: usize, w: usize) -> usize {
        match (self, i) {
            (_, 0) => w,
            (PixFmt::Nv12, _) => w.div_ceil(2) * 2,
            (PixFmt::I420, _) => w.div_ceil(2),
        }
    }
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub format: PixFmt,
    /// Planes packed back to back (render.rs uploads them as textures).
    /// Empty when the picture lives on the GPU (`dma`).
    pub data: Vec<u8>,
    /// Zero-copy: the decoded surface itself, as a DMA-BUF (NV12).
    pub dma: Option<Arc<crate::gpu::DmaFrame>>,
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
    /// Sessions that went silent and were cut off by the watchdog.
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
    /// The decoder thread (its /proc entry gives the nerd stats its CPU).
    pub tid: Option<u32>,
    /// Whether pictures come off a hardware decoder.
    pub hardware: bool,
}

const SAMPLE_CAP: usize = 512;
/// A session silent this long is dead: cut it off and reconnect
/// (CameraStream.swift's watchdog).
pub const STALL_TIMEOUT: Duration = Duration::from_secs(12);

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
    /// A one-shot (pipe-fed) stream is over: the footage ran out, the feed
    /// failed, or it was stopped. Never set for live streams, which
    /// reconnect instead.
    ended: AtomicBool,
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

    /// End the stream: the decoder's interrupt callback sees this within
    /// its next read and the thread winds down.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
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
                self.recycle(old.data);
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

    pub fn take_buffer(&self, len: usize) -> Vec<u8> {
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
            // the UI isn't consuming — drop from the front. GPU-resident
            // frames hold decoder surfaces from a fixed pool, so fewer.
            let cap = if q.back().is_some_and(|f| f.dma.is_some()) {
                8
            } else {
                30
            };
            while q.len() > cap {
                overflow.push(q.pop_front().unwrap());
            }
        }
        for f in overflow {
            self.recycle(f.data);
        }
    }
}

/// The ffmpeg binary, still used for clips, playback snapshots and the
/// decoder probe: a bundled one next to our executable (release archives
/// ship it) wins over whatever is on PATH.
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

/// Which decoder a stream gets.
#[derive(Clone)]
pub struct Decode {
    /// ffmpeg's -hwaccel name (cuda, vaapi, …); None = software.
    pub hwaccel: Option<&'static str>,
    /// One decoder thread: the grid's small substreams on the CPU, where
    /// frame threads only add latency. (Measured against NVDEC with sixteen
    /// 704×576 streams: hardware decode with a download cost *more* CPU
    /// and kept the discrete GPU clocked up.)
    pub single_thread: bool,
    /// Export VAAPI surfaces as DMA-BUFs for the renderer (gpu.rs) while the
    /// flag holds; a failed import flips it and frames get downloaded.
    pub zero_copy: Option<crate::gpu::ZeroCopy>,
}

/// Per-session frame clock (port of VideoStreamParser.swift's smoothing):
/// the camera emits frames on a steady beat; the network delivers them
/// jittered. Reconstruct the beat by scheduling each frame at prev + gap
/// (gap = EWMA of arrival spacing ≈ 1/fps), anchored SMOOTHING_DELAY behind
/// arrival so late deliveries still make their slot. Also the source of
/// every per-frame stat.
pub struct Pacer {
    launch: Instant,
    epoch: Instant,
    seq: u64,
    win_start: Instant,
    win_frames: u32,
    last_arrival: f64,
    /// Seconds; EWMA of arrival gaps.
    frame_gap: f64,
    next_pts: f64,
}

impl Pacer {
    pub fn new(launch: Instant) -> Self {
        let now = Instant::now();
        Pacer {
            launch,
            epoch: now,
            seq: 0,
            win_start: now,
            win_frames: 0,
            last_arrival: -1.0,
            frame_gap: 0.04,
            next_pts: -1.0,
        }
    }

    /// A decoded picture arrived: schedule, publish, record. `live`
    /// streams smooth when the setting is on; playback never does — the
    /// NVR paces it.
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &mut self,
        sh: &Shared,
        data: Vec<u8>,
        dma: Option<Arc<crate::gpu::DmaFrame>>,
        width: usize,
        height: usize,
        format: PixFmt,
        live: bool,
        path: crate::decode::Path,
        wake: &(impl Fn(Duration) + Send),
    ) {
        self.seq += 1;
        let zero_copy = dma.is_some();
        // Burst arrivals (gap ≈ 0) are real data — the mean of the gaps is
        // the true frame interval. Cap only stall outliers.
        let now = self.epoch.elapsed().as_secs_f64();
        let gap = if self.last_arrival >= 0.0 {
            now - self.last_arrival
        } else {
            0.0
        };
        if self.last_arrival >= 0.0 {
            self.frame_gap =
                (self.frame_gap + 0.03 * (gap.min(0.35) - self.frame_gap)).clamp(1.0 / 120.0, 0.35);
        }
        self.last_arrival = now;

        let smoothing = live && SMOOTH.load(Ordering::Relaxed);
        let mut reanchored = false;
        let mut lead = -1.0f32;
        let due = if smoothing {
            let mut t = if self.next_pts < 0.0 {
                now + SMOOTHING_DELAY
            } else {
                self.next_pts + self.frame_gap
            };
            // Re-anchor when the schedule drains (frame would show late) or
            // runs ahead of the buffer bound — one brief hiccup, then smooth.
            if self.next_pts >= 0.0 && (t < now + 0.005 || t > now + SMOOTHING_DELAY + 0.3) {
                reanchored = true;
                t = now + SMOOTHING_DELAY;
            }
            self.next_pts = t;
            lead = (t - now) as f32;
            self.epoch + Duration::from_secs_f64(t)
        } else {
            self.next_pts = -1.0;
            Instant::now()
        };
        sh.publish(Frame {
            width,
            height,
            format,
            data,
            dma,
            seq: self.seq,
            due,
        });

        self.win_frames += 1;
        {
            let mut st = sh.stats.lock().unwrap();
            if self.seq == 1 {
                st.status = format!("{width}×{height}");
                st.first_frame_secs = Some(self.launch.elapsed().as_secs_f32());
                st.hardware = path == crate::decode::Path::Hardware;
                if std::env::var_os("HIK_DEBUG").is_some() {
                    eprintln!(
                        "[stream] first frame after {:.2} s ({width}×{height}, {:?}{})",
                        self.launch.elapsed().as_secs_f32(),
                        format,
                        if zero_copy { ", zero-copy" } else { "" }
                    );
                }
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
            let win = self.win_start.elapsed().as_secs_f32();
            if win >= 1.0 {
                st.fps = self.win_frames as f32 / win;
                self.win_start = Instant::now();
                self.win_frames = 0;
            }
        }
        sh.wake(wake);
    }
}

/// This thread's id, for the nerd stats' per-decoder CPU (Linux reads
/// /proc/<tid>/stat like a process's).
fn thread_id() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        Some(unsafe { libc::gettid() } as u32)
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// Spawn the supervisor thread: connect, decode, publish, reconnect on exit.
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
    // Named after the host, so per-thread CPU in top -H reads at a glance.
    let name = url
        .split('@')
        .nth(1)
        .and_then(|r| r.split([':', '/']).next())
        .map_or_else(|| "stream".to_string(), |h| format!("cam {h}"));
    let thread = std::thread::Builder::new().name(name);
    thread
        .spawn(move || {
            sh.stats.lock().unwrap().tid = thread_id();
            while !sh.stopped() {
                sh.set_status("connecting…");
                let result = if url == "--test" {
                    crate::decode::run_test(&sh, &wake)
                } else {
                    crate::decode::run_rtsp(&sh, &url, decode.clone(), &wake)
                };
                if let Err(e) = &result {
                    if e == "stalled" {
                        let mut st = sh.stats.lock().unwrap();
                        st.stalls += 1;
                        st.status = "stalled — reconnecting…".into();
                    }
                    if std::env::var_os("HIK_DEBUG").is_some() {
                        eprintln!("[stream] {e}");
                    }
                }
                if sh.stopped() {
                    break;
                }
                {
                    let mut st = sh.stats.lock().unwrap();
                    st.reconnects += 1;
                    st.last_reconnect = Some(Instant::now());
                    if st.status != "stalled — reconnecting…" {
                        st.status = "reconnecting…".into();
                    }
                    st.fps = 0.0;
                }
                sh.wake(&wake);
                std::thread::sleep(Duration::from_secs(2));
            }
            sh.stats.lock().unwrap().tid = None;
        })
        .expect("spawn stream thread");
    shared
}

/// The writing end of a pipe-fed stream: the RTSP client's NAL sink.
/// Dropping it ends the stream's input.
pub struct PipeSink(SyncSender<Vec<u8>>);

impl Write for PipeSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One decoder fed on a pipe, no supervisor: the caller writes the
/// elementary stream into the returned sink and drops it to end the stream;
/// `Shared::ended` reports when the decoder is gone. Never smoothed — the
/// NVR paces playback (VideoStreamParser: "playback never smooths").
pub fn start_pipe(
    codec: &'static str,
    decode: Decode,
    coalesce: Duration,
    wake: impl Fn(Duration) + Send + 'static,
) -> (Arc<Shared>, PipeSink) {
    let shared = Shared::new(true, coalesce);
    shared.set_status("connecting…");
    // Bounded: the NVR paces delivery; a decoder that can't keep up pushes
    // back on the network thread rather than growing without limit.
    let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = sync_channel(512);
    let sh = shared.clone();
    let thread = std::thread::Builder::new().name("playback".into());
    thread
        .spawn(move || {
            sh.stats.lock().unwrap().tid = thread_id();
            if let Err(e) = crate::decode::run_pipe(&sh, rx, codec, decode, &wake)
                && !sh.stopped()
                && std::env::var_os("HIK_DEBUG").is_some()
            {
                eprintln!("[playback] {e}");
            }
            sh.stats.lock().unwrap().tid = None;
            sh.ended.store(true, Ordering::SeqCst);
            sh.wake(&wake);
        })
        .expect("spawn playback thread");
    (shared, PipeSink(tx))
}
