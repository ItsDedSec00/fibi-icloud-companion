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

## Install (NSIS installer)

1. **Python 3.12 first.** Download from
   [python.org](https://www.python.org/downloads/) and tick *"Add python.exe
   to PATH"*. The sidecars rely on the system Python — the installer
   doesn't bundle one.
2. Run `Companion_X.Y.Z_x64-setup.exe`.
3. Right-click the installed `bridge\setup.ps1` → *"Run with PowerShell"*.
   It creates `bridge\pyicloud\.venv` and pip-installs the sidecar deps
   (openWakeWord, sounddevice, scipy, faster-whisper, pyicloud). Takes a
   couple of minutes once.
4. Drop a `.env` next to `Companion.exe` (copy `.env.example` and fill).
   At minimum `ANTHROPIC_API_KEY`, plus iCloud / IMAP creds for the
   integrations you want.
5. One-time Apple-ID auth — the Settings window's *"iCloud neu
   verbinden…"* drives the password + 2FA flow.

Done. Fibi shows up on the taskbar; right-click her for the quick menu,
say *"Fibi"* for voice.

## Build from source

Requirements (build host):
- Rust toolchain (cargo)
- LLVM (for `whisper-rs` bindgen) — `winget install LLVM.LLVM`, then
  `setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"`
- CMake — `winget install Kitware.CMake`
- Node 20+ + pnpm — `winget install OpenJS.NodeJS.LTS; npm i -g pnpm`
- Python 3.12 (same as runtime)

```powershell
git clone https://github.com/ItsDedSec00/fibi-icloud-companion.git
cd fibi-icloud-companion
pnpm install

# Dev venv for the sidecars (same script as the installer ships)
powershell -ExecutionPolicy Bypass -File bridge\setup.ps1

# Whisper model — German `small` (~470 MB), not in git
curl -L `
  "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin" `
  -o bridge\models\ggml-small.bin

# Wake-word model — train your own at openWakeWord's Colab. For decent
# recall, record ~150 samples of yourself saying "Fibi" first:
bridge\pyicloud\.venv\Scripts\python.exe bridge\pyicloud\record_samples.py
# Upload bridge/training/pixel/ as a ZIP to the Colab, train with
# `custom_positive_data`, drop the resulting .onnx at
# bridge\models\fibi.onnx.

copy .env.example .env  # then fill in the keys
pnpm tauri dev          # iterate
pnpm tauri build        # produces target\release\bundle\nsis\…-setup.exe
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
