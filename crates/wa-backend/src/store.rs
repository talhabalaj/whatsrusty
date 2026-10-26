//! The app's own chat/message/contact store (`messages.db`). whatsapp-rust keeps only
//! protocol state, so everything the UI lists lives here.
//!
//! JIDs are stored without device parts. 1:1 chats are keyed by the phone-number JID
//! whenever the LID -> PN mapping is known; [`Store::add_alias`] merges a chat that was
//! first seen under its LID into the PN chat, so a contact never shows up twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::prelude::wa;

use crate::extract::{
    MediaKind, StoredMedia, fallback_name, is_group_jid, is_lid_jid, non_ad, parse_vcard, preview_text,
};
use crate::stickers::StickerSync;
use crate::types::{
    ChatMessage, ChatSummary, ContactCard, DeliveryStatus, LocationInfo, MediaState, MessageBody, MessagePreview,
    PollInfo, QuotedMessage, Reaction, RichText, Timestamp, ViewOnceKind,
};

mod chat_list;
mod message_actions;

pub use chat_list::{ChatFlags, LastMessageKey};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chats (
    jid           TEXT PRIMARY KEY,
    name          TEXT,
    is_group      INTEGER NOT NULL DEFAULT 0,
    unread        INTEGER NOT NULL DEFAULT 0,
    pinned_at     INTEGER NOT NULL DEFAULT 0,
    archived      INTEGER NOT NULL DEFAULT 0,
    muted_until   INTEGER NOT NULL DEFAULT 0,
    last_activity INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS messages (
    chat_jid    TEXT NOT NULL,
    id          TEXT NOT NULL,
    sender_jid  TEXT NOT NULL,
    wire_chat   TEXT NOT NULL,
    wire_sender TEXT,
    push_name   TEXT,
    from_me     INTEGER NOT NULL,
    kind        INTEGER NOT NULL,
    text        TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    status      INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, id)
);
CREATE INDEX IF NOT EXISTS messages_chat_ts ON messages (chat_jid, ts);
CREATE TABLE IF NOT EXISTS contacts (
    jid       TEXT PRIMARY KEY,
    full_name TEXT,
    push_name TEXT
);
CREATE TABLE IF NOT EXISTS aliases (
    lid TEXT PRIMARY KEY,
    pn  TEXT NOT NULL
);
"#;

/// Schema migrations, applied in order; `PRAGMA user_version` counts the applied ones.
/// Additive only (nullable / defaulted columns), so an older build can keep using a
/// migrated DB.
const MIGRATIONS: &[&str] = &[
    // 1: media + replies + avatars.
    r#"
    -- Encoded ImageMessage / AudioMessage proto (download info, thumbnail, dimensions,
    -- caption, duration, waveform, ptt), without its contextInfo.
    ALTER TABLE messages ADD COLUMN media BLOB;
    -- Downloaded (images) / decoded WAV (voice) file.
    ALTER TABLE messages ADD COLUMN media_path TEXT;
    ALTER TABLE messages ADD COLUMN media_state INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN quoted_id TEXT;
    ALTER TABLE messages ADD COLUMN quoted_sender TEXT;
    ALTER TABLE messages ADD COLUMN quoted_from_me INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN quoted_preview TEXT;
    CREATE TABLE IF NOT EXISTS avatars (
        jid         TEXT PRIMARY KEY,
        picture_id  TEXT,
        -- NULL = no picture / hidden by privacy.
        path        TEXT,
        fetched_at  INTEGER NOT NULL DEFAULT 0
    );
    "#,
    // 2: more message kinds (video, document, sticker, location, contacts, poll, rich, system).
    r#"
    -- Encoded proto with the details of location / contacts / poll / rich bodies (see
    -- `body_to_row`). Media kinds keep using `media`.
    ALTER TABLE messages ADD COLUMN payload BLOB;
    "#,
    // 3: edit / revoke ordering, disappearing messages, durable read receipts.
    r#"
    -- Timestamp of the edit the row shows (NULL = original); older edits and replayed
    -- originals never overwrite it.
    ALTER TABLE messages ADD COLUMN edit_ts INTEGER;
    -- Deleted for everyone: content is gone for good, whatever arrives later.
    ALTER TABLE messages ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0;
    -- Disappearing message: deleted at this time.
    ALTER TABLE messages ADD COLUMN expires_at INTEGER;
    CREATE INDEX IF NOT EXISTS messages_expires ON messages (expires_at) WHERE expires_at IS NOT NULL;
    -- Edits / revokes whose message isn't stored (yet); applied when it arrives.
    CREATE TABLE IF NOT EXISTS pending_edits (
        chat_jid  TEXT NOT NULL,
        id        TEXT NOT NULL,
        revoked   INTEGER NOT NULL DEFAULT 0,
        edit_ts   INTEGER NOT NULL DEFAULT 0,
        kind      INTEGER,
        text      TEXT,
        payload   BLOB,
        PRIMARY KEY (chat_jid, id)
    );
    -- Read receipts not yet acknowledged by the server.
    CREATE TABLE IF NOT EXISTS pending_receipts (
        chat_jid    TEXT NOT NULL,
        id          TEXT NOT NULL,
        wire_chat   TEXT NOT NULL,
        wire_sender TEXT,
        PRIMARY KEY (chat_jid, id)
    );
    "#,
    // 4: forget pending edits by when they were stored (not by when the edit was made).
    // Rows stored by older builds that turn out to be view-once are converted in Rust
    // (`forget_view_once_media`).
    r#"
    ALTER TABLE pending_edits ADD COLUMN stored_at INTEGER NOT NULL DEFAULT 0;
    UPDATE pending_edits SET stored_at = CAST(strftime('%s', 'now') AS INTEGER);
    "#,
    // 5: reactions.
    r#"
    -- One row per (message, reactor), also before the message itself arrives. An empty
    -- `emoji` is a removed reaction, kept so older copies (replays, history) can't bring
    -- it back: the row with the newest `ts_ms` (the sender's clock) wins.
    CREATE TABLE IF NOT EXISTS reactions (
        chat_jid   TEXT NOT NULL,
        target_id  TEXT NOT NULL,
        -- Canonical, like `messages.sender_jid`.
        sender_jid TEXT NOT NULL,
        from_me    INTEGER NOT NULL DEFAULT 0,
        emoji      TEXT NOT NULL,
        ts_ms      INTEGER NOT NULL,
        push_name  TEXT,
        stored_at  INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (chat_jid, target_id, sender_jid)
    );
    CREATE INDEX IF NOT EXISTS reactions_sender ON reactions (sender_jid);
    "#,
    // 6: sticker panel.
    r#"
    -- Favorite stickers, from WhatsApp app state (`favoriteSticker`), replaced as a
    -- whole on every read of the collection. `sha256` is the hex file hash.
    CREATE TABLE IF NOT EXISTS favorite_stickers (
        sha256   TEXT PRIMARY KEY,
        -- Encoded `SyncActionValue.StickerAction` (media fields).
        action   BLOB NOT NULL,
        -- When it was favorited (ms).
        ts_ms    INTEGER NOT NULL DEFAULT 0,
        -- Learnt from the file once downloaded (NULL = unknown).
        animated INTEGER
    );
    -- `removeRecentSticker`: my sends of it up to `last_sent_ms` aren't recents.
    CREATE TABLE IF NOT EXISTS removed_recent_stickers (
        sha256       TEXT PRIMARY KEY,
        last_sent_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS messages_stickers ON messages (ts) WHERE kind = 7;
    "#,
    // 7: delete for me, forwarded mark.
    r#"
    -- Messages deleted for me: never stored again (re-delivery, history sync).
    CREATE TABLE IF NOT EXISTS deleted_for_me (
        chat_jid   TEXT NOT NULL,
        id         TEXT NOT NULL,
        deleted_at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (chat_jid, id)
    );
    ALTER TABLE messages ADD COLUMN forwarded INTEGER NOT NULL DEFAULT 0;
    "#,
    // 8: chats marked unread by hand (so reading them tells the phone, too).
    r#"
    ALTER TABLE chats ADD COLUMN marked_unread INTEGER NOT NULL DEFAULT 0;
    "#,
];

// `kind` column. Older builds show unknown kinds as `Unsupported { label: text }`.
const KIND_TEXT: i64 = 0;
const KIND_UNSUPPORTED: i64 = 1;
const KIND_IMAGE: i64 = 2;
const KIND_VOICE: i64 = 3;
const KIND_VIDEO: i64 = 4;
/// Round video note.
const KIND_PTV: i64 = 5;
const KIND_DOCUMENT: i64 = 6;
const KIND_STICKER: i64 = 7;
const KIND_LOCATION: i64 = 8;
const KIND_CONTACTS: i64 = 9;
const KIND_POLL: i64 = 10;
const KIND_RICH: i64 = 11;
const KIND_SYSTEM: i64 = 12;
/// View-once media: `payload` is one byte, the [`ViewOnceKind`]. No media is ever kept.
const KIND_VIEW_ONCE: i64 = 13;

/// Pending edits/revokes of messages never seen are forgotten after this long.
const PENDING_EDIT_TTL_SECS: i64 = 30 * 24 * 3600;

fn media_kind(kind: i64) -> Option<MediaKind> {
    Some(match kind {
        KIND_IMAGE => MediaKind::Image,
        KIND_VOICE => MediaKind::Audio,
        KIND_VIDEO => MediaKind::Video,
        KIND_PTV => MediaKind::Ptv,
        KIND_DOCUMENT => MediaKind::Document,
        KIND_STICKER => MediaKind::Sticker,
        _ => return None,
    })
}

/// A message to insert. JIDs may be LIDs and may carry device parts; the store
/// canonicalizes them.
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub chat_jid: String,
    pub id: String,
    pub sender_jid: String,
    /// Chat JID exactly as the server sent it (receipts must address it).
    pub wire_chat: String,
    /// Group participant exactly as the server sent it.
    pub wire_sender: Option<String>,
    pub push_name: Option<String>,
    pub from_me: bool,
    pub body: MessageBody,
    pub timestamp: Timestamp,
    pub status: DeliveryStatus,
    /// Download info for `Image` / `Voice` / `Video` / `Document` / `Sticker` bodies.
    pub media: Option<StoredMedia>,
    pub quoted: Option<NewQuote>,
    /// Disappearing message: delete at this time.
    pub expires_at: Option<Timestamp>,
}

/// The message a new message replies to.
#[derive(Clone, Debug, PartialEq)]
pub struct NewQuote {
    pub id: String,
    /// Sender of the quoted message (any JID form; canonicalized on insert).
    pub sender: Option<String>,
    pub from_me: bool,
    pub preview: String,
}

/// Everything stored about one message that's needed to act on it (download its media,
/// quote it in a reply).
#[derive(Clone, Debug)]
pub struct RawMessage {
    pub chat_jid: String,
    pub id: String,
    pub sender_jid: String,
    pub wire_chat: String,
    pub wire_sender: Option<String>,
    pub from_me: bool,
    pub body: MessageBody,
    pub media: Option<StoredMedia>,
    /// Set and present on disk.
    pub media_path: Option<PathBuf>,
    pub timestamp: Timestamp,
    pub status: DeliveryStatus,
    /// When the shown content was last edited; `None` = never.
    pub edit_ts: Option<Timestamp>,
    pub revoked: bool,
    /// The message this one replies to.
    pub quoted_id: Option<String>,
}

/// A favorite sticker as stored. See [`Store::favorite_stickers`].
#[derive(Clone, Debug)]
pub struct FavoriteSticker {
    /// Hex SHA-256 of the sticker file.
    pub id: String,
    pub action: wa::sync_action_value::StickerAction,
    pub ts_ms: i64,
    pub animated: Option<bool>,
}

/// A stored sticker message. See [`Store::sticker_messages`].
#[derive(Clone, Debug)]
pub struct StickerRow {
    pub chat_jid: String,
    pub id: String,
    pub ts: Timestamp,
    pub proto: Box<wa::message::StickerMessage>,
    /// Its downloaded PNG (may be gone from disk).
    pub media_path: Option<PathBuf>,
}

/// Cached profile picture state for a JID.
#[derive(Clone, Debug, PartialEq)]
pub struct AvatarRow {
    pub picture_id: Option<String>,
    pub path: Option<PathBuf>,
    pub fetched_at: Timestamp,
}

/// See [`Store::local_profile`].
#[derive(Clone, Debug, PartialEq)]
pub struct LocalProfile {
    pub canonical: String,
    pub name: String,
    pub is_group: bool,
    pub avatar: Option<PathBuf>,
}

/// Chat metadata from a history-sync conversation. `None` leaves the field unchanged.
#[derive(Clone, Debug, Default)]
pub struct ChatMeta {
    pub jid: String,
    pub name: Option<String>,
    pub unread: Option<u32>,
    pub pinned_at: Option<i64>,
    pub archived: Option<bool>,
    pub muted_until: Option<i64>,
    pub last_activity: Option<i64>,
}

/// A reaction to store. `emoji` empty = the sender removed their reaction.
#[derive(Clone, Debug)]
pub struct NewReaction {
    pub chat_jid: String,
    pub target_id: String,
    /// Any JID form; canonicalized. Own reactions: our own JID.
    pub sender_jid: String,
    pub from_me: bool,
    pub emoji: String,
    /// `senderTimestampMs`: decides which of two reactions by the same sender is newer.
    pub ts_ms: i64,
    pub push_name: Option<String>,
}

/// An own reaction row as it was before an optimistic change, to put back if the send
/// fails.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnReaction {
    pub sender_jid: String,
    pub emoji: String,
    pub ts_ms: i64,
}

