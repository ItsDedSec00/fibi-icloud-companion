"""Voice-activated batch recorder for openWakeWord training samples.

Opens the default Windows mic (WASAPI) and waits for you to say the
keyword (default "Pixel"). Each utterance is auto-saved as a 16 kHz mono
WAV with ~0.3 s of pre-/post-silence padding so the model sees the full
phoneme envelope.

Usage:
    python record_samples.py [--out <dir>] [--count <n>]

Defaults:
    out:   bridge/training/pixel/   (relative to this script)
    count: 200 samples

While running:
    - Status line shows: current count, current RMS, "RECORDING"/"…silent…"
    - Press Ctrl-C to stop early. Files are written incrementally so it's
      safe to bail any time.

Tips for good samples:
    - Vary distance to mic (close → 2 m)
    - Vary volume (whisper → loud)
    - Vary inflection (statement / question / called-out)
    - Record different times of day (morning hoarse, etc.)
    - Add some background noise (music low, kitchen sounds)
"""

from __future__ import annotations

import argparse
import os
import queue
import sys
import time
from datetime import datetime
from math import gcd
from pathlib import Path

import numpy as np
import sounddevice as sd
from scipy.io import wavfile
from scipy.signal import resample_poly

TARGET_RATE = 16_000  # what openWakeWord training expects
SILENCE_RMS = 0.01    # threshold below which we consider audio "silent"
SPEECH_RMS = 0.03     # threshold to consider audio "speech started"
PRE_ROLL_S = 0.3       # capture this much silence before speech onset
POST_ROLL_S = 0.5      # keep recording this long after speech drops
MIN_UTTERANCE_S = 0.3  # ignore blips shorter than this
MAX_UTTERANCE_S = 2.0  # ignore anything longer (probably not just "Pixel")


