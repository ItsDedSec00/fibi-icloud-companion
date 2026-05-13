//! Windows login-autostart toggle via the user-scope `Run` registry key.
//!
//! Writes to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Companion`
//! with the absolute path of the currently-running executable. Per-user
//! (no admin needed). The "Companion" name is just the value-name we own
//! under that key.
//!
//! `is_enabled()` reads the key and confirms the value still points at
//! THIS exe — protects against stale entries left over from an old
//! install / move that would silently launch a wrong (or missing) binary.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const ENTRY_NAME: &str = "Companion";

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("current_exe")
}

/// Autostart is "on" when the registry entry exists AND points at the
/// running exe. A mismatched path returns false so the UI prompts the
/// user to enable again, which re-points the entry.
pub fn is_enabled() -> bool {
    let Ok(exe) = current_exe() else { return false };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(run) = hkcu.open_subkey(RUN_SUBKEY) else {
        return false;
    };
    let Ok(stored) = run.get_value::<String, _>(ENTRY_NAME) else {
        return false;
    };
    // Stored value may be quoted; strip a single pair of double-quotes
    // before comparing to the current exe path.
    let stored = stored.trim().trim_matches('"');
    let exe_str = exe.to_string_lossy();
    stored.eq_ignore_ascii_case(&exe_str)
}

pub fn enable() -> Result<()> {
    let exe = current_exe()?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey(RUN_SUBKEY)
        .context("create Run subkey")?;
    let value = format!("\"{}\"", exe.display());
    run.set_value(ENTRY_NAME, &value)
        .context("set autostart value")?;
    tracing::info!("autostart enabled → {}", value);
    Ok(())
}

pub fn disable() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = hkcu
        .open_subkey_with_flags(RUN_SUBKEY, KEY_SET_VALUE)
        .map_err(|e| anyhow!("open Run for write: {}", e))?;
    // delete_value returns NOT_FOUND if the value isn't there; treat as ok.
    let _ = run.delete_value(ENTRY_NAME);
    tracing::info!("autostart disabled");
    Ok(())
}
