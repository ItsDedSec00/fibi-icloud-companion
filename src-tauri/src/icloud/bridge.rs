//! Pyicloud bridge — long-lived Python subprocess that speaks NDJSON over
//! stdin/stdout. Used for the **modern** iCloud Reminders (CloudKit-based),
//! which CalDAV cannot reach.
//!
//! Architecture:
//! - On first call, [`bridge`] spawns `bridge/pyicloud/.venv/Scripts/python.exe`
//!   with `bridge/pyicloud/bridge.py`. The child stays alive for the rest of
//!   the Companion session; subsequent calls reuse the same process.
//! - One mutex guards the stdin/stdout pipes — serializes requests. Each
//!   request gets a numeric id; the response carries the same id so future
//!   pipelining is trivial. For now we just `send → read` synchronously
//!   under the lock.
//! - On any I/O error or unexpected EOF, the bridge is dropped so the next
//!   call respawns from scratch. Self-healing for python crashes / auth
//!   expiry that needs a fresh process.
//!
//! Path resolution: in dev (`pnpm tauri dev`), CWD is the project root and
//! python lives at `bridge/pyicloud/.venv/Scripts/python.exe`. In production
//! we'll switch to PyInstaller-bundled `pyicloud_bridge.exe` next to the
//! Companion exe (see TODO at [`bridge_command`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, OnceCell, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReminderList {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub id: String,
    pub title: String,
    pub due: Option<String>,
    pub completed: bool,
    pub notes: Option<String>,
    pub list: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedReminder {
    pub success: bool,
    pub message: String,
    pub id: String,
    pub list: String,
}

// ── Process management ────────────────────────────────────────────────────

struct BridgeProcess {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl BridgeProcess {
    async fn spawn() -> Result<Self> {
        let (program, script) = bridge_command()?;
        tracing::info!(
            "spawning pyicloud bridge: {} {}",
            program.display(),
            script.display()
        );
        let mut child = Command::new(&program)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "konnte pyicloud-bridge nicht starten ({})",
                    program.display()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("bridge stdin handle missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("bridge stdout handle missing"))?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
        })
    }

    async fn call_raw(
        &mut self,
        op: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let req = serde_json::json!({
            "id": id.to_string(),
            "op": op,
            "args": args,
        });
        let line = format!("{}\n", serde_json::to_string(&req)?);
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        let response_line = self
            .stdout
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("pyicloud bridge: stdout EOF"))?;
        let resp: serde_json::Value = serde_json::from_str(&response_line)
            .with_context(|| format!("bridge response not JSON: {}", response_line))?;
        if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
            return Err(anyhow!("pyicloud bridge: {}", err));
        }
        Ok(resp.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }
}

fn bridge_command() -> Result<(PathBuf, PathBuf)> {
    let py = crate::paths::find_under_bridge(
        &["bridge", "pyicloud", ".venv", "Scripts", "python.exe"],
        5,
    )
    .ok_or_else(|| anyhow!(crate::paths::SETUP_HINT))?;
    let script = crate::paths::find_under_bridge(&["bridge", "pyicloud", "bridge.py"], 5)
        .ok_or_else(|| anyhow!("bridge.py fehlt — Installation defekt?"))?;
    Ok((py, script))
}

// ── Singleton ────────────────────────────────────────────────────────────

static BRIDGE: OnceCell<Arc<Mutex<Option<BridgeProcess>>>> = OnceCell::const_new();

async fn bridge_slot() -> Arc<Mutex<Option<BridgeProcess>>> {
    BRIDGE
        .get_or_init(|| async { Arc::new(Mutex::new(None)) })
        .await
        .clone()
}

async fn call(op: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    let slot = bridge_slot().await;
    let mut guard = slot.lock().await;
    if guard.is_none() {
        *guard = Some(BridgeProcess::spawn().await?);
    }
    let proc_ref = guard.as_mut().expect("just initialised");
    match proc_ref.call_raw(op, args.clone()).await {
        Ok(v) => Ok(v),
        Err(e) => {
            // Pipe-level failures (EOF, broken JSON) → drop the process so
            // the next call respawns. Logic errors (e.g. needs_reauth) just
            // propagate without restarting.
            let msg = e.to_string();
            if msg.contains("stdout EOF")
                || msg.contains("not JSON")
                || msg.contains("Broken pipe")
            {
                tracing::warn!("pyicloud bridge died ({}), wird respawned beim nächsten Call", msg);
                *guard = None;
            }
            Err(e)
        }
    }
}

// ── Cache layer ──────────────────────────────────────────────────────────
//
// Reads come from this in-memory cache; the chat-tool dispatch never waits
// for Apple. Writes go through the bridge AND mutate the cache on success
// so subsequent reads reflect the change immediately. A background task
// refreshes every REFRESH_INTERVAL to pick up changes from other devices
// (e.g. Sophie's phone).

