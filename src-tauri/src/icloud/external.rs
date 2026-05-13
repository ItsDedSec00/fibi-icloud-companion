//! External iCal feed reader. Used for calendars subscribed via URL in
//! the user's iCloud Calendar app (typical case: Google Calendar shared as
//! `https://calendar.google.com/calendar/ical/<id>/.../basic.ics`). These
//! events are NOT mirrored to iCloud's CalDAV servers — they live only on
//! the client device — so we have to fetch the original URL ourselves.
//!
//! The list of URLs is read from the `EXTRA_ICAL_URLS` env var (comma- or
//! newline-separated). `webcal://` is rewritten to `https://`.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use icalendar::{Calendar, CalendarComponent};

use super::caldav::{parse_event, CalendarEvent};

pub async fn list_events(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<CalendarEvent>> {
    let raw = std::env::var("EXTRA_ICAL_URLS").unwrap_or_default();
    let urls: Vec<String> = raw
        .split(|c: char| c == ',' || c == '\n' || c == ';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.replace("webcal://", "https://")
                .replace("webcals://", "https://")
        })
        .collect();
    if urls.is_empty() {
        return Ok(Vec::new());
    }

    let client = reqwest::Client::builder()
        .user_agent("Companion/0.1 (iCal subscription reader)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut events = Vec::new();
    for url in urls {
        match fetch_one(&client, &url, from, to).await {
            Ok(mut e) => events.append(&mut e),
            Err(err) => tracing::warn!("iCal-Feed {} fehlgeschlagen: {}", url, err),
        }
    }
    Ok(events)
}

async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>> {
    let response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("HTTP {}", status));
    }
    let body = response.text().await?;
    let calendar: Calendar = body
        .parse()
        .map_err(|e| anyhow!("iCal parse: {}", e))?;

    let mut events = Vec::new();
    for component in calendar.components.iter() {
        if let CalendarComponent::Event(ev) = component {
            if let Some(parsed) = parse_event(ev, from, to) {
                events.push(parsed);
            }
        }
    }
    Ok(events)
}
