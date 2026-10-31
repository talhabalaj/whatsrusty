//! Store side of pinned messages: who pinned what, until when.

use anyhow::Result;
use rusqlite::{OptionalExtension as _, params};

use super::{MESSAGE_COLS, MessageRow, Store};
use crate::types::{ChatMessage, Timestamp};

/// WhatsApp shows at most this many pins per chat (a fourth replaces the oldest).
pub const MAX_PINS: u32 = 3;

impl Store {
    /// Message `id` was pinned (`until`: when the pin ends) or unpinned (`until: None`)
    /// at `ts`. Only the latest action counts, whichever order they arrive in (history
    /// sync replays old ones). Returns whether anything changed.
    pub fn set_pin(&self, chat_jid: &str, id: &str, until: Option<Timestamp>, ts: Timestamp) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        let changed = self.conn.execute(
            "INSERT INTO pins (chat_jid, message_id, pinned, ts, expires_ts) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (chat_jid, message_id) DO UPDATE SET
                pinned = excluded.pinned, ts = excluded.ts, expires_ts = excluded.expires_ts
             WHERE excluded.ts >= ts",
            params![chat, id, until.is_some(), ts, until.unwrap_or(0)],
        )?;
        Ok(changed > 0)
    }

    /// When message `id`'s pin was last changed, and until when it's pinned (`None`: it isn't).
    pub fn pin_state(&self, chat_jid: &str, id: &str) -> Result<Option<(Timestamp, Option<Timestamp>)>> {
        let chat = self.canonical(chat_jid)?;
        Ok(self
            .conn
            .prepare_cached("SELECT ts, pinned, expires_ts FROM pins WHERE chat_jid = ?1 AND message_id = ?2")?
            .query_row(params![chat, id], |r| {
                Ok((r.get::<_, Timestamp>(0)?, r.get::<_, bool>(1)?.then_some(r.get::<_, Timestamp>(2)?)))
            })
            .optional()?)
    }

    /// Put back a pin state saved by [`Store::pin_state`] (a pin I sent that failed).
    pub fn restore_pin(&self, chat_jid: &str, id: &str, state: Option<(Timestamp, Option<Timestamp>)>) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        match state {
            Some((ts, until)) => {
                self.conn.execute(
                    "UPDATE pins SET pinned = ?3, ts = ?4, expires_ts = ?5 WHERE chat_jid = ?1 AND message_id = ?2",
                    params![chat, id, until.is_some(), ts, until.unwrap_or(0)],
                )?;
            }
            None => {
                self.conn.execute("DELETE FROM pins WHERE chat_jid = ?1 AND message_id = ?2", params![chat, id])?;
            }
        }
        Ok(())
    }

    /// The chat's pinned messages at `now`, the latest pinned first: still pinned, not
    /// expired, stored, and not deleted.
    pub fn pinned_messages(&self, chat_jid: &str, now: Timestamp) -> Result<Vec<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let cols = MESSAGE_COLS.split(',').map(|c| format!("m.{}", c.trim())).collect::<Vec<_>>().join(", ");
        let rows: Vec<MessageRow> = self
            .conn
            .prepare_cached(&format!(
                "SELECT {cols} FROM pins p JOIN messages m ON m.chat_jid = p.chat_jid AND m.id = p.message_id
                 WHERE p.chat_jid = ?1 AND p.pinned = 1 AND p.expires_ts > ?2 AND m.revoked = 0
                 ORDER BY p.ts DESC LIMIT {MAX_PINS}"
            ))?
            .query_map(params![chat, now], MessageRow::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        self.to_chat_messages(&chat, rows)
    }
}
