use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

use crate::api::anthropic::{self, DEFAULT_MODEL};
use crate::context;
use crate::events::{ChatToken, SpriteRect};
use crate::icloud;
use crate::platform;
use crate::storage::history::StoredMessage;
use crate::storage::secrets;
use crate::windows::cat::SpriteHitbox;
use crate::AppState;

use chrono::{DateTime, Utc};

const MODEL_SETTING_KEY: &str = "anthropic.model";

#[tauri::command]
pub fn set_sprite_rect(rect: SpriteRect, hitbox: State<'_, Arc<SpriteHitbox>>) {
    hitbox.set(&rect);
}

/// Returns the X coordinate (in window-local CSS pixels) where the cat should
/// sleep — centered above the taskbar clock. None if the clock can't be
/// located (rare; fall back to a static position in JS).
#[tauri::command]
pub fn get_sleep_position_x(window: WebviewWindow) -> Option<i32> {
    let clock = platform::taskbar_clock_rect()?;
    let win_pos = window.outer_position().ok()?;
    let scale = window.scale_factor().unwrap_or(1.0);
    let center_screen_x = (clock.left + clock.right) / 2;
    // Convert from physical-screen px to window-local CSS px.
    let local_physical = center_screen_x - win_pos.x;
    let local_css = (local_physical as f64 / scale).round() as i32;
    Some(local_css)
}

/// Seconds since the user's last keyboard or mouse input, system-wide.
#[tauri::command]
pub fn get_idle_seconds() -> u32 {
    platform::idle_duration_secs()
}

// ── Tool dispatcher ───────────────────────────────────────────────────────
//
// Called by the Anthropic loop whenever the model invokes a client tool.
// Returns a JSON string (or JSON-encoded error) that gets sent back as the
// tool_result content.

