// decode.rs — in-process video decoding with the FFmpeg libraries
// (libavformat for RTSP, libavcodec for the pictures), replacing the ffmpeg
// child processes that used to pipe raw yuv4mpegpipe frames into the app.
// Every decoded frame used to cross two pipe copies, a y4m parse and a
// pixel-format conversion; here it goes from the decoder straight into the
// frame buffer the renderer uploads. Hardware decoders (VAAPI, NVDEC, …)
// hand back NV12 which the shader samples directly.
//
// Each stream runs on its own thread; nothing here touches the UI.

use crate::gpu::DmaFrame;
use crate::stream::{Decode, Pacer, PixFmt, STALL_TIMEOUT, Shared};
use ffmpeg_next as ff;
use ffmpeg_next::ffi as sys;
use std::ffi::{CStr, CString, c_void};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Library setup, once: network init and a quiet log unless HIK_DEBUG.
pub fn init() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        ff::init().expect("ffmpeg init");
        ff::device::register_all();
        ff::util::log::set_level(if std::env::var_os("HIK_DEBUG").is_some() {
            ff::util::log::Level::Warning
        } else {
            ff::util::log::Level::Error
        });
    });
}

/// How a decoder's output reached us, for the nerd stats.
#[derive(Clone, Copy, PartialEq)]
pub enum Path {
    Software,
    Hardware,
}

/// Decode a live RTSP stream until it ends, stalls, fails or is stopped.
/// `Err` carries the reason for the log; the supervisor reconnects either way.
pub fn run_rtsp(
    sh: &Arc<Shared>,
    url: &str,
    decode: Decode,
    wake: &(impl Fn(Duration) + Send),
) -> Result<(), String> {
    init();
    let launch = Instant::now();
    sh.stats.lock().unwrap().last_activity = Some(launch);
    let mut opts = ff::Dictionary::new();
    opts.set("rtsp_transport", "tcp");
    // Socket timeout (µs): a dead host answers within this, not never.
    opts.set("timeout", "8000000");
    let ictx = open_with_interrupt(url, None, opts, sh)?;
    let mut session = Session::open(ictx, &decode)?;
    session.run(sh, Pacer::new(launch), true, wake)
}

/// Decode an elementary stream ("hevc" / "h264") arriving in chunks on `rx`
/// — the native RTSP playback client's output — until the sender is gone.
pub fn run_pipe(
    sh: &Arc<Shared>,
    rx: Receiver<Vec<u8>>,
    codec: &str,
    decode: Decode,
    wake: &(impl Fn(Duration) + Send),
) -> Result<(), String> {
    init();
    let launch = Instant::now();
    sh.stats.lock().unwrap().last_activity = Some(launch);
    let reader = PipeReader {
        rx,
        pending: Vec::new(),
        pos: 0,
        sh: sh.clone(),
    };
    let io = ff::format::context::StreamIo::from_read(reader).map_err(|e| e.to_string())?;
    let mut opts = ff::Dictionary::new();
    // The parameter sets lead the stream: no need to sniff further.
    opts.set("probesize", "32");
    opts.set("analyzeduration", "0");
    let ictx = open_with_interrupt("", Some((codec, Some(io))), opts, sh)?;
    let mut session = Session::open(ictx, &decode)?;
    session.run(sh, Pacer::new(launch), false, wake)
}

/// The --test source: libavfilter's test pattern, paced at 25 fps by hand
/// (lavfi produces frames as fast as they're read).
pub fn run_test(sh: &Arc<Shared>, wake: &(impl Fn(Duration) + Send)) -> Result<(), String> {
    init();
    let launch = Instant::now();
    sh.stats.lock().unwrap().last_activity = Some(launch);
    let ictx = open_with_interrupt(
        "testsrc2=size=704x576:rate=25",
        Some(("lavfi", None)),
        ff::Dictionary::new(),
        sh,
    )?;
    let mut session = Session::open(
        ictx,
        &Decode {
            hwaccel: None,
            single_thread: true,
            zero_copy: None,
        },
    )?;
    session.paced = Some(Duration::from_millis(40));
    session.run(sh, Pacer::new(launch), true, wake)
}

