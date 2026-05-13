//! Voice-listening lifecycle controller.
//!
//! Wraps the Python wake-word sidecar so the tray menu can flip it on/off
//! at runtime. When **off**, the subprocess is killed entirely — the
//! microphone is closed and no audio reaches Companion. When toggled
//! back **on**, a fresh subprocess is spawned (model load takes ~500 ms).
//!
//! Stored as Tauri-managed state so menu handlers can reach it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::AppHandle;

pub struct VoiceController {
    inner: Mutex<Inner>,
    app: AppHandle,
}

struct Inner {
    enabled: bool,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    should_run: Option<Arc<AtomicBool>>,
}

impl VoiceController {
    pub fn new(app: AppHandle) -> Self {
        Self {
            inner: Mutex::new(Inner {
                enabled: false,
                task: None,
                should_run: None,
            }),
            app,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.lock().expect("voice ctrl poisoned").enabled
    }

    /// Start the voice bridge subprocess if it isn't already running.
    /// Wraps the bridge loop in a watchdog that respawns on unexpected
    /// exit (Python crash, sounddevice error, etc.) with exponential
    /// backoff. The user-facing "toggle off" path uses `should_run` to
    /// break the loop cleanly without racing the respawn.
    pub fn enable(&self) {
        let mut guard = self.inner.lock().expect("voice ctrl poisoned");
        if guard.enabled {
            return;
        }
        let app = self.app.clone();
        let should_run = Arc::new(AtomicBool::new(true));
        let should_run_loop = should_run.clone();
        let task = tauri::async_runtime::spawn(async move {
            let mut consecutive_fast_failures: u32 = 0;
            while should_run_loop.load(Ordering::SeqCst) {
                let started = Instant::now();
                match super::bridge::run_loop(app.clone()).await {
                    Ok(()) => {
                        tracing::warn!("voice bridge run_loop ended cleanly (unexpected)");
                    }
                    Err(e) => {
                        tracing::warn!("voice bridge crashed: {}", e);
                    }
                }
                if !should_run_loop.load(Ordering::SeqCst) {
                    break;
                }
                // "Fast failure" = died within 30 s of starting. Long-lived
                // runs reset the counter — typical case is one transient
                // failure (e.g. Apple changed something) followed by a
                // successful retry.
                if started.elapsed() < Duration::from_secs(30) {
                    consecutive_fast_failures =
                        consecutive_fast_failures.saturating_add(1);
                } else {
                    consecutive_fast_failures = 0;
                }
                let wait = backoff(consecutive_fast_failures);
                tracing::info!(
                    "voice bridge: restart in {}s (failure #{} in a row)",
                    wait.as_secs(),
                    consecutive_fast_failures
                );
                // Sleep in small slices so disable() responds quickly.
                let deadline = Instant::now() + wait;
                while Instant::now() < deadline && should_run_loop.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            tracing::info!("voice bridge watchdog: exiting");
        });
        guard.task = Some(task);
        guard.should_run = Some(should_run);
        guard.enabled = true;
        tracing::info!("voice listening: ENABLED");
    }

    /// Abort the watchdog AND the spawned bridge. The
    /// `tokio::process::Child` inside `run_loop` was built with
    /// `kill_on_drop(true)`, so dropping the task frees the child handle
    /// and the OS kills the python.exe — closing the mic.
    pub fn disable(&self) {
        let mut guard = self.inner.lock().expect("voice ctrl poisoned");
        if !guard.enabled {
            return;
        }
        if let Some(flag) = guard.should_run.take() {
            flag.store(false, Ordering::SeqCst);
        }
        if let Some(task) = guard.task.take() {
            task.abort();
        }
        guard.enabled = false;
        tracing::info!("voice listening: DISABLED");
    }
}

/// Backoff schedule: 1s, 2s, 4s, 8s, 16s, 32s, then cap at 60s.
fn backoff(failures: u32) -> Duration {
    let secs = 1u64 << failures.min(6);
    Duration::from_secs(secs.min(60))
}
