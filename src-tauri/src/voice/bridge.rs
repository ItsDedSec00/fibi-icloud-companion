//! Manages the Python voice-bridge subprocess (openWakeWord wake detection).
//!
//! Lifecycle:
//! 1. On Companion startup, `spawn()` launches `bridge/pyicloud/voice_bridge.py`
//!    under that venv's python.exe.
//! 2. We read NDJSON events from its stdout in a background task.
//! 3. On `{"event":"wake", ...}`, we hand off to the recording+whisper
//!    pipeline (see `record_and_transcribe`).
//! 4. The subprocess is `kill_on_drop` so it dies cleanly with Companion.
//!
//! Why this is split from the Rust mic-capture path: openWakeWord is
//! a Python library with no equivalent Rust port. Audio for STT goes
//! through a separate Rust `cpal` mic stream so the Whisper hot path
//! doesn't traverse a Python pipe.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum BridgeEvent {
    Ready {
        wake_key: String,
        threshold: f32,
    },
    Wake {
        score: f32,
        #[serde(default)]
        ts: Option<String>,
    },
    Level {
        rms: f32,
        gain: f32,
        peak_score: f32,
    },
    Error {
        error: String,
    },
}

/// Resolve venv python + script path the same way the icloud bridge does.
fn bridge_command() -> Result<(PathBuf, PathBuf)> {
    let py = crate::paths::find_under_bridge(
        &["bridge", "pyicloud", ".venv", "Scripts", "python.exe"],
        5,
    )
    .ok_or_else(|| anyhow!("bridge venv python nicht gefunden"))?;
    let script = crate::paths::find_under_bridge(
        &["bridge", "pyicloud", "voice_bridge.py"],
        5,
    )
    .ok_or_else(|| anyhow!("voice_bridge.py nicht gefunden"))?;
    Ok((py, script))
}

/// Spawn the voice bridge and start listening for wake events. Returns
/// immediately; the actual loop runs as a detached background task. Wake
/// events trigger a Tauri event `voice://wake` on the cat window and (in
/// the next iteration) kick off the recording pipeline.
///
/// Used at startup. After that, `VoiceController` owns the lifecycle so
/// the tray menu can toggle listening on/off.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_loop(app).await {
            tracing::warn!("voice bridge stopped: {}", e);
        }
    });
}

/// Owned subprocess loop. Public so `VoiceController` can re-spawn it
/// after a toggle-off → toggle-on cycle.
pub async fn run_loop(app: AppHandle) -> Result<()> {
    let (program, script) = bridge_command()?;
    tracing::info!("spawning voice bridge: {} {}", program.display(), script.display());
    let mut child = Command::new(&program)
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("could not spawn voice_bridge.py")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("voice bridge stdout handle missing"))?;
    let mut lines = BufReader::new(stdout).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let event: BridgeEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!("voice bridge non-JSON line: {} ({})", line, err);
                continue;
            }
        };
        match event {
            BridgeEvent::Ready {
                wake_key,
                threshold,
            } => {
                tracing::info!(
                    "voice bridge ready, wake key '{}', threshold {}",
                    wake_key,
                    threshold
                );
            }
            BridgeEvent::Wake { score, ts: _ } => {
                tracing::info!("voice wake event, score={:.3}", score);
                // Emit immediately so the cat window can show a "listening"
                // indicator before the recorder has even captured a sample.
                let _ = app.emit_to("cat", "voice://wake", score);

                // Fire the record→transcribe pipeline on a fresh task so
                // the bridge loop can keep emitting subsequent events.
                let app2 = app.clone();
                tauri::async_runtime::spawn(async move {
                    match crate::voice::recorder::record_and_transcribe().await {
                        Ok(text) if !text.trim().is_empty() => {
                            tracing::info!("voice transcript: '{}'", text);
                            let _ = app2.emit_to("cat", "voice://transcript", text);
                        }
                        Ok(_) => {
                            tracing::info!("voice transcript empty");
                            let _ = app2.emit_to("cat", "voice://cancel", ());
                        }
                        Err(e) => {
                            tracing::warn!("voice pipeline failed: {}", e);
                            let _ = app2.emit_to(
                                "cat",
                                "voice://cancel",
                                e.to_string(),
                            );
                        }
                    }
                });
            }
            BridgeEvent::Level { rms, gain, peak_score } => {
                tracing::debug!(
                    "voice level rms={:.3} gain={:.1} peak_score={:.3}",
                    rms,
                    gain,
                    peak_score
                );
            }
            BridgeEvent::Error { error } => {
                tracing::warn!("voice bridge error: {}", error);
            }
        }
    }
    Err(anyhow!("voice bridge stdout EOF — process exited"))
}
