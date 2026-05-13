//! IMAP read access (Ionos / generic IMAPS).
//!
//! Read-only for now: list unread, fetch headers + a snippet. Write
//! actions (mark as read, delete) intentionally not exposed to Claude
//! yet — those are easy to mis-trigger and the recovery cost is high.
//!
//! Configuration via `.env`:
//!   IMAP_HOST=imap.ionos.de
//!   IMAP_PORT=993        # optional, default 993
//!   IMAP_USERNAME=david@…
//!   IMAP_PASSWORD=…
//!
//! Credentials live in plaintext in `.env` for now (matching the existing
//! iCloud setup). Moving to keyring is a future polish task.

pub mod client;
pub mod smtp;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct EmailSummary {
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: Option<String>,
    pub unread: bool,
}

pub struct Credentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Credentials {
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("IMAP_HOST").ok()?.trim().to_string();
        let username = std::env::var("IMAP_USERNAME").ok()?.trim().to_string();
        let password = std::env::var("IMAP_PASSWORD").ok()?.trim().to_string();
        if host.is_empty() || username.is_empty() || password.is_empty() {
            return None;
        }
        let port = std::env::var("IMAP_PORT")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(993u16);
        Some(Self { host, port, username, password })
    }
}
