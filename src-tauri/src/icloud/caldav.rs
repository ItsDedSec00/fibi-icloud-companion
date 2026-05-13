//! Minimal CalDAV client for iCloud.
//!
//! Read flow (`list_events`):
//!   1. PROPFIND `https://caldav.icloud.com/` → user's principal URL
//!   2. PROPFIND principal → calendar-home-set URL
//!   3. PROPFIND home (depth=1) → list of calendar collection URLs
//!   4. REPORT calendar-query on each calendar → iCalendar VEVENT components
//!
//! Write flow (`create_event`):
//!   1. Discover principal + home + calendar list (same as above)
//!   2. Pick target calendar (filter arg → ICLOUD_DEFAULT_WRITE_CALENDAR → first)
//!   3. Build a VEVENT via icalendar
//!   4. PUT it to `<calendar-url>/<uuid>.ics` with `If-None-Match: *`

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, Event, EventLike,
};
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;
use reqwest::Method;
use serde::Serialize;

const BASE: &str = "https://caldav.icloud.com";

#[derive(Debug, Clone, Serialize)]
pub struct CalendarEvent {
    pub summary: String,
    pub start: String,
    pub end: Option<String>,
    pub location: Option<String>,
    pub all_day: bool,
}

/// Diagnostic: returns name + URL of every calendar collection we'd query
/// (after whitelist filtering). Useful for matching displaynames to URLs
/// when configuring `ICLOUD_CALENDARS`.
pub async fn diagnose_calendars() -> Result<Vec<(String, String)>> {
    let (user, pass) = super::credentials()
        .ok_or_else(|| anyhow!("ICLOUD_USERNAME / ICLOUD_APP_PASSWORD nicht in .env gesetzt"))?;
    let client = reqwest::Client::builder()
        .user_agent("Companion/0.1 (CalDAV diagnose)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let principal = discover_principal(&client, &user, &pass).await?;
    let home = discover_calendar_home(&client, &user, &pass, &principal).await?;

    // For diagnostics we want the FULL list (no whitelist), so the user can
    // see what's available and pick names for ICLOUD_CALENDARS.
    let body = propfind(&client, &user, &pass, &home, 1, PROPFIND_CALENDARS).await?;
    let raw = parse_calendar_collections(&body);
    Ok(raw
        .into_iter()
        .map(|r| {
            let kind = match r.components {
                Some(c) if c.vevent && c.vtodo => " (Events+Reminder)",
                Some(c) if c.vtodo => " (Reminder)",
                Some(c) if c.vevent => "",
                _ => "",
            };
            let label = format!("{}{}", r.display_name.as_deref().unwrap_or("(ohne Name)"), kind);
            (label, absolute(&r.href))
        })
        .collect())
}

pub async fn list_events(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<CalendarEvent>> {
    let (user, pass) = super::credentials()
        .ok_or_else(|| anyhow!("ICLOUD_USERNAME / ICLOUD_APP_PASSWORD nicht in .env gesetzt"))?;

    let client = reqwest::Client::builder()
        .user_agent("Companion/0.1 (CalDAV)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let principal = discover_principal(&client, &user, &pass)
        .await
        .context("Apple-Principal-Auflösung fehlgeschlagen — Apple-ID korrekt? App-spezifisches Passwort?")?;
    let home = discover_calendar_home(&client, &user, &pass, &principal).await?;
    let calendars = list_calendars(&client, &user, &pass, &home).await?;

    let mut events = Vec::new();
    for cal in calendars {
        match fetch_events(&client, &user, &pass, &cal.url, from, to).await {
            Ok(mut e) => events.append(&mut e),
            Err(err) => tracing::warn!(
                "Kalender {} ({}) übersprungen: {}",
                cal.display_name.as_deref().unwrap_or("?"),
                cal.url,
                err
            ),
        }
    }
    events.sort_by(|a, b| a.start.cmp(&b.start));
    Ok(events)
}

// ── Reminders (VTODO) ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Reminder {
    pub uid: String,
    pub title: String,
    /// ISO 8601 string of DUE if set. Open reminders without a due date are
    /// returned with `due = None`.
    pub due: Option<String>,
    pub completed: bool,
    pub notes: Option<String>,
    /// Display name of the list this reminder lives in (e.g. "D&S ⚠️").
    pub list: String,
}

/// List VTODO items from the reminder lists (filtered by
/// `ICLOUD_REMINDER_LISTS` env var, plus optional `list_filter`).
/// If `only_open` is true, completed reminders are filtered out client-side.
pub async fn list_reminders(
    list_filter: Option<&str>,
    only_open: bool,
) -> Result<Vec<Reminder>> {
    let (user, pass) = super::credentials()
        .ok_or_else(|| anyhow!("ICLOUD_USERNAME / ICLOUD_APP_PASSWORD nicht in .env gesetzt"))?;
    let client = reqwest::Client::builder()
        .user_agent("Companion/0.1 (CalDAV todo)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let principal = discover_principal(&client, &user, &pass).await?;
    let home = discover_calendar_home(&client, &user, &pass, &principal).await?;
    let mut lists = list_reminder_lists(&client, &user, &pass, &home).await?;

    // Optional second-level filter (tool-call arg). Same matching rules
    // as the env-var whitelist.
    if let Some(f) = list_filter {
        let trimmed = f.trim();
        if !trimmed.is_empty() {
            let filtered: Vec<DiscoveredCalendar> = lists
                .iter()
                .filter(|c| match_quality(c, trimmed) != MatchQuality::None)
                .cloned()
                .collect();
            if !filtered.is_empty() {
                lists = filtered;
            } else {
                tracing::warn!(
                    "list_reminders: filter '{}' matched no reminder list, using all",
                    trimmed
                );
            }
        }
    }

    let mut all = Vec::new();
    for cal in lists {
        let list_label = cal
            .display_name
            .clone()
            .unwrap_or_else(|| "(ohne Name)".into());
        match fetch_todos(&client, &user, &pass, &cal.url).await {
            Ok(todos) => {
                for t in todos {
                    if only_open && t.completed {
                        continue;
                    }
                    all.push(Reminder {
                        list: list_label.clone(),
                        ..t
                    });
                }
            }
            Err(err) => tracing::warn!(
                "Reminder-Liste {} ({}) übersprungen: {}",
                list_label,
                cal.url,
                err
            ),
        }
    }
    // Open ones first, then by due (None last).
    all.sort_by(|a, b| {
        a.completed.cmp(&b.completed).then_with(|| match (&a.due, &b.due) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
    });
    Ok(all)
}

async fn fetch_todos(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    list_url: &str,
) -> Result<Vec<Reminder>> {
    // No time-range filter on VTODO — most reminders don't have a DUE.
    // We pull everything and filter client-side.
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VTODO"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;
    let response = client
        .request(Method::from_bytes(b"REPORT")?, list_url)
        .basic_auth(user, Some(pass))
        .header("Depth", "1")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!("REPORT {}: HTTP {}", list_url, status));
    }
    let mut todos = Vec::new();
    for ical_text in extract_calendar_data(&text) {
        let cal: Calendar = match ical_text.parse() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("VTODO iCal parse error: {}", e);
                continue;
            }
        };
        for component in cal.components.iter() {
            if let CalendarComponent::Todo(t) = component {
                if let Some(parsed) = parse_todo(t) {
                    todos.push(parsed);
                }
            }
        }
    }
    Ok(todos)
}

