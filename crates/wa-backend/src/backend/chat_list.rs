//! Chat-list commands: pin / mute / archive / mark unread (WhatsApp app state, so they
//! sync with the phone), contacts and phone-number lookups for New Message.

use std::sync::Arc;

use whatsapp_rust::{Jid, SyncActionMessageRange, message_key, message_range};

use super::{Session, Shared, now_secs, parse_jid};
use crate::store::{ChatFlags, LastMessageKey};
use crate::types::*;

/// WhatsApp allows this many pinned chats.
pub(super) const MAX_PINNED: u32 = 3;

#[derive(Clone, Copy, Debug)]
enum Change {
    Pin(bool),
    /// Unix time, `i64::MAX` = always, `0` = unmute.
    Mute(Timestamp),
    Archive(bool),
    MarkUnread,
}

impl Change {
    fn apply(self, mut f: ChatFlags, now: Timestamp) -> ChatFlags {
        match self {
            Change::Pin(p) => f.pinned_at = if p { now.max(1) } else { 0 },
            Change::Mute(until) => f.muted_until = until,
            Change::Archive(a) => {
                f.archived = a;
                // Archived chats can't stay pinned.
                if a {
                    f.pinned_at = 0;
                }
            }
            Change::MarkUnread => {
                f.marked_unread = true;
                f.unread = f.unread.max(1);
            }
        }
        f
    }

    fn failure(self) -> &'static str {
        match self {
            Change::Pin(true) => "Couldn't pin the chat",
            Change::Pin(false) => "Couldn't unpin the chat",
            Change::Mute(0) => "Couldn't unmute the chat",
            Change::Mute(_) => "Couldn't mute the chat",
            Change::Archive(true) => "Couldn't archive the chat",
            Change::Archive(false) => "Couldn't unarchive the chat",
            Change::MarkUnread => "Couldn't mark the chat as unread",
        }
    }
}

pub(super) async fn chat_command(shared: &Arc<Shared>, session: Option<&Arc<Session>>, cmd: Command) {
    match cmd {
        Command::SetPinned { chat_jid, pinned } => change_chat(shared, session, chat_jid, Change::Pin(pinned)),
        Command::SetMuted { chat_jid, until } => change_chat(shared, session, chat_jid, Change::Mute(until)),
        Command::SetArchived { chat_jid, archived } => change_chat(shared, session, chat_jid, Change::Archive(archived)),
        Command::MarkUnread { chat_jid } => change_chat(shared, session, chat_jid, Change::MarkUnread),
        Command::ListContacts => {
            let own = shared.own();
            let list = shared.store(|s| s.contacts()).unwrap_or_default();
            shared.emit(Event::Contacts(list.into_iter().filter(|c| !own.is_me(&c.jid)).collect()));
        }
        Command::LookupPhone { phone } => lookup_phone(shared, session, phone),
        Command::OpenChat { jid } => open_chat(shared, &jid),
        _ => {}
    }
}

fn open_chat(shared: &Shared, jid: &str) {
    if parse_jid(jid).is_some() && shared.store(|s| s.open_chat(jid, now_secs())).is_some() {
        // Right away (not debounced): the UI selects it next.
        shared.emit_chats();
    }
}

/// Apply a chat-list change locally, then tell the phone through app state; undone (with
/// an error) if that fails.
fn change_chat(shared: &Arc<Shared>, session: Option<&Arc<Session>>, chat_jid: String, change: Change) {
    let Some(session) = session.filter(|s| s.client.is_connected()) else {
        shared.emit(Event::Error(format!("{}: you're offline.", change.failure())));
        return;
    };
    if matches!(change, Change::Pin(true))
        && shared.store(|s| s.pinned_count(&chat_jid)).unwrap_or(0) >= MAX_PINNED
    {
        shared.emit(Event::Error(format!("You can only pin up to {MAX_PINNED} chats.")));
        return;
    }
    let Some(Some(before)) = shared.store(|s| s.chat_flags(&chat_jid)) else { return };
    let after = change.apply(before, now_secs());
    if after == before {
        return;
    }
    let generation = session.generation;
    shared.store_gen(generation, |s| s.set_chat_flags(&chat_jid, &after));
    shared.emit_chats();

    let (shared, client) = (shared.clone(), session.client.clone());
    session.track(tokio::spawn(async move {
        let Some((jid, range)) = app_state_target(&shared, &chat_jid) else { return };
        let actions = client.chat_actions();
        let result = match change {
            Change::Pin(true) => actions.pin_chat(&jid).await,
            Change::Pin(false) => actions.unpin_chat(&jid).await,
            Change::Mute(0) => actions.unmute_chat(&jid).await,
            Change::Mute(i64::MAX) => actions.mute_chat(&jid).await,
            Change::Mute(until) => actions.mute_chat_until(&jid, until.saturating_mul(1000)).await,
            Change::Archive(archived) => {
                let unpinned = match before.pinned_at > 0 && archived {
                    true => actions.unpin_chat(&jid).await,
                    false => Ok(()),
                };
                match (unpinned, archived) {
                    (Err(e), _) => Err(e),
                    (Ok(()), true) => actions.archive_chat(&jid, range).await,
                    (Ok(()), false) => actions.unarchive_chat(&jid, range).await,
                }
            }
            Change::MarkUnread => actions.mark_chat_as_read(&jid, false, range).await,
        };
        if let Err(e) = result {
            log::warn!("{} ({chat_jid}): {e}", change.failure());
            shared.store_gen(generation, |s| s.set_chat_flags(&chat_jid, &before));
            shared.emit_chats();
            shared.emit(Event::Error(format!("{}.", change.failure())));
        }
    }));
}