/// avformat_open_input with the stall/stop interrupt installed before the
/// connect, and *without* avformat_find_stream_info: RTSP's SDP already
/// names the codec (and carries the parameter sets), so the probe would
/// only delay the first frame by seconds. Dimensions come with the first
/// decoded picture.
fn open_with_interrupt(
    url: &str,
    custom: Option<(&str, Option<ff::format::context::StreamIo>)>,
    opts: ff::Dictionary,
    sh: &Arc<Shared>,
) -> Result<ff::format::context::Input, String> {
    let watch = sh.clone();
    let interrupt = ff::util::interrupt::new(Box::new(move || {
        watch.stopped()
            || watch
                .stats
                .lock()
                .unwrap()
                .last_activity
                .is_some_and(|t| t.elapsed() > STALL_TIMEOUT)
    }));
    let path = CString::new(url).map_err(|e| e.to_string())?;
    unsafe {
        let mut ps = sys::avformat_alloc_context();
        if ps.is_null() {
            return Err("out of memory".into());
        }
        (*ps).interrupt_callback = interrupt.interrupt;
        let mut fmt: *const sys::AVInputFormat = ptr::null();
        let mut io = None;
        if let Some((name, stream_io)) = custom {
            let cname = CString::new(name).unwrap();
            fmt = sys::av_find_input_format(cname.as_ptr());
            if fmt.is_null() {
                sys::avformat_free_context(ps);
                return Err(format!("no {name} demuxer"));
            }
            // (No interrupt mirror into the custom IO: PipeReader watches the
            // stop flag itself.)
            if let Some(mut stream_io) = stream_io {
                (*ps).pb = stream_io.as_mut_ptr();
                (*ps).flags |= sys::AVFMT_FLAG_CUSTOM_IO;
                io = Some(stream_io);
            }
        }
        let mut dict = opts.disown();
        let res = sys::avformat_open_input(&raw mut ps, path.as_ptr(), fmt, &raw mut dict);
        ff::Dictionary::own(dict);
        if res < 0 {
            return Err(format!("open failed: {}", ff::Error::from(res)));
        }
        Ok(match io {
            Some(io) => ff::format::context::Input::wrap_with_custom_io_and_interrupt(
                ps,
                io,
                interrupt.guard,
            ),
            None => ff::format::context::Input::wrap_with_interrupt(ps, interrupt.guard),
        })
    }
}

/// Chunks from the RTSP client, read by libavformat's custom IO.
struct PipeReader {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    pos: usize,
    sh: Arc<Shared>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.pending.len() {
            if self.sh.stopped() {
                return Ok(0);
            }
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.pos = 0;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(0), // EOF: the feed ended
            }
        }
        let n = buf.len().min(self.pending.len() - self.pos);
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The hardware pixel format the decoder should pick, handed to libavcodec's
/// get_format through the context's opaque pointer.
struct HwChoice {
    pix_fmt: sys::AVPixelFormat,
    device: *mut sys::AVBufferRef,
    /// Surfaces held beyond the decoder's own needs: the frame on screen,
    /// the smoothing queue (≤ 8) and the renderer's in-flight guard.
    extra_surfaces: i32,
}

unsafe extern "C" fn get_format(
    ctx: *mut sys::AVCodecContext,
    fmts: *const sys::AVPixelFormat,
) -> sys::AVPixelFormat {
    unsafe {
        let Some(choice) = ((*ctx).opaque as *const HwChoice).as_ref() else {
            return *fmts;
        };
        let mut p = fmts;
        while *p != sys::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == choice.pix_fmt {
                // Size the surface pool ourselves: libavcodec's default is
                // just what decoding needs, and we hold frames longer.
                let mut frames = ptr::null_mut();
                if sys::avcodec_get_hw_frames_parameters(ctx, choice.device, *p, &raw mut frames)
                    >= 0
                {
                    let fc = (*frames).data as *mut sys::AVHWFramesContext;
                    (*fc).initial_pool_size += choice.extra_surfaces;
                    if sys::av_hwframe_ctx_init(frames) >= 0 {
                        (*ctx).hw_frames_ctx = frames;
                    } else {
                        sys::av_buffer_unref(&raw mut frames);
                    }
                }
                return *p;
            }
            p = p.add(1);
        }
        // The hardware path isn't on offer: fall back to the first software
        // format; frames then arrive already downloaded.
        *fmts
    }
}

