// audio.rs — live audio for the focused camera. The RTSP session already
// carries the camera's audio track interleaved with the video, muted or
// not, so "on" adds only a decoder and the output device: packets decode on
// the stream's own thread, libswresample converts them to the device's
// native rate and channels, and cpal's callback drains a small ring buffer.
// Off costs nothing: no decoder, no device handle, no extra thread.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ffmpeg_next as ff;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Output starts once this much is buffered — the cushion the video pacer
/// keeps (stream.rs SMOOTHING_DELAY), so sound and picture stay together.
const PRIME_SECS: f32 = 0.2;
/// Beyond this the camera's clock has run ahead of the DAC's: drop back to
/// the cushion — one audible skip instead of ever-growing lag.
const MAX_SECS: f32 = 0.6;

struct Ring {
    buf: VecDeque<f32>,
    /// Filled to the cushion and playing; cleared by an underrun so the
    /// cushion rebuilds before sound resumes.
    primed: bool,
}

/// One session's audio: decoder, resampler and the output stream.
pub struct Track {
    decoder: ff::decoder::Audio,
    /// Made from the first decoded frame (its format is only known then).
    swr: Option<ff::software::resampling::Context>,
    frame: ff::frame::Audio,
    /// Resampler output, reused; grown when a frame needs more.
    out: ff::frame::Audio,
    out_cap: usize,
    ring: Arc<Mutex<Ring>>,
    prime: usize,
    max: usize,
    rate: u32,
    channels: usize,
    _stream: cpal::Stream,
}

impl Track {
    pub fn open(params: ff::codec::Parameters) -> Result<Track, String> {
        let codec = ff::decoder::find(params.id()).ok_or("no decoder for this audio codec")?;
        let mut ctx = ff::codec::context::Context::new_with_codec(codec);
        ctx.set_parameters(params).map_err(|e| e.to_string())?;
        let decoder = ctx.decoder().audio().map_err(|e| e.to_string())?;

        let device = cpal::default_host()
            .default_output_device()
            .ok_or("no audio output device")?;
        let cfg = device
            .default_output_config()
            .map_err(|e| format!("audio device: {e}"))?;
        let rate = cfg.sample_rate();
        let channels = usize::from(cfg.channels());
        let prime = (PRIME_SECS * rate as f32) as usize * channels;
        let max = (MAX_SECS * rate as f32) as usize * channels;
        let ring = Arc::new(Mutex::new(Ring {
            buf: VecDeque::with_capacity(max),
            primed: false,
        }));
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, cfg.config(), ring.clone(), prime),
            cpal::SampleFormat::I16 => build::<i16>(&device, cfg.config(), ring.clone(), prime),
            cpal::SampleFormat::U16 => build::<u16>(&device, cfg.config(), ring.clone(), prime),
            f => return Err(format!("audio device: unsupported sample format {f}")),
        }?;
        stream.play().map_err(|e| format!("audio device: {e}"))?;
        Ok(Track {
            decoder,
            swr: None,
            frame: ff::frame::Audio::empty(),
            out: ff::frame::Audio::empty(),
            out_cap: 0,
            ring,
            prime,
            max,
            rate,
            channels,
            _stream: stream,
        })
    }

    /// The output format, for the nerd stats.
    pub fn describe(&self) -> String {
        format!("{} Hz · {} ch", self.rate, self.channels)
    }

    /// Decode one packet of the track and queue its samples.
    pub fn feed(&mut self, packet: &ff::Packet) -> Result<(), String> {
        self.decoder
            .send_packet(packet)
            .map_err(|e| format!("audio: {e}"))?;
        while self.decoder.receive_frame(&mut self.frame).is_ok() {
            let out_layout = ff::ChannelLayout::default(self.channels as i32);
            let swr = match &mut self.swr {
                Some(s) => s,
                None => {
                    let mut layout = self.frame.channel_layout();
                    if layout.is_empty() {
                        layout = ff::ChannelLayout::default(i32::from(self.frame.channels()));
                    }
                    let swr = ff::software::resampling::Context::get(
                        self.frame.format(),
                        layout,
                        self.frame.rate(),
                        ff::format::Sample::F32(ff::format::sample::Type::Packed),
                        out_layout,
                        self.rate,
                    )
                    .map_err(|e| format!("audio resample: {e}"))?;
                    self.swr.insert(swr)
                }
            };
            let need =
                self.frame.samples() * self.rate as usize / self.frame.rate().max(1) as usize + 256;
            if need > self.out_cap {
                self.out = ff::frame::Audio::new(
                    ff::format::Sample::F32(ff::format::sample::Type::Packed),
                    need,
                    out_layout,
                );
                self.out_cap = need;
            }
            // swr fills up to nb_samples and sets it to what it wrote.
            self.out.set_samples(self.out_cap);
            swr.run(&self.frame, &mut self.out)
                .map_err(|e| format!("audio resample: {e}"))?;
            let n = self.out.samples() * self.channels;
            let mut r = self.ring.lock().unwrap();
            r.buf.extend(&self.out.plane::<f32>(0)[..n]);
            if r.buf.len() > self.max {
                let excess = r.buf.len() - self.prime;
                r.buf.drain(..excess);
            }
        }
        Ok(())
    }
}

/// The device callback: silence until the cushion is full, then the ring;
/// an underrun goes silent and waits for the cushion again.
fn build<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    ring: Arc<Mutex<Ring>>,
    prime: usize,
) -> Result<cpal::Stream, String> {
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let mut r = ring.lock().unwrap();
                if !r.primed && r.buf.len() >= prime {
                    r.primed = true;
                }
                for s in data.iter_mut() {
                    let v = if r.primed {
                        match r.buf.pop_front() {
                            Some(v) => v,
                            None => {
                                r.primed = false;
                                0.0
                            }
                        }
                    } else {
                        0.0
                    };
                    *s = T::from_sample(v);
                }
            },
            |e| {
                if std::env::var_os("HIK_DEBUG").is_some() {
                    eprintln!("[audio] {e}");
                }
            },
            None,
        )
        .map_err(|e| format!("audio device: {e}"))
}