fn is_apple_migration_stub(title: &str, notes: Option<&str>) -> bool {
    // Stable signatures from the 2019 iOS-13 Reminders migration. Apple
    // injects these into every legacy list. Title in any locale; notes always
    // mention the canonical support-article URL on Apple-owned domains.
    if let Some(n) = notes {
        let lower = n.to_ascii_lowercase();
        if lower.contains("support.apple.com/ht210220") {
            return true;
        }
    }
    // Fallback for title-only matches (e.g. when notes are missing).
    let t = title.trim();
    matches!(
        t,
        "Wo sind meine Erinnerungen?"
            | "Where are my reminders?"
            | "Where Are My Reminders?"
            | "Der Ersteller dieser Liste hat diese Erinnerungen aktualisiert."
    )
}

fn parse_todo(t: &icalendar::Todo) -> Option<Reminder> {
    let uid = t.get_uid().unwrap_or("").to_string();
    let title = t.get_summary().unwrap_or("(ohne Titel)").to_string();
    let notes = t
        .property_value("DESCRIPTION")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Apple's iOS-13 Reminders-Upgrade leaves "stub" todos in legacy CalDAV
    // lists that point to a support article — pure noise for the user. Drop.
    if is_apple_migration_stub(&title, notes.as_deref()) {
        return None;
    }
    // STATUS:COMPLETED or a non-empty COMPLETED property both signal done.
    let status_completed = t
        .property_value("STATUS")
        .map(|s| s.trim().eq_ignore_ascii_case("COMPLETED"))
        .unwrap_or(false);
    let has_completed_ts = t
        .property_value("COMPLETED")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let completed = status_completed || has_completed_ts;

    // DUE is the canonical due date for VTODO. May be DATE or DATE-TIME.
    let due = t.property_value("DUE").map(|s| s.trim().to_string());

    Some(Reminder {
        uid,
        title,
        due,
        completed,
        notes,
        list: String::new(), // filled in by list_reminders
    })
}

/// Create a new VTODO in a reminder list. `list_filter` matches the same way
/// as event-create's `calendar_filter` (display-name prefix / URL substring /
/// reverse substring). Falls back to `ICLOUD_DEFAULT_WRITE_REMINDER_LIST`,
/// then the first VTODO list found.
pub async fn create_reminder(
    title: &str,
    due: Option<DateTime<Utc>>,
    list_filter: Option<&str>,
    notes: Option<&str>,
) -> Result<String> {
    use icalendar::Todo;
    let (user, pass) = super::credentials()
        .ok_or_else(|| anyhow!("ICLOUD_USERNAME / ICLOUD_APP_PASSWORD nicht in .env gesetzt"))?;
    let client = reqwest::Client::builder()
        .user_agent("Companion/0.1 (CalDAV todo write)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let principal = discover_principal(&client, &user, &pass).await?;
    let home = discover_calendar_home(&client, &user, &pass, &principal).await?;
    let lists = list_reminder_lists(&client, &user, &pass, &home).await?;
    if lists.is_empty() {
        return Err(anyhow!(
            "Keine VTODO/Reminder-Liste gefunden. Setz ICLOUD_REMINDER_LISTS oder leg in iCloud eine Erinnerungsliste an."
        ));
    }

    let target = pick_write_target(&lists, list_filter, WriteKind::Todo)
        .ok_or_else(|| anyhow!("keine passende Reminder-Liste gefunden"))?;

    let uid = uuid::Uuid::new_v4().to_string();
    let mut todo = Todo::new();
    todo.uid(&uid)
        .summary(title)
        .timestamp(Utc::now())
        .status(icalendar::TodoStatus::NeedsAction);
    if let Some(d) = due {
        // RFC5545: DUE for VTODO uses the same syntax as DTSTART.
        // icalendar's `due()` accepts a DatePerhapsTime; build one explicitly.
        todo.due(d);
    }
    if let Some(n) = notes {
        todo.description(n);
    }
    let todo = todo.done();
    let mut cal = Calendar::new();
    cal.push(todo);
    let ical_text = cal.to_string();

    let url = format!("{}{}.ics", ensure_trailing_slash(&target.url), uid);
    let resp = client
        .put(&url)
        .basic_auth(&user, Some(&pass))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .header("If-None-Match", "*")
        .body(ical_text.clone())
        .send()
        .await?;
    let status = resp.status();
    let list_label = target.display_name.as_deref().unwrap_or("Liste");
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(
            "CalDAV VTODO PUT failed → liste='{}' url='{}' status={} body='{}'",
            list_label,
            url,
            status,
            snippet(&body)
        );
        tracing::error!("CalDAV VTODO PUT body sent was:\n{}", ical_text);
        return Err(anyhow!(
            "PUT in Reminder-Liste '{}' → HTTP {} ({})",
            list_label,
            status,
            snippet(&body)
        ));
    }
    Ok(format!("\"{}\" in {} angelegt", title, list_label))
}

// ── Discovery ──────────────────────────────────────────────────────────────

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:current-user-principal/></d:prop>
</d:propfind>"#;

const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-home-set/></d:prop>
</d:propfind>"#;

const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;

async fn discover_principal(client: &reqwest::Client, user: &str, pass: &str) -> Result<String> {
    let body = propfind(client, user, pass, &format!("{BASE}/"), 0, PROPFIND_PRINCIPAL).await?;
    let href = find_nested_href(&body, "current-user-principal")
        .ok_or_else(|| anyhow!("current-user-principal nicht in Antwort"))?;
    Ok(absolute(&href))
}

