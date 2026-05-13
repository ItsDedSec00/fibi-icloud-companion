//! Background loop that pushes proactive calendar reminders to the cat UI.
//!
//! Three triggers, all derived from the user's iCloud + external iCal feeds:
//!
//!   1. **1-hour-vorher** — for each event starting in roughly an hour,
//!      one push with the title.
//!   2. **09:00 Morgen-Briefing** — once per day, an overview of today.
//!   3. **12:00 Mittag-Update** — once per day, an overview of what's
//!      still ahead this afternoon.
//!
//! Tick is one minute. We refresh the calendar cache lazily through
//! `icloud::list_all_events` and keep an in-memory set of "already-pushed"
//! event keys so a single reminder doesn't fire repeatedly within a tick
//! window. The dedup state lives only for the process lifetime — restarting
//! companion may re-emit a reminder once, which is harmless.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike, Utc};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::icloud;
use crate::icloud::caldav::CalendarEvent;

#[derive(Debug, Clone, Serialize)]
pub struct ReminderPayload {
    pub kind: &'static str,
    pub text: String,
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        run_loop(app).await;
    });
}

async fn run_loop(app: AppHandle) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    let mut notified_events: HashSet<String> = HashSet::new();
    let mut last_morning: Option<NaiveDate> = None;
    let mut last_noon: Option<NaiveDate> = None;

    loop {
        ticker.tick().await;
        let now_local = Local::now();
        let now_utc = now_local.with_timezone(&Utc);

        // ── 1-hour-ahead per-event ─────────────────────────────────────
        // Use a wider [+58, +62] window so a 60-s tick can't slip past an
        // event that starts on the exact minute.
        let window_start = now_utc + chrono::Duration::minutes(58);
        let window_end = now_utc + chrono::Duration::minutes(62);
        match icloud::list_all_events(window_start, window_end).await {
            Ok(events) => {
                for ev in events {
                    let key = event_key(&ev);
                    if notified_events.contains(&key) {
                        continue;
                    }
                    let text = format!(
                        "psst~ in einer stunde: {} ({})",
                        ev.summary,
                        short_time(&ev.start),
                    );
                    push(&app, "event-1h", text);
                    notified_events.insert(key);
                }
            }
            Err(e) => tracing::warn!("Reminder-Loop iCloud-Fehler: {}", e),
        }

        // Garbage-collect notified set so it doesn't grow forever — keep
        // only keys for events whose start is still within the next 24 h.
        let one_day = now_utc + chrono::Duration::hours(24);
        notified_events.retain(|k| {
            k.split('|')
                .nth(1)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc) <= one_day)
                .unwrap_or(false)
        });

        // ── 09:00 Morgen-Briefing ──────────────────────────────────────
        // We fire any time between 09:00 and 11:30 if we haven't sent one
        // today yet, so launching the app late in the morning still gets a
        // briefing. The minute-check caps the late-launch window at 11:30.
        let today = now_local.date_naive();
        let hour = now_local.hour();
        let minute = now_local.minute();
        if (9..=11).contains(&hour) && last_morning != Some(today) {
            if hour < 11 || minute < 30 {
                let summary = build_today_summary(now_utc, /*morning=*/ true).await;
                let text = if summary.is_empty() {
                    "guten morgen, david — heute ist nüscht geplant, chill day :3".into()
                } else {
                    format!("guten morgen, david~ heute: {summary}")
                };
                push(&app, "morning", text);
                last_morning = Some(today);
            }
        }

        // ── 12:00 Mittag-Update ────────────────────────────────────────
        // Fire any time between 12:00 and 13:30 if not sent today.
        if (12..=13).contains(&hour) && last_noon != Some(today) {
            if hour < 13 || minute < 30 {
                let summary = build_today_summary(now_utc, /*morning=*/ false).await;
                let text = if summary.is_empty() {
                    "mittag~ nichts mehr heute, gönn dir was".into()
                } else {
                    format!("mittag~ rest vom tag: {summary}")
                };
                push(&app, "noon", text);
                last_noon = Some(today);
            }
        }
    }
}

fn event_key(ev: &CalendarEvent) -> String {
    format!("{}|{}", ev.summary, ev.start)
}

/// Build a comma-separated `HH:MM Title` list of today's events.
/// `morning=false` filters out events that already started.
async fn build_today_summary(now_utc: chrono::DateTime<Utc>, morning: bool) -> String {
    let today = Local::now().date_naive();
    let start_of_day = Local
        .from_local_datetime(&today.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now_utc);
    let end_of_day = start_of_day + chrono::Duration::days(1);

    let Ok(events) = icloud::list_all_events(start_of_day, end_of_day).await else {
        return String::new();
    };

    events
        .into_iter()
        .filter(|ev| {
            if morning {
                true
            } else {
                // For the noon digest skip anything that's already happened.
                chrono::DateTime::parse_from_rfc3339(&ev.start)
                    .map(|dt| dt.with_timezone(&Utc) >= now_utc)
                    .unwrap_or(true)
            }
        })
        .take(6)
        .map(|ev| format!("{} {}", short_time(&ev.start), ev.summary))
        .collect::<Vec<_>>()
        .join(", ")
}

fn short_time(iso: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&Local).format("%H:%M").to_string();
    }
    // All-day events are stored as plain YYYY-MM-DD without time.
    if iso.len() == 10 {
        return "ganztägig".into();
    }
    iso.to_string()
}

fn push(app: &AppHandle, kind: &'static str, text: String) {
    let _ = app.emit_to("cat", "cat://reminder", ReminderPayload { kind, text });
}

// silence dead_code on Datelike import — needed only via trait methods used elsewhere
#[allow(dead_code)]
fn _datelike_used(d: NaiveDate) -> i32 {
    d.year()
}
