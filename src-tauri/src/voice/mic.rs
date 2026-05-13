//! Microphone capture via `cpal`. Produces a stream of mono 16 kHz f32
//! samples, regardless of the device's native rate/channel layout.
//!
//! Design:
//! - `MicSession::open()` picks the default input device, configures the
//!   nearest-supported sample rate, opens the stream.
//! - Incoming frames are downmixed to mono and resampled to 16 kHz on the
//!   fly via a simple linear interpolator (good enough for speech; if we
//!   ever care about telephony-grade quality we can swap in rubato).
//! - Each ~80 ms chunk is pushed into a tokio `mpsc::UnboundedSender` so
//!   consumers (whisper, VAD) can `await` them.
//!
//! Windows shared-mode mics let multiple processes open the same device
//! concurrently — that's how this Rust capture co-exists with the Python
//! wake-word sidecar.

use std::sync::mpsc::{SendError, Sender};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, SampleRate, Stream, StreamConfig};

/// Target rate/format that whisper.cpp and openWakeWord both expect.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// Frame size we emit downstream — 80 ms = 1280 samples @ 16 kHz, matching
/// openWakeWord's input contract and small enough for snappy VAD updates.
pub const FRAME_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 80 / 1000;

pub struct MicSession {
    // The stream must be kept alive — drop closes the device.
    _stream: Stream,
    pub sample_rate: u32,
}

/// Open the default input device and start delivering mono 16 kHz frames
/// of `FRAME_SAMPLES` samples each into `out`. The caller keeps the
/// returned `MicSession` alive for as long as it wants audio.
///
/// We use `std::sync::mpsc::Sender` (not tokio's mpsc) so consumers can
/// drain frames synchronously from a blocking thread — `cpal::Stream`
/// holds non-`Send` internals, so the whole capture loop has to live on
/// one OS thread anyway.
pub fn open(out: Sender<Vec<f32>>) -> Result<MicSession> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("kein Default-Mikrofon gefunden"))?;
    tracing::info!(
        "mic device: {}",
        device.name().unwrap_or_else(|_| "?".into())
    );

    // Pick the best config the device offers — prefer 16 kHz mono if
    // supported, fall back to its default if not.
    let supported = device.default_input_config().context("default_input_config")?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();
    tracing::info!(
        "mic config: {} Hz × {} ch, format {:?}",
        sample_rate,
        channels,
        sample_format
    );

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let mut resampler = Resampler::new(sample_rate, TARGET_SAMPLE_RATE, channels as usize);
    let mut frame_buf: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);

    let err_fn = |err| tracing::warn!("cpal stream error: {}", err);

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                resampler.push_samples(data, |sample| {
                    frame_buf.push(sample);
                    if frame_buf.len() >= FRAME_SAMPLES {
                        let chunk = std::mem::take(&mut frame_buf);
                        let _: Result<(), SendError<_>> = out.send(chunk);
                    }
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let floats: Vec<f32> = data.iter().map(|&s| s.to_float_sample()).collect();
                resampler.push_samples(&floats, |sample| {
                    frame_buf.push(sample);
                    if frame_buf.len() >= FRAME_SAMPLES {
                        let chunk = std::mem::take(&mut frame_buf);
                        let _ = out.send(chunk);
                    }
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _| {
                let floats: Vec<f32> = data.iter().map(|&s| s.to_float_sample()).collect();
                resampler.push_samples(&floats, |sample| {
                    frame_buf.push(sample);
                    if frame_buf.len() >= FRAME_SAMPLES {
                        let chunk = std::mem::take(&mut frame_buf);
                        let _ = out.send(chunk);
                    }
                });
            },
            err_fn,
            None,
        ),
        other => return Err(anyhow!("Mic-Sample-Format {:?} nicht unterstützt", other)),
    }
    .context("build_input_stream")?;

    stream.play().context("stream.play")?;
    Ok(MicSession {
        _stream: stream,
        sample_rate,
    })
}

/// Simple downmix-to-mono + linear-interpolation resampler. Operates in
/// streaming mode (push chunks of interleaved input, get callback per
/// resampled mono sample). Good enough for speech; not audiophile.
struct Resampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    // Phase position in the input stream, fractional. Each output sample
    // advances this by `in_rate / out_rate`.
    pos: f64,
    // Last input mono sample, kept across pushes so we can interpolate
    // across chunk boundaries.
    last_mono: f32,
    have_last: bool,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        Self {
            in_rate,
            out_rate,
            channels: channels.max(1),
            pos: 0.0,
            last_mono: 0.0,
            have_last: false,
        }
    }

    /// Feed an interleaved input chunk; invoke `emit` once per output
    /// (resampled) mono sample.
    fn push_samples<F: FnMut(f32)>(&mut self, interleaved: &[f32], mut emit: F) {
        if interleaved.is_empty() {
            return;
        }
        let step = self.in_rate as f64 / self.out_rate as f64;
        let mut input_idx: usize = 0;
        let frames = interleaved.len() / self.channels;

        while input_idx < frames {
            let next_mono = downmix_frame(interleaved, input_idx, self.channels);
            if !self.have_last {
                self.last_mono = next_mono;
                self.have_last = true;
                input_idx += 1;
                continue;
            }
            // Emit every output sample whose phase falls in the interval
            // [input_idx-1, input_idx).
            while self.pos < input_idx as f64 {
                let frac = self.pos - (input_idx - 1) as f64;
                let interpolated =
                    self.last_mono + (next_mono - self.last_mono) * frac as f32;
                emit(interpolated);
                self.pos += step;
            }
            self.last_mono = next_mono;
            input_idx += 1;
        }
        // Wrap pos so it doesn't grow unbounded across pushes.
        self.pos -= frames as f64;
    }
}

fn downmix_frame(interleaved: &[f32], frame_idx: usize, channels: usize) -> f32 {
    let start = frame_idx * channels;
    let end = start + channels;
    if end > interleaved.len() {
        return 0.0;
    }
    let sum: f32 = interleaved[start..end].iter().sum();
    sum / channels as f32
}