async fn discover_calendar_home(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    principal: &str,
) -> Result<String> {
    let body = propfind(client, user, pass, principal, 0, PROPFIND_HOME).await?;
    let href = find_nested_href(&body, "calendar-home-set")
        .ok_or_else(|| anyhow!("calendar-home-set nicht in Antwort"))?;
    Ok(absolute(&href))
}

/// What component types a calendar collection accepts. iCloud splits things
/// strictly: a calendar advertises either `VEVENT` or `VTODO` (never both),
/// so the picker filters writes by the component being created.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ComponentSupport {
    pub vevent: bool,
    pub vtodo: bool,
}

/// One calendar collection from CalDAV's PROPFIND on the home-set.
#[derive(Debug, Clone)]
pub(super) struct DiscoveredCalendar {
    pub url: String,
    pub display_name: Option<String>,
    /// `None` when the server didn't advertise `supported-calendar-component-set`;
    /// `Some(flags)` when at least one `<comp/>` was found. Defaults to "supports
    /// VEVENT" for back-compat when querying older / non-iCloud servers.
    pub components: Option<ComponentSupport>,
}

impl DiscoveredCalendar {
    pub fn supports_vevent(&self) -> bool {
        self.components.map(|c| c.vevent).unwrap_or(true)
    }
    pub fn supports_vtodo(&self) -> bool {
        self.components.map(|c| c.vtodo).unwrap_or(false)
    }
}

async fn discover_calendars_raw(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    home: &str,
) -> Result<Vec<DiscoveredCalendar>> {
    let body = propfind(client, user, pass, home, 1, PROPFIND_CALENDARS).await?;
    let calendars: Vec<DiscoveredCalendar> = parse_calendar_collections(&body)
        .into_iter()
        .map(|raw| DiscoveredCalendar {
            url: absolute(&raw.href),
            display_name: raw.display_name,
            components: raw.components,
        })
        .collect();
    if calendars.is_empty() {
        return Err(anyhow!("keine Kalender unter {home}"));
    }
    Ok(calendars)
}

/// Discover + apply ICLOUD_CALENDARS whitelist. Used by event flows.
async fn list_calendars(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    home: &str,
) -> Result<Vec<DiscoveredCalendar>> {
    let calendars = discover_calendars_raw(client, user, pass, home).await?;
    Ok(apply_whitelist(calendars, "ICLOUD_CALENDARS"))
}

/// Discover + apply ICLOUD_REMINDER_LISTS whitelist + only keep VTODO lists.
/// Used by reminder flows.
async fn list_reminder_lists(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    home: &str,
) -> Result<Vec<DiscoveredCalendar>> {
    let raw = discover_calendars_raw(client, user, pass, home).await?;
    // Only the VTODO-capable collections are reminder lists. We do this
    // BEFORE applying the whitelist so the whitelist filter doesn't have to
    // also exclude event calendars.
    let vtodo_only: Vec<DiscoveredCalendar> =
        raw.into_iter().filter(|c| c.supports_vtodo()).collect();
    Ok(apply_whitelist(vtodo_only, "ICLOUD_REMINDER_LISTS"))
}

/// Apply a comma-separated env-var whitelist to a calendar list. Empty/unset
/// env = pass-through. Empty filter result = pass-through with a warning so
/// the user doesn't get a silently empty list.
fn apply_whitelist(
    mut calendars: Vec<DiscoveredCalendar>,
    env_var: &str,
) -> Vec<DiscoveredCalendar> {
    let Some(whitelist) = parse_whitelist(env_var) else {
        return calendars;
    };
    for w in &whitelist {
        let matches: Vec<&str> = calendars
            .iter()
            .filter(|c| match_quality(c, w) != MatchQuality::None)
            .map(|c| c.display_name.as_deref().unwrap_or("(ohne Name)"))
            .collect();
        if matches.is_empty() {
            tracing::warn!(
                "{}: Eintrag '{}' (len={}) → KEIN Treffer. Prüf auf Tippfehler / falsche Länge der UUID.",
                env_var,
                w,
                w.chars().count()
            );
        } else {
            tracing::info!(
                "{}: Eintrag '{}' (len={}) → {:?}",
                env_var,
                w,
                w.chars().count(),
                matches
            );
        }
    }
    let filtered: Vec<DiscoveredCalendar> = calendars
        .iter()
        .filter(|c| {
            whitelist
                .iter()
                .any(|w| match_quality(c, w) != MatchQuality::None)
        })
        .cloned()
        .collect();
    if !filtered.is_empty() {
        calendars = filtered;
    } else {
        tracing::warn!(
            "{} Filter ergibt 0 Treffer — verwende alle Kalender",
            env_var
        );
    }
    calendars
}

fn parse_whitelist(env_var: &str) -> Option<Vec<String>> {
    let raw = std::env::var(env_var).ok()?;
    let list: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

// ── Event query (calendar-query REPORT) ───────────────────────────────────

async fn fetch_events(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    calendar_url: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{}" end="{}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#,
        from.format("%Y%m%dT%H%M%SZ"),
        to.format("%Y%m%dT%H%M%SZ"),
    );

    let response = client
        .request(Method::from_bytes(b"REPORT")?, calendar_url)
        .basic_auth(user, Some(pass))
        .header("Depth", "1")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!("REPORT {}: HTTP {}", calendar_url, status));
    }

    let mut events = Vec::new();
    for ical_text in extract_calendar_data(&text) {
        let cal: Calendar = match ical_text.parse() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("iCal parse error: {}", e);
                continue;
            }
        };
        for component in cal.components.iter() {
            if let CalendarComponent::Event(ev) = component {
                if let Some(parsed) = parse_event(ev, from, to) {
                    events.push(parsed);
                }
            }
        }
    }
    Ok(events)
}