/// One decoder: demuxer, codec, optional hardware device, and the scratch
/// frames for downloads and conversions.
struct Session {
    ictx: ff::format::context::Input,
    stream_index: usize,
    decoder: ff::decoder::Video,
    hw_fmt: Option<sys::AVPixelFormat>,
    hw_device: *mut sys::AVBufferRef,
    _hw_choice: Option<Box<HwChoice>>,
    frame: ff::frame::Video,
    sw_frame: ff::frame::Video,
    scaler: Option<ff::software::scaling::Context>,
    scaled: ff::frame::Video,
    /// Sleep this long per frame (the unpaced test source only).
    paced: Option<Duration>,
    /// Zero-copy export (VAAPI → DRM PRIME): the flag, the derived DRM
    /// device and the frames context derived from the decoder's pool.
    zero_copy: Option<crate::gpu::ZeroCopy>,
    drm_device: *mut sys::AVBufferRef,
    drm_frames: *mut sys::AVBufferRef,
    drm_frames_src: usize,
}

impl Session {
    fn open(ictx: ff::format::context::Input, decode: &Decode) -> Result<Session, String> {
        let hwaccel = decode.hwaccel;
        let single_thread = decode.single_thread;
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .ok_or("no video stream")?;
        let stream_index = stream.index();
        let params = stream.parameters();
        let codec = ff::decoder::find(params.id()).ok_or("no decoder for this codec")?;
        let mut ctx = ff::codec::context::Context::new_with_codec(codec);
        ctx.set_parameters(params).map_err(|e| e.to_string())?;
        ctx.set_flags(ff::codec::Flags::LOW_DELAY);
        // Hikvision door stations send H.264 "Baseline" that is really
        // constrained baseline; without this the VAAPI driver refuses it
        // (ffmpeg's -hwaccel_flags allow_profile_mismatch).
        unsafe {
            (*ctx.as_mut_ptr()).hwaccel_flags |= sys::AV_HWACCEL_FLAG_ALLOW_PROFILE_MISMATCH;
        }
        if single_thread {
            ctx.set_threading(ff::codec::threading::Config::count(1));
        }

        let mut hw_fmt = None;
        let mut hw_device = ptr::null_mut();
        let mut hw_choice = None;
        if let Some(name) = hwaccel {
            match create_hw_device(name, codec) {
                Some((device, fmt)) => {
                    hw_device = device;
                    hw_fmt = Some(fmt);
                    let choice = Box::new(HwChoice {
                        pix_fmt: fmt,
                        device,
                        extra_surfaces: 12,
                    });
                    unsafe {
                        let raw = ctx.as_mut_ptr();
                        (*raw).hw_device_ctx = sys::av_buffer_ref(device);
                        (*raw).opaque = &*choice as *const HwChoice as *mut c_void;
                        (*raw).get_format = Some(get_format);
                    }
                    hw_choice = Some(choice);
                }
                None => {
                    if std::env::var_os("HIK_DEBUG").is_some() {
                        eprintln!("[decode] {name} unavailable — software decode");
                    }
                }
            }
        }
        let decoder = ctx.decoder().video().map_err(|e| e.to_string())?;
        Ok(Session {
            ictx,
            stream_index,
            decoder,
            hw_fmt,
            hw_device,
            _hw_choice: hw_choice,
            frame: ff::frame::Video::empty(),
            sw_frame: ff::frame::Video::empty(),
            scaler: None,
            scaled: ff::frame::Video::empty(),
            paced: None,
            zero_copy: if hwaccel == Some("vaapi") {
                decode.zero_copy.clone()
            } else {
                None
            },
            drm_device: ptr::null_mut(),
            drm_frames: ptr::null_mut(),
            drm_frames_src: 0,
        })
    }

