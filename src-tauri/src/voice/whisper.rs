//! Whisper STT wrapper. Wraps `whisper-rs` (which wraps whisper.cpp via FFI)
//! into a simple async-friendly transcribe call:
//!
//! ```ignore
//! let stt = WhisperEngine::load(model_path)?;
//! let text = stt.transcribe(samples, "de")?;
//! ```
//!
//! Model is loaded once and reused across transcriptions (avoids the
//! ~hundred-ms model-load cost per call). The struct is `Send + Sync` so
//! we can share it across the Tauri runtime via `Arc`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{Mutex, OnceCell};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// One loaded whisper.cpp model + a mutex to serialize inference calls.
/// whisper-rs's state is not `Send`-safe across concurrent inference, so we
/// hold a single state behind an async mutex.
pub struct WhisperEngine {
    ctx: Arc<WhisperContext>,
    /// Serialize concurrent transcribe calls — the underlying whisper.cpp
    /// state isn't thread-safe.
    state_lock: Mutex<()>,
    model_path: PathBuf,
}

impl WhisperEngine {
    pub fn load(model_path: impl AsRef<Path>) -> Result<Self> {
        let model_path = model_path.as_ref().to_path_buf();
        if !model_path.is_file() {
            return Err(anyhow!(
                "Whisper-Modell nicht gefunden: {}. Download von \
                 https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin \
                 oder setze WHISPER_MODEL_PATH in der .env.",
                model_path.display()
            ));
        }
        tracing::info!("loading whisper model: {}", model_path.display());
        let ctx = WhisperContext::new_with_params(
            &model_path,
            WhisperContextParameters::default(),
        )
        .with_context(|| format!("could not load whisper model {}", model_path.display()))?;
        Ok(Self {
            ctx: Arc::new(ctx),
            state_lock: Mutex::new(()),
            model_path,
        })
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Transcribe a mono 16 kHz f32 PCM buffer in the given language code
    /// (`"de"`, `"en"`, …). Returns the concatenated transcript text.
    pub async fn transcribe(&self, samples: Vec<f32>, lang: &str) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        // Serialize: whisper.cpp's state is single-threaded per context.
        let _guard = self.state_lock.lock().await;
        let ctx = self.ctx.clone();
        let lang = lang.to_string();
        // Inference is CPU-bound, blocking — run on a blocking task so we
        // don't stall the tokio runtime.
        let text = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut state = ctx
                .create_state()
                .context("whisper: create_state failed")?;
            // Beam search trades a bit of latency for noticeably better
            // accuracy than greedy on short noisy utterances (Whisper docs
            // recommend beam 5 for production transcription).
            let mut params = FullParams::new(SamplingStrategy::BeamSearch {
                beam_size: 5,
                patience: 1.0,
            });
            params.set_language(Some(&lang));
            params.set_translate(false);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_single_segment(false);
            params.set_suppress_blank(true);
            // Bias the decoder toward chat-like German vocabulary. Whisper
            // uses this as conditioning context (invisible in the output)
            // but it nudges token probabilities toward terms the user is
            // likely to say to Fibi.
            if lang == "de" {
                params.set_initial_prompt(
                    "Gespräch mit Fibi auf Deutsch. Stichworte: Wetter, Kalender, \
                     Termin, Reminder, Müll, D&S, Erinnerungen, Sophie.",
                );
            }

            state
                .full(params, &samples)
                .context("whisper: full() failed")?;

            let n = state.full_n_segments();
            let mut out = String::new();
            for i in 0..n {
                if let Some(seg) = state.get_segment(i) {
                    let text = seg
                        .to_str_lossy()
                        .context("segment to_str_lossy")?
                        .into_owned();
                    out.push_str(&text);
                }
            }
            Ok(out.trim().to_string())
        })
        .await
        .context("spawn_blocking join")??;
        Ok(text)
    }
}

/// Process-wide lazily-loaded engine. First access pays the ~1-2 s model
/// load cost; subsequent transcriptions reuse the same context.
static ENGINE: OnceCell<Arc<WhisperEngine>> = OnceCell::const_new();

pub async fn shared_engine() -> Result<Arc<WhisperEngine>> {
    ENGINE
        .get_or_try_init(|| async {
            let path = resolve_model_path();
            // Load on a blocking thread — model load is CPU+IO bound.
            let loaded = tokio::task::spawn_blocking(move || WhisperEngine::load(&path))
                .await
                .context("spawn_blocking join (whisper load)")??;
            Ok::<_, anyhow::Error>(Arc::new(loaded))
        })
        .await
        .cloned()
}

/// Resolve the on-disk whisper model path. Preference order:
/// 1. `WHISPER_MODEL_PATH` env var (absolute path).
/// 2. `<...>/bridge/models/ggml-small.bin`  — preferred for German.
/// 3. `<...>/bridge/models/ggml-base.bin`   — fallback.
pub fn resolve_model_path() -> PathBuf {
    if let Ok(p) = std::env::var("WHISPER_MODEL_PATH") {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    for filename in &["ggml-small.bin", "ggml-base.bin"] {
        if let Some(p) = crate::paths::find_under_bridge(&["bridge", "models", filename], 5) {
            return p;
        }
    }
    // Final fallback (likely missing — load() will produce a useful error).
    PathBuf::from("bridge/models/ggml-small.bin")
}