pub(super) fn parse_event(
    ev: &icalendar::Event,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<CalendarEvent> {
    let summary = ev.get_summary().unwrap_or("(ohne Titel)").to_string();
    // icalendar 0.16 doesn't expose a `get_location` shortcut — pull it
    // out of the raw properties instead.
    let location = ev
        .property_value("LOCATION")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (start_str, start_dt, all_day) = ev.get_start().and_then(format_date)?;
    let end_tuple = ev.get_end().and_then(format_date);
    let end_str = end_tuple.as_ref().map(|(s, _, _)| s.clone());
    let end_dt = end_tuple.as_ref().map(|(_, d, _)| *d);

    let event_end_dt = end_dt.unwrap_or(start_dt);
    if event_end_dt < from || start_dt > to {
        return None;
    }

    Some(CalendarEvent {
        summary,
        start: start_str,
        end: end_str,
        location,
        all_day,
    })
}

fn format_date(dpt: DatePerhapsTime) -> Option<(String, DateTime<Utc>, bool)> {
    match dpt {
        DatePerhapsTime::Date(d) => {
            let s = d.format("%Y-%m-%d").to_string();
            // All-day event — anchor it to local midnight so the reminder
            // window comparisons sit in the right calendar day, then convert
            // to UTC for storage.
            let naive = NaiveDate::from_ymd_opt(d.year(), d.month(), d.day())?
                .and_hms_opt(0, 0, 0)?;
            let utc = naive_local_to_utc(naive);
            Some((s, utc, true))
        }
        DatePerhapsTime::DateTime(cal_dt) => match cal_dt {
            CalendarDateTime::Utc(dt) => Some((dt.to_rfc3339(), dt, false)),
            CalendarDateTime::Floating(naive) => {
                // "Floating" = wall-clock time, no timezone. RFC5545 says to
                // interpret in the viewer's local TZ — exactly what we want.
                let utc = naive_local_to_utc(naive);
                let display = utc
                    .with_timezone(&Local)
                    .format("%Y-%m-%dT%H:%M:%S%:z")
                    .to_string();
                Some((display, utc, false))
            }
            CalendarDateTime::WithTimezone { date_time, tzid } => {
                let utc = naive_in_tz_to_utc(date_time, &tzid);
                let display = utc
                    .with_timezone(&Local)
                    .format("%Y-%m-%dT%H:%M:%S%:z")
                    .to_string();
                Some((display, utc, false))
            }
        },
    }
}

/// Treat a naive datetime as belonging to the named IANA timezone and
/// convert to UTC. Falls back to local-time interpretation if the TZID
/// can't be parsed (rare, e.g. for proprietary Outlook tzids).
fn naive_in_tz_to_utc(naive: NaiveDateTime, tzid: &str) -> DateTime<Utc> {
    if let Ok(tz) = tzid.parse::<chrono_tz::Tz>() {
        if let chrono::LocalResult::Single(dt) = tz.from_local_datetime(&naive) {
            return dt.with_timezone(&Utc);
        }
    }
    naive_local_to_utc(naive)
}

/// Treat a naive datetime as belonging to the system local timezone and
/// convert to UTC.
fn naive_local_to_utc(naive: NaiveDateTime) -> DateTime<Utc> {
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

// ── HTTP ──────────────────────────────────────────────────────────────────

async fn propfind(
    client: &reqwest::Client,
    user: &str,
    pass: &str,
    url: &str,
    depth: u32,
    body: &str,
) -> Result<String> {
    let response = client
        .request(Method::from_bytes(b"PROPFIND")?, url)
        .basic_auth(user, Some(pass))
        .header("Depth", depth.to_string())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "PROPFIND {}: HTTP {} — {}",
            url,
            status,
            snippet(&text)
        ));
    }
    Ok(text)
}

fn snippet(s: &str) -> String {
    s.chars().take(200).collect()
}

fn absolute(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else {
        format!("{BASE}{href}")
    }
}

// ── XML scanning ──────────────────────────────────────────────────────────

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

/// Returns true if a resourcetype child element signals that we should treat
/// the parent collection as a calendar — covers the standard `<calendar/>`
/// plus Apple's CalendarServer extensions for shared and delegated access.
fn is_calendar_marker(ln: &[u8]) -> bool {
    matches!(
        ln,
        b"calendar"
            | b"shared"
            | b"shared-by-me"
            | b"shared-with-me"
            | b"calendar-proxy-read"
            | b"calendar-proxy-write"
            | b"calendar-proxy-read-for"
            | b"calendar-proxy-write-for"
            | b"subscribed"
    )
}

