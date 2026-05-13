# Fibi — iCloud Companion

A small grey cat (Fibi) that lives on your Windows taskbar and helps you
stay on top of Apple's stuff — Calendar, Reminders, Mail, Contacts —
without opening any of their apps. Powered by Claude.

> Personal project, Windows-only, hand-built for myself. Works on my
> machine; probably needs love to work on yours.

## What she does

- **Animated taskbar pet** — sprite-sheet cat that idles, walks, sleeps
  over the system clock, reacts to your activity (subtle CPU heat plate
  glows under her when the machine warms up).
- **Chat bubble** above her head — type to Claude. Answers are streamed
  text or compact pixel-style cards (Termine, Reminder, Wetter).
- **Wake-word voice** — say *"Fibi"* (custom-trained openWakeWord model)
  and ask via mic. Whisper.cpp transcribes locally, the answer pops as a
  toast next to her.
- **iCloud Reminders** read + write via reverse-engineered CloudKit
  (`pyicloud`) — sees your *real* modern Reminders, not the legacy
  CalDAV stubs.
- **iCloud Calendar** read + write via CalDAV (incl. shared calendars
  and external iCal feed mixing for Google etc.).
- **iCloud Contacts** lookup (`find_contact`, `upcoming_birthdays`).
- **IMAP / SMTP mail** (Ionos or any IMAPS provider) — read summary +
  send replies.
- **Proactive reminders** 1 h before each event, plus 09:00 + 12:00
  briefings (Fibi sprints frantically across the taskbar if you ignore
  an alert for 30 s).
- **Tray menu**: iCloud-status indicator, "iCloud neu verbinden…" 2FA
  flow, "Zuhören" toggle, login-autostart, debug overlay.

## Architecture

```
companion (Rust + Tauri 2)
├── windows::cat            transparent always-on-top sprite window
├── windows::tray           system-tray menu
├── api::anthropic          Claude streaming + tool-use loop
├── icloud
│   ├── caldav              calendar events (read + write)
│   ├── bridge              pyicloud sidecar (Reminders, Contacts)
│   ├── reauth              pyicloud trust-cookie refresh
│   ├── contacts            in-memory contacts cache + fuzzy find
│   └── external            EXTRA_ICAL_URLS feed parser
├── mail                    IMAP read + SMTP send (lettre)
├── voice
│   ├── bridge              openWakeWord sidecar (Python)
│   ├── controller          watchdog + tray-toggle lifecycle
│   ├── mic                 cpal capture (Windows-default WASAPI)
│   ├── whisper             whisper.cpp via whisper-rs (German small)
│   └── recorder            VAD-driven utterance capture
└── platform
    ├── win                 Win32 hooks (idle, fullscreen, clock rect)
    ├── cpu                 CPU heat sampler → tray-side glow
    └── autostart           login-autostart registry toggle

bridge/pyicloud/            Python sidecar scripts + venv
bridge/models/              fibi.onnx wake-word + ggml whisper model
src/cat/                    main webview (frontend)
src/settings/               settings window (iCloud re-auth)
src/shared/ipc.ts           typed IPC surface
```

## Requirements

- Windows 10 / 11
- Rust toolchain (cargo) + LLVM (for `whisper-rs` bindgen) + CMake
- Node 20+ + pnpm
- Python 3.12 for the sidecars
- A reasonably modern microphone if you want voice

## Setup

```powershell
# Native build deps (one-time)
winget install LLVM.LLVM Kitware.CMake OpenJS.NodeJS.LTS
# After install: ensure LIBCLANG_PATH is set:
#   setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"
# and that CMake's bin is on PATH.
npm install -g pnpm

# Frontend deps
pnpm install

# Python sidecar venv (300 MB once)
python -m venv bridge\pyicloud\.venv
bridge\pyicloud\.venv\Scripts\python.exe -m pip install `
    openwakeword sounddevice scipy faster-whisper `
    git+https://github.com/timlaing/pyicloud.git

# Whisper model — German `small` (~470 MB)
mkdir bridge\models -Force
Invoke-WebRequest `
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin" `
    -OutFile "bridge\models\ggml-small.bin"

# Wake-word model — train your own at openWakeWord's Colab. For better
# recall, record ~150 samples of yourself saying "Fibi" with:
bridge\pyicloud\.venv\Scripts\python.exe bridge\pyicloud\record_samples.py
# Upload the resulting bridge/training/pixel/ ZIP into the Colab, train
# with custom_positive_data, drop the resulting .onnx as
# bridge\models\fibi.onnx.

# Config
copy .env.example .env
# Edit .env: ANTHROPIC_API_KEY, ICLOUD_USERNAME, ICLOUD_APP_PASSWORD,
# IMAP/SMTP if you want mail, ICLOUD_CALENDARS whitelist if you have many.

# One-time iCloud auth (Apple-ID password + 2FA on a trusted device)
bridge\pyicloud\.venv\Scripts\python.exe bridge\pyicloud\auth_setup.py
```

## Run

```powershell
pnpm tauri dev      # development with HMR
pnpm tauri build    # production MSI + NSIS installer
```

## Privacy / What leaves your machine

- **Wake-word detection** — fully local (openWakeWord ONNX, ~150 KB).
- **Speech-to-text** — fully local (whisper.cpp `small`).
- **Chat** — Anthropic API (your `.env` key, your prompts).
- **iCloud** — CalDAV / CardDAV / CloudKit talking directly to Apple.
- **IMAP / SMTP** — your own mail server.

No telemetry. The mic stream never leaves the machine before a wake word.
The Apple-ID password (the real one, not the app-specific one) is stored
in the Windows Credential Manager via the `keyring` Python package.

## License

Personal project, no license declared. If you want to fork: fine, but
the trained `fibi.onnx` (my voice) and any `.env` data are obviously not
covered by anything.
