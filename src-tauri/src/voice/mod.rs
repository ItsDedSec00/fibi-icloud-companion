//! Voice pipeline.
//!
//! Architecture:
//! - **Wake-word** lives in `bridge/pyicloud/voice_bridge.py` (openWakeWord,
//!   ~100 MB RAM). It owns one mic stream and signals Companion via NDJSON
//!   over stdout when the user says "Fibi".
//! - **Speech-to-text** runs natively in this Rust crate via `whisper.cpp`
//!   (FFI through the `whisper-rs` crate). After wake, we open a second
//!   shared mic stream via `cpal`, buffer ~5-10 s of audio, feed it to
//!   the model, and stream the transcript back to the frontend.
//! - **Voice activity detection** (silence end-of-speech) is a simple RMS
//!   energy threshold for now — replaceable with Silero ONNX later.
//!
//! Why the split? The Python sidecar is small at runtime (the wake-word
//! model is ~30 KB onnx) and gives us battle-tested wake detection. Doing
//! STT natively in Rust means **no Python interpreter in the STT hot path**,
//! which roughly halves the active RAM footprint versus running whisper
//! inside the Python process.

pub mod bridge;
pub mod controller;
pub mod mic;
pub mod recorder;
pub mod whisper;
