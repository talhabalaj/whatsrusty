//! Chat-list state: pin / mute / archive flags, "Mark as Unread", contacts, new chats.

use anyhow::Result;
use rusqlite::{OptionalExtension as _, params};

use super::{ChatMeta, Store, chat_display_name};
use crate::types::Timestamp;

/// A chat's list flags, to put back if an app-state change can't be sent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChatFlags {
    pub pinned_at: i64,
    pub archived: bool,
    pub muted_until: i64,
    pub unread: u32,
    pub marked_unread: bool,
}

/// The newest message of a chat, for an app-state message range.
#[derive(Clone, Debug, PartialEq)]
pub struct LastMessageKey {
    pub id: String,
    pub from_me: bool,
    pub ts: Timestamp,
    pub wire_chat: String,
    pub wire_sender: Option<String>,
}

impl Store {
    pub fn chat_flags(&self, jid: &str) -> Result<Option<ChatFlags>> {
        let jid = self.canonical(jid)?;
        self.conn
            .query_row(
                "SELECT pinned_at, archived, muted_until, unread, marked_unread FROM chats WHERE jid = ?1",
                [&jid],
                |r| {
                    Ok(ChatFlags {
                        pinned_at: r.get(0)?,
                        archived: r.get(1)?,
                        muted_until: r.get(2)?,
                        unread: r.get(3)?,
                        marked_unread: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Write the flags exactly as given (a change, or putting back the state before it).
    pub fn set_chat_flags(&self, jid: &str, f: &ChatFlags) -> Result<()> {
        self.upsert_chat_meta(&ChatMeta {
            jid: jid.into(),
            unread: Some(f.unread),
            pinned_at: Some(f.pinned_at),
            archived: Some(f.archived),
            muted_until: Some(f.muted_until),
            ..Default::default()
        })?;
        let jid = self.canonical(jid)?;
        self.conn.execute("UPDATE chats SET marked_unread = ?2 WHERE jid = ?1", params![jid, f.marked_unread])?;
        Ok(())
    }

    /// "Mark as Unread" by hand (here or on another device). Shows as one unread message
    /// unless more arrive; cleared when the chat is read anywhere.
    pub fn set_marked_unread(&self, jid: &str, marked: bool) -> Result<()> {
        let jid = self.canonical(jid)?;
        self.ensure_chat(&jid)?;
        if marked {
            self.conn.execute("UPDATE chats SET marked_unread = 1, unread = MAX(unread, 1) WHERE jid = ?1", [&jid])?;
        } else {
            self.conn.execute("UPDATE chats SET marked_unread = 0 WHERE jid = ?1", [&jid])?;
        }
        Ok(())
    }

    /// The chat was read on another device (or here): nothing is unread any more.
    pub fn clear_unread(&self, jid: &str) -> Result<()> {
        self.set_unread(jid, 0)?;
        self.set_marked_unread(jid, false)
    }

    /// Pinned chats other than `except`.
    pub fn pinned_count(&self, except: &str) -> Result<u32> {
        let except = self.canonical(except)?;
        Ok(self.conn.query_row("SELECT COUNT(*) FROM chats WHERE pinned_at > 0 AND jid != ?1", [&except], |r| r.get(0))?)
    }

    pub fn last_message_key(&self, chat_jid: &str) -> Result<Option<LastMessageKey>> {
        let chat = self.canonical(chat_jid)?;
        self.conn
            .query_row(
                "SELECT id, from_me, ts, wire_chat, wire_sender FROM messages WHERE chat_jid = ?1
                 ORDER BY ts DESC, rowid DESC LIMIT 1",
                [&chat],
                |r| {
                    Ok(LastMessageKey {
                        id: r.get(0)?,
                        from_me: r.get(1)?,
                        ts: r.get(2)?,
                        wire_chat: r.get(3)?,
                        wire_sender: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// The soonest time a timed mute ends, after `now`.
    pub fn next_mute_expiry(&self, now: Timestamp) -> Result<Option<Timestamp>> {
        Ok(self.conn.query_row(
            "SELECT MIN(muted_until) FROM chats WHERE muted_until > ?1 AND muted_until < ?2",
            params![now, i64::MAX],
            |r| r.get(0),
        )?)
    }

    /// A 1:1 chat started from New Message: listed right away (at the top), before any
    /// message exists.
    pub fn open_chat(&self, jid: &str, now: Timestamp) -> Result<String> {
        let jid = self.canonical(jid)?;
        self.upsert_chat_meta(&ChatMeta { jid: jid.clone(), last_activity: Some(now), ..Default::default() })?;
        Ok(jid)
    }

    /// People I can start a chat with: saved contacts and everyone I have a 1:1 chat
    /// with, by name.
    pub fn contacts(&self) -> Result<Vec<crate::types::Contact>> {
        let mut stmt = self.conn.prepare(
            "SELECT j.jid, ct.full_name, ct.push_name, c.name FROM (
                SELECT jid FROM contacts WHERE full_name IS NOT NULL AND full_name != ''
                UNION SELECT jid FROM chats WHERE is_group = 0
             ) j LEFT JOIN contacts ct ON ct.jid = j.jid LEFT JOIN chats c ON c.jid = j.jid
             WHERE j.jid LIKE '%@s.whatsapp.net' OR j.jid LIKE '%@lid'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (jid, full, push, name) = row?;
            let saved = full.as_deref().is_some_and(|f| !f.is_empty());
            let phone = crate::profile::display_phone(&jid);
            let name = chat_display_name(&jid, false, name, full, push);
            out.push(crate::types::Contact { jid, name, phone, saved });
        }
        out.sort_by_cached_key(|c| (c.name.to_lowercase(), c.jid.clone()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewMessage;
    use crate::types::{DeliveryStatus, MessageBody};

    const ALICE: &str = "15550000001@s.whatsapp.net";
    const ALICE_LID: &str = "900000000001@lid";
    const BOB: &str = "15550000002@s.whatsapp.net";
    const GROUP: &str = "120363000000000009@g.us";

    fn incoming(chat: &str, id: &str, sender: &str, ts: i64) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: sender.into(),
            wire_chat: chat.into(),
            wire_sender: chat.ends_with("@g.us").then(|| sender.to_string()),
            push_name: None,
            from_me: false,
            body: MessageBody::Text("hi".into()),
            timestamp: ts,
            status: DeliveryStatus::Delivered,
            media: None,
            quoted: None,
            expires_at: None,
        }
    }

    fn flags(s: &Store, jid: &str) -> ChatFlags {
        s.chat_flags(jid).unwrap().unwrap()
    }

    #[test]
    fn marked_unread_lifecycle() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message(&incoming(ALICE, "a", ALICE, 10), true).unwrap();
        s.set_unread(ALICE, 0).unwrap();

        // Marked by hand: one unread, flagged.
        s.set_marked_unread(ALICE, true).unwrap();
        assert_eq!((flags(&s, ALICE).unread, flags(&s, ALICE).marked_unread), (1, true));
        // A real message on top keeps counting.
        s.insert_message(&incoming(ALICE, "b", ALICE, 11), true).unwrap();
        assert_eq!(flags(&s, ALICE).unread, 2);
        // Read on another device: nothing unread, no mark.
        s.clear_unread(ALICE).unwrap();
        assert_eq!(flags(&s, ALICE), ChatFlags { unread: 0, marked_unread: false, ..flags(&s, ALICE) });

        // Marking a chat with real unread keeps its count.
        s.insert_message(&incoming(ALICE, "c", ALICE, 12), true).unwrap();
        s.insert_message(&incoming(ALICE, "d", ALICE, 13), true).unwrap();
        s.set_marked_unread(ALICE, true).unwrap();
        assert_eq!(flags(&s, ALICE).unread, 2);
        // Un-marking alone leaves real unread messages unread.
        s.set_marked_unread(ALICE, false).unwrap();
        assert_eq!((flags(&s, ALICE).unread, flags(&s, ALICE).marked_unread), (2, false));

        // Addressed by LID: lands on the phone-number chat.
        s.add_alias(ALICE_LID, ALICE).unwrap();
        s.set_marked_unread(ALICE_LID, true).unwrap();
        assert!(flags(&s, ALICE).marked_unread);
    }

    #[test]
    fn flags_round_trip_and_pins() {
        let s = Store::open_in_memory().unwrap();
        for (jid, ts) in [(ALICE, 10), (BOB, 11)] {
            s.insert_message(&incoming(jid, "m", jid, ts), true).unwrap();
        }
        let before = flags(&s, ALICE);
        let after = ChatFlags { pinned_at: 5, archived: true, muted_until: 99, unread: 0, marked_unread: true };
        s.set_chat_flags(ALICE, &after).unwrap();
        assert_eq!(flags(&s, ALICE), after);
        s.set_chat_flags(ALICE, &before).unwrap();
        assert_eq!(flags(&s, ALICE), before, "reverted");

        s.set_pinned(ALICE, 5).unwrap();
        s.set_pinned(BOB, 6).unwrap();
        assert_eq!(s.pinned_count(ALICE).unwrap(), 1, "not counting itself");
        assert_eq!(s.pinned_count(GROUP).unwrap(), 2);
    }

    #[test]
    fn mute_expiry() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.next_mute_expiry(100).unwrap(), None);
        s.set_muted_until(ALICE, 500).unwrap();
        s.set_muted_until(BOB, 300).unwrap();
        s.set_muted_until(GROUP, i64::MAX).unwrap();
        assert_eq!(s.next_mute_expiry(100).unwrap(), Some(300));
        assert_eq!(s.next_mute_expiry(300).unwrap(), Some(500), "ended mutes don't count");
        assert_eq!(s.next_mute_expiry(500).unwrap(), None, "muted forever never ends");
    }

    #[test]
    fn last_message_and_new_chats() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.last_message_key(GROUP).unwrap(), None);
        s.insert_message(&incoming(GROUP, "g1", ALICE, 10), true).unwrap();
        s.insert_message(&incoming(GROUP, "g2", BOB, 20), true).unwrap();
        let last = s.last_message_key(GROUP).unwrap().unwrap();
        assert_eq!((last.id.as_str(), last.ts, last.wire_sender.as_deref()), ("g2", 20, Some(BOB)));

        // A new chat is listed before it has messages.
        let jid = s.open_chat(BOB, 1_000).unwrap();
        assert_eq!(jid, BOB);
        let chats = s.list_chats(1_000).unwrap();
        assert!(chats.iter().any(|c| c.jid == BOB && c.last_message.is_none()));
    }

    #[test]
    fn contacts_are_saved_or_chatted_with() {
        let s = Store::open_in_memory().unwrap();
        s.set_contact_name(ALICE, "Alice Smith").unwrap();
        // Only seen in a group: not offered.
        s.set_push_name("15550000003@s.whatsapp.net", "Stranger").unwrap();
        s.insert_message(&incoming(GROUP, "g", "15550000003@s.whatsapp.net", 10), true).unwrap();
        // A 1:1 chat without a saved name: offered under their WhatsApp name.
        s.set_push_name(BOB, "Bobby").unwrap();
        s.insert_message(&incoming(BOB, "b", BOB, 11), true).unwrap();

        let list = s.contacts().unwrap();
        let names: Vec<_> = list.iter().map(|c| (c.name.as_str(), c.saved)).collect();
        assert_eq!(names, [("Alice Smith", true), ("Bobby", false)]);
        assert!(list[0].phone.as_deref().is_some_and(|p| p.starts_with('+')));
    }
}