async fn dispatch_tool(name: String, input: serde_json::Value) -> anyhow::Result<String> {
    match name.as_str() {
        "get_calendar_events" => {
            let start = input
                .get("start_iso")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("start_iso fehlt"))?;
            let end = input
                .get("end_iso")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("end_iso fehlt"))?;
            let start_dt: DateTime<Utc> = DateTime::parse_from_rfc3339(start)
                .map_err(|e| anyhow::anyhow!("start_iso: {}", e))?
                .with_timezone(&Utc);
            let end_dt: DateTime<Utc> = DateTime::parse_from_rfc3339(end)
                .map_err(|e| anyhow::anyhow!("end_iso: {}", e))?
                .with_timezone(&Utc);
            match icloud::list_all_events(start_dt, end_dt).await {
                Ok(events) => Ok(serde_json::to_string(&events).unwrap_or_else(|_| "[]".into())),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "create_calendar_event" => {
            let title = input
                .get("title")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("title fehlt"))?;
            let start = input
                .get("start_iso")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("start_iso fehlt"))?;
            let end = input
                .get("end_iso")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("end_iso fehlt"))?;
            let calendar = input.get("calendar").and_then(|v| v.as_str());
            let location = input.get("location").and_then(|v| v.as_str());
            let notes = input.get("notes").and_then(|v| v.as_str());

            let start_dt: DateTime<Utc> = DateTime::parse_from_rfc3339(start)
                .map_err(|e| anyhow::anyhow!("start_iso: {}", e))?
                .with_timezone(&Utc);
            let end_dt: DateTime<Utc> = DateTime::parse_from_rfc3339(end)
                .map_err(|e| anyhow::anyhow!("end_iso: {}", e))?
                .with_timezone(&Utc);

            match icloud::caldav::create_event(
                title, start_dt, end_dt, calendar, location, notes,
            )
            .await
            {
                Ok(msg) => Ok(serde_json::json!({"success": true, "message": msg}).to_string()),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        // Reminders/VTODO operations go through the pyicloud bridge — not
        // CalDAV — because Apple's modern Reminders live in CloudKit and
        // are invisible to CalDAV. See src/icloud/bridge.rs for details.
        "get_reminders" => {
            let list = input.get("list").and_then(|v| v.as_str());
            let only_open = input
                .get("only_open")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            match icloud::bridge::list_reminders(list, only_open).await {
                Ok(reminders) => {
                    Ok(serde_json::to_string(&reminders).unwrap_or_else(|_| "[]".into()))
                }
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "create_reminder" => {
            let title = input
                .get("title")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("title fehlt"))?;
            let list = input.get("list").and_then(|v| v.as_str());
            let notes = input.get("notes").and_then(|v| v.as_str());
            let due_iso = input
                .get("due_iso")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            match icloud::bridge::create_reminder(title, due_iso, list, notes).await {
                Ok(res) => Ok(serde_json::to_string(&res).unwrap_or_else(|_| "{}".into())),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "complete_reminder" => {
            let id = input
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("id fehlt"))?;
            match icloud::bridge::complete_reminder(id).await {
                Ok(()) => Ok(serde_json::json!({"success": true}).to_string()),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "delete_reminder" => {
            let id = input
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("id fehlt"))?;
            match icloud::bridge::delete_reminder(id).await {
                Ok(()) => Ok(serde_json::json!({"success": true}).to_string()),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        // Mail (IMAP, sync) — wrap in spawn_blocking so the network round-trip
        // doesn't park the tokio runtime.
        "get_unread_emails" => {
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(5)
                .min(50);
            let result = tokio::task::spawn_blocking(move || crate::mail::client::fetch_unread(limit))
                .await
                .map_err(|e| anyhow::anyhow!("mail join: {}", e))?;
            match result {
                Ok(mails) => Ok(serde_json::to_string(&mails).unwrap_or_else(|_| "[]".into())),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "get_recent_emails" => {
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(10)
                .min(50);
            let result = tokio::task::spawn_blocking(move || crate::mail::client::fetch_recent(limit))
                .await
                .map_err(|e| anyhow::anyhow!("mail join: {}", e))?;
            match result {
                Ok(mails) => Ok(serde_json::to_string(&mails).unwrap_or_else(|_| "[]".into())),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "find_contact" => {
            let query = input
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(5)
                .min(20);
            match icloud::contacts::find(query, limit).await {
                Ok(list) => Ok(serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "upcoming_birthdays" => {
            let days = input
                .get("days")
                .and_then(|v| v.as_i64())
                .unwrap_or(30)
                .clamp(1, 365);
            match icloud::contacts::upcoming_birthdays(days).await {
                Ok(list) => Ok(serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        "send_email" => {
            let to = input
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("to fehlt"))?;
            let subject = input
                .get("subject")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("subject fehlt"))?;
            let body = input
                .get("body")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("body fehlt"))?;
            match crate::mail::smtp::send_email(to, subject, body).await {
                Ok(()) => Ok(serde_json::json!({
                    "success": true,
                    "message": format!("Mail an {} verschickt", to)
                })
                .to_string()),
                Err(err) => Ok(serde_json::json!({"error": err.to_string()}).to_string()),
            }
        }
        other => Ok(format!("{{\"error\":\"Unknown tool: {}\"}}", other)),
    }
}

/// Returns the runtime context block that gets injected into Claude's
/// system prompt — used for debugging "is my .env actually loaded?".
#[tauri::command]
pub fn get_runtime_context() -> String {
    context::user_context()
}

/// Diagnostic: returns the iCloud calendar URLs discovered via CalDAV
/// plus the count of external iCal feeds configured. Helpful when shared
/// calendars don't show up.
#[tauri::command]
pub async fn get_calendar_sources() -> Result<String, String> {
    let mut out = String::new();

    let extra = std::env::var("EXTRA_ICAL_URLS").unwrap_or_default();
    let extra_count = extra
        .split(|c: char| c == ',' || c == '\n' || c == ';')
        .filter(|s| !s.trim().is_empty())
        .count();
    out.push_str(&format!("Externe iCal-Feeds: {}\n", extra_count));

    if icloud::credentials().is_none() {
        out.push_str("CalDAV: ICLOUD_USERNAME/ICLOUD_APP_PASSWORD nicht gesetzt");
        return Ok(out);
    }

    let whitelist = std::env::var("ICLOUD_CALENDARS").unwrap_or_default();
    if !whitelist.trim().is_empty() {
        out.push_str(&format!("Whitelist aktiv: {}\n", whitelist.trim()));
    }
    match icloud::caldav::diagnose_calendars().await {
        Ok(cals) => {
            out.push_str(&format!("CalDAV-Kalender ({}):\n", cals.len()));
            for (name, url) in cals {
                out.push_str("  • ");
                out.push_str(&name);
                out.push_str(" — ");
                out.push_str(&url);
                out.push('\n');
            }
        }
        Err(e) => out.push_str(&format!("CalDAV-Fehler: {}", e)),
    }
    Ok(out)
}

#[tauri::command]
pub async fn list_messages(state: State<'_, AppState>) -> Result<Vec<StoredMessage>, String> {
    let history = state.history.lock().await;
    history.list(200).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clear_history(state: State<'_, AppState>) -> Result<(), String> {
    let mut history = state.history.lock().await;
    history.clear().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_api_key_status() -> bool {
    secrets::get_api_key().is_some()
}

/// Show + focus the Settings window. Created lazily — it's `visible:false`
/// in tauri.conf so we just flip it on, raise, and focus.
#[tauri::command]
pub fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("settings") {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
        win.unminimize().ok();
        return Ok(());
    }
    Err("settings window not registered".into())
}

// ── iCloud re-auth (pyicloud trust-cookie refresh) ───────────────────────
//
// The Apple Trust cookie that pyicloud stores expires every ~30 days. When
// the bridge logs `needs_reauth`, the user opens Settings → "iCloud neu
// verbinden", types their Apple-ID password, gets a 2FA prompt on a
// trusted device, types the code. These two commands drive that flow.

#[tauri::command]
pub async fn icloud_auth_login(password: String) -> Result<serde_json::Value, String> {
    crate::icloud::reauth::login(&password)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn icloud_auth_submit_2fa(
    app: AppHandle,
    code: String,
) -> Result<serde_json::Value, String> {
    let res = crate::icloud::reauth::submit_2fa(&code)
        .await
        .map_err(|e| e.to_string())?;
    if res.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        crate::windows::tray::set_icloud_status(&app, true);
        // Refresh the reminder cache so the cat sees data immediately.
        tauri::async_runtime::spawn(async move {
            if let Err(e) = crate::icloud::bridge::refresh_now().await {
                tracing::warn!("post-reauth refresh failed: {}", e);
            }
        });
    }
    Ok(res)
}

#[tauri::command]
pub fn icloud_auth_username() -> String {
    std::env::var("ICLOUD_USERNAME").unwrap_or_default()
}

#[derive(Serialize)]
pub struct ModelInfo {
    pub model: String,
}

#[tauri::command]
pub async fn get_model(state: State<'_, AppState>) -> Result<ModelInfo, String> {
    let history = state.history.lock().await;
    let model = history
        .get_setting(MODEL_SETTING_KEY)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    Ok(ModelInfo { model })
}

#[tauri::command]
pub async fn set_model(model: String, state: State<'_, AppState>) -> Result<(), String> {
    let history = state.history.lock().await;
    history
        .set_setting(MODEL_SETTING_KEY, model.trim())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    text: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err("empty message".into());
    }

    let api_key = match secrets::get_api_key() {
        Some(k) => k,
        None => return Err("api-key-missing".into()),
    };

    let model = {
        let history = state.history.lock().await;
        let model = history
            .get_setting(MODEL_SETTING_KEY)
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        history.append("user", &trimmed).map_err(|e| e.to_string())?;
        model
    };

    // Stitch the static persona prompt together with the live context block
    // (date/time, location) every send — so the model always has fresh info
    // for queries like "wie wird das Wetter heute".
    let system_prompt = format!(
        "{}\n\n## Kontext\n{}",
        anthropic::SYSTEM_PROMPT_BASE,
        context::user_context()
    );

    let emit_app = app.clone();
    let model_str = model.clone();
    let result = anthropic::send_completion(
        &api_key,
        &model_str,
        &system_prompt,
        &trimmed,
        |name, input| dispatch_tool(name, input),
        |delta| {
            let _ = emit_app.emit_to("cat", "chat://token", ChatToken::Delta { text: delta });
        },
    )
    .await;

    match result {
        Ok(full) => {
            let history = state.history.lock().await;
            history
                .append("assistant", &full)
                .map_err(|e| e.to_string())?;
            let _ = app.emit_to("cat", "chat://token", ChatToken::Done);
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = app.emit_to(
                "cat",
                "chat://token",
                ChatToken::Error {
                    message: msg.clone(),
                },
            );
            Err(msg)
        }
    }
}
