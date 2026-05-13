//! SMTP send for the `send_email` tool. Reuses the IMAP credentials by
//! default — Ionos (and most providers) use the same user/password for
//! both. Host defaults to swapping `imap.` → `smtp.` in IMAP_HOST.
//!
//! Configuration via `.env`:
//!   SMTP_HOST=smtp.ionos.de       # optional, derived from IMAP_HOST
//!   SMTP_PORT=587                 # optional, default 587 (STARTTLS)
//!   SMTP_USERNAME=…               # optional, default IMAP_USERNAME
//!   SMTP_PASSWORD=…               # optional, default IMAP_PASSWORD
//!   SMTP_FROM_NAME=David Dülle    # optional display name

use anyhow::{anyhow, Context, Result};
use lettre::message::{header::ContentType, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Tokio1Executor};

pub struct SmtpCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from_name: Option<String>,
}

impl SmtpCredentials {
    pub fn from_env() -> Option<Self> {
        let host = explicit_or("SMTP_HOST")
            .or_else(|| explicit_or("IMAP_HOST").map(|h| h.replace("imap.", "smtp.")))?;
        let port = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(587u16);
        let username = explicit_or("SMTP_USERNAME").or_else(|| explicit_or("IMAP_USERNAME"))?;
        let password = explicit_or("SMTP_PASSWORD").or_else(|| explicit_or("IMAP_PASSWORD"))?;
        let from_name = explicit_or("SMTP_FROM_NAME");
        Some(Self {
            host,
            port,
            username,
            password,
            from_name,
        })
    }
}

fn explicit_or(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Send a plain-text mail via SMTP STARTTLS. Returns Ok on a 2xx response
/// from the server; transport / auth errors bubble up via anyhow.
pub async fn send_email(to: &str, subject: &str, body: &str) -> Result<()> {
    let creds = SmtpCredentials::from_env()
        .ok_or_else(|| anyhow!("SMTP-Credentials nicht in .env (siehe .env.example)"))?;

    let from = match &creds.from_name {
        Some(name) => format!("{} <{}>", name, creds.username),
        None => creds.username.clone(),
    };

    let email = Message::builder()
        .from(from.parse().context("From-Adresse ungültig")?)
        .to(to.parse().context("To-Adresse ungültig")?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .context("Mail-Builder")?;

    let mailer: AsyncSmtpTransport<Tokio1Executor> =
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&creds.host)
            .context("STARTTLS-Transport")?
            .port(creds.port)
            .credentials(Credentials::new(
                creds.username.clone(),
                creds.password.clone(),
            ))
            .build();

    mailer.send(email).await.context("SMTP send")?;
    tracing::info!("SMTP: Mail an {} verschickt", to);
    Ok(())
}
