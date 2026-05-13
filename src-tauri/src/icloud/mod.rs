//! iCloud + externe iCal-Feeds. CalDAV liest aus iCloud's eigenem Storage;
//! externe Feeds (Google Calendar via iCal-URL, etc.) werden direkt von
//! ihrer Quelle gefetched, weil iCloud client-side-Subscriptions nicht auf
//! seine Server spiegelt.

pub mod bridge;
pub mod caldav;
pub mod contacts;
pub mod external;
pub mod reauth;

use anyhow::Result;
use caldav::CalendarEvent;
use chrono::{DateTime, Utc};

/// Returns Some((username, app_password)) if both `ICLOUD_USERNAME` and
/// `ICLOUD_APP_PASSWORD` are set.
pub fn credentials() -> Option<(String, String)> {
    let user = std::env::var("ICLOUD_USERNAME").ok()?;
    let pass = std::env::var("ICLOUD_APP_PASSWORD").ok()?;
    let user = user.trim();
    let pass = pass.trim();
    if user.is_empty() || pass.is_empty() {
        return None;
    }
    Some((user.to_string(), pass.to_string()))
}

/// Combined event list from iCloud CalDAV (if configured) + every iCal-URL
/// in `EXTRA_ICAL_URLS`. Failures in either source are logged but don't
/// abort the whole call — we'd rather return partial results than nothing.
pub async fn list_all_events(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>> {
    let mut all = Vec::new();

    if credentials().is_some() {
        match caldav::list_events(from, to).await {
            Ok(mut e) => all.append(&mut e),
            Err(err) => tracing::warn!("CalDAV (iCloud) fehlgeschlagen: {}", err),
        }
    }

    match external::list_events(from, to).await {
        Ok(mut e) => all.append(&mut e),
        Err(err) => tracing::warn!("Externe iCal-Feeds fehlgeschlagen: {}", err),
    }

    all.sort_by(|a, b| a.start.cmp(&b.start));
    Ok(all)
}