/// Receipt addressing for one unread incoming message.
#[derive(Debug, Clone)]
pub struct OldestRef {
    pub id: String,
    pub from_me: bool,
    pub ts: Timestamp,
    pub wire_chat: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnreadRef {
    pub id: String,
    pub wire_chat: String,
    pub wire_sender: Option<String>,
}

pub struct Store {
    conn: Connection,
    /// `None` for in-memory stores (tests).
    path: Option<PathBuf>,
    /// LID chats merged into PN chats (`(lid, pn)`) by committed changes, for the backend
    /// to announce. See [`Store::take_merges`].
    merges: std::cell::RefCell<Vec<(String, String)>>,
}

/// A message removed by [`Store::purge_expired`].
#[derive(Clone, Debug, PartialEq)]
pub struct Removed {
    pub chat_jid: String,
    pub id: String,
    pub media_path: Option<PathBuf>,
}

fn status_to_i64(s: DeliveryStatus) -> i64 {
    match s {
        DeliveryStatus::Failed => -1,
        DeliveryStatus::Pending => 0,
        DeliveryStatus::Sent => 1,
        DeliveryStatus::Delivered => 2,
        DeliveryStatus::Read => 3,
    }
}

fn status_from_i64(v: i64) -> DeliveryStatus {
    match v {
        i64::MIN..=-1 => DeliveryStatus::Failed,
        0 => DeliveryStatus::Pending,
        1 => DeliveryStatus::Sent,
        2 => DeliveryStatus::Delivered,
        _ => DeliveryStatus::Read,
    }
}

/// `kind`, `text` and `payload` columns. Rows keep their preview text in `text` (so older
/// builds still show something); media details live in the `media` proto, the details of
/// other structured bodies in `payload` (reusing WhatsApp's own protos as the format).
fn body_to_row(body: &MessageBody) -> (i64, String, Option<Vec<u8>>) {
    let text = preview_text(body).into_owned();
    match body {
        MessageBody::Text(t) => (KIND_TEXT, t.clone(), None),
        MessageBody::Unsupported { label } => (KIND_UNSUPPORTED, label.clone(), None),
        MessageBody::System { text } => (KIND_SYSTEM, text.clone(), None),
        MessageBody::Image(_) => (KIND_IMAGE, text, None),
        MessageBody::Voice(_) => (KIND_VOICE, text, None),
        MessageBody::Video(v) => (if v.round { KIND_PTV } else { KIND_VIDEO }, text, None),
        MessageBody::Document(_) => (KIND_DOCUMENT, text, None),
        MessageBody::Sticker(_) => (KIND_STICKER, text, None),
        MessageBody::Location(l) => (KIND_LOCATION, text, Some(encode_location(l))),
        MessageBody::Contacts(c) => (KIND_CONTACTS, text, Some(encode_contacts(c))),
        MessageBody::Poll(p) => (KIND_POLL, text, Some(encode_poll(p))),
        MessageBody::Rich(r) => (KIND_RICH, text, Some(encode_rich(r))),
        MessageBody::ViewOnce(k) => (KIND_VIEW_ONCE, text, Some(vec![view_once_to_u8(*k)])),
    }
}

fn view_once_to_u8(k: ViewOnceKind) -> u8 {
    match k {
        ViewOnceKind::Photo => 0,
        ViewOnceKind::Video => 1,
        ViewOnceKind::Voice => 2,
    }
}

fn view_once_from(p: &[u8]) -> Option<ViewOnceKind> {
    Some(match p.first()? {
        0 => ViewOnceKind::Photo,
        1 => ViewOnceKind::Video,
        2 => ViewOnceKind::Voice,
        _ => return None,
    })
}

fn encode_location(l: &LocationInfo) -> Vec<u8> {
    wa::message::LocationMessage {
        degrees_latitude: Some(l.latitude),
        degrees_longitude: Some(l.longitude),
        name: l.name.clone(),
        address: l.address.clone(),
        jpeg_thumbnail: l.thumbnail.clone(),
        is_live: Some(l.live),
        ..Default::default()
    }
    .encode_to_vec()
}

fn decode_location(b: &[u8]) -> Option<LocationInfo> {
    let l = wa::message::LocationMessage::decode_from_slice(b).ok()?;
    Some(LocationInfo {
        latitude: l.degrees_latitude.unwrap_or(0.0),
        longitude: l.degrees_longitude.unwrap_or(0.0),
        name: l.name,
        address: l.address,
        thumbnail: l.jpeg_thumbnail,
        live: l.is_live == Some(true),
    })
}

fn escape_vcard(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | ',' | ';' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Cards as a `ContactsArrayMessage` with minimal vCards (`FN` + `TEL`s).
fn encode_contacts(cards: &[ContactCard]) -> Vec<u8> {
    let contacts = cards
        .iter()
        .map(|c| {
            let mut vcard = format!("BEGIN:VCARD\nVERSION:3.0\nFN:{}\n", escape_vcard(&c.display_name));
            for phone in &c.phones {
                vcard.push_str(&format!("TEL:{}\n", escape_vcard(phone)));
            }
            vcard.push_str("END:VCARD");
            wa::message::ContactMessage {
                display_name: Some(c.display_name.clone()),
                vcard: Some(vcard),
                ..Default::default()
            }
        })
        .collect();
    wa::message::ContactsArrayMessage { contacts, ..Default::default() }.encode_to_vec()
}

fn decode_contacts(b: &[u8]) -> Option<Vec<ContactCard>> {
    let arr = wa::message::ContactsArrayMessage::decode_from_slice(b).ok()?;
    Some(
        arr.contacts
            .iter()
            .map(|c| {
                let vcard = parse_vcard(c.vcard.as_deref().unwrap_or(""));
                ContactCard { display_name: c.display_name.clone().unwrap_or_default(), phones: vcard.phones }
            })
            .collect(),
    )
}

fn encode_poll(p: &PollInfo) -> Vec<u8> {
    wa::message::PollCreationMessage {
        name: Some(p.question.clone()),
        options: p
            .options
            .iter()
            .map(|o| wa::message::poll_creation_message::Option {
                option_name: Some(o.clone()),
                ..Default::default()
            })
            .collect(),
        selectable_options_count: Some(if p.multiple { 0 } else { 1 }),
        ..Default::default()
    }
    .encode_to_vec()
}

fn decode_poll(b: &[u8]) -> Option<PollInfo> {
    let p = wa::message::PollCreationMessage::decode_from_slice(b).ok()?;
    Some(PollInfo {
        question: p.name.unwrap_or_default(),
        options: p.options.into_iter().map(|o| o.option_name.unwrap_or_default()).collect(),
        multiple: p.selectable_options_count != Some(1),
    })
}

/// A hydrated template has exactly the shape of [`RichText`].
fn encode_rich(r: &RichText) -> Vec<u8> {
    use wa::hydrated_template_button::{HydratedButton, HydratedQuickReplyButton};
    use wa::message::template_message::hydrated_four_row_template::Title;
    wa::message::template_message::HydratedFourRowTemplate {
        title: r.header.clone().map(Title::HydratedTitleText),
        hydrated_content_text: Some(r.body.clone()),
        hydrated_footer_text: r.footer.clone(),
        hydrated_buttons: r
            .buttons
            .iter()
            .map(|b| wa::HydratedTemplateButton {
                hydrated_button: Some(HydratedButton::QuickReplyButton(Box::new(HydratedQuickReplyButton {
                    display_text: Some(b.clone()),
                    ..Default::default()
                }))),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
    .encode_to_vec()
}

fn decode_rich(b: &[u8]) -> Option<RichText> {
    use wa::hydrated_template_button::HydratedButton;
    use wa::message::template_message::hydrated_four_row_template::Title;
    let t = wa::message::template_message::HydratedFourRowTemplate::decode_from_slice(b).ok()?;
    Some(RichText {
        header: match t.title {
            Some(Title::HydratedTitleText(h)) => Some(h),
            _ => None,
        },
        body: t.hydrated_content_text.unwrap_or_default(),
        footer: t.hydrated_footer_text,
        buttons: t
            .hydrated_buttons
            .into_iter()
            .filter_map(|b| match b.hydrated_button {
                Some(HydratedButton::QuickReplyButton(q)) => q.display_text,
                _ => None,
            })
            .collect(),
    })
}

fn media_state_to_i64(s: MediaState) -> i64 {
    match s {
        MediaState::Idle => 0,
        MediaState::Downloading => 1,
        MediaState::Failed => 2,
    }
}

fn media_state_from_i64(v: i64) -> MediaState {
    match v {
        1 => MediaState::Downloading,
        2 => MediaState::Failed,
        _ => MediaState::Idle,
    }
}

/// The stored path, if the file is still there.
fn existing_path(path: Option<String>) -> Option<PathBuf> {
    path.map(PathBuf::from).filter(|p| p.is_file())
}

fn body_from_row(
    kind: i64,
    text: String,
    media: Option<&[u8]>,
    payload: Option<&[u8]>,
    path: Option<PathBuf>,
    state: MediaState,
) -> (MessageBody, Option<StoredMedia>) {
    let media = media_kind(kind).zip(media).and_then(|(k, b)| StoredMedia::decode(b, k));
    if let Some(m) = &media {
        return (m.body(path, state), media);
    }
    let body = match (kind, payload) {
        (KIND_TEXT, _) => Some(MessageBody::Text(text.clone())),
        (KIND_SYSTEM, _) => Some(MessageBody::System { text: text.clone() }),
        (KIND_LOCATION, Some(p)) => decode_location(p).map(MessageBody::Location),
        (KIND_CONTACTS, Some(p)) => decode_contacts(p).map(MessageBody::Contacts),
        (KIND_POLL, Some(p)) => decode_poll(p).map(MessageBody::Poll),
        (KIND_RICH, Some(p)) => decode_rich(p).map(MessageBody::Rich),
        (KIND_VIEW_ONCE, Some(p)) => view_once_from(p).map(MessageBody::ViewOnce),
        _ => None,
    };
    // Unsupported, media rows from before media metadata was kept, unknown kinds.
    (body.unwrap_or(MessageBody::Unsupported { label: text }), None)
}

/// The downloaded file of a media body.
fn media_path(body: &MessageBody) -> Option<PathBuf> {
    match body {
        MessageBody::Image(m) => m.path.clone(),
        MessageBody::Voice(m) => m.path.clone(),
        MessageBody::Video(m) => m.path.clone(),
        MessageBody::Document(m) => m.path.clone(),
        MessageBody::Sticker(m) => m.path.clone(),
        _ => None,
    }
}

/// The caption of a photo / video / document body (`Some(None)` = none set).
fn caption_of(body: &MessageBody) -> Option<Option<String>> {
    match body {
        MessageBody::Image(m) => Some(m.caption.clone()),
        MessageBody::Video(m) => Some(m.caption.clone()),
        MessageBody::Document(m) => Some(m.caption.clone()),
        _ => None,
    }
}

const MESSAGE_COLS: &str = "chat_jid, id, sender_jid, push_name, from_me, kind, text, ts, status, rowid, \
    media, media_path, media_state, quoted_id, quoted_sender, quoted_from_me, quoted_preview, wire_chat, wire_sender, \
    payload, edit_ts, revoked, forwarded";

struct MessageRow {
    chat_jid: String,
    id: String,
    sender_jid: String,
    push_name: Option<String>,
    from_me: bool,
    body: MessageBody,
    media: Option<StoredMedia>,
    ts: i64,
    status: DeliveryStatus,
    rowid: i64,
    quoted: Option<StoredQuote>,
    wire_chat: String,
    wire_sender: Option<String>,
    /// When the shown content was last edited; `None` = never.
    edit_ts: Option<i64>,
    revoked: bool,
    forwarded: bool,
}

struct StoredQuote {
    id: String,
    sender: Option<String>,
    from_me: bool,
    preview: String,
}

impl MessageRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let media: Option<Vec<u8>> = row.get(10)?;
        let payload: Option<Vec<u8>> = row.get(19)?;
        let path = existing_path(row.get(11)?);
        let state = media_state_from_i64(row.get(12)?);
        let (body, media) = body_from_row(row.get(5)?, row.get(6)?, media.as_deref(), payload.as_deref(), path, state);
        let quoted = match row.get::<_, Option<String>>(13)? {
            Some(id) => Some(StoredQuote {
                id,
                sender: row.get(14)?,
                from_me: row.get(15)?,
                preview: row.get::<_, Option<String>>(16)?.unwrap_or_default(),
            }),
            None => None,
        };
        Ok(Self {
            chat_jid: row.get(0)?,
            id: row.get(1)?,
            sender_jid: row.get(2)?,
            push_name: row.get(3)?,
            from_me: row.get(4)?,
            body,
            media,
            ts: row.get(7)?,
            status: status_from_i64(row.get(8)?),
            rowid: row.get(9)?,
            quoted,
            wire_chat: row.get(17)?,
            wire_sender: row.get(18)?,
            edit_ts: row.get(20)?,
            revoked: row.get(21)?,
            forwarded: row.get(22)?,
        })
    }
}

impl Store {
    /// Open (or create) the store. The file is created owner-only (SQLite gives its
    /// `-wal` / `-shm` files the same mode); existing files get their mode fixed.
    pub fn open(path: &Path) -> Result<Self> {
        crate::media::create_private_file(path)?;
        let store = Self::init(Connection::open(path)?, Some(path.to_owned()))?;
        for file in db_files(path) {
            crate::media::restrict_file(&file);
        }
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        // `secure_delete`: deleted rows (revoked messages, wiped chats) are zeroed, not
        // left in free pages.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA secure_delete = ON;")?;
        conn.execute_batch(SCHEMA)?;
        let from_version = Self::migrate(&conn)?;
        let store = Self { conn, path, merges: Default::default() };
        if from_version < 4 {
            store.forget_view_once_media()?;
        }
        // Downloads don't survive a restart, nor do sends cut off by a crash (they
        // show as failed, so the UI offers a retry).
        store.reset_downloading()?;
        store.fail_pending()?;
        Ok(store)
    }

    /// Apply pending migrations. Returns the schema version the database had.
    fn migrate(conn: &Connection) -> Result<i64> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
            conn.execute_batch(&format!("BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;", i + 1))
                .inspect_err(|_| {
                    let _ = conn.execute_batch("ROLLBACK");
                })?;
            log::info!("messages.db migrated to schema {}", i + 1);
        }
        Ok(version)
    }

    /// Older builds stored view-once photos / videos / voice notes as regular media
    /// (and may have downloaded them). Those whose proto carries the `viewOnce` flag
    /// become placeholders, and their files are deleted. (Ones only marked by a
    /// wrapper can't be recognised anymore.)
    fn forget_view_once_media(&self) -> Result<()> {
        /// chat, id, kind, media proto, media path
        type Row = (String, String, i64, Vec<u8>, Option<String>);
        let rows: Vec<Row> = self
            .conn
            .prepare("SELECT chat_jid, id, kind, media, media_path FROM messages WHERE kind IN (?1, ?2, ?3, ?4) AND media IS NOT NULL")?
            .query_map(params![KIND_IMAGE, KIND_VIDEO, KIND_PTV, KIND_VOICE], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (chat, id, kind, media, path) in rows {
            let Some(kind) = media_kind(kind).and_then(|k| StoredMedia::decode(&media, k)).and_then(|m| m.view_once())
            else {
                continue;
            };
            let (kind, text, payload) = body_to_row(&MessageBody::ViewOnce(kind));
            self.conn.execute(
                "UPDATE messages SET kind = ?3, text = ?4, payload = ?5, media = NULL, media_path = NULL, media_state = 0
                 WHERE chat_jid = ?1 AND id = ?2",
                params![chat, id, kind, text, payload],
            )?;
            if let Some(path) = path {
                remove_media_file(Path::new(&path));
            }
        }
        Ok(())
    }

    /// Run `f` in one transaction (history-sync chunks insert thousands of rows).
    pub fn transaction<R>(&mut self, f: impl FnOnce(&Self) -> Result<R>) -> Result<R> {
        let merges = self.merges.borrow().len();
        self.conn.execute_batch("BEGIN")?;
        let result = f(self).and_then(|r| {
            self.conn.execute_batch("COMMIT")?;
            Ok(r)
        });
        if result.is_err() {
            let _ = self.conn.execute_batch("ROLLBACK");
            self.merges.borrow_mut().truncate(merges);
        }
        result
    }

    /// Run `f` in a savepoint (nests inside [`Store::transaction`]).
    fn savepoint<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R> {
        self.conn.execute_batch("SAVEPOINT sp")?;
        match f() {
            Ok(r) => {
                self.conn.execute_batch("RELEASE sp")?;
                Ok(r)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO sp; RELEASE sp");
                Err(e)
            }
        }
    }

    /// Chat merges (`(lid, pn)`) since the last call.
    pub fn take_merges(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.merges.borrow_mut())
    }

    /// Checkpoint and close the database (shutdown). The store keeps working in memory.
    pub fn close(&mut self) -> Result<()> {
        if self.path.take().is_none() {
            return Ok(());
        }
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        let old = std::mem::replace(&mut self.conn, Connection::open_in_memory()?);
        old.close().map_err(|(_, e)| anyhow::Error::new(e).context("closing messages.db"))
    }

    /// Forget everything (logout): close the database, delete its files (`-wal` /
    /// `-shm` included) and start over with an empty one, so no old data survives in
    /// free pages or the WAL. In-memory stores just delete all rows.
    pub fn wipe(&mut self) -> Result<()> {
        let Some(path) = self.path.clone() else { return self.clear_all() };
        let old = std::mem::replace(&mut self.conn, Connection::open_in_memory()?);
        if let Err((conn, e)) = old.close() {
            // Still open: at least delete the rows (zeroed by `secure_delete`) and shrink.
            self.conn = conn;
            self.clear_all()?;
            self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
            return Err(anyhow::Error::new(e).context("closing messages.db for the wipe"));
        }
        let removed = db_files(&path).iter().try_for_each(|f| match std::fs::remove_file(f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        });
        let reopened = Self::open(&path);
        match (removed, reopened) {
            (Ok(()), Ok(store)) => {
                *self = store;
                Ok(())
            }
            (removed, reopened) => {
                // Keep working (in memory) rather than on a half-deleted file.
                *self = Self::init(Connection::open_in_memory()?, None)?;
                removed?;
                reopened.map(|_| ())
            }
        }
    }

    /// Delete all rows.
    pub fn clear_all(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "DELETE FROM chats; DELETE FROM messages; DELETE FROM contacts; DELETE FROM aliases;
                 DELETE FROM avatars; DELETE FROM pending_edits; DELETE FROM pending_receipts;
                 DELETE FROM reactions; DELETE FROM favorite_stickers; DELETE FROM removed_recent_stickers;",
            )?;
        Ok(())
    }

    /// Canonical key for a JID: device parts stripped, LIDs replaced by their PN when known.
    pub fn canonical(&self, jid: &str) -> Result<String> {
        let jid = non_ad(jid);
        if !is_lid_jid(&jid) {
            return Ok(jid);
        }
        let pn: Option<String> = self
            .conn
            .prepare_cached("SELECT pn FROM aliases WHERE lid = ?1")?
            .query_row([&jid], |r| r.get(0))
            .optional()?;
        Ok(pn.unwrap_or(jid))
    }

    /// Record a LID <-> PN mapping and fold anything stored under the LID into the PN,
    /// atomically. Returns true if something changed.
    pub fn add_alias(&self, lid: &str, pn: &str) -> Result<bool> {
        let (lid, pn) = (non_ad(lid), non_ad(pn));
        if !is_lid_jid(&lid) || !pn.ends_with("@s.whatsapp.net") {
            return Ok(false);
        }
        let known: Option<String> = self
            .conn
            .prepare_cached("SELECT pn FROM aliases WHERE lid = ?1")?
            .query_row([&lid], |r| r.get(0))
            .optional()?;
        if known.as_deref() == Some(pn.as_str()) {
            return Ok(false);
        }
        let (merged, orphans) = self.savepoint(|| self.merge_lid(&lid, &pn))?;
        for path in orphans {
            remove_media_file(&path);
        }
        if merged {
            self.merges.borrow_mut().push((lid, pn));
        }
        Ok(true)
    }

    /// The body of [`Store::add_alias`]. Returns whether a LID chat existed, and media
    /// files no row refers to anymore.
    fn merge_lid(&self, lid: &str, pn: &str) -> Result<(bool, Vec<PathBuf>)> {
        let c = &self.conn;
        c.execute("INSERT OR REPLACE INTO aliases (lid, pn) VALUES (?1, ?2)", params![lid, pn])?;

        // Chat metadata.
        let lid_chat_exists: bool =
            c.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE jid = ?1)", [lid], |r| r.get(0))?;
        if lid_chat_exists {
            c.execute("INSERT OR IGNORE INTO chats (jid) VALUES (?1)", [pn])?;
            c.execute(
                "UPDATE chats SET
                    name = COALESCE(chats.name, l.name),
                    unread = chats.unread + l.unread,
                    pinned_at = MAX(chats.pinned_at, l.pinned_at),
                    archived = CASE WHEN chats.last_activity >= l.last_activity THEN chats.archived ELSE l.archived END,
                    muted_until = MAX(chats.muted_until, l.muted_until),
                    last_activity = MAX(chats.last_activity, l.last_activity)
                 FROM (SELECT * FROM chats WHERE jid = ?1) AS l
                 WHERE chats.jid = ?2",
                params![lid, pn],
            )?;
            c.execute("DELETE FROM chats WHERE jid = ?1", [lid])?;
        }

