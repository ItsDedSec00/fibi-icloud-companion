//! Wraps `bridge/pyicloud/reauth_helper.py` so the Settings window can drive
//! the pyicloud trust-cookie refresh flow (Apple-ID password + 2FA code).
//!
//! Two one-shot calls:
//!   - `login(password?)` → tells the helper to start a fresh PyiCloudService,
//!     triggers Apple's 2FA push, returns `{ needs_2fa, needs_password }`.
//!   - `submit_2fa(code)` → validates the code + calls `trust_session()`,
//!     returns `{ success }`.
//!
//! Both calls spawn the helper, send no stdin, parse one JSON line from
//! stdout, and exit. State (auth-cookie cache) lives on disk in
//! `~/.pyicloud/` so the second call picks up where the first left off.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

fn helper_command() -> Result<(PathBuf, PathBuf)> {
    let py = crate::paths::find_under_bridge(
        &["bridge", "pyicloud", ".venv", "Scripts", "python.exe"],
        5,
    )
    .ok_or_else(|| anyhow!("bridge venv python nicht gefunden"))?;
    let script = crate::paths::find_under_bridge(
        &["bridge", "pyicloud", "reauth_helper.py"],
        5,
    )
    .ok_or_else(|| anyhow!("reauth_helper.py nicht gefunden"))?;
    Ok((py, script))
}

/// Run the helper with `args`, return the single JSON line it writes.
async fn run_helper(args: &[&str]) -> Result<serde_json::Value> {
    let (program, script) = helper_command()?;
    tracing::info!("spawning reauth helper: {} {:?}", script.display(), args);
    let mut cmd = Command::new(&program);
    cmd.arg(&script);
    for a in args {
        cmd.arg(*a);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawn reauth_helper.py")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("reauth helper has no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();

    // First non-empty JSON line is the result. The helper exits after one.
    let mut payload: Option<serde_json::Value> = None;
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            payload = Some(v);
            break;
        }
        tracing::debug!("reauth helper non-JSON line: {}", trimmed);
    }
    let _ = child.wait().await;
    payload.ok_or_else(|| anyhow!("reauth helper produced no JSON output"))
}

/// Kick off a fresh login. If `password` is empty we expect the helper to
/// pull it from the Windows Credential Manager (set previously by
/// auth_setup.py or a prior login call).
pub async fn login(password: &str) -> Result<serde_json::Value> {
    let p = password.trim();
    if p.is_empty() {
        run_helper(&["trigger_2fa"]).await
    } else {
        run_helper(&["trigger_2fa", "--password", p]).await
    }
}

pub async fn submit_2fa(code: &str) -> Result<serde_json::Value> {
    let c = code.trim();
    if c.is_empty() {
        return Err(anyhow!("2FA-Code ist leer"));
    }
    run_helper(&["submit_2fa", c]).await
}