/// After `MarkRead`: a chat marked unread by hand is read again on the phone too.
/// (Read receipts alone don't clear the phone's "unread" mark.)
pub(super) fn sync_marked_read(shared: &Arc<Shared>, session: Option<&Arc<Session>>, chat_jid: &str) {
    let Some(Some(flags)) = shared.store(|s| s.chat_flags(chat_jid)) else { return };
    if !flags.marked_unread {
        return;
    }
    shared.store(|s| s.set_marked_unread(chat_jid, false));
    let Some(session) = session else { return };
    let (shared, client, chat_jid, generation) = (shared.clone(), session.client.clone(), chat_jid.to_string(), session.generation);
    session.track(tokio::spawn(async move {
        let Some((jid, range)) = app_state_target(&shared, &chat_jid) else { return };
        if let Err(e) = client.chat_actions().mark_chat_as_read(&jid, true, range).await {
            log::warn!("marking {chat_jid} read on the phone: {e}");
            // Try again on the next read.
            shared.store_gen(generation, |s| s.set_marked_unread(&chat_jid, true));
        }
    }));
}

/// The JID app-state mutations name a chat by, and the range of messages they apply to
/// (the newest one), which the phone uses to settle races with messages arriving later.
///
/// Chats are named as the server addresses them now: the newest message's wire chat
/// (a LID for LID-addressed 1:1 chats), falling back to the stored (phone number) JID.
fn app_state_target(shared: &Shared, chat_jid: &str) -> Option<(Jid, Option<SyncActionMessageRange>)> {
    let last = shared.store(|s| s.last_message_key(chat_jid)).flatten();
    let jid = last.as_ref().and_then(|l| parse_jid(&l.wire_chat)).or_else(|| parse_jid(chat_jid))?;
    let range = last.map(|l| range_for(&jid, &l));
    Some((jid, range))
}

fn range_for(jid: &Jid, last: &LastMessageKey) -> SyncActionMessageRange {
    let participant = match last.from_me {
        true => None,
        false => last.wire_sender.as_deref().and_then(parse_jid),
    };
    let key = message_key(last.id.clone(), jid, last.from_me, participant.as_ref());
    message_range(last.ts, None, vec![(key, last.ts)])
}

/// Digits of a typed phone number, if it can be one (with country code: 8-15 digits).
pub(crate) fn phone_digits(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.chars().any(|c| !(c.is_ascii_digit() || " +-().".contains(c))) {
        return Err("That doesn't look like a phone number.");
    }
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    // "00" is the international prefix in much of the world.
    let digits = digits.strip_prefix("00").map(str::to_string).unwrap_or(digits);
    if digits.starts_with('0') {
        return Err("Include the country code, e.g. +92 300 1234567.");
    }
    if !(8..=15).contains(&digits.len()) {
        return Err("Enter the full number with country code.");
    }
    Ok(digits)
}

fn lookup_phone(shared: &Arc<Shared>, session: Option<&Arc<Session>>, phone: String) {
    let reply = |shared: &Shared, phone: String, jid: Option<String>, error: Option<String>| {
        shared.emit(Event::PhoneLookup { phone, jid, error });
    };
    let digits = match phone_digits(&phone) {
        Ok(d) => d,
        Err(e) => return reply(shared, phone, None, Some(e.into())),
    };
    let Some(session) = session.filter(|s| s.client.is_connected()) else {
        return reply(shared, phone, None, Some("You're offline.".into()));
    };
    let (shared, client, generation) = (shared.clone(), session.client.clone(), session.generation);
    session.track(tokio::spawn(async move {
        let result = client.contacts().is_on_whatsapp(&[Jid::pn(digits.as_str())]).await;
        let (jid, error) = match result {
            Ok(results) => match results.into_iter().find(|r| r.is_registered) {
                Some(r) => {
                    let pn = r.pn_jid.filter(|j| j.is_pn()).unwrap_or_else(|| Jid::pn(digits.as_str()));
                    let pn = pn.to_non_ad_string();
                    // Learn the LID now, so a reply addressed by LID lands in the same chat.
                    let lid = r.lid.or_else(|| Some(r.jid).filter(|j| j.is_lid()));
                    if let Some(lid) = lid {
                        shared.store_gen(generation, |s| s.add_alias(&lid.to_non_ad_string(), &pn));
                    }
                    (Some(pn), None)
                }
                None => (None, None),
            },
            Err(e) => {
                log::warn!("is_on_whatsapp: {e}");
                (None, Some("Couldn't check the number. Try again.".to_string()))
            }
        };
        reply(&shared, phone, jid, error);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_numbers() {
        assert_eq!(phone_digits("+92 300 123-4567"), Ok("923001234567".into()));
        assert_eq!(phone_digits("0092 (300) 1234567"), Ok("923001234567".into()));
        assert!(phone_digits("0300 1234567").is_err(), "no country code");
        assert!(phone_digits("+1 555").is_err(), "too short");
        assert!(phone_digits("ayesha").is_err());
    }

    #[test]
    fn changes() {
        let pinned = ChatFlags { pinned_at: 5, ..Default::default() };
        let archived = Change::Archive(true).apply(pinned, 100);
        assert!(archived.archived && archived.pinned_at == 0, "archiving unpins");
        assert_eq!(Change::Pin(true).apply(ChatFlags::default(), 100).pinned_at, 100);
        let unread = Change::MarkUnread.apply(ChatFlags { unread: 3, ..Default::default() }, 0);
        assert_eq!((unread.unread, unread.marked_unread), (3, true), "keeps the real count");
        assert_eq!(Change::Mute(i64::MAX).apply(ChatFlags::default(), 0).muted_until, i64::MAX);
    }
}
