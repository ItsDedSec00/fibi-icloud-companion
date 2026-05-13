"""Voice wake-word sidecar for Companion.

Long-running process. Reads from the default Windows microphone (WASAPI),
runs openWakeWord on a continuous 80 ms frame stream, and emits NDJSON
events on stdout when the user says "Fibi":

    {"event": "ready"}                              # once on startup
    {"event": "wake",  "score": 0.82, "ts": ".."}   # one per detection
    {"event": "level", "rms": 0.04, "gain": 4.2}    # periodic, optional

Configuration via env vars:
    VOICE_MODEL_PATH     path to .onnx wake-word model
                         (default: <bridge>/models/fibi.onnx)
    VOICE_THRESHOLD      detection threshold, 0..1     (default: 0.5)
    VOICE_COOLDOWN_MS    suppress repeats within this  (default: 1500)
    MIC_DEVICE           optional substring of mic name to override
                         WASAPI default

Companion (Rust) spawns this process, reads NDJSON lines from its stdout,
and uses cpal directly for the post-wake STT capture so the two mic
consumers stay independent.
"""

from __future__ import annotations

import json
import os
import queue
import sys
import time
from math import gcd
from pathlib import Path

import numpy as np
import sounddevice as sd
from openwakeword.model import Model
from scipy.signal import resample_poly

TARGET_RATE = 16_000
FRAME_MS = 80
FRAME_SAMPLES = TARGET_RATE * FRAME_MS // 1000  # 1280 @ 16 kHz


def emit(obj: dict) -> None:
    """Write one NDJSON event to stdout. Flushes so the parent sees it
    promptly. stdout is the **only** channel for events; stderr is for
    human-readable logs."""
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    print(f"[voice-bridge] {msg}", file=sys.stderr, flush=True)


def pick_input_device() -> int | None:
    devices = sd.query_devices()
    override = (os.environ.get("MIC_DEVICE") or "").strip().lower()
    if override:
        for i, d in enumerate(devices):
            if d["max_input_channels"] > 0 and override in d["name"].lower():
                log(f"MIC_DEVICE override: [{i}] {d['name']}")
                return i
        log(f"MIC_DEVICE='{override}' matched no input device, fallback")

    for h in sd.query_hostapis():
        if "wasapi" in h["name"].lower():
            idx = h.get("default_input_device", -1)
            if isinstance(idx, int) and 0 <= idx < len(devices):
                log(f"Windows-default WASAPI input: [{idx}] {devices[idx]['name']}")
                return idx
            break
    log("no WASAPI default — using sounddevice default")
    return None


def resolve_model_path() -> Path:
    raw = (os.environ.get("VOICE_MODEL_PATH") or "").strip()
    if raw:
        return Path(raw)
    return Path(__file__).resolve().parent.parent / "models" / "fibi.onnx"


def main() -> int:
    model_path = resolve_model_path()
    # Default 0.3 reflects what we measured with the current Fibi model
    # (peaks 0.17-0.82, clear utterances reliably > 0.3). Bump higher
    # via env var if you get false positives, lower for more recall.
    threshold = float(os.environ.get("VOICE_THRESHOLD") or "0.3")
    cooldown_s = float(os.environ.get("VOICE_COOLDOWN_MS") or "1500") / 1000.0
    level_emit_interval = 1.0  # seconds

    if not model_path.is_file():
        log(f"ERROR: model not found at {model_path}")
        emit({"event": "error", "error": f"model not found: {model_path}"})
        return 2

    log(f"loading model: {model_path}")
    model = Model(wakeword_models=[str(model_path)], inference_framework="onnx")
    wake_keys = list(model.models.keys())
    log(f"wake keys: {wake_keys}")
    if not wake_keys:
        emit({"event": "error", "error": "no wake key loaded from model"})
        return 3
    key = wake_keys[0]

    device_idx = pick_input_device()
    device_info = (
        sd.query_devices(device_idx)
        if device_idx is not None
        else sd.query_devices(kind="input")
    )
    native_rate = int(device_info["default_samplerate"])
    native_block = native_rate * FRAME_MS // 1000
    log(f"mic native rate {native_rate} Hz, resample to {TARGET_RATE} Hz")

    g = gcd(native_rate, TARGET_RATE)
    up = TARGET_RATE // g
    down = native_rate // g

    audio_q: queue.Queue[np.ndarray] = queue.Queue()
    agc_peak = [0.01]

    def callback(indata, frames, time_info, status):
        if status:
            log(f"mic status: {status}")
        mono = indata[:, 0].astype(np.float32)
        if native_rate != TARGET_RATE:
            mono = resample_poly(mono, up, down).astype(np.float32)
        peak = float(np.max(np.abs(mono)))
        # Fast attack, ~3 s release. Gain clamped ≥ 1 so we never attenuate.
        agc_peak[0] = max(peak, agc_peak[0] * 0.98)
        gain = max(1.0, min(30.0, 0.3 / max(0.01, agc_peak[0])))
        boosted = mono * gain
        pcm16 = np.clip(boosted * 32767.0, -32768, 32767).astype(np.int16)
        audio_q.put(pcm16)

    emit({"event": "ready", "wake_key": key, "threshold": threshold})
    log("listening …")

    last_detect = 0.0
    last_level_emit = time.time()
    level_max_rms = 0.0
    level_max_score = 0.0
    cur_gain = 1.0

    with sd.InputStream(
        samplerate=native_rate,
        channels=1,
        dtype="float32",
        blocksize=native_block,
        callback=callback,
        device=device_idx,
    ):
        while True:
            pcm = audio_q.get()
            now = time.time()
            rms = float(np.sqrt(np.mean(pcm.astype(np.float32) ** 2))) / 32768.0
            level_max_rms = max(level_max_rms, rms)
            cur_gain = max(1.0, min(30.0, 0.3 / max(0.01, agc_peak[0])))

            preds = model.predict(pcm)
            score = float(preds.get(key, 0.0))
            level_max_score = max(level_max_score, score)

            if score >= threshold and (now - last_detect) > cooldown_s:
                last_detect = now
                emit({
                    "event": "wake",
                    "score": round(score, 3),
                    "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
                })
                log(f"WAKE  score={score:.3f}")
                # Reset internal smoothing buffer so subsequent unrelated
                # speech doesn't re-trigger immediately.
                model.reset()

            if now - last_level_emit >= level_emit_interval:
                emit({
                    "event": "level",
                    "rms": round(level_max_rms, 4),
                    "gain": round(cur_gain, 2),
                    "peak_score": round(level_max_score, 3),
                })
                level_max_rms = 0.0
                level_max_score = 0.0
                last_level_emit = now


if __name__ == "__main__":
    try:
        code = main()
    except KeyboardInterrupt:
        log("stopped (Ctrl-C)")
        code = 0
    except Exception as e:  # noqa: BLE001
        import traceback
        traceback.print_exc(file=sys.stderr)
        emit({"event": "error", "error": str(e)})
        code = 99
    raise SystemExit(code)