def pick_input_device() -> int | None:
    """Windows-default input via WASAPI (same logic as voice_smoketest.py)."""
    override = (os.environ.get("MIC_DEVICE") or "").strip().lower()
    if override:
        devices = sd.query_devices()
        for i, d in enumerate(devices):
            if d["max_input_channels"] > 0 and override in d["name"].lower():
                return i
    for h in sd.query_hostapis():
        if "wasapi" in h["name"].lower():
            idx = h.get("default_input_device", -1)
            if isinstance(idx, int) and idx >= 0:
                return idx
            break
    return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "training" / "pixel",
    )
    parser.add_argument("--count", type=int, default=200)
    args = parser.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    existing = sorted(args.out.glob("sample_*.wav"))
    start_idx = (int(existing[-1].stem.split("_")[1]) + 1) if existing else 1
    print(f"Speichere nach: {args.out}")
    print(f"Bereits vorhanden: {len(existing)} Samples")
    print(f"Ziel: {args.count} weitere Samples (insgesamt {start_idx - 1 + args.count})\n")

    device_idx = pick_input_device()
    device_info = (
        sd.query_devices(device_idx)
        if device_idx is not None
        else sd.query_devices(kind="input")
    )
    native_rate = int(device_info["default_samplerate"])
    print(f"Mic: {device_info['name']}  @  {native_rate} Hz → resample {TARGET_RATE} Hz")

    g = gcd(native_rate, TARGET_RATE)
    up = TARGET_RATE // g
    down = native_rate // g

    pre_roll_samples = int(PRE_ROLL_S * TARGET_RATE)
    post_roll_samples = int(POST_ROLL_S * TARGET_RATE)
    min_samples = int(MIN_UTTERANCE_S * TARGET_RATE)
    max_samples = int(MAX_UTTERANCE_S * TARGET_RATE)

    audio_q: queue.Queue[np.ndarray] = queue.Queue()

    def callback(indata, frames, time_info, status):
        if status:
            print(f"[mic] {status}", file=sys.stderr, flush=True)
        mono = indata[:, 0].astype(np.float32)
        if native_rate != TARGET_RATE:
            mono = resample_poly(mono, up, down).astype(np.float32)
        audio_q.put(mono)

    native_block = native_rate * 80 // 1000  # 80 ms frames

    print("\n→ Bereit. Sag 'Pixel' im natürlichen Tonfall.")
    print("  Variiere Distanz, Lautstärke, Tageszeit.")
    print("  Ctrl-C zum Stoppen.\n")

    rolling = np.zeros(pre_roll_samples, dtype=np.float32)  # circular pre-roll
    rolling_pos = 0
    in_utterance = False
    utterance_buf: list[np.ndarray] = []
    silence_streak = 0
    silence_streak_needed = post_roll_samples
    saved = 0

    try:
        with sd.InputStream(
            samplerate=native_rate,
            channels=1,
            dtype="float32",
            blocksize=native_block,
            callback=callback,
            device=device_idx,
        ):
            while saved < args.count:
                chunk = audio_q.get()
                rms = float(np.sqrt(np.mean(chunk ** 2)))

                if not in_utterance:
                    # Keep filling the pre-roll buffer (circular).
                    n = len(chunk)
                    if n >= pre_roll_samples:
                        rolling = chunk[-pre_roll_samples:].copy()
                        rolling_pos = 0
                    else:
                        end = rolling_pos + n
                        if end <= pre_roll_samples:
                            rolling[rolling_pos:end] = chunk
                        else:
                            split = pre_roll_samples - rolling_pos
                            rolling[rolling_pos:] = chunk[:split]
                            rolling[: n - split] = chunk[split:]
                        rolling_pos = end % pre_roll_samples

                    if rms > SPEECH_RMS:
                        # Speech onset — pull pre-roll into utterance.
                        in_utterance = True
                        ordered = np.concatenate(
                            (rolling[rolling_pos:], rolling[:rolling_pos])
                        )
                        utterance_buf = [ordered, chunk]
                        silence_streak = 0
                        sys.stdout.write("\r● RECORDING               ")
                        sys.stdout.flush()
                else:
                    utterance_buf.append(chunk)
                    if rms < SILENCE_RMS:
                        silence_streak += len(chunk)
                    else:
                        silence_streak = 0
                    if silence_streak >= silence_streak_needed:
                        # Utterance complete — save if long enough.
                        full = np.concatenate(utterance_buf)
                        # Trim trailing silence beyond the post-roll budget.
                        trim_to = max(min_samples, len(full) - silence_streak + post_roll_samples)
                        full = full[:trim_to]
                        n = len(full)
                        if min_samples <= n <= max_samples:
                            idx = start_idx + saved
                            path = args.out / f"sample_{idx:04d}.wav"
                            pcm16 = np.clip(full * 32767.0, -32768, 32767).astype(np.int16)
                            wavfile.write(str(path), TARGET_RATE, pcm16)
                            saved += 1
                            sys.stdout.write(
                                f"\r✓ {path.name}  ({n / TARGET_RATE:.2f}s)   "
                                f"saved: {saved}/{args.count}\n"
                            )
                        else:
                            reason = "zu kurz" if n < min_samples else "zu lang"
                            sys.stdout.write(
                                f"\r✗ verworfen ({reason}, {n / TARGET_RATE:.2f}s)\n"
                            )
                        sys.stdout.flush()
                        in_utterance = False
                        utterance_buf = []
                        silence_streak = 0
                        rolling = np.zeros(pre_roll_samples, dtype=np.float32)
                        rolling_pos = 0
                    else:
                        sys.stdout.write(
                            f"\r● RECORDING   silence={silence_streak / TARGET_RATE:.2f}s  "
                        )
                        sys.stdout.flush()
    except KeyboardInterrupt:
        print("\n\nGestoppt.")

    print(f"\nFertig. {saved} neue Samples in {args.out}")
    print("Lade den Ordner als ZIP zur Colab hoch und setze")
    print("  `custom_positive_data` auf den entpackten Pfad.")
    return 0


if __name__ == "__main__":
    try:
        code = main()
    except Exception:
        import traceback
        traceback.print_exc()
        code = 99
    print(f"\n[Exit {code}. Enter zum Schließen.]")
    try:
        input()
    except EOFError:
        pass
    raise SystemExit(code)