    /// Demux, decode and publish until the input ends or the interrupt fires.
    fn run(
        &mut self,
        sh: &Arc<Shared>,
        mut pacer: Pacer,
        live: bool,
        wake: &(impl Fn(Duration) + Send),
    ) -> Result<(), String> {
        let path = if self.hw_fmt.is_some() {
            Path::Hardware
        } else {
            Path::Software
        };
        loop {
            let packet = {
                let mut pkt = ff::Packet::empty();
                match pkt.read(&mut self.ictx) {
                    Ok(()) => pkt,
                    Err(ff::Error::Eof) => break,
                    Err(ff::Error::Exit) => {
                        return if sh.stopped() {
                            Ok(())
                        } else {
                            Err("stalled".into())
                        };
                    }
                    Err(e) => return Err(format!("read: {e}")),
                }
            };
            if packet.stream() != self.stream_index {
                continue;
            }
            if let Err(e) = self.decoder.send_packet(&packet) {
                // A damaged packet is not the end of the world; the decoder
                // resyncs on the next keyframe.
                if std::env::var_os("HIK_DEBUG").is_some() {
                    eprintln!("[decode] send_packet: {e}");
                }
                continue;
            }
            self.drain(sh, &mut pacer, live, path, wake)?;
            if sh.stopped() {
                return Ok(());
            }
            if let Some(d) = self.paced {
                std::thread::sleep(d);
            }
        }
        let _ = self.decoder.send_eof();
        let _ = self.drain(sh, &mut pacer, live, path, wake);
        Ok(())
    }

    fn drain(
        &mut self,
        sh: &Arc<Shared>,
        pacer: &mut Pacer,
        live: bool,
        path: Path,
        wake: &(impl Fn(Duration) + Send),
    ) -> Result<(), String> {
        while self.decoder.receive_frame(&mut self.frame).is_ok() {
            let is_hw = Some(self.frame.format().into()) == self.hw_fmt;
            if is_hw
                && self
                    .zero_copy
                    .as_ref()
                    .is_some_and(|z| z.load(Ordering::Relaxed))
            {
                match self.export_dma() {
                    Some(dma) => {
                        let (w, h) = (dma.width as usize, dma.height as usize);
                        pacer.publish(
                            sh,
                            Vec::new(),
                            Some(Arc::new(dma)),
                            w,
                            h,
                            PixFmt::Nv12,
                            live,
                            path,
                            wake,
                        );
                        continue;
                    }
                    None => {
                        // Can't export: this decoder downloads from here on.
                        self.zero_copy = None;
                    }
                }
            }
            let src: &ff::frame::Video = if is_hw {
                // Download from the GPU (NV12 on every hardware decoder here).
                let ret = unsafe {
                    sys::av_frame_unref(self.sw_frame.as_mut_ptr());
                    sys::av_hwframe_transfer_data(
                        self.sw_frame.as_mut_ptr(),
                        self.frame.as_ptr(),
                        0,
                    )
                };
                if ret < 0 {
                    return Err(format!("hw download: {}", ff::Error::from(ret)));
                }
                &self.sw_frame
            } else {
                &self.frame
            };
            let (fmt, planes): (PixFmt, usize) = match src.format() {
                ff::format::Pixel::NV12 => (PixFmt::Nv12, 2),
                ff::format::Pixel::YUV420P | ff::format::Pixel::YUVJ420P => (PixFmt::I420, 3),
                other => {
                    // Anything else (4:2:2 cameras, 10-bit) goes through
                    // swscale once per frame.
                    let (w, h) = (src.width(), src.height());
                    let scaler = match &mut self.scaler {
                        Some(s) if s.input().width == w && s.input().height == h => s,
                        _ => self.scaler.insert(
                            ff::software::scaling::Context::get(
                                other,
                                w,
                                h,
                                ff::format::Pixel::YUV420P,
                                w,
                                h,
                                ff::software::scaling::Flags::BILINEAR,
                            )
                            .map_err(|e| e.to_string())?,
                        ),
                    };
                    scaler
                        .run(src, &mut self.scaled)
                        .map_err(|e| e.to_string())?;
                    let scaled = &self.scaled;
                    publish(sh, pacer, scaled, PixFmt::I420, 3, live, path, wake);
                    continue;
                }
            };
            publish(sh, pacer, src, fmt, planes, live, path, wake);
        }
        Ok(())
    }
}

