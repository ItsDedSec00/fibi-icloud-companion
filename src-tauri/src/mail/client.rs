//! Synchronous IMAP fetch helpers, called from `tokio::task::spawn_blocking`
//! at the Tauri-command layer. The `imap` crate is sync, which is fine for
//! our usage (one-shot lookups when Claude asks "any mails?") — we keep it
//! off the tokio executor with spawn_blocking.

use anyhow::{anyhow, Context, Result};
use mail_parser::MessageParser;

use super::{Credentials, EmailSummary};

const FETCH_HEADERS: &str = "BODY.PEEK[HEADER.FIELDS (SUBJECT FROM DATE)]";

/// List the most recent unread mails (capped at `limit`, newest first).
pub fn fetch_unread(limit: usize) -> Result<Vec<EmailSummary>> {
    let creds = Credentials::from_env()
        .ok_or_else(|| anyhow!("IMAP-Credentials nicht in .env gesetzt"))?;
    let client = imap::ClientBuilder::new(&creds.host, creds.port)
        .connect()
        .with_context(|| format!("IMAP connect {}:{}", creds.host, creds.port))?;
    let mut session = client
        .login(&creds.username, &creds.password)
        .map_err(|(e, _)| anyhow!("IMAP login: {}", e))?;
    session.select("INBOX").context("SELECT INBOX")?;

    let unread_uids = session.uid_search("UNSEEN").context("UID SEARCH UNSEEN")?;
    let mut uids: Vec<u32> = unread_uids.iter().copied().collect();
    uids.sort_unstable();
    let tail: Vec<u32> = uids.iter().rev().take(limit).copied().collect();

    let summaries = if tail.is_empty() {
        Vec::new()
    } else {
        let uid_set = tail
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let messages = session
            .uid_fetch(&uid_set, FETCH_HEADERS)
            .context("UID FETCH headers")?;
        parse_messages(&messages, true, &std::collections::HashSet::new())
    };
    let _ = session.logout();
    Ok(summaries)
}

/// Recent mails (read + unread), newest first.
pub fn fetch_recent(limit: usize) -> Result<Vec<EmailSummary>> {
    let creds = Credentials::from_env()
        .ok_or_else(|| anyhow!("IMAP-Credentials nicht in .env gesetzt"))?;
    let client = imap::ClientBuilder::new(&creds.host, creds.port)
        .connect()
        .with_context(|| format!("IMAP connect {}:{}", creds.host, creds.port))?;
    let mut session = client
        .login(&creds.username, &creds.password)
        .map_err(|(e, _)| anyhow!("IMAP login: {}", e))?;
    let mbox = session.select("INBOX").context("SELECT INBOX")?;
    let total = mbox.exists;
    if total == 0 {
        let _ = session.logout();
        return Ok(Vec::new());
    }
    let take = (limit as u32).min(total);
    let start = total.saturating_sub(take) + 1;
    let seq_set = format!("{}:{}", start, total);
    let fetch_query = format!("({} UID)", FETCH_HEADERS);
    let messages = session
        .fetch(&seq_set, &fetch_query)
        .context("FETCH recent")?;
    let unread_set: std::collections::HashSet<u32> = session
        .uid_search("UNSEEN")
        .map(|s| s.into_iter().collect())
        .unwrap_or_default();
    let out = parse_messages(&messages, false, &unread_set);
    let _ = session.logout();
    Ok(out)
}

// ── helpers ─────────────────────────────────────────────────────────────

fn parse_messages(
    messages: &imap::types::Fetches,
    force_unread: bool,
    unread_set: &std::collections::HashSet<u32>,
) -> Vec<EmailSummary> {
    let mut out: Vec<EmailSummary> = messages
        .iter()
        .filter_map(|msg| {
            let uid = msg.uid?;
            let header_bytes = msg.header()?;
            let parsed = MessageParser::default().parse_headers(header_bytes)?;
            Some(EmailSummary {
                uid,
                from: format_from(&parsed),
                subject: parsed
                    .subject()
                    .unwrap_or("(ohne Betreff)")
                    .to_string(),
                date: parsed.date().map(|d| d.to_rfc3339()),
                unread: force_unread || unread_set.contains(&uid),
            })
        })
        .collect();
    out.sort_by(|a, b| b.uid.cmp(&a.uid));
    out
}

fn format_from(msg: &mail_parser::Message) -> String {
    let Some(from) = msg.from() else {
        return "(unbekannt)".into();
    };
    if let Some(addr) = from.first() {
        let name = addr.name().unwrap_or("").trim();
        let email = addr.address().unwrap_or("").trim();
        if !name.is_empty() && !email.is_empty() {
            return format!("{} <{}>", name, email);
        }
        if !email.is_empty() {
            return email.to_string();
        }
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "(unbekannt)".into()
}