        // Messages stored under both: merge the LID copy into the PN copy field by field.
        // The content comes from the newer version (a revoke beats any edit, a later edit
        // an earlier one); delivery status is the higher; a downloaded file, media info,
        // quote and expiry are kept from whichever copy has them.
        let mut orphans = Vec::new();
        {
            let mut stmt = c.prepare(
                "SELECT l.media_path, p.media_path, MAX(l.revoked, p.revoked) FROM messages l
                 JOIN messages p ON p.chat_jid = ?2 AND p.id = l.id WHERE l.chat_jid = ?1",
            )?;
            let rows = stmt.query_map(params![lid, pn], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, bool>(2)?))
            })?;
            for row in rows {
                let (l_path, p_path, revoked) = row?;
                // Files that won't be referenced: both if revoked, else the LID copy's
                // if the PN copy keeps its own.
                if revoked {
                    orphans.extend(l_path.iter().chain(p_path.iter()).map(PathBuf::from));
                } else if let (Some(l), Some(p)) = (l_path, p_path)
                    && l != p
                {
                    orphans.push(PathBuf::from(l));
                }
            }
        }
        // `W`: the LID copy has the newer content.
        const W: &str = "(l.revoked > messages.revoked OR (l.revoked = messages.revoked
            AND COALESCE(l.edit_ts, -1) > COALESCE(messages.edit_ts, -1)))";
        const R: &str = "MAX(l.revoked, messages.revoked)";
        c.execute(
            &format!(
                "UPDATE messages SET
                    status = MAX(messages.status, l.status),
                    kind = CASE WHEN {W} THEN l.kind ELSE messages.kind END,
                    text = CASE WHEN {W} THEN l.text ELSE messages.text END,
                    payload = CASE WHEN {W} THEN l.payload ELSE messages.payload END,
                    edit_ts = CASE WHEN {W} THEN l.edit_ts ELSE messages.edit_ts END,
                    media = CASE WHEN {R} OR (CASE WHEN {W} THEN l.kind ELSE messages.kind END) = {KIND_VIEW_ONCE}
                        THEN NULL
                        WHEN {W} THEN COALESCE(l.media, messages.media)
                        ELSE COALESCE(messages.media, l.media) END,
                    media_path = CASE WHEN {R} THEN NULL ELSE COALESCE(messages.media_path, l.media_path) END,
                    media_state = CASE WHEN {R} OR COALESCE(messages.media_path, l.media_path) IS NOT NULL THEN 0
                        ELSE messages.media_state END,
                    revoked = {R},
                    quoted_id = CASE WHEN {R} THEN NULL WHEN messages.quoted_id IS NULL THEN l.quoted_id ELSE messages.quoted_id END,
                    quoted_sender = CASE WHEN {R} THEN NULL WHEN messages.quoted_id IS NULL THEN l.quoted_sender ELSE messages.quoted_sender END,
                    quoted_from_me = CASE WHEN {R} THEN 0 WHEN messages.quoted_id IS NULL THEN l.quoted_from_me ELSE messages.quoted_from_me END,
                    quoted_preview = CASE WHEN {R} THEN NULL WHEN messages.quoted_id IS NULL THEN l.quoted_preview ELSE messages.quoted_preview END,
                    push_name = COALESCE(messages.push_name, l.push_name),
                    expires_at = COALESCE(messages.expires_at, l.expires_at)
                 FROM (SELECT * FROM messages WHERE chat_jid = ?1) AS l
                 WHERE messages.chat_jid = ?2 AND messages.id = l.id"
            ),
            params![lid, pn],
        )?;
        c.execute(
            "DELETE FROM messages WHERE chat_jid = ?1 AND id IN (SELECT id FROM messages WHERE chat_jid = ?2)",
            params![lid, pn],
        )?;
        c.execute("UPDATE messages SET chat_jid = ?2 WHERE chat_jid = ?1", params![lid, pn])?;
        c.execute("UPDATE OR IGNORE deleted_for_me SET chat_jid = ?2 WHERE chat_jid = ?1", params![lid, pn])?;
        c.execute("UPDATE messages SET sender_jid = ?2 WHERE sender_jid = ?1", params![lid, pn])?;
        c.execute("UPDATE messages SET quoted_sender = ?2 WHERE quoted_sender = ?1", params![lid, pn])?;
        c.execute("UPDATE OR IGNORE avatars SET jid = ?2 WHERE jid = ?1", params![lid, pn])?;
        c.execute("DELETE FROM avatars WHERE jid = ?1", [lid])?;

        // Pending edits (the newer one wins) and receipts.
        c.execute(
            "INSERT INTO pending_edits (chat_jid, id, revoked, edit_ts, kind, text, payload, stored_at)
             SELECT ?2, id, revoked, edit_ts, kind, text, payload, stored_at FROM pending_edits WHERE chat_jid = ?1 AND true
             ON CONFLICT (chat_jid, id) DO UPDATE SET
                stored_at = MAX(stored_at, excluded.stored_at),
                revoked = MAX(revoked, excluded.revoked),
                edit_ts = CASE WHEN excluded.edit_ts > edit_ts THEN excluded.edit_ts ELSE edit_ts END,
                kind = CASE WHEN excluded.edit_ts > edit_ts THEN excluded.kind ELSE kind END,
                text = CASE WHEN excluded.edit_ts > edit_ts THEN excluded.text ELSE text END,
                payload = CASE WHEN excluded.edit_ts > edit_ts THEN excluded.payload ELSE payload END",
            params![lid, pn],
        )?;
        c.execute("DELETE FROM pending_edits WHERE chat_jid = ?1", [lid])?;
        c.execute(
            "INSERT OR IGNORE INTO pending_receipts (chat_jid, id, wire_chat, wire_sender)
             SELECT ?2, id, wire_chat, wire_sender FROM pending_receipts WHERE chat_jid = ?1",
            params![lid, pn],
        )?;
        c.execute("DELETE FROM pending_receipts WHERE chat_jid = ?1", [lid])?;
        // Reactions in the LID chat, and by the LID sender anywhere (group chats): one
        // per person, the newer one wins. Deleted messages lose theirs.
        c.execute(
            "INSERT INTO reactions (chat_jid, target_id, sender_jid, from_me, emoji, ts_ms, push_name, stored_at)
             SELECT CASE WHEN chat_jid = ?1 THEN ?2 ELSE chat_jid END, target_id,
                    CASE WHEN sender_jid = ?1 THEN ?2 ELSE sender_jid END, from_me, emoji, ts_ms, push_name, stored_at
             FROM reactions WHERE chat_jid = ?1 OR sender_jid = ?1
             ON CONFLICT (chat_jid, target_id, sender_jid) DO UPDATE SET
                emoji = CASE WHEN excluded.ts_ms > ts_ms THEN excluded.emoji ELSE emoji END,
                from_me = CASE WHEN excluded.ts_ms > ts_ms THEN excluded.from_me ELSE from_me END,
                ts_ms = MAX(ts_ms, excluded.ts_ms),
                push_name = COALESCE(push_name, excluded.push_name),
                stored_at = MAX(stored_at, excluded.stored_at)",
            params![lid, pn],
        )?;
        c.execute("DELETE FROM reactions WHERE chat_jid = ?1 OR sender_jid = ?1", [lid])?;
        c.execute(
            "DELETE FROM reactions WHERE chat_jid = ?1
                AND target_id IN (SELECT id FROM messages WHERE chat_jid = ?1 AND revoked = 1)",
            [pn],
        )?;
        // Edits that were waiting under one JID for a message stored under the other.
        let waiting: Vec<String> = c
            .prepare(
                "SELECT e.id FROM pending_edits e JOIN messages m ON m.chat_jid = e.chat_jid AND m.id = e.id
                 WHERE e.chat_jid = ?1",
            )?
            .query_map([pn], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in waiting {
            self.apply_pending_edit(pn, &id)?;
        }

        // Contact names.
        let lid_contact_exists: bool =
            c.query_row("SELECT EXISTS(SELECT 1 FROM contacts WHERE jid = ?1)", [lid], |r| r.get(0))?;
        if lid_contact_exists {
            c.execute("INSERT OR IGNORE INTO contacts (jid) VALUES (?1)", [pn])?;
            c.execute(
                "UPDATE contacts SET
                    full_name = COALESCE(contacts.full_name, l.full_name),
                    push_name = COALESCE(contacts.push_name, l.push_name)
                 FROM (SELECT * FROM contacts WHERE jid = ?1) AS l
                 WHERE contacts.jid = ?2",
                params![lid, pn],
            )?;
            c.execute("DELETE FROM contacts WHERE jid = ?1", [lid])?;
        }
        Ok((lid_chat_exists, orphans))
    }

    /// LID-keyed chats whose PN we don't know yet.
    pub fn lid_chats(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT jid FROM chats WHERE jid LIKE '%@lid'")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn ensure_chat(&self, jid: &str) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO chats (jid, is_group) VALUES (?1, ?2)")?
            .execute(params![jid, is_group_jid(jid)])?;
        Ok(())
    }

    /// Insert a message, or update body/status of an existing one. Returns true if a new
    /// row was created. Unread is bumped only for new incoming rows when `count_unread`.
    pub fn insert_message(&self, m: &NewMessage, count_unread: bool) -> Result<bool> {
        let chat = self.canonical(&m.chat_jid)?;
        let sender = self.canonical(&m.sender_jid)?;
        if self.is_deleted_for_me(&chat, &m.id)? {
            return Ok(false);
        }
        self.ensure_chat(&chat)?;
        let (kind, text, payload) = body_to_row(&m.body);
        let status = status_to_i64(m.status);
        let push_name = m.push_name.as_deref().filter(|p| !p.is_empty());
        let media = m.media.as_ref().map(StoredMedia::encode);
        let (q_id, q_sender, q_from_me, q_preview) = match &m.quoted {
            Some(q) => {
                let sender = q.sender.as_deref().map(|s| self.canonical(s)).transpose()?;
                (Some(q.id.as_str()), sender, q.from_me, Some(q.preview.as_str()))
            }
            None => (None, None, false, None),
        };
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO messages
                    (chat_jid, id, sender_jid, wire_chat, wire_sender, push_name, from_me, kind, text, ts, status,
                     media, quoted_id, quoted_sender, quoted_from_me, quoted_preview, payload, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            )?
            .execute(params![
                chat,
                m.id,
                sender,
                m.wire_chat,
                m.wire_sender,
                push_name,
                m.from_me,
                kind,
                text,
                m.timestamp,
                status,
                media,
                q_id,
                q_sender,
                q_from_me,
                q_preview,
                payload,
                m.expires_at
            ])?
            == 1;
        let mut revoked = false;
        if inserted {
            // An edit or revoke may have arrived first.
            revoked = self.apply_pending_edit(&chat, &m.id)?;
        } else {
            // Re-delivery / history re-sync: refresh the content, but keep a downloaded
            // file and anything the new copy lacks. An edited or revoked message keeps
            // its newer content: the copy is its original.
            let view_once = kind == KIND_VIEW_ONCE;
            let old_path: Option<String> = if view_once {
                self.conn
                    .prepare_cached("SELECT media_path FROM messages WHERE chat_jid = ?1 AND id = ?2 AND revoked = 0")?
                    .query_row(params![chat, m.id], |r| r.get(0))
                    .optional()?
                    .flatten()
            } else {
                None
            };
            self.conn
                .prepare_cached(
                    "UPDATE messages SET
                        status = CASE WHEN ?3 > status THEN ?3 ELSE status END,
                        push_name = COALESCE(?4, push_name),
                        expires_at = COALESCE(?5, expires_at)
                     WHERE chat_jid = ?1 AND id = ?2",
                )?
                .execute(params![chat, m.id, status, push_name, m.expires_at])?;
            self.conn
                .prepare_cached(
                    "UPDATE messages SET kind = ?3, text = ?4,
                        media = CASE WHEN ?3 = ?11 THEN NULL ELSE COALESCE(?5, media) END,
                        media_path = CASE WHEN ?3 = ?11 THEN NULL ELSE media_path END,
                        media_state = CASE WHEN ?3 = ?11 THEN 0 ELSE media_state END,
                        quoted_id = COALESCE(?6, quoted_id),
                        quoted_sender = COALESCE(?7, quoted_sender),
                        quoted_from_me = CASE WHEN ?6 IS NULL THEN quoted_from_me ELSE ?8 END,
                        quoted_preview = COALESCE(?9, quoted_preview),
                        payload = ?10
                     WHERE chat_jid = ?1 AND id = ?2 AND revoked = 0 AND edit_ts IS NULL",
                )?
                .execute(params![
                    chat,
                    m.id,
                    kind,
                    text,
                    media,
                    q_id,
                    q_sender,
                    q_from_me,
                    q_preview,
                    payload,
                    KIND_VIEW_ONCE
                ])?;
            if let Some(path) = old_path {
                remove_media_file(Path::new(&path));
            }
        }
        let bump = i64::from(inserted && count_unread && !m.from_me && !revoked);
        self.conn
            .prepare_cached(
                "UPDATE chats SET last_activity = MAX(last_activity, ?2), unread = unread + ?3 WHERE jid = ?1",
            )?
            .execute(params![chat, m.timestamp, bump])?;
        Ok(inserted)
    }

    /// Apply an edit made at `edit_ts`. Returns the updated message, or `None` if nothing
    /// changed: the message is revoked or shows a newer edit already, or isn't stored yet
    /// (then the edit waits for it).
    pub fn apply_edit(&self, chat_jid: &str, id: &str, body: &MessageBody, edit_ts: Timestamp) -> Result<Option<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let state: Option<(bool, Option<i64>)> = self
            .conn
            .prepare_cached("SELECT revoked, edit_ts FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match state {
            None => {
                let (kind, text, payload) = body_to_row(&pending_edit_body(body));
                self.conn
                    .prepare_cached(
                        "INSERT INTO pending_edits (chat_jid, id, edit_ts, kind, text, payload, stored_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT (chat_jid, id) DO UPDATE SET edit_ts = excluded.edit_ts, kind = excluded.kind,
                            text = excluded.text, payload = excluded.payload, stored_at = excluded.stored_at
                         WHERE excluded.edit_ts >= pending_edits.edit_ts",
                    )?
                    .execute(params![chat, id, edit_ts, kind, text, payload, crate::backend::now_secs()])?;
                Ok(None)
            }
            Some((true, _)) => Ok(None),
            Some((false, Some(shown))) if shown > edit_ts => Ok(None),
            Some(_) => {
                self.set_body(&chat, id, body)?;
                self.conn.execute(
                    "UPDATE messages SET edit_ts = ?3 WHERE chat_jid = ?1 AND id = ?2",
                    params![chat, id, edit_ts],
                )?;
                self.get_message(&chat, id)
            }
        }
    }

    /// Undo an edit of mine that wasn't sent: put back `previous` and its `edit_ts`
    /// (`None` = never edited), unless the message shows another edit by now or was
    /// deleted. Returns the updated message if reverted.
    pub fn revert_edit(
        &self,
        chat_jid: &str,
        id: &str,
        applied_ts: Timestamp,
        previous: &MessageBody,
        previous_ts: Option<Timestamp>,
    ) -> Result<Option<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let state: Option<(bool, Option<i64>)> = self
            .conn
            .prepare_cached("SELECT revoked, edit_ts FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if state != Some((false, Some(applied_ts))) {
            return Ok(None);
        }
        self.set_body(&chat, id, previous)?;
        self.conn.execute(
            "UPDATE messages SET edit_ts = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat, id, previous_ts],
        )?;
        self.get_message(&chat, id)
    }

    /// Delete a message for everyone: its content, quote and downloaded file go, for
    /// good. A revoke of a message not stored (yet) is remembered. Returns the updated
    /// message, if stored and not already revoked.
    pub fn revoke(&self, chat_jid: &str, id: &str, label: &str) -> Result<Option<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let revoked: Option<bool> = self
            .conn
            .prepare_cached("SELECT revoked FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, id], |r| r.get(0))
            .optional()?;
        match revoked {
            None => {
                self.conn
                    .prepare_cached(
                        "INSERT INTO pending_edits (chat_jid, id, revoked, stored_at) VALUES (?1, ?2, 1, ?3)
                         ON CONFLICT (chat_jid, id) DO UPDATE SET revoked = 1, stored_at = excluded.stored_at",
                    )?
                    .execute(params![chat, id, crate::backend::now_secs()])?;
                Ok(None)
            }
            Some(true) => Ok(None),
            Some(false) => {
                self.set_body(&chat, id, &MessageBody::Unsupported { label: label.to_owned() })?;
                self.conn.execute("UPDATE messages SET revoked = 1 WHERE chat_jid = ?1 AND id = ?2", params![chat, id])?;
                self.conn.execute("DELETE FROM reactions WHERE chat_jid = ?1 AND target_id = ?2", params![chat, id])?;
                self.get_message(&chat, id)
            }
        }
    }

    /// Apply (and forget) an edit / revoke that arrived before message `id`. Returns true
    /// if the message turned out to be deleted.
    fn apply_pending_edit(&self, chat: &str, id: &str) -> Result<bool> {
        /// revoked, edit_ts, kind, text, payload
        type Pending = (bool, i64, Option<i64>, Option<String>, Option<Vec<u8>>);
        let pending: Option<Pending> = self
            .conn
            .prepare_cached("SELECT revoked, edit_ts, kind, text, payload FROM pending_edits WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        let Some((revoked, edit_ts, kind, text, payload)) = pending else { return Ok(false) };
        self.conn.execute("DELETE FROM pending_edits WHERE chat_jid = ?1 AND id = ?2", params![chat, id])?;
        if revoked {
            self.revoke(chat, id, crate::extract::DELETED_LABEL)?;
        } else if let Some(kind) = kind {
            let (body, _) = body_from_row(kind, text.unwrap_or_default(), None, payload.as_deref(), None, MediaState::Idle);
            self.apply_edit(chat, id, &body, edit_ts)?;
        }
        Ok(revoked)
    }

    /// Whether message `id` was deleted for everyone.
    pub fn is_revoked(&self, chat_jid: &str, id: &str) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        Ok(self
            .conn
            .prepare_cached("SELECT revoked FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    /// Forget pending edits / revokes / reactions whose message never showed up (stored
    /// long ago).
    pub fn prune_pending_edits(&self, now: Timestamp) -> Result<()> {
        let cutoff = now - PENDING_EDIT_TTL_SECS;
        self.conn.execute("DELETE FROM pending_edits WHERE stored_at < ?1", [cutoff])?;
        self.conn.execute(
            "DELETE FROM reactions WHERE stored_at < ?1 AND NOT EXISTS
                (SELECT 1 FROM messages m WHERE m.chat_jid = reactions.chat_jid AND m.id = reactions.target_id)",
            [cutoff],
        )?;
        Ok(())
    }

    /// Store a reaction unless a newer one by the same sender is stored, or the message
    /// was deleted. The message itself needn't be stored yet. Returns true if what the
    /// message shows changed.
    pub fn upsert_reaction(&self, r: &NewReaction) -> Result<bool> {
        let chat = self.canonical(&r.chat_jid)?;
        let sender = self.canonical(&r.sender_jid)?;
        // Unattributable (e.g. a group reaction without its participant): keyed on
        // nothing, it would stand for everyone.
        if sender.is_empty() || r.target_id.is_empty() || self.is_revoked(&chat, &r.target_id)? {
            return Ok(false);
        }
        let current = self.reaction_rows(&chat, &r.target_id, &sender, r.from_me)?;
        if current.iter().any(|c| c.ts_ms >= r.ts_ms) {
            return Ok(false);
        }
        let shown = current.iter().map(|c| c.emoji.as_str()).find(|e| !e.is_empty()).unwrap_or("");
        self.write_reaction(&chat, &r.target_id, &sender, r.from_me, &r.emoji, r.ts_ms, r.push_name.as_deref())?;
        Ok(shown != r.emoji)
    }

    /// Set (or, with an empty `emoji`, remove) my reaction right away, ahead of the
    /// send: it beats whatever is stored, whatever the clocks say. Returns the previous
    /// row (for [`Store::revert_own_reaction`]) and the timestamp used, or `None` if the
    /// message isn't stored or was deleted.
    pub fn set_own_reaction(
        &self,
        chat_jid: &str,
        target_id: &str,
        own_jid: &str,
        emoji: &str,
        now_ms: i64,
    ) -> Result<Option<(Option<OwnReaction>, i64)>> {
        let chat = self.canonical(chat_jid)?;
        let sender = self.canonical(own_jid)?;
        let stored: Option<bool> = self
            .conn
            .prepare_cached("SELECT revoked FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, target_id], |r| r.get(0))
            .optional()?;
        if stored != Some(false) || sender.is_empty() {
            return Ok(None);
        }
        let previous = self.reaction_rows(&chat, target_id, &sender, true)?.into_iter().max_by_key(|r| r.ts_ms);
        let ts_ms = previous.as_ref().map_or(now_ms, |p| now_ms.max(p.ts_ms + 1));
        self.write_reaction(&chat, target_id, &sender, true, emoji, ts_ms, None)?;
        Ok(Some((previous, ts_ms)))
    }

    /// Undo [`Store::set_own_reaction`] (the send failed), unless my reaction changed
    /// again since. Returns true if it was undone.
    pub fn revert_own_reaction(
        &self,
        chat_jid: &str,
        target_id: &str,
        own_jid: &str,
        ts_ms: i64,
        previous: Option<&OwnReaction>,
    ) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        let sender = self.canonical(own_jid)?;
        let current = self.reaction_rows(&chat, target_id, &sender, true)?;
        if current.iter().map(|r| r.ts_ms).max() != Some(ts_ms) {
            return Ok(false);
        }
        self.conn.execute(
            "DELETE FROM reactions WHERE chat_jid = ?1 AND target_id = ?2 AND (sender_jid = ?3 OR from_me = 1)",
            params![chat, target_id, sender],
        )?;
        if let Some(p) = previous {
            self.write_reaction(&chat, target_id, &p.sender_jid, true, &p.emoji, p.ts_ms, None)?;
        }
        Ok(true)
    }

    /// The stored reaction rows of `sender` (with `from_me`: any of mine) on a message.
    fn reaction_rows(&self, chat: &str, target_id: &str, sender: &str, from_me: bool) -> Result<Vec<OwnReaction>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT sender_jid, emoji, ts_ms FROM reactions
             WHERE chat_jid = ?1 AND target_id = ?2 AND (sender_jid = ?3 OR (?4 AND from_me = 1))",
        )?;
        let rows = stmt.query_map(params![chat, target_id, sender, from_me], |r| {
            Ok(OwnReaction { sender_jid: r.get(0)?, emoji: r.get(1)?, ts_ms: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replace the sender's reaction on a message (all of mine, for own ones: my JID may
    /// have been unknown or different when an older one was stored).
    #[allow(clippy::too_many_arguments)]
    fn write_reaction(
        &self,
        chat: &str,
        target_id: &str,
        sender: &str,
        from_me: bool,
        emoji: &str,
        ts_ms: i64,
        push_name: Option<&str>,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "DELETE FROM reactions WHERE chat_jid = ?1 AND target_id = ?2 AND (sender_jid = ?3 OR (?4 AND from_me = 1))",
            )?
            .execute(params![chat, target_id, sender, from_me])?;
        self.conn
            .prepare_cached(
                "INSERT INTO reactions (chat_jid, target_id, sender_jid, from_me, emoji, ts_ms, push_name, stored_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                chat,
                target_id,
                sender,
                from_me,
                emoji,
                ts_ms,
                push_name.filter(|p| !p.is_empty()),
                crate::backend::now_secs()
            ])?;
        Ok(())
    }

    /// Current reactions on the messages of `chat` at seconds `min_ts..=max_ts` (or on
    /// message `id`), by message id, oldest first.
    fn reactions(&self, chat: &str, range: ReactionScope<'_>) -> Result<HashMap<String, Vec<Reaction>>> {
        const COLS: &str = "r.target_id, r.sender_jid, r.from_me, r.emoji, r.ts_ms, r.push_name, ct.full_name, ct.push_name";
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(String, Reaction)> {
            let sender: String = r.get(1)?;
            let from_me: bool = r.get(2)?;
            let own_push: Option<String> = r.get(5)?;
            let full: Option<String> = r.get(6)?;
            let contact_push: Option<String> = r.get(7)?;
            let nonempty = |s: Option<String>| s.filter(|s| !s.is_empty());
            let sender_name = (!from_me).then(|| {
                nonempty(full).or(nonempty(contact_push)).or(nonempty(own_push)).unwrap_or_else(|| fallback_name(&sender))
            });
            let ts_ms: i64 = r.get(4)?;
            Ok((
                r.get(0)?,
                Reaction { emoji: r.get(3)?, sender_jid: sender, sender_name, from_me, timestamp: ts_ms.div_euclid(1000) },
            ))
        };
        let rows: Vec<(String, Reaction)> = match range {
            ReactionScope::Message(id) => self
                .conn
                .prepare_cached(&format!(
                    "SELECT {COLS} FROM reactions r LEFT JOIN contacts ct ON ct.jid = r.sender_jid
                     WHERE r.chat_jid = ?1 AND r.target_id = ?2 AND r.emoji != ''
                     ORDER BY r.ts_ms, r.sender_jid"
                ))?
                .query_map(params![chat, id], map)?
                .collect::<rusqlite::Result<_>>()?,
            ReactionScope::Between(min_ts, max_ts) => self
                .conn
                .prepare_cached(&format!(
                    "SELECT {COLS} FROM reactions r
                     JOIN messages m ON m.chat_jid = r.chat_jid AND m.id = r.target_id
                     LEFT JOIN contacts ct ON ct.jid = r.sender_jid
                     WHERE r.chat_jid = ?1 AND m.ts BETWEEN ?2 AND ?3 AND r.emoji != ''
                     ORDER BY r.ts_ms, r.sender_jid"
                ))?
                .query_map(params![chat, min_ts, max_ts], map)?
                .collect::<rusqlite::Result<_>>()?,
        };
        let mut by_message: HashMap<String, Vec<Reaction>> = HashMap::new();
        for (id, reaction) in rows {
            by_message.entry(id).or_default().push(reaction);
        }
        Ok(by_message)
    }

    /// Rows to messages, with their reactions fetched in one query.
    fn to_chat_messages(&self, chat: &str, rows: Vec<MessageRow>) -> Result<Vec<ChatMessage>> {
        let (Some(min), Some(max)) = (rows.iter().map(|r| r.ts).min(), rows.iter().map(|r| r.ts).max()) else {
            return Ok(Vec::new());
        };
        let mut reactions = self.reactions(chat, ReactionScope::Between(min, max))?;
        rows.into_iter()
            .map(|r| {
                let reactions = reactions.remove(&r.id).unwrap_or_default();
                self.to_chat_message_with(r, reactions)
            })
            .collect()
    }

    /// Replace the body of a stored message (edit / revoke). Returns the updated message.
    /// Editing a photo / video / document only changes its caption (edits may arrive as
    /// plain text); other media can't be edited. Anything else replaces the body and drops
    /// the media; a deleted message (`Unsupported`) also loses its quote and downloaded file.
    pub fn set_body(&self, chat_jid: &str, id: &str, body: &MessageBody) -> Result<Option<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let Some(raw) = self.raw_message(&chat, id)? else { return Ok(None) };
        let same_kind = |m: &StoredMedia| {
            matches!(
                (body, m.kind()),
                (MessageBody::Image(_), MediaKind::Image)
                    | (MessageBody::Document(_), MediaKind::Document)
                    | (MessageBody::Video(_), MediaKind::Video | MediaKind::Ptv)
                    | (MessageBody::Voice(_), MediaKind::Audio)
                    | (MessageBody::Sticker(_), MediaKind::Sticker)
            )
        };
        let takes_caption =
            |m: &StoredMedia| matches!(m.kind(), MediaKind::Image | MediaKind::Video | MediaKind::Ptv | MediaKind::Document);
        let text_caption = match body {
            MessageBody::Text(t) => Some(Some(t.trim().to_owned()).filter(|t| !t.is_empty())),
            _ => None,
        };
        match raw.media {
            Some(mut media) if same_kind(&media) || (text_caption.is_some() && takes_caption(&media)) => {
                if let Some(caption) = caption_of(body).or(text_caption)
                    && media.set_caption(caption)
                {
                    let (_, text, _) = body_to_row(&media.body(None, MediaState::Idle));
                    self.conn.execute(
                        "UPDATE messages SET text = ?3, media = ?4 WHERE chat_jid = ?1 AND id = ?2",
                        params![chat, id, text, media.encode()],
                    )?;
                }
            }
            _ => {
                let (kind, text, payload) = body_to_row(body);
                let deleted = matches!(body, MessageBody::Unsupported { .. });
                self.conn.execute(
                    "UPDATE messages SET kind = ?3, text = ?4, payload = ?5, media = NULL, media_path = NULL,
                        media_state = 0,
                        quoted_id = CASE WHEN ?6 THEN NULL ELSE quoted_id END,
                        quoted_sender = CASE WHEN ?6 THEN NULL ELSE quoted_sender END,
                        quoted_from_me = CASE WHEN ?6 THEN 0 ELSE quoted_from_me END,
                        quoted_preview = CASE WHEN ?6 THEN NULL ELSE quoted_preview END
                     WHERE chat_jid = ?1 AND id = ?2",
                    params![chat, id, kind, text, payload, deleted],
                )?;
                if deleted
                    && let Some(path) = &raw.media_path
                    && let Err(e) = std::fs::remove_file(path)
                {
                    log::warn!("removing {}: {e}", path.display());
                }
            }
        }
        self.get_message(&chat, id)
    }

    /// The stored message with its media proto and wire addressing.
    pub fn raw_message(&self, chat_jid: &str, id: &str) -> Result<Option<RawMessage>> {
        let chat = self.canonical(chat_jid)?;
        let row = self
            .conn
            .prepare_cached(&format!("SELECT {MESSAGE_COLS} FROM messages WHERE chat_jid = ?1 AND id = ?2"))?
            .query_row(params![chat, id], MessageRow::from_row)
            .optional()?;
        Ok(row.map(|r| {
            let media_path = media_path(&r.body);
            RawMessage {
                chat_jid: r.chat_jid,
                id: r.id,
                sender_jid: r.sender_jid,
                wire_chat: r.wire_chat,
                wire_sender: r.wire_sender,
                from_me: r.from_me,
                body: r.body,
                media: r.media,
                media_path,
                timestamp: r.ts,
                status: r.status,
                edit_ts: r.edit_ts,
                revoked: r.revoked,
                quoted_id: r.quoted.map(|q| q.id),
            }
        }))
    }

    /// Record a finished download (`Some`) or reset to not-downloaded (`None`).
    pub fn set_media_path(&self, chat_jid: &str, id: &str, path: Option<&Path>) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        let path = path.map(|p| p.to_string_lossy().into_owned());
        self.conn.execute(
            "UPDATE messages SET media_path = ?3, media_state = 0 WHERE chat_jid = ?1 AND id = ?2",
            params![chat, id, path],
        )?;
        Ok(())
    }

    pub fn set_media_state(&self, chat_jid: &str, id: &str, state: MediaState) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.conn.execute(
            "UPDATE messages SET media_state = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat, id, media_state_to_i64(state)],
        )?;
        Ok(())
    }

    /// Replace the media proto (e.g. a new direct path after a media re-upload).
    pub fn set_media(&self, chat_jid: &str, id: &str, media: &StoredMedia) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.conn.execute(
            "UPDATE messages SET media = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat, id, media.encode()],
        )?;
        Ok(())
    }

    /// Replace the favorite stickers and hidden recents with what the app state says now
    /// (run it in a transaction). What we learnt about a favorite's file stays.
    pub fn replace_sticker_sync(&self, sync: &StickerSync) -> Result<()> {
        let animated: HashMap<String, bool> = self
            .conn
            .prepare("SELECT sha256, animated FROM favorite_stickers WHERE animated IS NOT NULL")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        self.conn.execute_batch("DELETE FROM favorite_stickers; DELETE FROM removed_recent_stickers;")?;
        let mut insert = self
            .conn
            .prepare("INSERT INTO favorite_stickers (sha256, action, ts_ms, animated) VALUES (?1, ?2, ?3, ?4)")?;
        for (id, (action, ts)) in &sync.favorites {
            insert.execute(params![id, action.encode_to_vec(), ts, animated.get(id)])?;
        }
        let mut insert =
            self.conn.prepare("INSERT INTO removed_recent_stickers (sha256, last_sent_ms) VALUES (?1, ?2)")?;
        for (id, ts) in &sync.removed_recents {
            insert.execute(params![id, ts])?;
        }
        Ok(())
    }

    /// Favorite stickers, newest first: (hex sha256, media fields, favorited at ms, animated).
    pub fn favorite_stickers(&self) -> Result<Vec<FavoriteSticker>> {
        let rows: Vec<(String, Vec<u8>, i64, Option<bool>)> = self
            .conn
            .prepare_cached("SELECT sha256, action, ts_ms, animated FROM favorite_stickers ORDER BY ts_ms DESC, sha256")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, action, ts_ms, animated)| {
                let action = wa::sync_action_value::StickerAction::decode_from_slice(&action).ok()?;
                Some(FavoriteSticker { id, action, ts_ms, animated })
            })
            .collect())
    }

    pub fn removed_recent_stickers(&self) -> Result<HashMap<String, i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT sha256, last_sent_ms FROM removed_recent_stickers")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_favorite_sticker_animated(&self, id: &str, animated: bool) -> Result<()> {
        self.conn.execute("UPDATE favorite_stickers SET animated = ?2 WHERE sha256 = ?1", params![id, animated])?;
        Ok(())
    }

    /// The newest `limit` stored sticker messages (only mine if `mine`), newest first.
    /// Revoked ones and ones without media info are left out.
    pub fn sticker_messages(&self, mine: bool, limit: u32) -> Result<Vec<StickerRow>> {
        // `kind = 7` spelled out so the partial index `messages_stickers` applies.
        const _: () = assert!(KIND_STICKER == 7);
        let sql = format!(
            "SELECT chat_jid, id, ts, media, media_path FROM messages
             WHERE kind = 7 AND revoked = 0 AND media IS NOT NULL {}
             ORDER BY ts DESC LIMIT ?1",
            if mine { "AND from_me = 1" } else { "" }
        );
        type Row = (String, String, i64, Vec<u8>, Option<String>);
        let rows: Vec<Row> = self
            .conn
            .prepare_cached(&sql)?
            .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(chat_jid, id, ts, media, path)| {
                let Some(StoredMedia::Sticker(proto)) = StoredMedia::decode(&media, MediaKind::Sticker) else {
                    return None;
                };
                Some(StickerRow { chat_jid, id, ts, proto, media_path: path.map(PathBuf::from) })
            })
            .collect())
    }

    /// Downloads interrupted by a restart or a session teardown go back to idle.
    pub fn reset_downloading(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET media_state = 0 WHERE media_state = ?1",
            [media_state_to_i64(MediaState::Downloading)],
        )?;
        Ok(())
    }

    pub fn avatar(&self, jid: &str) -> Result<Option<AvatarRow>> {
        let jid = self.canonical(jid)?;
        Ok(self
            .conn
            .prepare_cached("SELECT picture_id, path, fetched_at FROM avatars WHERE jid = ?1")?
            .query_row([&jid], |r| {
                Ok(AvatarRow { picture_id: r.get(0)?, path: r.get::<_, Option<String>>(1)?.map(PathBuf::from), fetched_at: r.get(2)? })
            })
            .optional()?)
    }

    /// Remember a fetched profile picture (`path: None` = none / hidden).
    pub fn set_avatar(&self, jid: &str, picture_id: Option<&str>, path: Option<&Path>, fetched_at: Timestamp) -> Result<()> {
        let jid = self.canonical(jid)?;
        let path = path.map(|p| p.to_string_lossy().into_owned());
        self.conn
            .prepare_cached(
                "INSERT INTO avatars (jid, picture_id, path, fetched_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (jid) DO UPDATE SET picture_id = excluded.picture_id, path = excluded.path,
                    fetched_at = excluded.fetched_at",
            )?
            .execute(params![jid, picture_id, path, fetched_at])?;
        Ok(())
    }

    /// Mark a cached picture stale (a `PictureUpdate` arrived). Returns true if we had one.
    pub fn invalidate_avatar(&self, jid: &str) -> Result<bool> {
        let jid = self.canonical(jid)?;
        Ok(self.conn.execute("UPDATE avatars SET fetched_at = 0 WHERE jid = ?1", [&jid])? > 0)
    }

    /// Display name, group flag and mute state of a chat, for notifications.
    pub fn chat_info(&self, jid: &str, now: Timestamp) -> Result<Option<(String, bool, bool)>> {
        let jid = self.canonical(jid)?;
        let row = self
            .conn
            .prepare_cached(
                "SELECT c.name, c.is_group, c.muted_until, ct.full_name, ct.push_name
                 FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid WHERE c.jid = ?1",
            )?
            .query_row([&jid], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()?;
        Ok(row.map(|(name, is_group, muted_until, full, push)| {
            (chat_display_name(&jid, is_group, name, full, push), is_group, muted_until > now)
        }))
    }

    /// Upgrade the delivery status of own messages (never downgrades). Returns the ids
    /// that changed.
    pub fn upgrade_status(&self, chat_jid: &str, ids: &[String], status: DeliveryStatus) -> Result<Vec<String>> {
        let chat = self.canonical(chat_jid)?;
        let status = status_to_i64(status);
        let mut stmt = self.conn.prepare_cached(
            "UPDATE messages SET status = ?3 WHERE chat_jid = ?1 AND id = ?2 AND from_me = 1 AND status < ?3",
        )?;
        let mut changed = Vec::new();
        for id in ids {
            if stmt.execute(params![chat, id, status])? > 0 {
                changed.push(id.clone());
            }
        }
        Ok(changed)
    }

    /// Fail every own message still `Pending` (its send was cut off). Returns
    /// (canonical chat, id) of each.
    pub fn fail_pending(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT chat_jid, id FROM messages WHERE from_me = 1 AND status = 0")?;
        let rows: Vec<(String, String)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        self.conn.execute(
            "UPDATE messages SET status = ?1 WHERE from_me = 1 AND status = 0",
            [status_to_i64(DeliveryStatus::Failed)],
        )?;
        Ok(rows)
    }

    /// Mark an own message as failed (explicit, since `Failed` sorts below `Pending`).
    pub fn mark_failed(&self, chat_jid: &str, id: &str) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.conn.execute(
            "UPDATE messages SET status = ?3 WHERE chat_jid = ?1 AND id = ?2 AND status <= 0",
            params![chat, id, status_to_i64(DeliveryStatus::Failed)],
        )?;
        Ok(())
    }

    /// Put a failed own message back to `Pending` for a resend, unless it's revoked or
    /// has expired (at `now`). Returns whether it was reset (`false` also when it's
    /// already being resent, or a receipt overtook the failure).
    pub fn reset_failed(&self, chat_jid: &str, id: &str, now: Timestamp) -> Result<bool> {
        let chat = self.canonical(chat_jid)?;
        let changed = self.conn.execute(
            "UPDATE messages SET status = ?3 WHERE chat_jid = ?1 AND id = ?2 AND from_me = 1 AND status = ?4
                AND revoked = 0 AND (expires_at IS NULL OR expires_at > ?5)",
            params![chat, id, status_to_i64(DeliveryStatus::Pending), status_to_i64(DeliveryStatus::Failed), now],
        )?;
        Ok(changed > 0)
    }

    pub fn get_message(&self, chat_jid: &str, id: &str) -> Result<Option<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let row = self
            .conn
            .prepare_cached(&format!("SELECT {MESSAGE_COLS} FROM messages WHERE chat_jid = ?1 AND id = ?2"))?
            .query_row(params![chat, id], MessageRow::from_row)
            .optional()?;
        row.map(|r| self.to_chat_message(r)).transpose()
    }

    /// Apply history-sync conversation metadata.
    pub fn upsert_chat_meta(&self, meta: &ChatMeta) -> Result<()> {
        let jid = self.canonical(&meta.jid)?;
        self.ensure_chat(&jid)?;
        self.conn
            .prepare_cached(
                "UPDATE chats SET
                    name = COALESCE(?2, name),
                    unread = COALESCE(?3, unread),
                    pinned_at = COALESCE(?4, pinned_at),
                    archived = COALESCE(?5, archived),
                    muted_until = COALESCE(?6, muted_until),
                    last_activity = MAX(last_activity, COALESCE(?7, 0))
                 WHERE jid = ?1",
            )?
            .execute(params![
                jid,
                meta.name.as_deref().filter(|n| !n.is_empty()),
                meta.unread,
                meta.pinned_at,
                meta.archived,
                meta.muted_until,
                meta.last_activity
            ])?;
        Ok(())
    }

    pub fn set_chat_name(&self, jid: &str, name: &str) -> Result<()> {
        self.upsert_chat_meta(&ChatMeta { jid: jid.into(), name: Some(name.into()), ..Default::default() })
    }

    pub fn set_unread(&self, jid: &str, unread: u32) -> Result<()> {
        self.upsert_chat_meta(&ChatMeta { jid: jid.into(), unread: Some(unread), ..Default::default() })
    }

    pub fn set_pinned(&self, jid: &str, pinned_at: i64) -> Result<()> {
        self.upsert_chat_meta(&ChatMeta { jid: jid.into(), pinned_at: Some(pinned_at), ..Default::default() })
    }

    pub fn set_archived(&self, jid: &str, archived: bool) -> Result<()> {
        self.upsert_chat_meta(&ChatMeta { jid: jid.into(), archived: Some(archived), ..Default::default() })
    }

    pub fn set_muted_until(&self, jid: &str, muted_until: i64) -> Result<()> {
        self.upsert_chat_meta(&ChatMeta { jid: jid.into(), muted_until: Some(muted_until), ..Default::default() })
    }

    pub fn unread_count(&self, jid: &str) -> Result<u32> {
        let jid = self.canonical(jid)?;
        let n: Option<u32> =
            self.conn.query_row("SELECT unread FROM chats WHERE jid = ?1", [&jid], |r| r.get(0)).optional()?;
        Ok(n.unwrap_or(0))
    }

    /// Groups we have no subject for yet.
    pub fn unnamed_groups(&self) -> Result<Vec<String>> {
        let mut stmt =
            self.conn.prepare("SELECT jid FROM chats WHERE is_group = 1 AND (name IS NULL OR name = '')")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_contact_name(&self, jid: &str, full_name: &str) -> Result<()> {
        if full_name.is_empty() {
            return Ok(());
        }
        let jid = self.canonical(jid)?;
        self.conn
            .prepare_cached(
                "INSERT INTO contacts (jid, full_name) VALUES (?1, ?2)
                 ON CONFLICT (jid) DO UPDATE SET full_name = excluded.full_name",
            )?
            .execute(params![jid, full_name])?;
        Ok(())
    }

    pub fn set_push_name(&self, jid: &str, push_name: &str) -> Result<()> {
        if push_name.is_empty() {
            return Ok(());
        }
        let jid = self.canonical(jid)?;
        self.conn
            .prepare_cached(
                "INSERT INTO contacts (jid, push_name) VALUES (?1, ?2)
                 ON CONFLICT (jid) DO UPDATE SET push_name = excluded.push_name",
            )?
            .execute(params![jid, push_name])?;
        Ok(())
    }

    /// Saved contact name > push name for a (canonical) JID.
    fn contact_name(&self, jid: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .prepare_cached("SELECT full_name, push_name FROM contacts WHERE jid = ?1")?
            .query_row([jid], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(row.and_then(|(full, push)| full.filter(|n| !n.is_empty()).or(push.filter(|n| !n.is_empty()))))
    }

    /// Display name for a message sender.
    pub fn sender_display_name(&self, sender_jid: &str, msg_push_name: Option<&str>) -> Result<String> {
        let jid = self.canonical(sender_jid)?;
        Ok(match self.contact_name(&jid)? {
            Some(n) => n,
            None => match msg_push_name.filter(|n| !n.is_empty()) {
                Some(n) => n.to_owned(),
                None => fallback_name(&jid),
            },
        })
    }

    /// What the store knows for a profile panel: canonical JID, display name, group flag
    /// and cached profile picture (only if the file is still there).
    pub fn local_profile(&self, jid: &str) -> Result<LocalProfile> {
        let canonical = self.canonical(jid)?;
        let chat: Option<(Option<String>, bool)> = self
            .conn
            .prepare_cached("SELECT name, is_group FROM chats WHERE jid = ?1")?
            .query_row([&canonical], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let is_group = is_group_jid(&canonical) || chat.as_ref().is_some_and(|c| c.1);
        let contact: Option<(Option<String>, Option<String>)> = self
            .conn
            .prepare_cached("SELECT full_name, push_name FROM contacts WHERE jid = ?1")?
            .query_row([&canonical], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (full, push) = contact.unwrap_or_default();
        let name = chat_display_name(&canonical, is_group, chat.and_then(|c| c.0), full, push);
        let avatar = self.avatar(&canonical)?.and_then(|a| a.path).filter(|p| p.is_file());
        Ok(LocalProfile { canonical, name, is_group, avatar })
    }

    /// A group member's canonical JID (its PN when known, also from `pn_hint`, the PN the
    /// group metadata gave for a LID member) and display name: saved name > push name >
    /// formatted phone.
    pub fn member(&self, jid: &str, pn_hint: Option<&str>) -> Result<(String, String)> {
        let mut canonical = self.canonical(jid)?;
        if is_lid_jid(&canonical)
            && let Some(pn) = pn_hint.map(non_ad).filter(|p| p.ends_with("@s.whatsapp.net"))
        {
            canonical = pn;
        }
        let name = match self.contact_name(&canonical)? {
            Some(n) => n,
            // A push name may still be filed under the LID (no alias stored yet).
            None => match self.contact_name(&non_ad(jid))? {
                Some(n) => n,
                None => crate::profile::display_phone(&canonical).unwrap_or_else(|| fallback_name(&canonical)),
            },
        };
        Ok((canonical, name))
    }

    fn to_chat_message(&self, r: MessageRow) -> Result<ChatMessage> {
        let reactions = self.reactions(&r.chat_jid, ReactionScope::Message(&r.id))?.remove(&r.id).unwrap_or_default();
        self.to_chat_message_with(r, reactions)
    }

    fn to_chat_message_with(&self, r: MessageRow, reactions: Vec<Reaction>) -> Result<ChatMessage> {
        let sender_name =
            if r.from_me { None } else { Some(self.sender_display_name(&r.sender_jid, r.push_name.as_deref())?) };
        let quoted = match r.quoted {
            Some(q) => Some(self.resolve_quote(&r.chat_jid, q)?),
            None => None,
        };
        let edited = r.edit_ts.is_some() && !r.revoked;
        Ok(ChatMessage {
            id: r.id,
            chat_jid: r.chat_jid,
            sender_jid: r.sender_jid,
            sender_name,
            from_me: r.from_me,
            quoted,
            reactions,
            edited,
            body: r.body,
            timestamp: r.ts,
            status: r.status,
            forwarded: r.forwarded,
            deleted: r.revoked,
        })
    }

    /// Who wrote the quoted message: the stored original if we have it (most reliable),
    /// else what the reply's contextInfo said; 1:1 replies without a participant quote
    /// the other person.
    fn resolve_quote(&self, chat: &str, q: StoredQuote) -> Result<QuotedMessage> {
        let original: Option<(bool, String, Option<String>)> = self
            .conn
            .prepare_cached("SELECT from_me, sender_jid, push_name FROM messages WHERE chat_jid = ?1 AND id = ?2")?
            .query_row(params![chat, q.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let (from_me, sender, push) = match original {
            Some((from_me, sender, push)) => (from_me, Some(sender), push),
            None => (q.from_me, q.sender.or_else(|| (!is_group_jid(chat)).then(|| chat.to_owned())), None),
        };
        let sender_name = match (from_me, sender) {
            (false, Some(s)) if !s.is_empty() => Some(self.sender_display_name(&s, push.as_deref())?),
            _ => None,
        };
        Ok(QuotedMessage { id: q.id, from_me, sender_name, preview: q.preview })
    }

    /// Full chat list: pinned first (most recently pinned on top), then by last activity.
    pub fn list_chats(&self, now: Timestamp) -> Result<Vec<ChatSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.jid, c.name, c.is_group, c.unread, c.pinned_at, c.archived, c.muted_until, c.last_activity,
                    ct.full_name, ct.push_name, av.path
             FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid LEFT JOIN avatars av ON av.jid = c.jid
             ORDER BY (c.pinned_at > 0) DESC, c.pinned_at DESC, c.last_activity DESC, c.jid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, u32>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
            ))
        })?;
        let mut last_stmt = self.conn.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE chat_jid = ?1 ORDER BY ts DESC, rowid DESC LIMIT 1"
        ))?;
        let mut out = Vec::new();
        for row in rows {
            let (jid, name, is_group, unread, pinned_at, archived, muted_until, last_activity, full, push, avatar) = row?;
            let last = last_stmt.query_row([&jid], MessageRow::from_row).optional()?;
            if last.is_none() && last_activity == 0 {
                continue;
            }
            let display = chat_display_name(&jid, is_group, name, full, push);
            let last_message = match last {
                Some(m) => Some(MessagePreview {
                    text: preview_text(&m.body).into_owned(),
                    timestamp: m.ts,
                    from_me: m.from_me,
                    sender_name: if is_group && !m.from_me {
                        Some(self.sender_display_name(&m.sender_jid, m.push_name.as_deref())?)
                    } else {
                        None
                    },
                }),
                None => None,
            };
            out.push(ChatSummary {
                jid,
                name: display,
                is_group,
                last_message,
                unread_count: unread,
                pinned: pinned_at > 0,
                archived,
                muted: muted_until > now,
                avatar: existing_path(avatar),
            });
        }
        Ok(out)
    }

    /// A page of messages older than `before` (newest page when `None`), oldest first.
    /// Never splits a group of same-second messages across pages.
    pub fn load_messages(
        &self,
        chat_jid: &str,
        before: Option<Timestamp>,
        limit: u32,
    ) -> Result<(Vec<ChatMessage>, bool)> {
        let chat = self.canonical(chat_jid)?;
        let limit = limit.max(1);
        let mut rows: Vec<MessageRow> = self
            .conn
            .prepare_cached(&format!(
                "SELECT {MESSAGE_COLS} FROM messages
                 WHERE chat_jid = ?1 AND (?2 IS NULL OR ts < ?2)
                 ORDER BY ts DESC, rowid DESC LIMIT ?3"
            ))?
            .query_map(params![chat, before, limit], MessageRow::from_row)?
            .collect::<rusqlite::Result<_>>()?;

        if rows.len() as u32 == limit
            && let Some(oldest) = rows.last().map(|r| r.ts)
        {
            // Pull in the rest of the boundary second so `before = oldest` can't skip any.
            let have: std::collections::HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
            let extra: Vec<MessageRow> = self
                .conn
                .prepare_cached(&format!(
                    "SELECT {MESSAGE_COLS} FROM messages WHERE chat_jid = ?1 AND ts = ?2 ORDER BY rowid DESC"
                ))?
                .query_map(params![chat, oldest], MessageRow::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|r| !have.contains(&r.id))
                .collect();
            rows.extend(extra);
        }

        let has_more = match rows.iter().map(|r| r.ts).min() {
            Some(oldest) => self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE chat_jid = ?1 AND ts < ?2)",
                params![chat, oldest],
                |r| r.get(0),
            )?,
            None => false,
        };
        // Oldest first; same-second messages in insertion order.
        rows.sort_by_key(|r| (r.ts, r.rowid));
        let messages = self.to_chat_messages(&chat, rows)?;
        Ok((messages, has_more))
    }

    /// The oldest stored message of a chat: the anchor for asking the phone for older history.
    pub fn oldest_message(&self, chat_jid: &str) -> Result<Option<OldestRef>> {
        let chat = self.canonical(chat_jid)?;
        self.conn
            .query_row(
                "SELECT id, from_me, ts, wire_chat FROM messages WHERE chat_jid = ?1 ORDER BY ts ASC, rowid ASC LIMIT 1",
                params![chat],
                |r| Ok(OldestRef { id: r.get(0)?, from_me: r.get(1)?, ts: r.get(2)?, wire_chat: r.get(3)? }),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Number of stored messages older than `ts`.
    pub fn count_before(&self, chat_jid: &str, ts: Timestamp) -> Result<u32> {
        let chat = self.canonical(chat_jid)?;
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE chat_jid = ?1 AND ts < ?2",
            params![chat, ts],
            |r| r.get(0),
        )?)
    }

    /// Ids of a chat's messages stored at second `ts`.
    pub fn ids_at(&self, chat_jid: &str, ts: Timestamp) -> Result<std::collections::HashSet<String>> {
        let chat = self.canonical(chat_jid)?;
        let mut stmt = self.conn.prepare_cached("SELECT id FROM messages WHERE chat_jid = ?1 AND ts = ?2")?;
        let rows = stmt.query_map(params![chat, ts], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A chat's messages at second `ts`, except `known` ones, oldest first.
    pub fn messages_at(
        &self,
        chat_jid: &str,
        ts: Timestamp,
        known: &std::collections::HashSet<String>,
    ) -> Result<Vec<ChatMessage>> {
        let chat = self.canonical(chat_jid)?;
        let rows: Vec<MessageRow> = self
            .conn
            .prepare_cached(&format!("SELECT {MESSAGE_COLS} FROM messages WHERE chat_jid = ?1 AND ts = ?2 ORDER BY rowid"))?
            .query_map(params![chat, ts], MessageRow::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let rows = rows.into_iter().filter(|r| !known.contains(&r.id)).collect();
        self.to_chat_messages(&chat, rows)
    }

    /// Queue read receipts for the chat's unread (newest `unread`) incoming messages.
    /// Idempotent. Returns how many receipts are pending for the chat.
    pub fn queue_read_receipts(&self, chat_jid: &str) -> Result<usize> {
        let chat = self.canonical(chat_jid)?;
        let unread = self.unread_count(&chat)?;
        if unread > 0 {
            self.conn
                .prepare_cached(
                    "INSERT OR IGNORE INTO pending_receipts (chat_jid, id, wire_chat, wire_sender)
                     SELECT chat_jid, id, wire_chat, wire_sender FROM messages
                     WHERE chat_jid = ?1 AND from_me = 0 ORDER BY ts DESC, rowid DESC LIMIT ?2",
                )?
                .execute(params![chat, unread])?;
        }
        Ok(self.conn.query_row("SELECT COUNT(*) FROM pending_receipts WHERE chat_jid = ?1", [&chat], |r| {
            r.get::<_, i64>(0)
        })? as usize)
    }

    /// Read receipts not acknowledged yet, as (canonical chat, receipt), of one chat or all.
    pub fn pending_receipts(&self, chat_jid: Option<&str>) -> Result<Vec<(String, UnreadRef)>> {
        let chat = chat_jid.map(|c| self.canonical(c)).transpose()?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT chat_jid, id, wire_chat, wire_sender FROM pending_receipts WHERE ?1 IS NULL OR chat_jid = ?1",
        )?;
        let rows = stmt.query_map([chat], |r| {
            Ok((r.get(0)?, UnreadRef { id: r.get(1)?, wire_chat: r.get(2)?, wire_sender: r.get(3)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The server took these receipts (or they can never be sent): forget them, and
    /// clear the chat's unread count except for messages newer than them.
    pub fn ack_receipts(&self, chat_jid: &str, ids: &[String]) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        let mut newest: Option<(i64, i64)> = None;
        {
            let mut del = self.conn.prepare_cached("DELETE FROM pending_receipts WHERE chat_jid = ?1 AND id = ?2")?;
            let mut pos =
                self.conn.prepare_cached("SELECT ts, rowid FROM messages WHERE chat_jid = ?1 AND id = ?2")?;
            for id in ids {
                del.execute(params![chat, id])?;
                if let Some(p) = pos.query_row(params![chat, id], |r| Ok((r.get(0)?, r.get(1)?))).optional()? {
                    newest = newest.max(Some(p));
                }
            }
        }
        let Some((ts, rowid)) = newest else { return Ok(()) };
        self.conn.execute(
            "UPDATE chats SET unread = MIN(unread, (SELECT COUNT(*) FROM messages
                WHERE chat_jid = ?1 AND from_me = 0 AND (ts > ?2 OR (ts = ?2 AND rowid > ?3))))
             WHERE jid = ?1",
            params![chat, ts, rowid],
        )?;
        Ok(())
    }

    /// Delete disappearing messages whose time is up (`expires_at <= now`).
    pub fn purge_expired(&self, now: Timestamp) -> Result<Vec<Removed>> {
        let removed: Vec<Removed> = self
            .conn
            .prepare_cached("SELECT chat_jid, id, media_path FROM messages WHERE expires_at <= ?1")?
            .query_map([now], |r| {
                Ok(Removed { chat_jid: r.get(0)?, id: r.get(1)?, media_path: r.get::<_, Option<String>>(2)?.map(PathBuf::from) })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if !removed.is_empty() {
            self.conn.execute(
                "DELETE FROM reactions WHERE (chat_jid, target_id) IN
                    (SELECT chat_jid, id FROM messages WHERE expires_at <= ?1)",
                [now],
            )?;
            self.conn.execute("DELETE FROM messages WHERE expires_at <= ?1", [now])?;
            self.conn.execute(
                "DELETE FROM pending_receipts WHERE NOT EXISTS
                    (SELECT 1 FROM messages m WHERE m.chat_jid = pending_receipts.chat_jid AND m.id = pending_receipts.id)",
                [],
            )?;
        }
        Ok(removed)
    }
}

/// Which messages' reactions to fetch.
#[derive(Clone, Copy)]
enum ReactionScope<'a> {
    Message(&'a str),
    /// Messages at these seconds (inclusive).
    Between(Timestamp, Timestamp),
}

/// An edit body as stored while it waits for its message: media bodies only carry the
/// new caption, which a text body applies the same way (see [`Store::set_body`]).
fn pending_edit_body(body: &MessageBody) -> MessageBody {
    match caption_of(body) {
        Some(caption) => MessageBody::Text(caption.unwrap_or_default()),
        None => body.clone(),
    }
}

/// Delete a media file nothing refers to anymore.
fn remove_media_file(path: &Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => log::warn!("removing {}: {e}", path.display()),
        _ => {}
    }
}

/// Groups: subject. 1:1: saved contact name > push name > conversation name > phone.
fn chat_display_name(
    jid: &str,
    is_group: bool,
    name: Option<String>,
    full: Option<String>,
    push: Option<String>,
) -> String {
    let nonempty = |s: Option<String>| s.filter(|s| !s.is_empty());
    if is_group {
        nonempty(name).unwrap_or_else(|| fallback_name(jid))
    } else {
        nonempty(full).or(nonempty(push)).or(nonempty(name)).unwrap_or_else(|| fallback_name(jid))
    }
}

/// A database file and its WAL / shared-memory companions.
fn db_files(path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        PathBuf::from(p)
    };
    [path.to_owned(), with("-wal"), with("-shm")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DocumentMedia, ImageMedia, StickerMedia, VideoMedia, VoiceMedia};
    use std::os::unix::fs::PermissionsExt as _;

    const ALICE: &str = "15550000001@s.whatsapp.net";
    const ALICE_LID: &str = "900000000001@lid";
    const BOB: &str = "15550000002@s.whatsapp.net";
    const BOB_LID: &str = "900000000002@lid";
    const ME: &str = "15550000009@s.whatsapp.net";
    const GROUP: &str = "120363000000000001@g.us";

    fn msg(chat: &str, id: &str, sender: &str, text: &str, ts: i64) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: sender.into(),
            wire_chat: chat.into(),
            wire_sender: is_group_jid(chat).then(|| sender.to_string()),
            push_name: None,
            from_me: sender == ME,
            body: MessageBody::Text(text.into()),
            timestamp: ts,
            status: if sender == ME { DeliveryStatus::Sent } else { DeliveryStatus::Delivered },
            media: None,
            quoted: None,
            expires_at: None,
        }
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn insert_and_upsert() {
        let s = store();
        assert!(s.insert_message(&msg(ALICE, "1", ALICE, "hi", 10), true).unwrap());
        // Same id again: update, not a second row, no extra unread.
        let mut edited = msg(ALICE, "1", ALICE, "hi (edited)", 10);
        edited.push_name = Some("Alice".into());
        assert!(!s.insert_message(&edited, true).unwrap());
        let (msgs, has_more) = s.load_messages(ALICE, None, 50).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!has_more);
        assert_eq!(msgs[0].body, MessageBody::Text("hi (edited)".into()));
        assert_eq!(msgs[0].sender_name.as_deref(), Some("Alice"));
        assert_eq!(s.unread_count(ALICE).unwrap(), 1);

        // Own messages never count as unread.
        s.insert_message(&msg(ALICE, "2", ME, "yo", 11), true).unwrap();
        assert_eq!(s.unread_count(ALICE).unwrap(), 1);
    }

    #[test]
    fn status_only_upgrades() {
        let s = store();
        let mut m = msg(ALICE, "1", ME, "hi", 10);
        m.status = DeliveryStatus::Pending;
        s.insert_message(&m, true).unwrap();
        let ids = vec!["1".to_string()];
        assert_eq!(s.upgrade_status(ALICE, &ids, DeliveryStatus::Read).unwrap(), ids);
        assert!(s.upgrade_status(ALICE, &ids, DeliveryStatus::Delivered).unwrap().is_empty());
        s.mark_failed(ALICE, "1").unwrap(); // too late: already read
        // A re-delivered copy with a lower status doesn't downgrade either.
        s.insert_message(&msg(ALICE, "1", ME, "hi", 10), true).unwrap();
        assert_eq!(s.get_message(ALICE, "1").unwrap().unwrap().status, DeliveryStatus::Read);

        let mut p = msg(ALICE, "2", ME, "lost", 11);
        p.status = DeliveryStatus::Pending;
        s.insert_message(&p, true).unwrap();
        s.mark_failed(ALICE, "2").unwrap();
        assert_eq!(s.get_message(ALICE, "2").unwrap().unwrap().status, DeliveryStatus::Failed);
    }

    #[test]
    fn chat_ordering_pinned_then_recent() {
        let s = store();
        s.insert_message(&msg(ALICE, "a", ALICE, "old", 100), true).unwrap();
        s.insert_message(&msg(BOB, "b", BOB, "new", 200), true).unwrap();
        s.insert_message(&msg(GROUP, "g", BOB, "newest", 300), true).unwrap();
        let order = |s: &Store| s.list_chats(0).unwrap().into_iter().map(|c| c.jid).collect::<Vec<_>>();
        assert_eq!(order(&s), vec![GROUP, BOB, ALICE]);
        s.set_pinned(ALICE, 50).unwrap();
        assert_eq!(order(&s), vec![ALICE, GROUP, BOB]);
        let chats = s.list_chats(0).unwrap();
        assert!(chats[0].pinned);
        assert_eq!(chats[1].last_message.as_ref().unwrap().text, "newest");
        assert_eq!(chats[1].last_message.as_ref().unwrap().sender_name.as_deref(), Some("+15550000002"));
    }

    #[test]
    fn name_precedence() {
        let s = store();
        s.insert_message(&msg(ALICE, "1", ALICE, "hi", 10), true).unwrap();
        let name = |s: &Store| s.list_chats(0).unwrap()[0].name.clone();
        assert_eq!(name(&s), "+15550000001");
        s.upsert_chat_meta(&ChatMeta { jid: ALICE.into(), name: Some("Conv name".into()), ..Default::default() })
            .unwrap();
        assert_eq!(name(&s), "Conv name");
        s.set_push_name(ALICE, "ally").unwrap();
        assert_eq!(name(&s), "ally");
        s.set_contact_name(ALICE, "Alice Smith").unwrap();
        assert_eq!(name(&s), "Alice Smith");
        s.set_push_name(ALICE, "ally2").unwrap();
        assert_eq!(name(&s), "Alice Smith");

        let s = store();
        s.insert_message(&msg(GROUP, "1", BOB, "hi", 10), true).unwrap();
        assert_eq!(name(&s), "Group");
        s.set_chat_name(GROUP, "Family").unwrap();
        assert_eq!(name(&s), "Family");
        assert!(s.list_chats(0).unwrap()[0].is_group);
        assert!(s.unnamed_groups().unwrap().is_empty());
    }

    #[test]
    fn lid_and_pn_chats_merge_into_one() {
        let s = store();
        // Seen first under the LID (e.g. a live message), with a push name.
        s.insert_message(&msg(ALICE_LID, "1", ALICE_LID, "from lid", 10), true).unwrap();
        s.set_push_name(ALICE_LID, "ally").unwrap();
        // And under the PN (history sync), including the same message id.
        s.insert_message(&msg(ALICE, "1", ALICE, "from lid", 10), false).unwrap();
        s.insert_message(&msg(ALICE, "2", ALICE, "from pn", 20), true).unwrap();
        assert_eq!(s.list_chats(0).unwrap().len(), 2);

        assert!(s.add_alias(ALICE_LID, ALICE).unwrap());
        assert!(!s.add_alias(ALICE_LID, ALICE).unwrap());
        let chats = s.list_chats(0).unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].jid, ALICE);
        assert_eq!(chats[0].name, "ally");
        assert_eq!(chats[0].unread_count, 2);
        assert_eq!(s.load_messages(ALICE, None, 50).unwrap().0.len(), 2);
        assert!(s.lid_chats().unwrap().is_empty());

        // Later traffic addressed to the LID lands in the PN chat.
        s.insert_message(&msg("900000000001:3@lid", "3", ALICE_LID, "again", 30), true).unwrap();
        let chats = s.list_chats(0).unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].last_message.as_ref().unwrap().text, "again");
        assert_eq!(s.load_messages(ALICE, None, 50).unwrap().0[2].sender_jid, ALICE);
    }

    #[test]
    fn paging_oldest_first_without_gaps() {
        let s = store();
        // 5 messages, three of them in the same second.
        for (id, ts) in [("a", 1), ("b", 2), ("c", 3), ("d", 3), ("e", 3), ("f", 4)] {
            s.insert_message(&msg(ALICE, id, ALICE, id, ts), false).unwrap();
        }
        let ids = |v: &[ChatMessage]| v.iter().map(|m| m.id.clone()).collect::<Vec<_>>();

        let (page, more) = s.load_messages(ALICE, None, 2).unwrap();
        // Boundary second 3 is pulled in completely.
        assert_eq!(ids(&page), vec!["c", "d", "e", "f"]);
        assert!(more);
        let (page, more) = s.load_messages(ALICE, Some(page[0].timestamp), 2).unwrap();
        assert_eq!(ids(&page), vec!["a", "b"]);
        assert!(!more);
        let (page, more) = s.load_messages(ALICE, Some(1), 2).unwrap();
        assert!(page.is_empty() && !more);
    }

    /// Every unread message gets a receipt (no 256 cap), receipts survive until the
    /// server takes them, and only acknowledged messages stop counting as unread.
    #[test]
    fn read_receipts_are_durable_until_acknowledged() {
        let path = std::env::temp_dir().join(format!("wa-store-receipts-{}.db", crate::new_message_id()));
        let s = Store::open(&path).unwrap();
        s.insert_message(&msg(GROUP, "mine", ME, "b", 0), true).unwrap();
        for i in 0..300 {
            s.insert_message(&msg(GROUP, &format!("m{i}"), if i % 2 == 0 { ALICE } else { BOB }, "a", i + 1), true)
                .unwrap();
        }
        assert_eq!(s.unread_count(GROUP).unwrap(), 300);
        assert_eq!(s.queue_read_receipts(GROUP).unwrap(), 300);
        // Idempotent (the UI asks again while offline); the count isn't touched yet.
        assert_eq!(s.queue_read_receipts(GROUP).unwrap(), 300);
        assert_eq!(s.unread_count(GROUP).unwrap(), 300);
        let pending = s.pending_receipts(None).unwrap();
        assert!(pending.iter().all(|(chat, r)| chat == GROUP && r.id != "mine"));
        assert!(pending.iter().any(|(_, r)| r.id == "m1" && r.wire_sender.as_deref() == Some(BOB)));
        // Survives a restart.
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.pending_receipts(Some(GROUP)).unwrap().len(), 300);
        // A message arrives while the receipts are out; then the server takes them.
        s.insert_message(&msg(GROUP, "late", ALICE, "new", 1000), true).unwrap();
        let ids: Vec<String> = pending.into_iter().map(|(_, r)| r.id).collect();
        s.ack_receipts(GROUP, &ids).unwrap();
        assert!(s.pending_receipts(None).unwrap().is_empty());
        assert_eq!(s.unread_count(GROUP).unwrap(), 1, "the newer message stays unread");
        drop(s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn reaction(chat: &str, target: &str, sender: &str, emoji: &str, ts_ms: i64) -> NewReaction {
        NewReaction {
            chat_jid: chat.into(),
            target_id: target.into(),
            sender_jid: sender.into(),
            from_me: sender == ME,
            emoji: emoji.into(),
            ts_ms,
            push_name: None,
        }
    }

    /// (emoji, sender) of a stored message's reactions.
    fn reactions_of(s: &Store, chat: &str, id: &str) -> Vec<(String, String)> {
        s.get_message(chat, id).unwrap().unwrap().reactions.into_iter().map(|r| (r.emoji, r.sender_jid)).collect()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    /// One reaction per sender; the newest (by the sender's timestamp) wins, removals
    /// included, so older copies arriving late change nothing.
    #[test]
    fn reactions_newest_wins() {
        let s = store();
        s.insert_message(&msg(GROUP, "1", BOB, "hi", 10), true).unwrap();
        assert!(s.upsert_reaction(&reaction(GROUP, "1", ALICE, "❤️", 1000)).unwrap());
        assert!(s.upsert_reaction(&reaction(GROUP, "1", ALICE, "👍", 2000)).unwrap());
        assert!(!s.upsert_reaction(&reaction(GROUP, "1", ALICE, "😂", 1500)).unwrap(), "older one ignored");
        assert!(!s.upsert_reaction(&reaction(GROUP, "1", ALICE, "👍", 2000)).unwrap(), "replay ignored");
        assert_eq!(reactions_of(&s, GROUP, "1"), pairs(&[("👍", ALICE)]));
        // Removed; a replay of the older reaction doesn't bring it back.
        assert!(s.upsert_reaction(&reaction(GROUP, "1", ALICE, "", 3000)).unwrap());
        assert!(!s.upsert_reaction(&reaction(GROUP, "1", ALICE, "👍", 2000)).unwrap());
        assert!(reactions_of(&s, GROUP, "1").is_empty());
        // Others' reactions are separate, sorted by time; the chat list doesn't change.
        assert!(s.upsert_reaction(&reaction(GROUP, "1", ME, "🔥", 5000)).unwrap());
        assert!(s.upsert_reaction(&reaction(GROUP, "1", ALICE, "😮", 4000)).unwrap());
        s.set_contact_name(ALICE, "Alice").unwrap();
        let reactions = s.get_message(GROUP, "1").unwrap().unwrap().reactions;
        assert_eq!(
            reactions,
            vec![
                Reaction { emoji: "😮".into(), sender_jid: ALICE.into(), sender_name: Some("Alice".into()), from_me: false, timestamp: 4 },
                Reaction { emoji: "🔥".into(), sender_jid: ME.into(), sender_name: None, from_me: true, timestamp: 5 },
            ]
        );
        let chat = &s.list_chats(0).unwrap()[0];
        assert_eq!((chat.last_message.as_ref().unwrap().text.as_str(), chat.unread_count), ("hi", 1));
        // Unattributable reactions aren't stored.
        assert!(!s.upsert_reaction(&reaction(GROUP, "1", "", "👍", 9000)).unwrap());
    }

    /// Reactions that arrive before their message show up with it, also in pages.
    #[test]
    fn reactions_before_their_message_and_in_pages() {
        let s = store();
        s.upsert_reaction(&reaction(ALICE, "2", ALICE, "❤️", 2000)).unwrap();
        s.upsert_reaction(&reaction(ALICE, "2", ME, "👍", 1000)).unwrap();
        s.upsert_reaction(&reaction(ALICE, "3", ALICE, "😂", 3000)).unwrap();
        assert!(s.list_chats(0).unwrap().is_empty(), "a reaction alone makes no chat");
        for (id, ts) in [("1", 1), ("2", 2), ("3", 3)] {
            s.insert_message(&msg(ALICE, id, ALICE, id, ts), true).unwrap();
        }
        assert_eq!(reactions_of(&s, ALICE, "2"), pairs(&[("👍", ME), ("❤️", ALICE)]));
        let (page, _) = s.load_messages(ALICE, None, 50).unwrap();
        let got: Vec<Vec<&str>> = page.iter().map(|m| m.reactions.iter().map(|r| r.emoji.as_str()).collect()).collect();
        assert_eq!(got, vec![vec![], vec!["👍", "❤️"], vec!["😂"]]);
        let (older, _) = s.load_messages(ALICE, Some(3), 1).unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].reactions.len(), 2);
        // Survives a restart.
        let path = std::env::temp_dir().join(format!("wa-store-reactions-{}.db", crate::new_message_id()));
        {
            let s = Store::open(&path).unwrap();
            s.upsert_reaction(&reaction(ALICE, "1", ALICE, "❤️", 1)).unwrap();
            s.insert_message(&msg(ALICE, "1", ALICE, "x", 1), true).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(reactions_of(&s, ALICE, "1"), pairs(&[("❤️", ALICE)]));
        drop(s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// LID and PN identities of one person merge into one reaction (the newer), in their
    /// own chat and in groups.
    #[test]
    fn reactions_merge_with_lid_and_pn() {
        let s = store();
        s.insert_message(&msg(ALICE_LID, "1", ALICE_LID, "hi", 10), true).unwrap();
        s.insert_message(&msg(ALICE, "1", ALICE, "hi", 10), true).unwrap();
        s.upsert_reaction(&reaction(ALICE_LID, "1", ALICE_LID, "❤️", 2000)).unwrap();
        s.upsert_reaction(&reaction(ALICE, "1", ALICE, "👍", 1000)).unwrap();
        s.upsert_reaction(&reaction(ALICE_LID, "1", ME, "😂", 1500)).unwrap();
        s.upsert_reaction(&reaction(ALICE, "1", ME, "🔥", 1600)).unwrap();
        s.insert_message(&msg(GROUP, "g", ME, "group", 20), true).unwrap();
        s.upsert_reaction(&reaction(GROUP, "g", BOB_LID, "👍", 3000)).unwrap();
        s.upsert_reaction(&reaction(GROUP, "g", BOB, "😮", 2500)).unwrap();
        assert_eq!(reactions_of(&s, GROUP, "g").len(), 2);

        s.add_alias(ALICE_LID, ALICE).unwrap();
        assert_eq!(reactions_of(&s, ALICE, "1"), pairs(&[("🔥", ME), ("❤️", ALICE)]));
        s.add_alias(BOB_LID, BOB).unwrap();
        assert_eq!(reactions_of(&s, GROUP, "g"), pairs(&[("👍", BOB)]));
        // Later reactions by the LID land on the same row (an older one is ignored).
        assert!(!s.upsert_reaction(&reaction(GROUP, "g", "900000000002:4@lid", "😢", 2900)).unwrap());
        assert!(s.upsert_reaction(&reaction(GROUP, "g", "900000000002:4@lid", "😢", 3100)).unwrap());
        assert_eq!(reactions_of(&s, GROUP, "g"), pairs(&[("😢", BOB)]));
    }

    /// Deleted messages (for everyone, or disappearing) take their reactions with them,
    /// and get no new ones.
    #[test]
    fn reactions_go_with_their_message() {
        let s = store();
        s.insert_message(&msg(ALICE, "1", ALICE, "hi", 10), true).unwrap();
        s.upsert_reaction(&reaction(ALICE, "1", ME, "👍", 1000)).unwrap();
        s.revoke(ALICE, "1", crate::extract::DELETED_LABEL).unwrap();
        assert!(reactions_of(&s, ALICE, "1").is_empty());
        assert!(!s.upsert_reaction(&reaction(ALICE, "1", ALICE, "❤️", 2000)).unwrap());
        assert!(s.set_own_reaction(ALICE, "1", ME, "❤️", 3000).unwrap().is_none());
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);

        // Deleted before it arrived; reacted to meanwhile.
        s.revoke(ALICE, "2", crate::extract::DELETED_LABEL).unwrap();
        s.upsert_reaction(&reaction(ALICE, "2", ALICE, "❤️", 2000)).unwrap();
        s.insert_message(&msg(ALICE, "2", ALICE, "secret", 20), true).unwrap();
        assert!(reactions_of(&s, ALICE, "2").is_empty());

        let mut m = msg(ALICE, "3", ALICE, "poof", 30);
        m.expires_at = Some(100);
        s.insert_message(&m, true).unwrap();
        s.upsert_reaction(&reaction(ALICE, "3", ALICE, "❤️", 2000)).unwrap();
        assert_eq!(s.purge_expired(100).unwrap().len(), 1);
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);

        // Reactions to messages that never show up are forgotten eventually.
        s.upsert_reaction(&reaction(ALICE, "never", ALICE, "❤️", 2000)).unwrap();
        s.insert_message(&msg(ALICE, "4", ALICE, "kept", 40), true).unwrap();
        s.upsert_reaction(&reaction(ALICE, "4", ALICE, "❤️", 2000)).unwrap();
        s.prune_pending_edits(crate::backend::now_secs() + PENDING_EDIT_TTL_SECS + 1).unwrap();
        let left: Vec<String> = s
            .conn
            .prepare("SELECT target_id FROM reactions")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, vec!["4"]);
        s.clear_all().unwrap();
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    /// My reaction shows at once, whatever the stored timestamps, and a failed send puts
    /// back exactly what was there (nothing, a removal, another emoji), unless it changed
    /// again meanwhile.
    #[test]
    fn own_reactions_are_optimistic_and_revertible() {
        let s = store();
        s.insert_message(&msg(ALICE, "1", ALICE, "hi", 10), true).unwrap();
        let rows = |s: &Store| -> Vec<(String, String, i64)> {
            s.conn
                .prepare("SELECT sender_jid, emoji, ts_ms FROM reactions WHERE from_me = 1")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        // Nothing before.
        let (prev, ts) = s.set_own_reaction(ALICE, "1", ME, "👍", 1000).unwrap().unwrap();
        assert_eq!((prev.clone(), ts), (None, 1000));
        assert_eq!(reactions_of(&s, ALICE, "1"), pairs(&[("👍", ME)]));
        assert!(s.revert_own_reaction(ALICE, "1", ME, ts, prev.as_ref()).unwrap());
        assert!(rows(&s).is_empty());

        // Another emoji before, stamped by a phone whose clock is ahead of ours.
        s.upsert_reaction(&reaction(ALICE, "1", ME, "❤️", 50_000)).unwrap();
        let (prev, ts) = s.set_own_reaction(ALICE, "1", ME, "😂", 1000).unwrap().unwrap();
        assert_eq!(ts, 50_001, "beats the stored one");
        assert_eq!(reactions_of(&s, ALICE, "1"), pairs(&[("😂", ME)]));
        assert!(s.revert_own_reaction(ALICE, "1", ME, ts, prev.as_ref()).unwrap());
        assert_eq!(rows(&s), vec![(ME.into(), "❤️".into(), 50_000)]);

        // A removal before.
        s.upsert_reaction(&reaction(ALICE, "1", ME, "", 60_000)).unwrap();
        let (prev, ts) = s.set_own_reaction(ALICE, "1", ME, "🔥", 1000).unwrap().unwrap();
        assert!(s.revert_own_reaction(ALICE, "1", ME, ts, prev.as_ref()).unwrap());
        assert_eq!(rows(&s), vec![(ME.into(), String::new(), 60_000)]);
        assert!(reactions_of(&s, ALICE, "1").is_empty());

        // Changed again before the first send failed: the newer state stays.
        let (prev, first) = s.set_own_reaction(ALICE, "1", ME, "👍", 70_000).unwrap().unwrap();
        s.set_own_reaction(ALICE, "1", ME, "🙏", 70_000).unwrap();
        assert!(!s.revert_own_reaction(ALICE, "1", ME, first, prev.as_ref()).unwrap());
        assert_eq!(reactions_of(&s, ALICE, "1"), pairs(&[("🙏", ME)]));
        // Removing mine (`None` from the UI) clears it.
        s.set_own_reaction(ALICE, "1", ME, "", 80_000).unwrap();
        assert!(reactions_of(&s, ALICE, "1").is_empty());
        assert!(s.set_own_reaction(ALICE, "missing", ME, "👍", 1).unwrap().is_none());
    }

    fn deleted() -> MessageBody {
        MessageBody::Unsupported { label: crate::extract::DELETED_LABEL.into() }
    }

    /// My own edit: shown as edited everywhere a message is built (pages, the message,
    /// the chat list preview), survives a replay of the original, and a revert puts the
    /// exact previous state back unless something newer arrived.
    #[test]
    fn own_edit_applies_and_reverts() {
        let s = store();
        let get = |s: &Store, id: &str| s.get_message(ALICE, id).unwrap().unwrap();
        s.insert_message(&msg(ALICE, "t", ME, "helo", 10), true).unwrap();
        assert!(!get(&s, "t").edited);
        let edited = s.apply_edit(ALICE, "t", &MessageBody::Text("hello".into()), 20).unwrap().unwrap();
        assert!(edited.edited);
        assert_eq!(edited.body, MessageBody::Text("hello".into()));
        let (page, _) = s.load_messages(ALICE, None, 10).unwrap();
        assert!(page.iter().any(|m| m.id == "t" && m.edited));
        let preview = |s: &Store| {
            s.list_chats(0).unwrap().into_iter().find(|c| c.jid == ALICE).unwrap().last_message.unwrap().text
        };
        assert_eq!(preview(&s), "hello");
        // A replay of the original doesn't undo it.
        s.insert_message(&msg(ALICE, "t", ME, "helo", 10), false).unwrap();
        assert_eq!(get(&s, "t").body, MessageBody::Text("hello".into()));
        // Revert: content and "never edited" back; a stale revert does nothing.
        assert!(s.revert_edit(ALICE, "t", 19, &MessageBody::Text("helo".into()), None).unwrap().is_none());
        let reverted = s.revert_edit(ALICE, "t", 20, &MessageBody::Text("helo".into()), None).unwrap().unwrap();
        assert_eq!(reverted.body, MessageBody::Text("helo".into()));
        assert!(!reverted.edited);
        assert_eq!(preview(&s), "helo");
        let edit_ts: Option<i64> =
            s.conn.query_row("SELECT edit_ts FROM messages WHERE id = 't'", [], |r| r.get(0)).unwrap();
        assert_eq!(edit_ts, None);

        // Reverting to an earlier edit keeps it edited; a newer edit (from my phone) or a
        // revoke since wins over the revert.
        s.apply_edit(ALICE, "t", &MessageBody::Text("one".into()), 30).unwrap();
        s.apply_edit(ALICE, "t", &MessageBody::Text("two".into()), 31).unwrap();
        let back = s.revert_edit(ALICE, "t", 31, &MessageBody::Text("one".into()), Some(30)).unwrap().unwrap();
        assert!(back.edited);
        assert_eq!(back.body, MessageBody::Text("one".into()));
        s.apply_edit(ALICE, "t", &MessageBody::Text("three".into()), 40).unwrap();
        s.apply_edit(ALICE, "t", &MessageBody::Text("phone".into()), 41).unwrap();
        assert!(s.revert_edit(ALICE, "t", 40, &MessageBody::Text("one".into()), Some(30)).unwrap().is_none());
        assert_eq!(get(&s, "t").body, MessageBody::Text("phone".into()));
        s.revoke(ALICE, "t", crate::extract::DELETED_LABEL).unwrap();
        assert!(!get(&s, "t").edited);
        assert!(s.revert_edit(ALICE, "t", 41, &MessageBody::Text("one".into()), Some(30)).unwrap().is_none());

        // A caption edit keeps the photo; its revert puts the caption back.
        s.insert_message(&image_msg(ALICE, "img", ME, 50), true).unwrap();
        let before = get(&s, "img").body;
        let edited = s.apply_edit(ALICE, "img", &MessageBody::Text("sunset".into()), 60).unwrap().unwrap();
        assert!(edited.edited);
        assert!(matches!(&edited.body, MessageBody::Image(i) if i.caption.as_deref() == Some("sunset") && i.width == Some(800)));
        assert!(s.raw_message(ALICE, "img").unwrap().unwrap().media.is_some());
        let reverted = s.revert_edit(ALICE, "img", 60, &before, None).unwrap().unwrap();
        assert_eq!(reverted.body, before);
        assert!(!reverted.edited);
    }

    /// Replayed originals (history after an edit / revoke) and late older edits never
    /// undo newer state; edits and revokes that arrive before their message wait for it.
    #[test]
    fn edits_and_revokes_are_never_undone() {
        let s = store();
        let text = |s: &Store, id: &str| s.get_message(ALICE, id).unwrap().unwrap().body;
        s.insert_message(&msg(ALICE, "e", ALICE, "original", 10), true).unwrap();
        assert!(s.apply_edit(ALICE, "e", &MessageBody::Text("second".into()), 30).unwrap().is_some());
        // An older edit and the original replayed by history: no change.
        assert!(s.apply_edit(ALICE, "e", &MessageBody::Text("first".into()), 20).unwrap().is_none());
        s.insert_message(&msg(ALICE, "e", ALICE, "original", 10), false).unwrap();
        assert_eq!(text(&s, "e"), MessageBody::Text("second".into()));

        // Revoked: replays and edits can't bring it back.
        s.insert_message(&msg(ALICE, "r", ALICE, "secret", 11), true).unwrap();
        assert_eq!(s.revoke(ALICE, "r", crate::extract::DELETED_LABEL).unwrap().unwrap().body, deleted());
        s.insert_message(&msg(ALICE, "r", ALICE, "secret", 11), false).unwrap();
        assert!(s.apply_edit(ALICE, "r", &MessageBody::Text("edited".into()), 40).unwrap().is_none());
        assert_eq!(text(&s, "r"), deleted());

        // Revoke before the message (history order): the message arrives deleted.
        assert!(s.revoke(ALICE, "early", crate::extract::DELETED_LABEL).unwrap().is_none());
        s.insert_message(&msg(ALICE, "early", ALICE, "secret", 12), false).unwrap();
        assert_eq!(text(&s, "early"), deleted());
        // Edit before the message, also of a photo's caption.
        s.apply_edit(ALICE, "pe", &MessageBody::Text("newest".into()), 50).unwrap();
        s.apply_edit(ALICE, "pe", &MessageBody::Text("older".into()), 45).unwrap();
        s.insert_message(&msg(ALICE, "pe", ALICE, "original", 13), false).unwrap();
        assert_eq!(text(&s, "pe"), MessageBody::Text("newest".into()));
        let caption = MessageBody::Image(ImageMedia {
            path: None,
            thumbnail: None,
            width: None,
            height: None,
            caption: Some("sunset".into()),
            state: MediaState::Idle,
        });
        s.apply_edit(ALICE, "img", &caption, 60).unwrap();
        s.insert_message(&image_msg(ALICE, "img", ALICE, 14), false).unwrap();
        assert!(matches!(text(&s, "img"), MessageBody::Image(i) if i.caption.as_deref() == Some("sunset") && i.width == Some(800)));
        // Waiting edits of messages that never show up are forgotten eventually.
        // An edit made long ago (old history) waits like any other ...
        s.apply_edit(ALICE, "old", &MessageBody::Text("edited long ago".into()), 1).unwrap();
        let now = crate::backend::now_secs();
        s.prune_pending_edits(now).unwrap();
        s.insert_message(&msg(ALICE, "old", ALICE, "original", 0), false).unwrap();
        assert_eq!(text(&s, "old"), MessageBody::Text("edited long ago".into()));
        // ... and waiting ones are forgotten long after they were stored.
        s.apply_edit(ALICE, "never", &MessageBody::Text("x".into()), 1).unwrap();
        s.prune_pending_edits(now + 1).unwrap();
        let count = |s: &Store| -> i64 { s.conn.query_row("SELECT COUNT(*) FROM pending_edits", [], |r| r.get(0)).unwrap() };
        assert_eq!(count(&s), 1);
        s.prune_pending_edits(now + PENDING_EDIT_TTL_SECS + 1).unwrap();
        assert_eq!(count(&s), 0);
        // A message deleted before it arrived doesn't count as unread.
        s.revoke(BOB, "gone", crate::extract::DELETED_LABEL).unwrap();
        s.insert_message(&msg(BOB, "gone", BOB, "secret", 5), true).unwrap();
        assert!(s.is_revoked(BOB, "gone").unwrap());
        assert_eq!(s.unread_count(BOB).unwrap(), 0);
    }

    /// Merging a LID chat into its PN chat keeps the better copy of each message.
    #[test]
    fn alias_merge_keeps_the_better_duplicate() {
        let s = store();
        // PN copy: pending own message, plain photo. LID copy: read, downloaded, edited.
        let mut pn_own = msg(ALICE, "own", ME, "hi", 10);
        pn_own.status = DeliveryStatus::Pending;
        s.insert_message(&pn_own, false).unwrap();
        let mut lid_own = msg(ALICE_LID, "own", ME, "hi", 10);
        lid_own.status = DeliveryStatus::Read;
        s.insert_message(&lid_own, false).unwrap();
        s.insert_message(&image_msg(ALICE, "img", ALICE, 11), false).unwrap();
        s.insert_message(&image_msg(ALICE_LID, "img", ALICE_LID, 11), false).unwrap();
        let file = std::env::temp_dir().join(format!("wa-merge-{}.jpg", crate::new_message_id()));
        std::fs::write(&file, b"jpeg").unwrap();
        s.set_media_path(ALICE_LID, "img", Some(&file)).unwrap();
        s.insert_message(&msg(ALICE, "ed", ALICE, "typo", 12), false).unwrap();
        s.insert_message(&msg(ALICE_LID, "ed", ALICE_LID, "typo", 12), false).unwrap();
        s.apply_edit(ALICE_LID, "ed", &MessageBody::Text("fixed".into()), 20).unwrap();
        // Revoked under the PN, not (yet) under the LID.
        s.insert_message(&msg(ALICE, "gone", ALICE, "secret", 13), false).unwrap();
        s.insert_message(&msg(ALICE_LID, "gone", ALICE_LID, "secret", 13), false).unwrap();
        s.revoke(ALICE, "gone", crate::extract::DELETED_LABEL).unwrap();
        // A receipt and an edit waiting under the LID.
        s.insert_message(&msg(ALICE_LID, "u", ALICE_LID, "unread", 14), true).unwrap();
        s.queue_read_receipts(ALICE_LID).unwrap();
        s.apply_edit(ALICE_LID, "later", &MessageBody::Text("edited".into()), 30).unwrap();
        s.insert_message(&msg(ALICE, "later", ALICE, "orig", 15), false).unwrap();
        assert_eq!(s.get_message(ALICE, "later").unwrap().unwrap().body, MessageBody::Text("orig".into()));

        assert!(s.take_merges().is_empty());
        assert!(s.add_alias(ALICE_LID, ALICE).unwrap());
        assert_eq!(s.take_merges(), vec![(ALICE_LID.to_owned(), ALICE.to_owned())]);
        let get = |id: &str| s.get_message(ALICE, id).unwrap().unwrap();
        assert_eq!(get("own").status, DeliveryStatus::Read);
        assert!(matches!(get("img").body, MessageBody::Image(i) if i.path.as_ref() == Some(&file)));
        assert_eq!(get("ed").body, MessageBody::Text("fixed".into()));
        assert_eq!(get("gone").body, deleted());
        assert_eq!(get("later").body, MessageBody::Text("edited".into()));
        assert_eq!(s.pending_receipts(None).unwrap()[0].0, ALICE);
        assert_eq!(s.load_messages(ALICE, None, 50).unwrap().0.len(), 6);
        let lid_rows: i64 =
            s.conn.query_row("SELECT COUNT(*) FROM messages WHERE chat_jid = ?1", [ALICE_LID], |r| r.get(0)).unwrap();
        assert_eq!(lid_rows, 0);
        std::fs::remove_file(&file).unwrap();

        // A merge inside a transaction that fails is not announced.
        let mut s = store();
        s.insert_message(&msg(BOB_LID, "1", BOB_LID, "x", 1), true).unwrap();
        let failed: Result<()> = s.transaction(|s| {
            s.add_alias(BOB_LID, BOB)?;
            anyhow::bail!("rolled back")
        });
        assert!(failed.is_err());
        assert!(s.take_merges().is_empty());
        assert_eq!(s.lid_chats().unwrap(), vec![BOB_LID.to_owned()]);
    }

    /// View-once media is stored as a placeholder only: no media info, no file, even if a
    /// copy with media was stored before.
    #[test]
    fn view_once_keeps_no_media() {
        let s = store();
        let file = std::env::temp_dir().join(format!("wa-vo-{}.jpg", crate::new_message_id()));
        s.insert_message(&image_msg(ALICE, "vo", ALICE, 10), true).unwrap();
        std::fs::write(&file, b"jpeg").unwrap();
        s.set_media_path(ALICE, "vo", Some(&file)).unwrap();
        let vo = NewMessage { body: MessageBody::ViewOnce(ViewOnceKind::Photo), media: None, ..msg(ALICE, "vo", ALICE, "", 10) };
        s.insert_message(&vo, true).unwrap();
        let raw = s.raw_message(ALICE, "vo").unwrap().unwrap();
        assert_eq!(raw.body, MessageBody::ViewOnce(ViewOnceKind::Photo));
        assert!(raw.media.is_none() && raw.media_path.is_none());
        assert!(!file.exists());
        let text: String = s.conn.query_row("SELECT text FROM messages WHERE id = 'vo'", [], |r| r.get(0)).unwrap();
        assert_eq!(text, "📷 View once photo");
        assert_eq!(s.list_chats(0).unwrap()[0].last_message.as_ref().unwrap().text, "📷 View once photo");
    }

    /// View-once photos stored (and downloaded) as regular media by older builds become
    /// placeholders when the store is upgraded.
    #[test]
    fn upgrade_forgets_view_once_media_of_older_builds() {
        let dir = std::env::temp_dir().join(format!("wa-store-vo-{}", crate::new_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("messages.db");
        let file = dir.join("vo.jpg");
        std::fs::write(&file, b"jpeg").unwrap();
        {
            let s = Store::open(&path).unwrap();
            let mut vo = image_msg(ALICE, "vo", ALICE, 10);
            if let Some(StoredMedia::Image(p)) = &mut vo.media {
                p.view_once = Some(true);
            }
            s.insert_message(&vo, true).unwrap();
            s.set_media_path(ALICE, "vo", Some(&file)).unwrap();
            s.insert_message(&image_msg(ALICE, "normal", ALICE, 11), true).unwrap();
            // As schema 3 left it.
            s.conn
                .execute_batch(
                    "ALTER TABLE pending_edits DROP COLUMN stored_at; ALTER TABLE messages DROP COLUMN forwarded;
                     DROP TABLE deleted_for_me; ALTER TABLE chats DROP COLUMN marked_unread;
                     PRAGMA user_version = 3",
                )
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let raw = s.raw_message(ALICE, "vo").unwrap().unwrap();
        assert_eq!(raw.body, MessageBody::ViewOnce(ViewOnceKind::Photo));
        assert!(raw.media.is_none() && !file.exists());
        assert!(matches!(s.get_message(ALICE, "normal").unwrap().unwrap().body, MessageBody::Image(_)));
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Disappearing messages are deleted (with their file) once expired.
    #[test]
    fn expired_messages_are_purged() {
        let s = store();
        let file = std::env::temp_dir().join(format!("wa-exp-{}.jpg", crate::new_message_id()));
        std::fs::write(&file, b"jpeg").unwrap();
        s.insert_message(&NewMessage { expires_at: Some(100), ..image_msg(ALICE, "a", ALICE, 10) }, true).unwrap();
        s.set_media_path(ALICE, "a", Some(&file)).unwrap();
        s.insert_message(&NewMessage { expires_at: Some(200), ..msg(ALICE, "b", ALICE, "b", 11) }, true).unwrap();
        s.insert_message(&msg(ALICE, "c", ALICE, "c", 12), true).unwrap();
        assert!(s.purge_expired(99).unwrap().is_empty());
        let removed = s.purge_expired(150).unwrap();
        assert_eq!(removed, vec![Removed { chat_jid: ALICE.into(), id: "a".into(), media_path: Some(file.clone()) }]);
        assert_eq!(s.load_messages(ALICE, None, 10).unwrap().0.len(), 2);
        std::fs::remove_file(&file).unwrap();
    }

    /// Sends cut off by a crash show as failed after a restart.
    #[test]
    fn pending_sends_fail_on_startup() {
        let path = std::env::temp_dir().join(format!("wa-store-crash-{}.db", crate::new_message_id()));
        let s = Store::open(&path).unwrap();
        let mut p = msg(ALICE, "p", ME, "hi", 1);
        p.status = DeliveryStatus::Pending;
        s.insert_message(&p, false).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.get_message(ALICE, "p").unwrap().unwrap().status, DeliveryStatus::Failed);
        drop(s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn image_msg(chat: &str, id: &str, sender: &str, ts: i64) -> NewMessage {
        let proto = whatsapp_rust::prelude::wa::message::ImageMessage {
            caption: Some("beach".into()),
            width: Some(800),
            height: Some(600),
            jpeg_thumbnail: Some(vec![0xFF, 0xD8, 0xFF]),
            direct_path: Some("/v/t62.7118-24/x".into()),
            media_key: Some(vec![1; 32]),
            file_sha256: Some(vec![2; 32]),
            file_enc_sha256: Some(vec![3; 32]),
            file_length: Some(12345),
            mimetype: Some("image/jpeg".into()),
            ..Default::default()
        };
        let body = MessageBody::Image(ImageMedia {
            path: None,
            thumbnail: proto.jpeg_thumbnail.clone(),
            width: Some(800),
            height: Some(600),
            caption: Some("beach".into()),
            state: MediaState::Idle,
        });
        NewMessage { body, media: Some(StoredMedia::Image(Box::new(proto))), ..msg(chat, id, sender, "", ts) }
    }

    #[test]
    fn migrates_old_schema_and_keeps_rows() {
        let path = std::env::temp_dir().join(format!("wa-store-migrate-{}.db", crate::new_message_id()));
        {
            // A messages.db as written by the previous build: v0 schema, user_version 0.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO messages (chat_jid, id, sender_jid, wire_chat, wire_sender, push_name, from_me, kind, text, ts, status)
                 VALUES (?1, 'old1', ?1, ?1, NULL, 'Al', 0, 1, '📷 Photo: beach', 5, 2)",
                [ALICE],
            )
            .unwrap();
            conn.execute("INSERT INTO chats (jid, last_activity) VALUES (?1, 5)", [ALICE]).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let version: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let old = s.get_message(ALICE, "old1").unwrap().unwrap();
        // Old media rows have no proto: they stay placeholders.
        assert_eq!(old.body, MessageBody::Unsupported { label: "📷 Photo: beach".into() });
        assert_eq!(old.quoted, None);
        // A re-delivery (e.g. history re-sync) upgrades the row with the media proto.
        s.insert_message(&image_msg(ALICE, "old1", ALICE, 5), false).unwrap();
        assert!(matches!(s.get_message(ALICE, "old1").unwrap().unwrap().body, MessageBody::Image(_)));
        s.insert_message(&image_msg(ALICE, "new1", ALICE, 6), true).unwrap();
        drop(s);
        // Reopening doesn't re-run migrations.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.load_messages(ALICE, None, 10).unwrap().0.len(), 2);
        assert!(s.avatar(ALICE).unwrap().is_none());
        drop(s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn media_metadata_round_trip_and_download_state() {
        let s = store();
        s.insert_message(&image_msg(ALICE, "img", ALICE, 10), true).unwrap();
        let m = s.get_message(ALICE, "img").unwrap().unwrap();
        let MessageBody::Image(img) = &m.body else { panic!("not an image: {:?}", m.body) };
        assert_eq!((img.width, img.height, img.caption.as_deref()), (Some(800), Some(600), Some("beach")));
        assert_eq!(img.thumbnail.as_deref(), Some(&[0xFF, 0xD8, 0xFF][..]));
        assert_eq!((img.path.clone(), img.state), (None, MediaState::Idle));
        assert_eq!(s.list_chats(0).unwrap()[0].last_message.as_ref().unwrap().text, "📷 beach");

        let raw = s.raw_message(ALICE, "img").unwrap().unwrap();
        let Some(StoredMedia::Image(proto)) = &raw.media else { panic!("no proto") };
        assert_eq!(proto.direct_path.as_deref(), Some("/v/t62.7118-24/x"));
        assert_eq!(proto.media_key.as_deref(), Some(&[1u8; 32][..]));

        s.set_media_state(ALICE, "img", MediaState::Downloading).unwrap();
        s.reset_downloading().unwrap();
        s.set_media_state(ALICE, "img", MediaState::Failed).unwrap();
        let state = |s: &Store| match s.get_message(ALICE, "img").unwrap().unwrap().body {
            MessageBody::Image(i) => (i.state, i.path),
            other => panic!("{other:?}"),
        };
        assert_eq!(state(&s), (MediaState::Failed, None));
        let file = std::env::temp_dir().join(format!("wa-store-img-{}.jpg", crate::new_message_id()));
        std::fs::write(&file, b"jpeg").unwrap();
        s.set_media_path(ALICE, "img", Some(&file)).unwrap();
        assert_eq!(state(&s), (MediaState::Idle, Some(file.clone())));
        // A re-delivered copy keeps the downloaded file.
        s.insert_message(&image_msg(ALICE, "img", ALICE, 10), true).unwrap();
        assert_eq!(state(&s).1, Some(file.clone()));
        // Caption edit keeps the media; a deleted file reads back as not downloaded.
        let edited = MessageBody::Image(ImageMedia { caption: Some("sunset".into()), ..img.clone() });
        let after = s.set_body(ALICE, "img", &edited).unwrap().unwrap();
        assert!(matches!(&after.body, MessageBody::Image(i) if i.caption.as_deref() == Some("sunset") && i.width == Some(800)));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(state(&s).1, None);
        // Revoke drops media.
        let gone = s.set_body(ALICE, "img", &MessageBody::Unsupported { label: "deleted".into() }).unwrap().unwrap();
        assert_eq!(gone.body, MessageBody::Unsupported { label: "deleted".into() });
        assert!(s.raw_message(ALICE, "img").unwrap().unwrap().media.is_none());

        // Voice notes.
        let audio = whatsapp_rust::prelude::wa::message::AudioMessage {
            ptt: Some(true),
            seconds: Some(7),
            waveform: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        let voice = NewMessage {
            body: MessageBody::Voice(VoiceMedia {
                path: None,
                duration_secs: 7,
                waveform: vec![1, 2, 3],
                ptt: true,
                state: MediaState::Idle,
            }),
            media: Some(StoredMedia::Audio(Box::new(audio))),
            ..msg(ALICE, "v", ALICE, "", 11)
        };
        s.insert_message(&voice, true).unwrap();
        assert_eq!(s.get_message(ALICE, "v").unwrap().unwrap().body, voice.body);
    }

    #[test]
    fn quoted_round_trip_resolves_sender() {
        let s = store();
        s.insert_message(&msg(GROUP, "orig", ALICE, "lunch?", 1), true).unwrap();
        s.set_contact_name(ALICE, "Alice").unwrap();
        s.insert_message(&msg(GROUP, "mine", ME, "ok", 2), true).unwrap();
        let reply = |id: &str, quoted: NewQuote| NewMessage { quoted: Some(quoted), ..msg(GROUP, id, BOB, "sure", 3) };

        // Quoted message we have: sender comes from it, even if contextInfo used a LID.
        s.insert_message(
            &reply("r1", NewQuote { id: "orig".into(), sender: Some(ALICE_LID.into()), from_me: false, preview: "lunch?".into() }),
            true,
        )
        .unwrap();
        let q = s.get_message(GROUP, "r1").unwrap().unwrap().quoted.unwrap();
        assert_eq!(q, QuotedMessage { id: "orig".into(), from_me: false, sender_name: Some("Alice".into()), preview: "lunch?".into() });

        // Quoting my own message.
        s.insert_message(&reply("r2", NewQuote { id: "mine".into(), sender: None, from_me: false, preview: "ok".into() }), true)
            .unwrap();
        let q = s.get_message(GROUP, "r2").unwrap().unwrap().quoted.unwrap();
        assert!(q.from_me && q.sender_name.is_none());

        // Quoted message not stored: fall back to the contextInfo, LID resolved later.
        s.insert_message(
            &reply("r3", NewQuote { id: "gone".into(), sender: Some(ALICE_LID.into()), from_me: false, preview: "old".into() }),
            true,
        )
        .unwrap();
        assert_eq!(s.get_message(GROUP, "r3").unwrap().unwrap().quoted.unwrap().sender_name.as_deref(), Some("900000000001"));
        s.add_alias(ALICE_LID, ALICE).unwrap();
        let (page, _) = s.load_messages(GROUP, None, 10).unwrap();
        let r3 = page.iter().find(|m| m.id == "r3").unwrap();
        assert_eq!(r3.quoted.as_ref().unwrap().sender_name.as_deref(), Some("Alice"));
        assert_eq!(r3.quoted.as_ref().unwrap().preview, "old");
        assert!(page.iter().find(|m| m.id == "orig").unwrap().quoted.is_none());
    }

    #[test]
    fn local_profiles() {
        let s = store();
        s.insert_message(&msg(ALICE, "1", ALICE, "a", 1), true).unwrap();
        let p = s.local_profile(ALICE).unwrap();
        assert_eq!((p.canonical.as_str(), p.name.as_str(), p.is_group, p.avatar), (ALICE, "+15550000001", false, None));
        s.set_push_name(ALICE, "ali").unwrap();
        assert_eq!(s.local_profile(ALICE).unwrap().name, "ali");
        s.set_contact_name(ALICE, "Alice").unwrap();
        assert_eq!(s.local_profile(ALICE).unwrap().name, "Alice");
        // A LID resolves to its PN chat.
        s.add_alias(ALICE_LID, ALICE).unwrap();
        assert_eq!(s.local_profile(ALICE_LID).unwrap().canonical, ALICE);
        // A group we only know by JID.
        let g = s.local_profile("120363040237990503@g.us").unwrap();
        assert!(g.is_group);
        assert_eq!(g.name, "Group");
    }

    #[test]
    fn member_names() {
        let s = store();
        // Saved name > push name > formatted phone.
        assert_eq!(s.member("923001234567@s.whatsapp.net", None).unwrap().1, "+92 300 1234567");
        s.set_push_name("923001234567@s.whatsapp.net", "ali").unwrap();
        assert_eq!(s.member("923001234567@s.whatsapp.net", None).unwrap().1, "ali");
        s.set_contact_name("923001234567@s.whatsapp.net", "Ali Khan").unwrap();
        assert_eq!(s.member("923001234567:5@s.whatsapp.net", None).unwrap().1, "Ali Khan");
        // A LID member with a PN from the group metadata is shown by that PN.
        let (jid, name) = s.member("200000000000001@lid", Some("923001234567@s.whatsapp.net")).unwrap();
        assert_eq!((jid.as_str(), name.as_str()), ("923001234567@s.whatsapp.net", "Ali Khan"));
        // A LID member without one: its push name if we have it, else the LID user.
        assert_eq!(s.member("200000000000002@lid", None).unwrap().1, "200000000000002");
        s.set_push_name("200000000000002@lid", "zee").unwrap();
        assert_eq!(s.member("200000000000002@lid", None).unwrap(), ("200000000000002@lid".into(), "zee".into()));
    }

    #[test]
    fn avatars_and_chat_info() {
        let s = store();
        s.insert_message(&msg(ALICE, "1", ALICE, "a", 1), true).unwrap();
        assert_eq!(s.list_chats(0).unwrap()[0].avatar, None);
        let file = std::env::temp_dir().join(format!("wa-avatar-{}.jpg", crate::new_message_id()));
        std::fs::write(&file, b"jpeg").unwrap();
        s.set_avatar(ALICE, Some("pic1"), Some(&file), 100).unwrap();
        assert_eq!(s.list_chats(0).unwrap()[0].avatar, Some(file.clone()));
        assert_eq!(
            s.avatar(ALICE).unwrap(),
            Some(AvatarRow { picture_id: Some("pic1".into()), path: Some(file.clone()), fetched_at: 100 })
        );
        assert!(s.invalidate_avatar(ALICE).unwrap());
        assert_eq!(s.avatar(ALICE).unwrap().unwrap().fetched_at, 0);
        assert!(!s.invalidate_avatar(BOB).unwrap());
        // "No picture" is remembered too.
        s.set_avatar(BOB, None, None, 100).unwrap();
        assert_eq!(s.avatar(BOB).unwrap().unwrap().path, None);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(s.list_chats(0).unwrap()[0].avatar, None);

        s.set_contact_name(ALICE, "Alice").unwrap();
        s.set_muted_until(ALICE, 1_000).unwrap();
        assert_eq!(s.chat_info(ALICE, 500).unwrap(), Some(("Alice".into(), false, true)));
        assert_eq!(s.chat_info(ALICE, 2_000).unwrap(), Some(("Alice".into(), false, false)));
        assert_eq!(s.chat_info(BOB, 0).unwrap(), None);
        s.clear_all().unwrap();
        assert!(s.avatar(BOB).unwrap().is_none());
    }

    #[test]
    fn pending_sends_fail_at_teardown() {
        let s = store();
        let mut p = msg(ALICE, "p", ME, "hi", 1);
        p.status = DeliveryStatus::Pending;
        s.insert_message(&p, false).unwrap();
        s.insert_message(&msg(ALICE, "s", ME, "sent", 2), false).unwrap();
        assert_eq!(s.fail_pending().unwrap(), vec![(ALICE.to_string(), "p".to_string())]);
        assert_eq!(s.get_message(ALICE, "p").unwrap().unwrap().status, DeliveryStatus::Failed);
        assert!(s.fail_pending().unwrap().is_empty());
    }

    /// Only my own failed messages go back to `Pending` for a resend, once: not sent /
    /// others' / revoked / expired ones; crash-failed ones (startup reconcile) too.
    #[test]
    fn failed_sends_reset_for_retry() {
        let s = store();
        let failed = |id: &str, sender: &str| {
            s.insert_message(&NewMessage { status: DeliveryStatus::Pending, ..msg(ALICE, id, sender, "hi", 1) }, false).unwrap();
            s.mark_failed(ALICE, id).unwrap();
        };
        failed("f", ME);
        assert!(s.reset_failed(ALICE, "f", 10).unwrap());
        assert_eq!(s.get_message(ALICE, "f").unwrap().unwrap().status, DeliveryStatus::Pending);
        assert!(!s.reset_failed(ALICE, "f", 10).unwrap(), "already being resent");

        s.insert_message(&msg(ALICE, "sent", ME, "hi", 1), false).unwrap();
        assert!(!s.reset_failed(ALICE, "sent", 10).unwrap());
        assert_eq!(s.get_message(ALICE, "sent").unwrap().unwrap().status, DeliveryStatus::Sent);

        s.insert_message(&NewMessage { status: DeliveryStatus::Failed, ..msg(ALICE, "theirs", ALICE, "hi", 1) }, false).unwrap();
        assert!(!s.reset_failed(ALICE, "theirs", 10).unwrap());

        failed("revoked", ME);
        s.conn.execute("UPDATE messages SET revoked = 1 WHERE id = 'revoked'", []).unwrap();
        assert!(!s.reset_failed(ALICE, "revoked", 10).unwrap());

        s.insert_message(&NewMessage { expires_at: Some(10), ..msg(ALICE, "exp", ME, "hi", 1) }, false).unwrap();
        s.mark_failed(ALICE, "exp").unwrap();
        s.conn.execute("UPDATE messages SET status = -1 WHERE id = 'exp'", []).unwrap();
        assert!(!s.reset_failed(ALICE, "exp", 10).unwrap(), "expired");
        assert!(s.reset_failed(ALICE, "exp", 9).unwrap(), "not yet expired");

        s.insert_message(&NewMessage { status: DeliveryStatus::Pending, ..msg(ALICE, "crash", ME, "hi", 1) }, false).unwrap();
        s.fail_pending().unwrap();
        assert!(s.reset_failed(ALICE, "crash", 10).unwrap());
        assert!(!s.reset_failed(ALICE, "missing", 10).unwrap());
    }

    /// Migrate a copy of a real messages.db: `WA_TEST_DB=/tmp/copy/messages.db cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn migrate_real_db_copy() {
        let Ok(path) = std::env::var("WA_TEST_DB") else { return };
        let count = |c: &Connection| -> i64 { c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap() };
        let before = count(&Connection::open(&path).unwrap());
        let s = Store::open(Path::new(&path)).unwrap();
        assert_eq!(count(&s.conn), before);
        let chats = s.list_chats(0).unwrap();
        let mut loaded = 0;
        for c in &chats {
            loaded += s.load_messages(&c.jid, None, 50).unwrap().0.len();
        }
        eprintln!("{before} messages, {} chats, {loaded} loaded after migration", chats.len());
    }

    fn with_body(id: &str, ts: i64, body: MessageBody, media: Option<StoredMedia>) -> NewMessage {
        NewMessage { body, media, ..msg(ALICE, id, ALICE, "", ts) }
    }

    /// Every kind, as extraction would produce it (body + media proto).
    fn all_kinds() -> Vec<NewMessage> {
        use crate::extract::{body_of, media_of};
        use whatsapp_rust::prelude::MessageField;
        let from = |id: &str, ts: i64, m: wa::Message| with_body(id, ts, body_of(&m).unwrap(), media_of(&m));
        let video = wa::message::VideoMessage {
            caption: Some("clip".into()),
            seconds: Some(9),
            width: Some(640),
            height: Some(360),
            gif_playback: Some(true),
            jpeg_thumbnail: Some(vec![1]),
            mimetype: Some("video/mp4".into()),
            direct_path: Some("/v/vid".into()),
            media_key: Some(vec![5; 32]),
            ..Default::default()
        };
        vec![
            from("video", 1, wa::Message { video_message: MessageField::some(video.clone()), ..Default::default() }),
            from("ptv", 2, wa::Message { ptv_message: MessageField::some(video), ..Default::default() }),
            from(
                "doc",
                3,
                wa::Message {
                    document_message: MessageField::some(wa::message::DocumentMessage {
                        file_name: Some("r.pdf".into()),
                        mimetype: Some("application/pdf".into()),
                        file_length: Some(99),
                        page_count: Some(2),
                        caption: Some("see".into()),
                        jpeg_thumbnail: Some(vec![2]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            from(
                "sticker",
                4,
                wa::Message {
                    sticker_message: MessageField::some(wa::message::StickerMessage {
                        width: Some(512),
                        height: Some(512),
                        is_animated: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            with_body(
                "loc",
                5,
                MessageBody::Location(LocationInfo {
                    latitude: -33.86,
                    longitude: 151.21,
                    name: Some("Opera House".into()),
                    address: None,
                    thumbnail: Some(vec![3, 4]),
                    live: true,
                }),
                None,
            ),
            with_body(
                "contacts",
                6,
                MessageBody::Contacts(vec![
                    ContactCard { display_name: "Jane; \\Doe, Jr".into(), phones: vec!["+1 555".into(), "+44 1".into()] },
                    ContactCard { display_name: "No phone".into(), phones: vec![] },
                ]),
                None,
            ),
            with_body(
                "poll1",
                7,
                MessageBody::Poll(PollInfo { question: "Q?".into(), options: vec!["a".into(), "b".into()], multiple: false }),
                None,
            ),
            with_body(
                "poll2",
                8,
                MessageBody::Poll(PollInfo { question: "Many?".into(), options: vec![], multiple: true }),
                None,
            ),
            with_body(
                "rich",
                9,
                MessageBody::Rich(RichText {
                    header: Some("Hi".into()),
                    body: "Body".into(),
                    footer: Some("Foot".into()),
                    buttons: vec!["One".into(), "Two".into()],
                }),
                None,
            ),
            with_body(
                "rich2",
                10,
                MessageBody::Rich(RichText { header: None, body: "Just body".into(), footer: None, buttons: vec![] }),
                None,
            ),
            with_body("sys", 11, MessageBody::System { text: "📞 Missed voice call".into() }, None),
        ]
    }

    #[test]
    fn every_kind_round_trips() {
        let s = store();
        let kinds = all_kinds();
        for m in &kinds {
            s.insert_message(m, false).unwrap();
        }
        for m in &kinds {
            let got = s.get_message(ALICE, &m.id).unwrap().unwrap();
            assert_eq!(got.body, m.body, "{}", m.id);
            assert_eq!(s.raw_message(ALICE, &m.id).unwrap().unwrap().media, m.media, "{}", m.id);
        }
        // Spot checks of what extraction produced.
        assert!(matches!(&kinds[1].body, MessageBody::Video(v) if v.round && v.gif));
        assert!(matches!(&kinds[3].body, MessageBody::Sticker(st) if st.animated));
        let (page, _) = s.load_messages(ALICE, None, 50).unwrap();
        assert_eq!(page.len(), kinds.len());
        assert_eq!(s.list_chats(0).unwrap()[0].last_message.as_ref().unwrap().text, "📞 Missed voice call");
        // Re-delivery keeps the body (payload refreshed, not dropped).
        s.insert_message(&kinds[4], false).unwrap();
        assert_eq!(s.get_message(ALICE, "loc").unwrap().unwrap().body, kinds[4].body);
    }

    #[test]
    fn downloads_of_new_media_kinds() {
        let s = store();
        for m in all_kinds().into_iter().take(4) {
            s.insert_message(&m, false).unwrap();
            let file = std::env::temp_dir().join(format!("wa-store-dl-{}", crate::new_message_id()));
            std::fs::write(&file, b"x").unwrap();
            s.set_media_state(ALICE, &m.id, MediaState::Downloading).unwrap();
            let state = |s: &Store| crate::store::media_path(&s.get_message(ALICE, &m.id).unwrap().unwrap().body);
            assert_eq!(state(&s), None);
            s.set_media_path(ALICE, &m.id, Some(&file)).unwrap();
            assert_eq!(state(&s), Some(file.clone()), "{}", m.id);
            // `raw_message` sees the file too: `DownloadMedia` is idempotent.
            assert_eq!(s.raw_message(ALICE, &m.id).unwrap().unwrap().media_path, Some(file.clone()));
            std::fs::remove_file(&file).unwrap();
            assert_eq!(state(&s), None);
        }
    }

    #[test]
    fn caption_edits_keep_media_and_revoke_drops_file() {
        let s = store();
        let kinds = all_kinds();
        let (video, doc, sticker) = (&kinds[0], &kinds[2], &kinds[3]);
        for m in [video, doc, sticker] {
            s.insert_message(m, false).unwrap();
        }
        let MessageBody::Video(v) = &video.body else { unreachable!() };
        let edited = MessageBody::Video(VideoMedia { caption: Some("new".into()), ..v.clone() });
        let after = s.set_body(ALICE, "video", &edited).unwrap().unwrap();
        assert!(matches!(&after.body, MessageBody::Video(x) if x.caption.as_deref() == Some("new") && x.width == Some(640)));
        let MessageBody::Document(d) = &doc.body else { unreachable!() };
        let edited = MessageBody::Document(DocumentMedia { caption: None, ..d.clone() });
        let after = s.set_body(ALICE, "doc", &edited).unwrap().unwrap();
        assert!(matches!(&after.body, MessageBody::Document(x) if x.caption.is_none() && x.page_count == Some(2)));
        // Stickers can't be edited.
        let MessageBody::Sticker(st) = &sticker.body else { unreachable!() };
        let same = s.set_body(ALICE, "sticker", &MessageBody::Sticker(StickerMedia { animated: false, ..st.clone() }));
        assert_eq!(same.unwrap().unwrap().body, sticker.body);
        // An edit of a text keeps its quote; a revoke drops it, the media and the file.
        let file = std::env::temp_dir().join(format!("wa-store-rev-{}", crate::new_message_id()));
        std::fs::write(&file, b"x").unwrap();
        s.set_media_path(ALICE, "doc", Some(&file)).unwrap();
        let gone = s.set_body(ALICE, "doc", &MessageBody::Unsupported { label: "deleted".into() }).unwrap().unwrap();
        assert_eq!(gone.body, MessageBody::Unsupported { label: "deleted".into() });
        assert!(!file.exists());
        // Caption edits sent as plain text keep the media and its file.
        let file = std::env::temp_dir().join(format!("wa-store-cap-{}", crate::new_message_id()));
        std::fs::write(&file, b"x").unwrap();
        s.set_media_path(ALICE, "video", Some(&file)).unwrap();
        let after = s.set_body(ALICE, "video", &MessageBody::Text(" newer ".into())).unwrap().unwrap();
        assert!(
            matches!(&after.body, MessageBody::Video(x) if x.caption.as_deref() == Some("newer") && x.path.as_ref() == Some(&file)),
            "{:?}",
            after.body
        );
        // A sticker can't take a caption: a text "edit" replaces it, but its file stays.
        s.set_media_path(ALICE, "sticker", Some(&file)).unwrap();
        let after = s.set_body(ALICE, "sticker", &MessageBody::Text("hm".into())).unwrap().unwrap();
        assert_eq!(after.body, MessageBody::Text("hm".into()));
        assert!(file.exists());
        std::fs::remove_file(&file).unwrap();
        let quote = NewQuote { id: "video".into(), sender: None, from_me: false, preview: "🎥 Video".into() };
        s.insert_message(&NewMessage { quoted: Some(quote), ..msg(ALICE, "t", ALICE, "typo", 20) }, false).unwrap();
        let fixed = s.set_body(ALICE, "t", &MessageBody::Text("fixed".into())).unwrap().unwrap();
        assert_eq!(fixed.quoted.map(|q| q.id).as_deref(), Some("video"));
    }

    #[test]
    fn migrates_v1_schema() {
        let dir = std::env::temp_dir().join(format!("wa-store-v1-{}", crate::new_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("messages.db");
        {
            // A messages.db as written by the previous build (schema 1), including a
            // video it could only store as a placeholder.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch(&format!("BEGIN; {}; PRAGMA user_version = 1; COMMIT;", MIGRATIONS[0])).unwrap();
            conn.execute(
                "INSERT INTO messages (chat_jid, id, sender_jid, wire_chat, from_me, kind, text, ts, status, media_state)
                 VALUES (?1, 'v', ?1, ?1, 0, 1, '🎥 Video', 5, 2, 0)",
                [ALICE],
            )
            .unwrap();
            // Created with default permissions.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let version: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(s.get_message(ALICE, "v").unwrap().unwrap().body, MessageBody::Unsupported { label: "🎥 Video".into() });
        // A re-delivery upgrades the placeholder.
        let video = all_kinds().remove(0);
        s.insert_message(&NewMessage { id: "v".into(), ..video.clone() }, false).unwrap();
        assert_eq!(s.get_message(ALICE, "v").unwrap().unwrap().body, video.body);
        for m in all_kinds() {
            s.insert_message(&m, false).unwrap();
        }
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.load_messages(ALICE, None, 50).unwrap().0.len(), all_kinds().len() + 1);
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Bytes of every file under `dir`.
    fn all_bytes(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(all_bytes(&path));
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
        out
    }

    pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn wipe_deletes_the_database_files() {
        let dir = std::env::temp_dir().join(format!("wa-store-wipe-{}", crate::new_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("messages.db");
        let marker = format!("marker-{}", crate::new_message_id());
        let mut s = Store::open(&path).unwrap();
        for i in 0..200 {
            s.insert_message(&msg(ALICE, &format!("m{i}"), ALICE, &format!("{marker} {i}"), i), true).unwrap();
        }
        s.set_contact_name(ALICE, &marker).unwrap();
        assert!(all_bytes(&dir).iter().any(|(_, b)| contains(b, marker.as_bytes())));
        s.wipe().unwrap();
        for (file, bytes) in all_bytes(&dir) {
            assert!(!contains(&bytes, marker.as_bytes()), "{} still has old data", file.display());
        }
        // Still usable, empty, owner-only.
        assert!(s.list_chats(0).unwrap().is_empty());
        s.insert_message(&msg(BOB, "1", BOB, "after", 1), true).unwrap();
        assert_eq!(s.list_chats(0).unwrap().len(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn muted_and_archived() {
        let s = store();
        s.insert_message(&msg(ALICE, "1", ALICE, "a", 1), true).unwrap();
        s.set_muted_until(ALICE, 1_000).unwrap();
        s.set_archived(ALICE, true).unwrap();
        let c = &s.list_chats(500).unwrap()[0];
        assert!(c.muted && c.archived);
        assert!(!s.list_chats(2_000).unwrap()[0].muted);
    }
}
