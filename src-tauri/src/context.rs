//! Runtime context injected into Claude's system prompt on every message —
//! current date/time and (optionally) the user's location. The location is
//! configured via `.env` (`USER_LOCATION="…"`); unset, we omit the line and
//! the model will ask in one sentence if it needs to know.

use chrono::{Datelike, Local, Timelike};

pub fn user_context() -> String {
    let mut lines = Vec::with_capacity(2);
    lines.push(format!("Aktuelle Zeit: {}", current_time_string()));
    if let Some(loc) = user_location() {
        lines.push(format!("Aufenthaltsort des Users: {loc}"));
    }
    lines.join("\n")
}

fn current_time_string() -> String {
    let now = Local::now();
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "Montag",
        chrono::Weekday::Tue => "Dienstag",
        chrono::Weekday::Wed => "Mittwoch",
        chrono::Weekday::Thu => "Donnerstag",
        chrono::Weekday::Fri => "Freitag",
        chrono::Weekday::Sat => "Samstag",
        chrono::Weekday::Sun => "Sonntag",
    };
    let month = match now.month() {
        1 => "Januar",
        2 => "Februar",
        3 => "März",
        4 => "April",
        5 => "Mai",
        6 => "Juni",
        7 => "Juli",
        8 => "August",
        9 => "September",
        10 => "Oktober",
        11 => "November",
        12 => "Dezember",
        _ => "?",
    };
    format!(
        "{weekday}, {day}. {month} {year}, {hour:02}:{minute:02} Uhr ({offset})",
        day = now.day(),
        year = now.year(),
        hour = now.hour(),
        minute = now.minute(),
        offset = now.format("%:z"),
    )
}

fn user_location() -> Option<String> {
    std::env::var("USER_LOCATION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
