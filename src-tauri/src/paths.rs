//! Tiny shared utility: enumerate candidate roots where `bridge/...` may
//! live, in priority order. Used by all four path-finders (icloud bridge,
//! voice bridge, reauth helper, whisper model).
//!
//! Dev: bridge lives at the project root, parallel to `src-tauri/` and
//! `src/`. Production (Tauri bundle): Tauri copies the `bridge/` resource
//! to `<exe_dir>/resources/bridge/` on Windows MSI/NSIS installs. We
//! search both, so the same exe binary works in both modes.

use std::path::PathBuf;

/// Shared error string for the "Python sidecar venv is missing" case.
/// We point the user at the bundled `setup.ps1` rather than telling them
/// to recreate the venv by hand.
pub const SETUP_HINT: &str =
    "Python-Sidecar-venv fehlt. Rechtsklick auf `bridge\\setup.ps1` → \
     `Mit PowerShell ausführen`. Voraussetzung: Python 3.12 installiert.";

pub fn bridge_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // Tauri bundle puts resources at <exe>/resources/.
            roots.push(parent.join("resources"));
            roots.push(parent.to_path_buf());
        }
    }
    roots
}

/// Locate a file relative to one of the bridge roots, walking up the
/// candidate dirs by `walk_up` parents. Used by callers that need to
/// find e.g. `bridge/pyicloud/voice_bridge.py`.
pub fn find_under_bridge(rel: &[&str], walk_up: usize) -> Option<PathBuf> {
    for start in bridge_search_roots() {
        let mut cur = Some(start);
        for _ in 0..=walk_up {
            let Some(dir) = cur.take() else { break };
            let mut p = dir.clone();
            for seg in rel {
                p = p.join(seg);
            }
            if p.is_file() {
                return Some(p);
            }
            cur = dir.parent().map(PathBuf::from);
        }
    }
    None
}
