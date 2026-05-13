//! Background CPU-load sampler that emits a `cat://heat` event when the
//! PC heats up. Three states drive Fibi's "hot-plate" indicator:
//!
//!   "cool" — load ≤ 40 %  (default, nothing shown)
//!   "warm" — sustained 50-80 %, orange glow under Fibi
//!   "hot"  — sustained ≥ 80 %, red glow + heat-shimmer animation
//!
//! Hysteresis on the boundaries so the indicator doesn't flicker between
//! states during normal usage. Samples every 2 s.

use std::time::Duration;

use serde::Serialize;
use sysinfo::System;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum HeatState {
    Cool,
    Warm,
    Hot,
}

#[derive(Debug, Clone, Serialize)]
struct HeatPayload {
    state: HeatState,
    cpu: u8, // smoothed percent
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        run(app).await;
    });
}

async fn run(app: AppHandle) {
    let mut sys = System::new();
    sys.refresh_cpu_usage();
    // sysinfo needs at least ~200ms between refreshes for usable deltas.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut smoothed: f32 = 0.0;
    let mut last_emitted: Option<HeatState> = None;

    loop {
        sys.refresh_cpu_usage();
        let raw: f32 = sys.global_cpu_usage();
        // EMA. Half-life ~5 samples (= 10 s) — fast enough that David
        // sees the plate within a few seconds of a real load, slow
        // enough that a one-frame spike doesn't fire it.
        smoothed = smoothed * 0.7 + raw * 0.3;

        let next = decide(smoothed, last_emitted);
        if Some(next) != last_emitted {
            let payload = HeatPayload {
                state: next,
                cpu: smoothed.round().clamp(0.0, 100.0) as u8,
            };
            let _ = app.emit_to("cat", "cat://heat", payload);
            tracing::debug!("heat state → {:?} ({}%)", next, smoothed as u8);
            last_emitted = Some(next);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Threshold ladder with hysteresis: the *enter* thresholds are higher
/// than the *exit* ones, so the state has to drop a margin before
/// downgrading. Prevents jitter when load hovers near a boundary.
fn decide(smoothed: f32, prev: Option<HeatState>) -> HeatState {
    match prev {
        Some(HeatState::Hot) => {
            // exit hot at 65 % → step down to warm; exit warm at 35 % → cool.
            if smoothed < 65.0 {
                if smoothed < 35.0 { HeatState::Cool } else { HeatState::Warm }
            } else {
                HeatState::Hot
            }
        }
        Some(HeatState::Warm) => {
            if smoothed >= 75.0 {
                HeatState::Hot
            } else if smoothed < 35.0 {
                HeatState::Cool
            } else {
                HeatState::Warm
            }
        }
        _ => {
            // Entry thresholds — must be ≥10 % above the exit thresholds
            // so a load hovering near a boundary doesn't flap.
            if smoothed >= 75.0 {
                HeatState::Hot
            } else if smoothed >= 45.0 {
                HeatState::Warm
            } else {
                HeatState::Cool
            }
        }
    }
}
