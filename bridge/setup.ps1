# Companion post-install setup
#
# Builds the Python sidecar venv next to this script (`pyicloud/.venv`)
# and installs the bridge dependencies. Run once after installing
# Companion; safe to re-run (idempotent).
#
# Requires Python 3.12 in PATH or at a standard install location.
# Tries `py -3.12`, then `python3.12`, then `python` (with version check).
#
# Usage:
#     Right-click setup.ps1 → "Run with PowerShell"
#   or from a shell:
#     powershell -ExecutionPolicy Bypass -File setup.ps1

$ErrorActionPreference = "Stop"

# Resolve paths
$ScriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$BridgeDir = Join-Path $ScriptRoot "pyicloud"
$VenvDir = Join-Path $BridgeDir ".venv"
$VenvPython = Join-Path $VenvDir "Scripts\python.exe"

Write-Host ""
Write-Host "── Fibi Companion: Setup ─────────────────────────────────" -ForegroundColor Cyan
Write-Host "Sidecar-venv: $VenvDir"
Write-Host ""

# ── Locate Python 3.12 ──────────────────────────────────────────────────

function Find-Python312 {
    # 1. Windows py.exe launcher with version pin (preferred)
    try {
        $out = & py -3.12 -c "import sys; print(sys.executable)" 2>$null
        if ($LASTEXITCODE -eq 0 -and $out) {
            return $out.Trim()
        }
    } catch { }

    # 2. `python3.12` on PATH
    $cmd = Get-Command python3.12 -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    # 3. `python` on PATH (only if it's 3.12.x)
    $cmd = Get-Command python -ErrorAction SilentlyContinue
    if ($cmd) {
        $ver = & $cmd.Source --version 2>&1
        if ($ver -match "Python 3\.12\.") {
            return $cmd.Source
        }
    }

    # 4. Common install paths (per-user + machine)
    $candidates = @(
        "$env:LOCALAPPDATA\Programs\Python\Python312\python.exe",
        "$env:ProgramFiles\Python312\python.exe",
        "${env:ProgramFiles(x86)}\Python312\python.exe"
    )
    foreach ($c in $candidates) {
        if (Test-Path $c) { return $c }
    }
    return $null
}

$Python = Find-Python312
if (-not $Python) {
    Write-Host "FEHLER: Python 3.12 nicht gefunden." -ForegroundColor Red
    Write-Host ""
    Write-Host "Lade Python 3.12 von https://www.python.org/downloads/ herunter und"
    Write-Host "installiere es mit der Option ""Add python.exe to PATH"". Danach diesen"
    Write-Host "Setup nochmal starten."
    Write-Host ""
    Read-Host "Enter zum Beenden"
    exit 1
}
Write-Host "✓ Python gefunden: $Python" -ForegroundColor Green

# ── Create venv ─────────────────────────────────────────────────────────

if (Test-Path $VenvPython) {
    Write-Host "✓ venv existiert schon — überspringe Erstellung." -ForegroundColor Green
} else {
    Write-Host "→ Erstelle venv …"
    & $Python -m venv "$VenvDir"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FEHLER beim venv-Erstellen." -ForegroundColor Red
        Read-Host "Enter zum Beenden"
        exit 1
    }
    Write-Host "✓ venv erstellt." -ForegroundColor Green
}

# ── Install / update deps ───────────────────────────────────────────────

Write-Host "→ Aktualisiere pip …"
& $VenvPython -m pip install --upgrade --quiet pip

$Packages = @(
    "openwakeword",
    "sounddevice",
    "scipy",
    "faster-whisper",
    "git+https://github.com/timlaing/pyicloud.git"
)

Write-Host "→ Installiere Pakete (kann ein paar Minuten dauern) …"
foreach ($pkg in $Packages) {
    Write-Host "    $pkg"
    & $VenvPython -m pip install --quiet $pkg
    if ($LASTEXITCODE -ne 0) {
        Write-Host "    FEHLER bei $pkg" -ForegroundColor Red
        Read-Host "Enter zum Beenden"
        exit 1
    }
}
Write-Host "✓ Pakete installiert." -ForegroundColor Green

# ── Pre-download openWakeWord pretrained models ─────────────────────────

Write-Host "→ Lade openWakeWord-Built-In-Modelle …"
& $VenvPython -c "import openwakeword.utils; openwakeword.utils.download_models()" 2>&1 | Out-Null
Write-Host "✓ Built-In-Modelle bereit." -ForegroundColor Green

# ── Done ────────────────────────────────────────────────────────────────

Write-Host ""
Write-Host "── Setup fertig ──────────────────────────────────────────" -ForegroundColor Cyan
Write-Host ""
Write-Host "Nächste Schritte:"
Write-Host "  1. Lege eine `.env`-Datei neben Companion.exe an (siehe `.env.example`)."
Write-Host "     Mindestens ANTHROPIC_API_KEY + ICLOUD_USERNAME + ICLOUD_APP_PASSWORD."
Write-Host ""
Write-Host "  2. Einmalig die iCloud-Auth (Apple-Passwort + 2FA-Code):"
Write-Host "     `"$VenvPython`" `"$BridgeDir\auth_setup.py`""
Write-Host ""
Write-Host "  3. Companion.exe starten — Fibi sollte sich auf der Taskleiste melden."
Write-Host ""
Read-Host "Enter zum Schließen"
