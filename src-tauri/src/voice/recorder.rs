//! Post-wake recorder.
//!
//! After the Python wake-word bridge emits a `wake` event, we open our
//! own `cpal` mic stream and capture audio until the user stops speaking
//! (RMS energy drops below a silence threshold for `SILENCE_HANG` ms) or
//! the hard `MAX_DURATION` is hit, whichever comes first.
//!
//! The captured `Vec<f32>` is handed off to Whisper for transcription.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use super::{mic, whisper};

/// Treat audio above this RMS (0..1 scale) as speech.
const SPEECH_RMS: f32 = 0.015;
/// Treat audio below this RMS as silence.
const SILENCE_RMS: f32 = 0.008;
/// Stop recording after this much continuous silence post-speech.
const SILENCE_HANG: Duration = Duration::from_millis(1300);
/// Hard ceiling — never record longer than this even if the user keeps talking.
/// Limits damage from any spurious wake event, and matches typical voice
/// command length (~5-7 s for most chat requests).
const MAX_DURATION: Duration = Duration::from_secs(10);
/// Bail if we never detect any speech within this window after a wake event.
const SPEECH_GRACE: Duration = Duration::from_millis(2500);

pub struct Capture {
    pub samples: Vec<f32>,
    pub duration: Duration,
}

/// Block until the user finishes speaking (or timeout). Returns the
/// captured mono 16 kHz f32 buffer. Errors if we never heard any speech.
///
/// Runs synchronously inside [`tokio::task::spawn_blocking`] — `cpal`'s
/// `Stream` isn't `Send`, so the whole mic-owning section has to stay on
/// one OS thread.
pub async fn capture_utterance() -> Result<Capture> {
    tokio::task::spawn_blocking(capture_utterance_blocking)
        .await
        .context("spawn_blocking join (recorder)")?
}

fn capture_utterance_blocking() -> Result<Capture> {
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let _mic = mic::open(tx)?; // dropped at end of function → closes stream

    let mut buf: Vec<f32> = Vec::with_capacity(mic::TARGET_SAMPLE_RATE as usize * 8);
    let start = Instant::now();
    let mut speech_started = false;
    let mut last_speech = start;

    loop {
        if start.elapsed() > MAX_DURATION {
            tracing::info!("recorder: MAX_DURATION reached");
            break;
        }
        let chunk = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if speech_started && last_speech.elapsed() > SILENCE_HANG {
                    break;
                }
                if !speech_started && start.elapsed() > SPEECH_GRACE {
                    return Err(anyhow!("no speech detected after wake"));
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let rms_val = rms(&chunk);
        if rms_val >= SPEECH_RMS {
            speech_started = true;
            last_speech = Instant::now();
        } else if rms_val < SILENCE_RMS && speech_started && last_speech.elapsed() > SILENCE_HANG {
            buf.extend_from_slice(&chunk);
            break;
        }
        buf.extend_from_slice(&chunk);

        if !speech_started && start.elapsed() > SPEECH_GRACE {
            return Err(anyhow!("no speech detected after wake"));
        }
    }

    if !speech_started || buf.is_empty() {
        return Err(anyhow!("no speech captured"));
    }
    Ok(Capture {
        samples: buf,
        duration: start.elapsed(),
    })
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Convenience: capture an utterance and transcribe it through the shared
/// Whisper engine. Returns the recognized text in German.
pub async fn record_and_transcribe() -> Result<String> {
    let cap = capture_utterance().await?;
    tracing::info!(
        "recorder: captured {:?} ({} samples)",
        cap.duration,
        cap.samples.len()
    );
    let engine = whisper::shared_engine().await?;
    let started = Instant::now();
    let text = engine.transcribe(cap.samples, "de").await?;
    tracing::info!("whisper: {:?} → '{}'", started.elapsed(), text);
    Ok(text)
}