const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Default)]
struct Cache {
    /// All visible reminder lists from the most recent successful refresh.
    lists: Vec<ReminderList>,
    /// list_id → all reminders in that list (open + completed; the read
    /// path applies the only_open filter).
    by_list: HashMap<String, Vec<Reminder>>,
    last_refresh: Option<Instant>,
}

static CACHE: OnceCell<Arc<RwLock<Cache>>> = OnceCell::const_new();

async fn cache() -> Arc<RwLock<Cache>> {
    CACHE
        .get_or_init(|| async { Arc::new(RwLock::new(Cache::default())) })
        .await
        .clone()
}

/// Trigger an immediate cache refresh from outside this module (e.g. after
/// a successful re-auth). Useful so the user sees data right away instead
/// of waiting for the next 5-min background tick.
pub async fn refresh_now() -> Result<()> {
    refresh_cache().await
}

/// Replace the cache from a fresh `list_lists` + per-list fetch round.
async fn refresh_cache() -> Result<()> {
    let lists = list_lists_uncached().await?;
    let mut by_list: HashMap<String, Vec<Reminder>> = HashMap::new();
    for lst in &lists {
        match list_reminders_uncached(Some(&lst.id), false).await {
            Ok(reminders) => {
                by_list.insert(lst.id.clone(), reminders);
            }
            Err(e) => tracing::warn!("cache refresh: Liste '{}' übersprungen: {}", lst.title, e),
        }
    }
    let cache = cache().await;
    let mut guard = cache.write().await;
    guard.lists = lists;
    guard.by_list = by_list;
    guard.last_refresh = Some(Instant::now());
    Ok(())
}

/// Fire-and-forget: prewarm Apple's CloudKit indices AND populate the cache.
/// Runs in a detached task; logs progress + errors without blocking app
/// startup. Also spawns the periodic background refresh task.
pub fn spawn_prewarm(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // Skip silently if iCloud credentials aren't configured.
        let user_set = std::env::var("ICLOUD_USERNAME")
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !user_set {
            tracing::info!("pyicloud bridge prewarm: ICLOUD_USERNAME nicht gesetzt, skip");
            crate::windows::tray::set_icloud_status(&app, false);
            return;
        }

        tracing::info!("pyicloud bridge prewarm: starte …");
        let start = Instant::now();
        match refresh_cache().await {
            Ok(()) => {
                let cache = cache().await;
                let guard = cache.read().await;
                let total: usize = guard.by_list.values().map(|v| v.len()).sum();
                tracing::info!(
                    "pyicloud bridge prewarm fertig: {} Listen, {} Reminder total, {:?}",
                    guard.lists.len(),
                    total,
                    start.elapsed()
                );
                crate::windows::tray::set_icloud_status(&app, true);
            }
            Err(e) => {
                tracing::warn!(
                    "pyicloud bridge prewarm fehlgeschlagen ({}). \
                     Bei der ersten User-Abfrage wird der Fallback-Pfad versucht.",
                    e
                );
                crate::windows::tray::set_icloud_status(&app, false);
            }
        }
        // Periodic refresh to pick up changes from other devices.
        spawn_background_refresh(app);
    });
}

fn spawn_background_refresh(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_INTERVAL).await;
            tracing::debug!("pyicloud bridge: background refresh tick");
            match refresh_cache().await {
                Ok(()) => {
                    crate::windows::tray::set_icloud_status(&app, true);
                }
                Err(e) => {
                    tracing::warn!("pyicloud bridge background refresh fehlgeschlagen: {}", e);
                    crate::windows::tray::set_icloud_status(&app, false);
                }
            }
        }
    });
}

/// Helper: apply the same name/id matching the picker uses, against a
/// cached list collection.
fn match_list_id<'a>(lists: &'a [ReminderList], needle: &str) -> Option<&'a ReminderList> {
    let n = needle.trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    // Exact ID containment first (handles "List/<uuid>" and bare uuids).
    if let Some(hit) = lists.iter().find(|l| l.id.to_lowercase().contains(&n)) {
        return Some(hit);
    }
    // Then title prefix, then substring.
    if let Some(hit) = lists
        .iter()
        .find(|l| l.title.to_lowercase().starts_with(&n))
    {
        return Some(hit);
    }
    lists.iter().find(|l| l.title.to_lowercase().contains(&n))
}

// ── Public API ───────────────────────────────────────────────────────────
//
// Public functions read from / mutate the cache. Bridge bypass is via the
// `*_uncached` helpers below.

