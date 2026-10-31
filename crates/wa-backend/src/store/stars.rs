//! Store side of starred messages, and searching all chats.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{OptionalExtension as _, params};

use super::message_actions::searchable_text;
use super::{MESSAGE_COLS, MessageRow, Store};
use crate::types::{ChatMessage, Timestamp};

/// At most this many results when searching all chats (the newest).
const SEARCH_ALL_LIMIT: usize = 100;

impl Store {
    /// Message `id` was starred or unstarred at `ts`. Only the latest action counts,
    /// whichever order they arrive in. Returns whether anything changed.
    pub fn set_star(&self, chat_jid: &str, id: &str, starred: bool, ts: Timestamp) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        let changed = self.conn.execute(
            "INSERT INTO stars (chat_jid, message_id, starred, ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (chat_jid, message_id) DO UPDATE SET starred = excluded.starred, ts = excluded.ts
             WHERE excluded.ts >= ts AND excluded.starred != starred",
            params![chat, id, starred, ts],
        )?;
        Ok(changed > 0)
    }

    pub fn is_starred(&self, chat_jid: &str, id: &str) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        Ok(self
            .conn
            .prepare_cached("SELECT starred FROM stars WHERE chat_jid = ?1 AND message_id = ?2")?
            .query_row(params![chat, id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    pub(super) fn starred_ids(&self, chat: &str) -> Result<HashSet<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT message_id FROM stars WHERE chat_jid = ?1 AND starred = 1")?
            .query_map([chat], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// My starred messages in all chats that are still here, the latest starred first.
    pub fn starred_messages(&self, limit: u32) -> Result<Vec<ChatMessage>> {
        let cols = MESSAGE_COLS.split(',').map(|c| format!("m.{}", c.trim())).collect::<Vec<_>>().join(", ");
        let rows: Vec<MessageRow> = self
            .conn
            .prepare_cached(&format!(
                "SELECT {cols} FROM stars s JOIN messages m ON m.chat_jid = s.chat_jid AND m.id = s.message_id
                 WHERE s.starred = 1 AND m.revoked = 0 ORDER BY s.ts DESC LIMIT ?1"
            ))?
            .query_map([limit], MessageRow::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter().map(|r| self.to_chat_message(r)).collect()
    }

    /// Messages in any chat whose text or caption has `query` in it (any case), newest
    /// first. The database narrows it down; the words shown decide.
    pub fn search_all(&self, query: &str) -> Result<Vec<ChatMessage>> {
        let needle = query.trim().to_lowercase();
        if needle.chars().count() < 2 {
            return Ok(Vec::new());
        }
        let pattern = format!("%{}%", needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE revoked = 0 AND text LIKE ?1 ESCAPE '\\'
             ORDER BY ts DESC, rowid DESC"
        ))?;
        let mut rows = stmt.query_map([pattern], MessageRow::from_row)?;
        let mut hits = Vec::new();
        while let Some(r) = rows.next().transpose()? {
            if searchable_text(&r.body).is_some_and(|t| t.to_lowercase().contains(&needle)) {
                hits.push(self.to_chat_message(r)?);
                if hits.len() == SEARCH_ALL_LIMIT {
                    break;
                }
            }
        }
        Ok(hits)
    }
}
