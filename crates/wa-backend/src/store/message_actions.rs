//! Store side of per-message actions: resending (restamp), delete for me, forwarding,
//! and searching a chat.

use std::path::Path;

use anyhow::Result;
use rusqlite::{OptionalExtension as _, params};

use super::{Store, body_from_row, remove_media_file};
use crate::types::{MediaState, MessageBody, SearchHit, Timestamp};

/// At most this many hits per search (the newest ones).
const SEARCH_LIMIT: usize = 2000;

impl Store {
    /// Move an own message to `ts` (a resend goes to the end of the transcript) and make
    /// it the chat's latest activity.
    pub fn restamp(&self, chat_jid: &str, id: &str, ts: Timestamp) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        let changed = self
            .conn
            .execute("UPDATE messages SET ts = ?3 WHERE chat_jid = ?1 AND id = ?2 AND from_me = 1", params![chat, id, ts])?;
        if changed > 0 {
            self.conn.execute("UPDATE chats SET last_activity = MAX(last_activity, ?2) WHERE jid = ?1", params![chat, ts])?;
        }
        Ok(changed > 0)
    }

    /// Delete a message from this account only, with its reactions and file. It stays
    /// gone: a later copy (re-delivery, history sync) isn't stored again. Returns whether
    /// it was stored.
    pub fn delete_for_me(&self, chat_jid: &str, id: &str, now: Timestamp) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        let path: Option<Option<String>> = self
            .conn
            .prepare_cached("SELECT media_path FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, id], |r| r.get(0))
            .optional()?;
        self.conn.execute(
            "INSERT OR REPLACE INTO deleted_for_me (chat_jid, id, deleted_at) VALUES (?1, ?2, ?3)",
            params![chat, id, now],
        )?;
        self.conn.execute("DELETE FROM messages WHERE chat_jid = ?1 AND id = ?2", params![chat, id])?;
        self.conn.execute("DELETE FROM reactions WHERE chat_jid = ?1 AND target_id = ?2", params![chat, id])?;
        self.conn.execute("DELETE FROM pending_edits WHERE chat_jid = ?1 AND id = ?2", params![chat, id])?;
        if let Some(Some(path)) = &path {
            remove_media_file(Path::new(path));
        }
        Ok(path.is_some())
    }

    /// Whether message `id` was deleted for me (and must not come back).
    pub fn is_deleted_for_me(&self, chat_jid: &str, id: &str) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM deleted_for_me WHERE chat_jid = ?1 AND id = ?2)")?
            .query_row(params![chat, id], |r| r.get(0))?)
    }

    /// Mark a message as forwarded (it showed WhatsApp's "Forwarded" label).
    pub fn set_forwarded(&self, chat_jid: &str, id: &str) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.conn.execute("UPDATE messages SET forwarded = 1 WHERE chat_jid = ?1 AND id = ?2", params![chat, id])?;
        Ok(())
    }

    /// Messages of a chat whose text, caption or rich-message body contains `query`
    /// (case-insensitive), oldest first.
    pub fn search_messages(&self, chat_jid: &str, query: &str) -> Result<Vec<SearchHit>> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let chat = self.canonical(chat_jid)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, ts, kind, text, media, payload FROM messages
             WHERE chat_jid = ?1 AND revoked = 0 ORDER BY ts DESC, rowid DESC",
        )?;
        let mut rows = stmt.query(params![chat])?;
        let mut hits = Vec::new();
        while let Some(r) = rows.next()? {
            let (kind, text): (i64, String) = (r.get(2)?, r.get(3)?);
            let media: Option<Vec<u8>> = r.get(4)?;
            let payload: Option<Vec<u8>> = r.get(5)?;
            let (body, _) = body_from_row(kind, text, media.as_deref(), payload.as_deref(), None, MediaState::Idle);
            if searchable_text(&body).is_some_and(|t| t.to_lowercase().contains(&needle)) {
                hits.push(SearchHit { id: r.get(0)?, timestamp: r.get(1)? });
                if hits.len() == SEARCH_LIMIT {
                    break;
                }
            }
        }
        hits.reverse();
        Ok(hits)
    }
}

