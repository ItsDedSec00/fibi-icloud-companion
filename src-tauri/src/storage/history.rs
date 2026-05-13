use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::db;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

pub struct History {
    conn: Connection,
    current_conversation: i64,
}

impl History {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = db::open(path)?;
        let current_conversation = ensure_active_conversation(&conn)?;
        Ok(Self {
            conn,
            current_conversation,
        })
    }

    pub fn list(&self, limit: i64) -> anyhow::Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, created_at FROM messages
             WHERE conversation_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows: Result<Vec<StoredMessage>, _> = stmt
            .query_map(params![self.current_conversation, limit], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect();
        let mut messages = rows?;
        messages.reverse();
        Ok(messages)
    }

    pub fn append(&self, role: &str, content: &str) -> anyhow::Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (conversation_id, role, content) VALUES (?1, ?2, ?3)",
            params![self.current_conversation, role, content],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            params![self.current_conversation],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn ensure_active_conversation(conn: &Connection) -> anyhow::Result<i64> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM conversations ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    if let Some(id) = id {
        return Ok(id);
    }
    conn.execute("INSERT INTO conversations DEFAULT VALUES", [])?;
    Ok(conn.last_insert_rowid())
}
