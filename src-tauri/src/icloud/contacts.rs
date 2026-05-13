//! iCloud Contacts via the pyicloud bridge.
//!
//! Address book changes rarely, so we fetch once on first use and cache
//! aggressively in-process. A manual `refresh()` clears the cache (called
//! after `refresh_now` if/when we wire it up). All Claude-facing tools
//! operate on the cache: fuzzy `find` + upcoming-birthday window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use tokio::sync::{OnceCell, RwLock};

use super::bridge as icloud_bridge;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 h

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabeledValue {
    #[serde(default)]
    pub label: Option<String>,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub emails: Vec<LabeledValue>,
    #[serde(default)]
    pub phones: Vec<LabeledValue>,
    /// `YYYY-MM-DD` if year known, `--MM-DD` if not.
    #[serde(default)]
    pub birthday: Option<String>,
}

#[derive(Default)]
struct Cache {
    contacts: Vec<Contact>,
    fetched_at: Option<Instant>,
}

static CACHE: OnceCell<Arc<RwLock<Cache>>> = OnceCell::const_new();

async fn cache() -> Arc<RwLock<Cache>> {
    CACHE
        .get_or_init(|| async { Arc::new(RwLock::new(Cache::default())) })
        .await
        .clone()
}

async fn ensure_fresh() -> Result<()> {
    let cache = cache().await;
    {
        let guard = cache.read().await;
        if let Some(t) = guard.fetched_at {
            if t.elapsed() < CACHE_TTL && !guard.contacts.is_empty() {
                return Ok(());
            }
        }
    }
    let fresh = icloud_bridge::list_contacts().await?;
    let mut guard = cache.write().await;
    guard.contacts = fresh;
    guard.fetched_at = Some(Instant::now());
    tracing::info!("contacts cache refreshed: {} entries", guard.contacts.len());
    Ok(())
}

/// Fuzzy-match contacts by name / nickname / company. Lower-case substring
/// match, empty query returns all. Caller normally caps the result count.
pub async fn find(query: &str, limit: usize) -> Result<Vec<Contact>> {
    ensure_fresh().await?;
    let q = query.trim().to_lowercase();
    let cache = cache().await;
    let guard = cache.read().await;
    let mut matches: Vec<Contact> = if q.is_empty() {
        guard.contacts.clone()
    } else {
        guard
            .contacts
            .iter()
            .filter(|c| haystack(c).contains(&q))
            .cloned()
            .collect()
    };
    // Best matches first: exact name match, then prefix, then substring.
    matches.sort_by_key(|c| match_rank(&q, c));
    matches.truncate(limit);
    Ok(matches)
}

#[derive(Debug, Clone, Serialize)]
pub struct UpcomingBirthday {
    pub name: String,
    /// e.g. "12-25"
    pub date: String,
    /// Days from today, inclusive (0 = today).
    pub in_days: i64,
    pub turning: Option<i32>,
}

/// Birthdays within the next `window_days` (inclusive). Today counts as 0.
pub async fn upcoming_birthdays(window_days: i64) -> Result<Vec<UpcomingBirthday>> {
    ensure_fresh().await?;
    let cache = cache().await;
    let guard = cache.read().await;
    let today = Local::now().date_naive();
    let mut out: Vec<UpcomingBirthday> = Vec::new();
    for c in &guard.contacts {
        let Some(bday) = c.birthday.as_deref() else {
            continue;
        };
        let Some((month, day, year)) = parse_birthday(bday) else {
            continue;
        };
        let in_days = days_until_next(today, month, day);
        if in_days <= window_days {
            let turning = year.map(|y| {
                let next_year = if days_until_next(today, month, day) == 0
                    && today.month() == month
                    && today.day() == day
                {
                    today.year()
                } else {
                    today.year() + if (today.month(), today.day()) > (month, day) { 1 } else { 0 }
                };
                next_year - y
            });
            out.push(UpcomingBirthday {
                name: c.name.clone(),
                date: format!("{:02}-{:02}", month, day),
                in_days,
                turning,
            });
        }
    }
    out.sort_by_key(|b| b.in_days);
    Ok(out)
}

// ── helpers ─────────────────────────────────────────────────────────────

fn haystack(c: &Contact) -> String {
    let mut s = c.name.to_lowercase();
    if let Some(n) = &c.nickname {
        s.push(' ');
        s.push_str(&n.to_lowercase());
    }
    if let Some(co) = &c.company {
        s.push(' ');
        s.push_str(&co.to_lowercase());
    }
    s
}

fn match_rank(q: &str, c: &Contact) -> u8 {
    let name = c.name.to_lowercase();
    if name == q {
        0
    } else if name.starts_with(q) {
        1
    } else if c.first_name.as_deref().map(|s| s.to_lowercase()) == Some(q.to_string()) {
        2
    } else if name.contains(q) {
        3
    } else {
        4
    }
}

/// Accepts both `YYYY-MM-DD` and `--MM-DD`. Returns (month, day, optional year).
fn parse_birthday(s: &str) -> Option<(u32, u32, Option<i32>)> {
    if let Some(rest) = s.strip_prefix("--") {
        let parts: Vec<&str> = rest.split('-').collect();
        if parts.len() == 2 {
            let m: u32 = parts[0].parse().ok()?;
            let d: u32 = parts[1].parse().ok()?;
            return Some((m, d, None));
        }
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 3 {
        let y: i32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        let d: u32 = parts[2].parse().ok()?;
        return Some((m, d, Some(y)));
    }
    None
}

fn days_until_next(today: NaiveDate, month: u32, day: u32) -> i64 {
    let this_year = NaiveDate::from_ymd_opt(today.year(), month, day)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap());
    let next = if this_year >= today {
        this_year
    } else {
        NaiveDate::from_ymd_opt(today.year() + 1, month, day)
            .unwrap_or_else(|| NaiveDate::from_ymd_opt(today.year() + 1, 1, 1).unwrap())
    };
    (next - today).num_days()
}

/// Exposed so the post-reauth path can force a refetch (when keychains
/// rotate or the user genuinely just added a contact and wants it now).
pub async fn force_refresh() -> Result<()> {
    let cache = cache().await;
    let mut guard = cache.write().await;
    guard.fetched_at = None;
    drop(guard);
    ensure_fresh().await.context("force_refresh")
}
