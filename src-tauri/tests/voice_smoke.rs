//! Mic → whisper end-to-end smoke test. Records 5 seconds from the default
//! input device, transcribes via whisper-rs, prints the result.
//!
//! Ignored by default. Run with:
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo test --test voice_smoke -- --ignored --nocapture record_and_transcribe
//!
//! Speak something in German for ~4 seconds. Expects `bridge/models/ggml-base.bin`
//! to exist (set WHISPER_MODEL_PATH to override).

use std::time::Duration;

use companion_lib::voice::{mic, whisper};

#[tokio::test]
#[ignore]
async fn record_and_transcribe() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,companion_lib=debug")),
        )
        .with_test_writer()
        .init();

    let model_path = whisper::resolve_model_path();
    println!("→ Modell: {}", model_path.display());

    println!("→ Lade Whisper …");
    let stt = whisper::WhisperEngine::load(&model_path).expect("load whisper model");
    println!("→ Modell geladen.");

    let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let _mic = mic::open(tx).expect("open mic");

    println!("\n🎤 Sprich jetzt ~5 Sekunden auf Deutsch …\n");

    // Collect ~5 seconds of audio (5 s × 12.5 frames/s = ~62 frames @ 80 ms).
    let target_samples = (mic::TARGET_SAMPLE_RATE as usize) * 5;
    let buf: Vec<f32> = tokio::task::spawn_blocking(move || {
        let mut buf: Vec<f32> = Vec::with_capacity(target_samples);
        let deadline = std::time::Instant::now() + Duration::from_secs(7);
        while buf.len() < target_samples && std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(chunk) => buf.extend_from_slice(&chunk),
                Err(_) => continue,
            }
        }
        buf
    })
    .await
    .expect("join blocking");
    println!("→ {} samples gepuffert.", buf.len());

    println!("→ Transkribiere …");
    let started = std::time::Instant::now();
    let text = stt.transcribe(buf, "de").await.expect("transcribe");
    println!(
        "\n=== ERGEBNIS ({:?}) ===\n{}\n=====================",
        started.elapsed(),
        text
    );
}