impl Session {
    /// Map the decoded VAAPI surface to a DRM PRIME descriptor (one
    /// DMA-BUF, NV12 as two layers) and wrap it for the renderer. The mapped
    /// frame keeps the surface referenced until the wrapper is dropped.
    fn export_dma(&mut self) -> Option<DmaFrame> {
        unsafe {
            let src = self.frame.as_ptr();
            let src_frames = (*src).hw_frames_ctx;
            if src_frames.is_null() {
                return None;
            }
            if self.drm_frames.is_null() || self.drm_frames_src != src_frames as usize {
                if !self.drm_frames.is_null() {
                    sys::av_buffer_unref(&raw mut self.drm_frames);
                }
                if self.drm_device.is_null()
                    && sys::av_hwdevice_ctx_create_derived(
                        &raw mut self.drm_device,
                        sys::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                        self.hw_device,
                        0,
                    ) < 0
                {
                    debug("no DRM device could be derived from VAAPI");
                    return None;
                }
                if sys::av_hwframe_ctx_create_derived(
                    &raw mut self.drm_frames,
                    sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
                    self.drm_device,
                    src_frames,
                    sys::AV_HWFRAME_MAP_READ as i32,
                ) < 0
                {
                    debug("VAAPI frames can't be mapped to DRM PRIME");
                    return None;
                }
                self.drm_frames_src = src_frames as usize;
            }
            let mut dst = sys::av_frame_alloc();
            (*dst).format = sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*dst).hw_frames_ctx = sys::av_buffer_ref(self.drm_frames);
            if sys::av_hwframe_map(dst, src, sys::AV_HWFRAME_MAP_READ as i32) < 0 {
                sys::av_frame_free(&raw mut dst);
                debug("surface export failed");
                return None;
            }
            let keep = Box::new(MappedFrame(dst));
            let desc = &*((*dst).data[0] as *const sys::AVDRMFrameDescriptor);
            // Two layers of one plane (VA_EXPORT_SURFACE_SEPARATE_LAYERS) or
            // one NV12 layer of two planes — either way, two planes in order.
            let mut planes: Vec<(u64, u64)> = Vec::with_capacity(2);
            let mut object = None;
            for l in 0..desc.nb_layers as usize {
                let layer = &desc.layers[l];
                for p in 0..layer.nb_planes as usize {
                    let plane = &layer.planes[p];
                    match object {
                        None => object = Some(plane.object_index),
                        Some(o) if o != plane.object_index => {
                            debug("surface spans several buffers");
                            return None;
                        }
                        _ => {}
                    }
                    planes.push((plane.offset as u64, plane.pitch as u64));
                }
            }
            let (Some(object), true) = (object, planes.len() == 2) else {
                debug("unexpected DRM layout");
                return None;
            };
            let obj = &desc.objects[object as usize];
            let dup = libc::dup(obj.fd);
            if dup < 0 {
                return None;
            }
            let fd = std::os::fd::OwnedFd::from_raw_fd(dup);
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(obj.fd, &raw mut st) != 0 {
                return None;
            }
            Some(DmaFrame {
                fd,
                ino: st.st_ino,
                modifier: obj.format_modifier,
                width: (*dst).width as u32,
                height: (*dst).height as u32,
                planes: [planes[0], planes[1]],
                keep,
            })
        }
    }
}

/// A mapped DRM PRIME frame; unmapping it releases the decoder's surface.
struct MappedFrame(*mut sys::AVFrame);

// The pointer is only ever freed here, and libavutil's refcounting is
// thread-safe, so the wrapper may travel between threads.
unsafe impl Send for MappedFrame {}
unsafe impl Sync for MappedFrame {}

impl Drop for MappedFrame {
    fn drop(&mut self) {
        unsafe { sys::av_frame_free(&raw mut self.0) };
    }
}

fn debug(msg: &str) {
    if std::env::var_os("HIK_DEBUG").is_some() {
        eprintln!("[decode] {msg}");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if !self.drm_frames.is_null() {
                sys::av_buffer_unref(&raw mut self.drm_frames);
            }
            if !self.drm_device.is_null() {
                sys::av_buffer_unref(&raw mut self.drm_device);
            }
            if !self.hw_device.is_null() {
                sys::av_buffer_unref(&raw mut self.hw_device);
            }
        }
    }
}