/// List reminders, served from the in-memory cache when possible. Falls
/// back to a direct bridge call if the cache is empty (e.g. prewarm still
/// in flight or failed). Applies `list_filter` + `only_open` client-side.
pub async fn list_reminders(
    list_filter: Option<&str>,
    only_open: bool,
) -> Result<Vec<Reminder>> {
    let cache = cache().await;
    let guard = cache.read().await;
    if guard.last_refresh.is_some() {
        let target_id: Option<String> = match list_filter {
            Some(f) if !f.trim().is_empty() => {
                match match_list_id(&guard.lists, f) {
                    Some(l) => Some(l.id.clone()),
                    None => {
                        return Err(anyhow!(
                            "Keine Reminder-Liste matcht '{}' (Cache hat: {:?})",
                            f,
                            guard.lists.iter().map(|l| &l.title).collect::<Vec<_>>()
                        ));
                    }
                }
            }
            _ => None,
        };

        let mut out: Vec<Reminder> = Vec::new();
        for (list_id, reminders) in guard.by_list.iter() {
            if let Some(tid) = &target_id {
                if list_id != tid {
                    continue;
                }
            }
            for r in reminders {
                if only_open && r.completed {
                    continue;
                }
                out.push(r.clone());
            }
        }
        // Open first, then by due (None last), then by title.
        out.sort_by(|a, b| {
            a.completed
                .cmp(&b.completed)
                .then_with(|| match (&a.due, &b.due) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.title.cmp(&b.title))
        });
        return Ok(out);
    }
    drop(guard);
    tracing::info!("list_reminders: Cache leer → direkter Bridge-Call");
    list_reminders_uncached(list_filter, only_open).await
}

pub async fn list_lists() -> Result<Vec<ReminderList>> {
    let cache = cache().await;
    let guard = cache.read().await;
    if guard.last_refresh.is_some() {
        return Ok(guard.lists.clone());
    }
    drop(guard);
    list_lists_uncached().await
}

/// Create a reminder via the bridge AND push it into the cache on success
/// so subsequent reads in the same session see it immediately (no need to
/// wait for the next background refresh).
pub async fn create_reminder(
    title: &str,
    due_iso: Option<&str>,
    list_filter: Option<&str>,
    notes: Option<&str>,
) -> Result<CreatedReminder> {
    let args = serde_json::json!({
        "title": title,
        "list": list_filter.unwrap_or(""),
        "due_iso": due_iso.unwrap_or(""),
        "notes": notes.unwrap_or(""),
    });
    let v = call("create_reminder", args).await?;
    let res: CreatedReminder =
        serde_json::from_value(v).context("decode create_reminder")?;

    // Mirror into cache. We don't have all CloudKit fields back from the
    // bridge (just id + list title), so reconstruct what we can — the
    // background refresh will overwrite with the canonical version soon.
    let cache = cache().await;
    let mut guard = cache.write().await;
    let list_id_opt = guard
        .lists
        .iter()
        .find(|l| l.title == res.list)
        .map(|l| l.id.clone());
    if let Some(list_id) = list_id_opt {
        let new = Reminder {
            id: res.id.clone(),
            title: title.to_string(),
            due: due_iso.filter(|s| !s.is_empty()).map(|s| s.to_string()),
            completed: false,
            notes: notes.filter(|s| !s.is_empty()).map(|s| s.to_string()),
            list: res.list.clone(),
        };
        guard
            .by_list
            .entry(list_id)
            .or_insert_with(Vec::new)
            .insert(0, new);
    }
    Ok(res)
}

pub async fn complete_reminder(id: &str) -> Result<()> {
    let _ = call("complete_reminder", serde_json::json!({"id": id})).await?;
    let cache = cache().await;
    let mut guard = cache.write().await;
    for reminders in guard.by_list.values_mut() {
        for r in reminders.iter_mut() {
            if r.id == id {
                r.completed = true;
            }
        }
    }
    Ok(())
}

pub async fn delete_reminder(id: &str) -> Result<()> {
    let _ = call("delete_reminder", serde_json::json!({"id": id})).await?;
    let cache = cache().await;
    let mut guard = cache.write().await;
    for reminders in guard.by_list.values_mut() {
        reminders.retain(|r| r.id != id);
    }
    Ok(())
}

/// One-shot contacts dump. The Rust side has its own cache; we don't
/// expose a refresh param. pyicloud's `contacts.all` always re-fetches.
pub async fn list_contacts() -> Result<Vec<crate::icloud::contacts::Contact>> {
    let v = call("list_contacts", serde_json::json!({})).await?;
    Ok(serde_json::from_value(v).context("decode list_contacts")?)
}

// ── Uncached bridge calls (used by cache refresher + cache-miss fallback) ─

async fn list_lists_uncached() -> Result<Vec<ReminderList>> {
    let v = call("list_lists", serde_json::json!({})).await?;
    Ok(serde_json::from_value(v).context("decode list_lists")?)
}

async fn list_reminders_uncached(
    list_filter: Option<&str>,
    only_open: bool,
) -> Result<Vec<Reminder>> {
    let args = serde_json::json!({
        "list": list_filter.unwrap_or(""),
        "only_open": only_open,
    });
    let v = call("list_reminders", args).await?;
    Ok(serde_json::from_value(v).context("decode list_reminders")?)
}