/// Find the first `<href>` text content nested anywhere inside an element
/// whose local name matches `parent`.
fn find_nested_href(xml: &str, parent: &str) -> Option<String> {
    let parent = parent.as_bytes();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_parent_depth: Option<usize> = None;
    let mut capture_depth: Option<usize> = None;
    let mut text = String::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) | Ok(XmlEvent::Eof) => break,
            Ok(XmlEvent::Start(e)) => {
                depth += 1;
                let name = e.name();
                let ln = local_name(name.as_ref());
                if in_parent_depth.is_none() && ln == parent {
                    in_parent_depth = Some(depth);
                } else if in_parent_depth.is_some() && capture_depth.is_none() && ln == b"href" {
                    capture_depth = Some(depth);
                    text.clear();
                }
            }
            Ok(XmlEvent::End(_)) => {
                if let Some(d) = capture_depth {
                    if depth == d {
                        return Some(text.trim().to_string());
                    }
                }
                if let Some(d) = in_parent_depth {
                    if depth == d {
                        in_parent_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(XmlEvent::Text(t)) if capture_depth.is_some() => {
                if let Ok(s) = t.unescape() {
                    text.push_str(&s);
                }
            }
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Walk a PROPFIND multistatus body and return `(href, displayname)` for
/// Raw fields from one `<response>` in the home-set PROPFIND.
pub(super) struct RawCalendar {
    pub href: String,
    pub display_name: Option<String>,
    pub components: Option<ComponentSupport>,
}

/// each `<response>` whose `<resourcetype>` marks it as a calendar
/// (standard `<calendar/>` plus Apple's shared / proxy variants).
fn parse_calendar_collections(xml: &str) -> Vec<RawCalendar> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut calendars = Vec::new();
    let mut depth = 0usize;

    let mut response_depth: Option<usize> = None;
    let mut response_href: Option<String> = None;
    let mut response_display: Option<String> = None;
    let mut response_is_calendar = false;
    let mut response_components: Option<ComponentSupport> = None;

    let mut resourcetype_depth: Option<usize> = None;
    let mut href_depth: Option<usize> = None;
    let mut href_text = String::new();
    let mut display_depth: Option<usize> = None;
    let mut display_text = String::new();
    let mut comp_set_depth: Option<usize> = None;

    // Helper: when inside <supported-calendar-component-set>, inspect a
    // `<comp name="..."/>` element and turn on the matching flag.
    let inspect_comp = |attrs: quick_xml::events::attributes::Attributes,
                        comps: &mut Option<ComponentSupport>| {
        for a in attrs.flatten() {
            if a.key.local_name().as_ref() == b"name" {
                if let Ok(v) = std::str::from_utf8(&a.value) {
                    let c = comps.get_or_insert(ComponentSupport::default());
                    if v.eq_ignore_ascii_case("VEVENT") {
                        c.vevent = true;
                    } else if v.eq_ignore_ascii_case("VTODO") {
                        c.vtodo = true;
                    }
                }
            }
        }
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) | Ok(XmlEvent::Eof) => break,
            Ok(XmlEvent::Start(e)) => {
                depth += 1;
                let name = e.name();
                let ln = local_name(name.as_ref());
                if response_depth.is_none() && ln == b"response" {
                    response_depth = Some(depth);
                    response_href = None;
                    response_display = None;
                    response_is_calendar = false;
                    response_components = None;
                } else if response_depth.is_some() {
                    if ln == b"resourcetype" && resourcetype_depth.is_none() {
                        resourcetype_depth = Some(depth);
                    } else if ln == b"href" && href_depth.is_none() && response_href.is_none() {
                        href_depth = Some(depth);
                        href_text.clear();
                    } else if ln == b"displayname" && display_depth.is_none() {
                        display_depth = Some(depth);
                        display_text.clear();
                    } else if ln == b"supported-calendar-component-set"
                        && comp_set_depth.is_none()
                    {
                        comp_set_depth = Some(depth);
                    } else if comp_set_depth.is_some() && ln == b"comp" {
                        inspect_comp(e.attributes(), &mut response_components);
                    } else if resourcetype_depth.is_some() && is_calendar_marker(ln) {
                        response_is_calendar = true;
                    }
                }
            }
            Ok(XmlEvent::Empty(e)) => {
                let name = e.name();
                let ln = local_name(name.as_ref());
                if resourcetype_depth.is_some() && is_calendar_marker(ln) {
                    response_is_calendar = true;
                }
                if comp_set_depth.is_some() && ln == b"comp" {
                    inspect_comp(e.attributes(), &mut response_components);
                }
            }
            Ok(XmlEvent::Text(t)) if href_depth.is_some() => {
                if let Ok(s) = t.unescape() {
                    href_text.push_str(&s);
                }
            }
            Ok(XmlEvent::Text(t)) if display_depth.is_some() => {
                if let Ok(s) = t.unescape() {
                    display_text.push_str(&s);
                }
            }
            Ok(XmlEvent::End(_)) => {
                if let Some(d) = href_depth {
                    if depth == d {
                        if response_href.is_none() {
                            response_href = Some(href_text.trim().to_string());
                        }
                        href_depth = None;
                    }
                }
                if let Some(d) = display_depth {
                    if depth == d {
                        let trimmed = display_text.trim().to_string();
                        if !trimmed.is_empty() && response_display.is_none() {
                            response_display = Some(trimmed);
                        }
                        display_depth = None;
                    }
                }
                if let Some(d) = resourcetype_depth {
                    if depth == d {
                        resourcetype_depth = None;
                    }
                }
                if let Some(d) = comp_set_depth {
                    if depth == d {
                        comp_set_depth = None;
                    }
                }
                if let Some(d) = response_depth {
                    if depth == d {
                        if response_is_calendar {
                            if let Some(h) = response_href.take() {
                                if !h.is_empty() {
                                    calendars.push(RawCalendar {
                                        href: h,
                                        display_name: response_display.take(),
                                        components: response_components.take(),
                                    });
                                }
                            }
                        }
                        response_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        buf.clear();
    }

    calendars
}

/// Collect every `<calendar-data>` element's text content from a REPORT body.
fn extract_calendar_data(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut capture_depth: Option<usize> = None;
    let mut current = String::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) | Ok(XmlEvent::Eof) => break,
            Ok(XmlEvent::Start(e)) => {
                depth += 1;
                let name = e.name();
                if capture_depth.is_none() && local_name(name.as_ref()) == b"calendar-data" {
                    capture_depth = Some(depth);
                    current.clear();
                }
            }
            Ok(XmlEvent::End(_)) => {
                if let Some(d) = capture_depth {
                    if depth == d {
                        out.push(std::mem::take(&mut current));
                        capture_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(XmlEvent::Text(t)) if capture_depth.is_some() => {
                if let Ok(s) = t.unescape() {
                    current.push_str(&s);
                }
            }
            Ok(XmlEvent::CData(c)) if capture_depth.is_some() => {
                if let Ok(s) = std::str::from_utf8(&c) {
                    current.push_str(s);
                }
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

// ── Write: create event ───────────────────────────────────────────────────

pub async fn create_event(
    title: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    calendar_filter: Option<&str>,
    location: Option<&str>,
    notes: Option<&str>,
) -> Result<String> {
    let (user, pass) = super::credentials()
        .ok_or_else(|| anyhow!("ICLOUD_USERNAME / ICLOUD_APP_PASSWORD nicht in .env gesetzt"))?;
    let client = reqwest::Client::builder()
        .user_agent("Companion/0.1 (CalDAV write)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let principal = discover_principal(&client, &user, &pass).await?;
    let home = discover_calendar_home(&client, &user, &pass, &principal).await?;
    let cals = list_calendars(&client, &user, &pass, &home).await?;

    // If the caller asked for a calendar that ONLY matches VTODO-only ones
    // (no event calendar matches at all), fail clearly. If at least one
    // event calendar also matches the filter (e.g. "D&S" matches both
    // "D&S" and "D&S ⚠️"), the picker handles preference and we proceed.
    if let Some(f) = calendar_filter {
        let any_vtodo_match = cals
            .iter()
            .any(|c| !c.supports_vevent() && match_quality(c, f) != MatchQuality::None);
        let any_event_match = cals
            .iter()
            .any(|c| c.supports_vevent() && match_quality(c, f) != MatchQuality::None);
        if any_vtodo_match && !any_event_match {
            let vtodo_name = cals
                .iter()
                .find(|c| !c.supports_vevent() && match_quality(c, f) != MatchQuality::None)
                .and_then(|c| c.display_name.as_deref())
                .unwrap_or("?");
            let alts: Vec<&str> = cals
                .iter()
                .filter(|c| c.supports_vevent())
                .filter_map(|c| c.display_name.as_deref())
                .collect();
            return Err(anyhow!(
                "Kalender '{}' ist nur für Reminders/Aufgaben (VTODO), kein Event-Kalender. Verfügbare Event-Kalender: {:?}",
                vtodo_name,
                alts
            ));
        }
    }

    let target = pick_write_target(&cals, calendar_filter, WriteKind::Event)
        .ok_or_else(|| anyhow!("kein passender iCloud-Kalender gefunden"))?;

    // Build a VEVENT and wrap it in a minimal VCALENDAR document.
    // DTSTAMP is mandatory in RFC5545; some CalDAV servers (including
    // iCloud at times) reject events without it with 403/Forbidden.
    let uid = uuid::Uuid::new_v4().to_string();
    let mut ev = Event::new();
    ev.uid(&uid)
        .summary(title)
        .starts(start)
        .ends(end)
        .timestamp(Utc::now());
    if let Some(loc) = location {
        ev.location(loc);
    }
    if let Some(n) = notes {
        ev.description(n);
    }
    let event = ev.done();
    let mut cal = Calendar::new();
    cal.push(event);
    let ical_text = cal.to_string();

    // PUT to `<calendar>/<uid>.ics`. `If-None-Match: *` makes this a strict
    // create (no accidental overwrite of an existing UID).
    let url = format!("{}{}.ics", ensure_trailing_slash(&target.url), uid);
    let resp = client
        .put(&url)
        .basic_auth(&user, Some(&pass))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .header("If-None-Match", "*")
        .body(ical_text.clone())
        .send()
        .await?;

    let status = resp.status();
    let cal_label = target.display_name.as_deref().unwrap_or("Kalender");
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(
            "CalDAV PUT failed → kalender='{}' url='{}' status={} body='{}'",
            cal_label,
            url,
            status,
            snippet(&body)
        );
        tracing::error!("CalDAV PUT body sent was:\n{}", ical_text);
        return Err(anyhow!(
            "PUT in Kalender '{}' ({}) → HTTP {} ({})",
            cal_label,
            target.url,
            status,
            snippet(&body)
        ));
    }
    Ok(format!("\"{}\" in {} angelegt", title, cal_label))
}

/// Match quality, ordered so higher variants win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchQuality {
    None,
    /// URL's last path segment is contained in the (longer) needle — tolerant
    /// fallback for whitelist entries with accidental extra chars.
    NeedleContainsUrlSegment,
    UrlSubstring,
    NamePrefix,
    NameExact,
}

/// Returns the URL's last non-empty path segment, lowercased. Used for the
/// tolerant reverse-substring match (when the user's whitelist entry has a
/// few extra chars appended to a real UUID).
fn url_tail_segment(url: &str) -> Option<String> {
    url.to_lowercase()
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn match_quality(c: &DiscoveredCalendar, needle: &str) -> MatchQuality {
    let n = needle.to_lowercase();
    let name = c.display_name.as_deref().unwrap_or("").to_lowercase();
    let name_trim = name.trim();
    if name_trim == n {
        return MatchQuality::NameExact;
    }
    if name.starts_with(&n) {
        return MatchQuality::NamePrefix;
    }
    if c.url.to_lowercase().contains(&n) {
        return MatchQuality::UrlSubstring;
    }
    // Tolerant fallback: maybe the whitelist entry has 1-2 extra chars from
    // a sloppy copy-paste. If the URL's UUID segment is contained in the
    // whitelist entry (and is long enough to not be a trivial match), accept.
    if let Some(seg) = url_tail_segment(&c.url) {
        if seg.len() >= 16 && n.contains(&seg) {
            return MatchQuality::NeedleContainsUrlSegment;
        }
    }
    MatchQuality::None
}

fn best_match<'a>(
    cals: &'a [DiscoveredCalendar],
    needle: &str,
) -> Option<&'a DiscoveredCalendar> {
    let mut best: Option<(&DiscoveredCalendar, MatchQuality)> = None;
    tracing::warn!("pick_write_target: filter='{}' (bytes={:?})", needle, needle.as_bytes());
    for c in cals {
        let q = match_quality(c, needle);
        let name = c.display_name.as_deref().unwrap_or("?");
        tracing::warn!(
            "  candidate name='{}' (bytes={:?}) url='{}' → {:?}",
            name,
            name.as_bytes(),
            c.url,
            q
        );
        if q == MatchQuality::None {
            continue;
        }
        match best {
            None => best = Some((c, q)),
            Some((_, prev_q)) if q > prev_q => best = Some((c, q)),
            _ => {}
        }
    }
    if let Some((c, q)) = best {
        tracing::warn!("  picked: '{}' ({:?})", c.display_name.as_deref().unwrap_or("?"), q);
    }
    best.map(|(c, _)| c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteKind {
    Event,
    Todo,
}

impl WriteKind {
    fn accepts(self, c: &DiscoveredCalendar) -> bool {
        match self {
            WriteKind::Event => c.supports_vevent(),
            WriteKind::Todo => c.supports_vtodo(),
        }
    }
    fn default_env_var(self) -> &'static str {
        match self {
            WriteKind::Event => "ICLOUD_DEFAULT_WRITE_CALENDAR",
            WriteKind::Todo => "ICLOUD_DEFAULT_WRITE_REMINDER_LIST",
        }
    }
    fn skip_label(self) -> &'static str {
        match self {
            WriteKind::Event => "VTODO-only",
            WriteKind::Todo => "VEVENT-only",
        }
    }
}

fn pick_write_target<'a>(
    cals: &'a [DiscoveredCalendar],
    filter: Option<&str>,
    kind: WriteKind,
) -> Option<&'a DiscoveredCalendar> {
    // Only consider calendars matching the requested component type.
    let typed: Vec<DiscoveredCalendar> =
        cals.iter().filter(|c| kind.accepts(c)).cloned().collect();
    let skipped: Vec<&str> = cals
        .iter()
        .filter(|c| !kind.accepts(c))
        .map(|c| c.display_name.as_deref().unwrap_or("?"))
        .collect();
    if !skipped.is_empty() {
        tracing::info!(
            "pick_write_target ({:?}): {} Kalender übersprungen: {:?}",
            kind,
            kind.skip_label(),
            skipped
        );
    }

    if let Some(f) = filter {
        if let Some(found) = best_match(&typed, f) {
            return cals.iter().find(|c| c.url == found.url);
        }
    }
    if let Ok(default) = std::env::var(kind.default_env_var()) {
        let default = default.trim();
        if !default.is_empty() {
            if let Some(found) = best_match(&typed, default) {
                return cals.iter().find(|c| c.url == found.url);
            }
        }
    }
    // Fallback: first matching-kind calendar, else first calendar at all.
    cals.iter().find(|c| kind.accepts(c)).or_else(|| cals.first())
}

fn ensure_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{}/", s)
    }
}

#[cfg(test)]
mod live_tests {
    //! Real-network diagnostics. Requires .env at the project root with
    //! ICLOUD_USERNAME / ICLOUD_APP_PASSWORD set. Tests are #[ignore]d so
    //! `cargo test` doesn't accidentally run them; opt-in via
    //! `cargo test -p companion --lib -- --ignored --nocapture live_picker`.
    use super::*;
    use crate::storage::secrets::load_env_file;

    fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,companion=debug,companion_lib=debug")),
            )
            .with_test_writer()
            .try_init();
    }

    #[tokio::test]
    #[ignore]
    async fn live_picker_for_ds() {
        load_env_file();
        init_tracing();
        let (user, pass) = crate::icloud::credentials().expect("creds in .env");
        let client = reqwest::Client::builder()
            .user_agent("Companion/0.1 (CalDAV diag)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();

        let principal = discover_principal(&client, &user, &pass).await.expect("principal");
        let home = discover_calendar_home(&client, &user, &pass, &principal).await.expect("home");
        let cals = list_calendars(&client, &user, &pass, &home).await.expect("cals");

        println!("\n=== Kalender nach Whitelist-Filter ({}) ===", cals.len());
        for c in &cals {
            println!(
                "  • {} (len-name={}) → {}",
                c.display_name.as_deref().unwrap_or("(ohne Name)"),
                c.display_name.as_deref().map(|s| s.chars().count()).unwrap_or(0),
                c.url,
            );
        }

        println!("\n=== Picker für 'D&S' ===");
        let picked = pick_write_target(&cals, Some("D&S"), WriteKind::Event);
        match picked {
            Some(c) => println!(
                "  → ausgewählt: '{}' url={}",
                c.display_name.as_deref().unwrap_or("?"),
                c.url
            ),
            None => println!("  → NICHTS gefunden"),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_privileges_per_calendar() {
        load_env_file();
        init_tracing();
        let (user, pass) = crate::icloud::credentials().expect("creds in .env");
        let client = reqwest::Client::builder()
            .user_agent("Companion/0.1 (CalDAV priv)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let principal = discover_principal(&client, &user, &pass).await.expect("principal");
        let home = discover_calendar_home(&client, &user, &pass, &principal).await.expect("home");
        let cals = list_calendars(&client, &user, &pass, &home).await.expect("cals");

        const PRIV_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-privilege-set/>
    <d:resourcetype/>
    <d:displayname/>
    <d:owner/>
  </d:prop>
</d:propfind>"#;

        for c in &cals {
            println!(
                "\n--- {} ---\nurl: {}",
                c.display_name.as_deref().unwrap_or("?"),
                c.url
            );
            match propfind(&client, &user, &pass, &c.url, 0, PRIV_BODY).await {
                Ok(xml) => {
                    println!("RAW XML (snippet):");
                    let snip: String = xml.chars().take(800).collect();
                    println!("{}", snip);

                    let mut privs: Vec<String> = Vec::new();
                    let mut reader = Reader::from_str(&xml);
                    reader.config_mut().trim_text(true);
                    let mut buf = Vec::new();
                    let mut depth_in_priv_set = 0i32;
                    let mut depth_in_priv = 0i32;
                    loop {
                        match reader.read_event_into(&mut buf) {
                            Ok(XmlEvent::Start(ref e)) | Ok(XmlEvent::Empty(ref e)) => {
                                let n = e.name();
                                let local = std::str::from_utf8(n.local_name().as_ref())
                                    .unwrap_or("")
                                    .to_string();
                                let is_empty = matches!(reader.read_event_into(&mut Vec::new()), _placeholder if false);
                                // We can't easily tell Start vs Empty from the matched
                                // pattern alone in this branch — track by handling them
                                // identically and using the local name.
                                let was_empty = matches!(reader.read_event_into(&mut Vec::new()), _ if false);
                                let _ = (is_empty, was_empty);

                                if depth_in_priv > 0 {
                                    // First child of <privilege> is the privilege name.
                                    privs.push(local.clone());
                                }
                                if local == "current-user-privilege-set" {
                                    depth_in_priv_set += 1;
                                }
                                if local == "privilege" && depth_in_priv_set > 0 {
                                    depth_in_priv += 1;
                                }
                            }
                            Ok(XmlEvent::End(ref e)) => {
                                let n = e.name();
                                let local = std::str::from_utf8(n.local_name().as_ref())
                                    .unwrap_or("")
                                    .to_string();
                                if local == "current-user-privilege-set" {
                                    depth_in_priv_set -= 1;
                                }
                                if local == "privilege" {
                                    depth_in_priv -= 1;
                                }
                            }
                            Ok(XmlEvent::Eof) => break,
                            _ => {}
                        }
                        buf.clear();
                    }
                    // Privilege names accidentally include "privilege" itself —
                    // filter that out plus the parent element names.
                    privs.retain(|p| p != "privilege" && p != "current-user-privilege-set");
                    println!("privileges: {:?}", privs);
                    let writable = privs
                        .iter()
                        .any(|p| p == "write" || p == "write-content" || p == "all");
                    println!("→ {}", if writable { "SCHREIBBAR ✓" } else { "READ-ONLY ✗" });
                }
                Err(e) => println!("PROPFIND-Fehler: {}", e),
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_dump_home_set() {
        load_env_file();
        init_tracing();
        let (user, pass) = crate::icloud::credentials().expect("creds in .env");
        let client = reqwest::Client::builder()
            .user_agent("Companion/0.1 (CalDAV home-dump)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let principal = discover_principal(&client, &user, &pass).await.expect("principal");
        let home = discover_calendar_home(&client, &user, &pass, &principal).await.expect("home");
        println!("principal: {}", principal);
        println!("calendar-home-set: {}", home);

        const RAW: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <c:supported-calendar-component-set/>
    <cs:source/>
  </d:prop>
</d:propfind>"#;

        let body = propfind(&client, &user, &pass, &home, 1, RAW).await.expect("propfind");
        println!("\n=== FULL HOME-SET RESPONSE ===\n{}", body);

        // Also probe the principal for any related groups/inboxes that might
        // hold delegated/shared calendars.
        const PRINCIPAL_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <c:calendar-home-set/>
    <cs:calendar-proxy-read-for/>
    <cs:calendar-proxy-write-for/>
    <d:group-membership/>
  </d:prop>
</d:propfind>"#;
        let body2 = propfind(&client, &user, &pass, &principal, 0, PRINCIPAL_PROPS).await.expect("propfind principal");
        println!("\n=== PRINCIPAL RESPONSE ===\n{}", body2);
    }

    #[tokio::test]
    #[ignore]
    async fn live_full_diag_ds() {
        load_env_file();
        init_tracing();
        let (user, pass) = crate::icloud::credentials().expect("creds in .env");
        let client = reqwest::Client::builder()
            .user_agent("Companion/0.1 (CalDAV full-diag)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();

        const FULL_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:owner/>
    <d:current-user-principal/>
    <d:current-user-privilege-set/>
    <c:supported-calendar-component-set/>
    <c:calendar-description/>
    <cs:source/>
    <cs:invite/>
    <cs:shared-url/>
    <ical:calendar-color/>
  </d:prop>
</d:propfind>"#;

        let target_url = "https://caldav.icloud.com/8269837075/calendars/6AF75BF4-187D-430B-92C2-5777702BE744/";
        println!("\n=== Full PROPFIND on D&S ⚠️ ===");
        match propfind(&client, &user, &pass, target_url, 0, FULL_PROPS).await {
            Ok(xml) => println!("{}", xml),
            Err(e) => println!("ERR: {}", e),
        }

        println!("\n=== Full PROPFIND on Privat (Vergleich) ===");
        match propfind(
            &client,
            &user,
            &pass,
            "https://caldav.icloud.com/8269837075/calendars/home/",
            0,
            FULL_PROPS,
        )
        .await
        {
            Ok(xml) => println!("{}", xml),
            Err(e) => println!("ERR: {}", e),
        }

        // Probe-PUT to D&S with verbose response capture.
        println!("\n=== Probe-PUT to D&S ⚠️ ===");
        let uid = uuid::Uuid::new_v4().to_string();
        let ical = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Companion//Diag//EN\r\nCALSCALE:GREGORIAN\r\nBEGIN:VEVENT\r\nUID:{}\r\nDTSTAMP:20260511T220000Z\r\nDTSTART:20260512T130000Z\r\nDTEND:20260512T140000Z\r\nSUMMARY:DIAG TEST\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            uid
        );
        let url = format!("{}{}.ics", target_url, uid);
        let resp = client
            .put(&url)
            .basic_auth(&user, Some(&pass))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .header("If-None-Match", "*")
            .body(ical.clone())
            .send()
            .await
            .expect("put");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();
        println!("status: {}", status);
        for (k, v) in headers.iter() {
            println!("  {}: {}", k, v.to_str().unwrap_or("?"));
        }
        println!("body: '{}'", body);
    }

    #[tokio::test]
    #[ignore]
    async fn live_check_notifications_and_proxies() {
        load_env_file();
        init_tracing();
        let (user, pass) = crate::icloud::credentials().expect("creds");
        let client = reqwest::Client::builder()
            .user_agent("Companion/0.1 (CalDAV probe)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let principal = discover_principal(&client, &user, &pass).await.expect("principal");
        let home = discover_calendar_home(&client, &user, &pass, &principal).await.expect("home");

        // 1) Look at /notification/ collection — pending share-invites land here.
        let notif_url = format!("{}notification/", ensure_trailing_slash(&home));
        const NOTIF_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <cs:notificationtype/>
  </d:prop>
</d:propfind>"#;
        println!("\n=== /notification/ (Depth 1) ===");
        match propfind(&client, &user, &pass, &notif_url, 1, NOTIF_PROPS).await {
            Ok(x) => println!("{}", x),
            Err(e) => println!("ERR: {}", e),
        }

        // 2) Look at the principal for proxy-for / group-memberships.
        const PRINC_PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <cs:calendar-proxy-read-for/>
    <cs:calendar-proxy-write-for/>
    <d:group-membership/>
    <d:principal-collection-set/>
  </d:prop>
</d:propfind>"#;
        println!("\n=== Principal proxy/group probe ===");
        match propfind(&client, &user, &pass, &principal, 0, PRINC_PROPS).await {
            Ok(x) => println!("{}", x),
            Err(e) => println!("ERR: {}", e),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_list_reminders() {
        load_env_file();
        init_tracing();
        let reminders = list_reminders(None, true).await.expect("list");
        println!("\n=== Offene Reminder ({}) ===", reminders.len());
        for r in &reminders {
            println!(
                "  • [{}] {}  due={:?}  notes={:?}",
                r.list, r.title, r.due, r.notes
            );
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_create_reminder_shared() {
        load_env_file();
        init_tracing();
        let due = chrono::Local::now()
            .checked_add_signed(chrono::Duration::hours(24))
            .unwrap()
            .with_timezone(&Utc);
        match create_reminder(
            "TEST Reminder (auto-test, lösch mich)",
            Some(due),
            Some("D&S"),
            Some("Erstellt von live_create_reminder_shared"),
        )
        .await
        {
            Ok(msg) => println!("\n=== ERFOLG: {} ===", msg),
            Err(e) => println!("\n=== FEHLER: {} ===", e),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_create_event_ds() {
        load_env_file();
        init_tracing();
        // Tomorrow 15:00 local → UTC.
        let now = chrono::Local::now();
        let tomorrow_date = now.date_naive().succ_opt().unwrap();
        let start_naive = tomorrow_date.and_hms_opt(15, 0, 0).unwrap();
        let end_naive = tomorrow_date.and_hms_opt(16, 0, 0).unwrap();
        let start = chrono::Local
            .from_local_datetime(&start_naive)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let end = chrono::Local
            .from_local_datetime(&end_naive)
            .single()
            .unwrap()
            .with_timezone(&Utc);

        let result = create_event(
            "TEST Date (auto-test, lösch mich)",
            start,
            end,
            Some("D&S"),
            None,
            Some("Erstellt von cargo test live_create_event_ds"),
        )
        .await;
        match result {
            Ok(msg) => println!("\n=== ERFOLG: {} ===", msg),
            Err(e) => println!("\n=== FEHLER: {} ===", e),
        }
    }
}