/// Copy the picture's planes, stride-packed, into a frame buffer and hand it
/// to the pacer (which times, records and publishes it).
#[allow(clippy::too_many_arguments)]
fn publish(
    sh: &Arc<Shared>,
    pacer: &mut Pacer,
    src: &ff::frame::Video,
    fmt: PixFmt,
    planes: usize,
    live: bool,
    path: Path,
    wake: &(impl Fn(Duration) + Send),
) {
    let (w, h) = (src.width() as usize, src.height() as usize);
    let len = fmt.len(w, h);
    let mut data = sh.take_buffer(len);
    let mut out = 0;
    for i in 0..planes {
        let row = fmt.row_bytes(i, w);
        let rows = if i == 0 { h } else { h.div_ceil(2) };
        let stride = src.stride(i);
        let plane = src.data(i);
        if stride == row {
            data[out..out + row * rows].copy_from_slice(&plane[..row * rows]);
        } else {
            for r in 0..rows {
                data[out + r * row..out + (r + 1) * row]
                    .copy_from_slice(&plane[r * stride..r * stride + row]);
            }
        }
        out += row * rows;
    }
    pacer.publish(sh, data, None, w, h, fmt, live, path, wake);
}

/// Open the named hardware device (ffmpeg's -hwaccel names: cuda, vaapi,
/// qsv, d3d11va, videotoolbox) and find the pixel format this codec
/// produces on it. VAAPI is pointed at an Intel render node with the iHD
/// driver explicitly: LIBVA_DRIVER_NAME may be set for the desktop's GPU.
fn create_hw_device(
    name: &str,
    codec: ff::Codec,
) -> Option<(*mut sys::AVBufferRef, sys::AVPixelFormat)> {
    let cname = CString::new(name).ok()?;
    let kind = unsafe { sys::av_hwdevice_find_type_by_name(cname.as_ptr()) };
    if kind == sys::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
        return None;
    }
    let pix_fmt = (0..)
        .map_while(|i| unsafe { sys::avcodec_get_hw_config(codec.as_ptr(), i).as_ref() })
        .find(|cfg| {
            cfg.device_type == kind
                && cfg.methods & (sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
        })
        .map(|cfg| cfg.pix_fmt)?;
    let mut device: *mut sys::AVBufferRef = ptr::null_mut();
    let ret = if kind == sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
        && let Some(path) = intel_render_node()
        && let Ok(node) = CString::new(path)
    {
        // Open the DRM node first and derive VAAPI from it (what the CLI's
        // `-init_hw_device drm=dr -init_hw_device vaapi=va@dr` does): the
        // decoder's surfaces can then be mapped back to DRM PRIME for the
        // zero-copy path, which only works through a DRM-derived device.
        let mut drm: *mut sys::AVBufferRef = ptr::null_mut();
        let ret = unsafe {
            sys::av_hwdevice_ctx_create(
                &raw mut drm,
                sys::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if ret < 0 {
            ret
        } else {
            let mut opts = ff::Dictionary::new();
            opts.set("driver", "iHD");
            let mut dict = unsafe { opts.disown() };
            let ret = unsafe {
                sys::av_hwdevice_ctx_create_derived_opts(&raw mut device, kind, drm, dict, 0)
            };
            unsafe {
                sys::av_dict_free(&raw mut dict);
                sys::av_buffer_unref(&raw mut drm); // the VAAPI device keeps its own ref
            }
            ret
        }
    } else {
        unsafe {
            sys::av_hwdevice_ctx_create(&raw mut device, kind, ptr::null(), ptr::null_mut(), 0)
        }
    };
    if ret < 0 || device.is_null() {
        if std::env::var_os("HIK_DEBUG").is_some() {
            eprintln!("[decode] {name} device: {}", ff::Error::from(ret));
        }
        return None;
    }
    if std::env::var_os("HIK_DEBUG").is_some() {
        let kind_name = unsafe { CStr::from_ptr(sys::av_hwdevice_get_type_name(kind)) };
        eprintln!(
            "[decode] {} device ready, frames as {:?}",
            kind_name.to_string_lossy(),
            ff::format::Pixel::from(pix_fmt)
        );
    }
    Some((device, pix_fmt))
}

/// The first Intel render node (/dev/dri/renderD*), for VAAPI on hybrid
/// laptops where the default would be the discrete GPU.
pub fn intel_render_node() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
            .collect();
        nodes.sort_by_key(|e| e.file_name());
        nodes
            .into_iter()
            .find(|e| {
                std::fs::read_to_string(format!(
                    "/sys/class/drm/{}/device/vendor",
                    e.file_name().to_string_lossy()
                ))
                .is_ok_and(|v| v.trim() == "0x8086")
            })
            .map(|e| e.path().to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    None
}