/// The words a message shows: its text or caption (not "📷 Photo"-style labels).
pub fn searchable_text(body: &MessageBody) -> Option<&str> {
    match body {
        MessageBody::Text(t) => Some(t),
        MessageBody::Image(i) => i.caption.as_deref(),
        MessageBody::Video(v) => v.caption.as_deref(),
        MessageBody::Document(d) => d.caption.as_deref(),
        MessageBody::Rich(r) => Some(&r.body),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewMessage;
    use crate::types::DeliveryStatus;

    const CHAT: &str = "1@s.whatsapp.net";

    fn message(id: &str, from_me: bool, text: &str, ts: Timestamp) -> NewMessage {
        NewMessage {
            chat_jid: CHAT.into(),
            id: id.into(),
            sender_jid: if from_me { "me@s.whatsapp.net".into() } else { CHAT.into() },
            wire_chat: CHAT.into(),
            wire_sender: None,
            push_name: None,
            from_me,
            body: MessageBody::Text(text.into()),
            timestamp: ts,
            status: if from_me { DeliveryStatus::Failed } else { DeliveryStatus::Delivered },
            media: None,
            quoted: None,
            expires_at: None,
        }
    }

    #[test]
    fn restamp_moves_a_resend_to_the_end() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message(&message("A", true, "first", 100), false).unwrap();
        s.insert_message(&message("B", false, "second", 200), false).unwrap();
        assert!(s.restamp(CHAT, "A", 300).unwrap());
        assert!(!s.restamp(CHAT, "B", 400).unwrap(), "only my own messages");
        let (page, _) = s.load_messages(CHAT, None, 10).unwrap();
        assert_eq!(page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["B", "A"]);
        assert_eq!(page[1].timestamp, 300);
        let chat = s.list_chats(0).unwrap().into_iter().find(|c| c.jid == CHAT).unwrap();
        assert_eq!(chat.last_message.unwrap().text, "first");
    }

    #[test]
    fn deleted_for_me_stays_deleted() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message(&message("A", false, "hello", 100), false).unwrap();
        assert!(s.delete_for_me(CHAT, "A", 500).unwrap());
        assert!(s.get_message(CHAT, "A").unwrap().is_none());
        assert!(s.is_deleted_for_me(CHAT, "A").unwrap());
        // A replayed copy (history sync) doesn't bring it back.
        assert!(!s.insert_message(&message("A", false, "hello", 100), false).unwrap());
        assert!(s.get_message(CHAT, "A").unwrap().is_none());
        // Not stored: remembered anyway.
        assert!(!s.delete_for_me(CHAT, "Z", 500).unwrap());
        assert!(!s.insert_message(&message("Z", false, "late", 90), false).unwrap());
    }

    #[test]
    fn forwarded_flag_round_trips() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message(&message("A", false, "fwd", 100), false).unwrap();
        assert!(!s.get_message(CHAT, "A").unwrap().unwrap().forwarded);
        s.set_forwarded(CHAT, "A").unwrap();
        assert!(s.get_message(CHAT, "A").unwrap().unwrap().forwarded);
    }

    #[test]
    fn search_matches_text_case_insensitively() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message(&message("A", false, "Dinner at Ümit's?", 100), false).unwrap();
        s.insert_message(&message("B", false, "no", 200), false).unwrap();
        s.insert_message(&message("C", true, "dinner works", 300), false).unwrap();
        s.insert_message(&message("D", false, "100% sure_thing", 400), false).unwrap();
        let ids = |q: &str| s.search_messages(CHAT, q).unwrap().into_iter().map(|h| h.id).collect::<Vec<_>>();
        assert_eq!(ids("DINNER"), ["A", "C"]);
        assert_eq!(ids("ümit"), ["A"]);
        assert_eq!(ids("%"), ["D"]);
        assert_eq!(ids("_"), ["D"]);
        assert!(ids("  ").is_empty());
        s.revoke(CHAT, "A", crate::extract::DELETED_LABEL).unwrap();
        assert_eq!(ids("dinner"), ["C"]);
        assert!(ids("deleted").is_empty());
    }
}
