//! Shared contract between the UI (`crates/app`) and the backend. Keep it UI-agnostic.

use std::path::PathBuf;

/// Unix timestamp in seconds.
pub type Timestamp = i64;

#[derive(Clone, Debug, PartialEq)]
pub struct ChatSummary {
    /// Chat JID string, e.g. `15551234567@s.whatsapp.net` or `1203...@g.us`.
    pub jid: String,
    /// Best available display name (saved contact name > push name > group subject > phone).
    pub name: String,
    pub is_group: bool,
    pub last_message: Option<MessagePreview>,
    pub unread_count: u32,
    pub pinned: bool,
    pub archived: bool,
    pub muted: bool,
    /// Cached profile picture (JPEG on disk), once fetched. See [`Command::FetchAvatar`].
    pub avatar: Option<PathBuf>,
    /// The community this group belongs to.
    pub community: Option<CommunityRef>,
    /// A community itself (its parent group), which holds no conversation of its own.
    pub is_community: bool,
}

/// A WhatsApp Community, as one of its groups refers to it.
#[derive(Clone, Debug, PartialEq)]
pub struct CommunityRef {
    pub jid: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessagePreview {
    pub text: String,
    pub timestamp: Timestamp,
    pub from_me: bool,
    /// For groups: display name of the sender.
    pub sender_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryStatus {
    Pending,
    Sent,
    Delivered,
    Read,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MessageBody {
    Text(String),
    Image(ImageMedia),
    /// Voice note (PTT) or audio file.
    Voice(VoiceMedia),
    /// Video, GIF, or round video note (PTV).
    Video(VideoMedia),
    Document(DocumentMedia),
    /// Rendered without a bubble, like Messages stickers.
    Sticker(StickerMedia),
    Location(LocationInfo),
    /// One or more shared contact cards.
    Contacts(Vec<ContactCard>),
    Poll(PollInfo),
    /// Business / bot messages: templates, buttons, lists, interactive messages.
    Rich(RichText),
    /// Call log / event / group invite etc.: rendered as a small centered system line.
    System { text: String },
    /// View-once photo / video / voice message. Its media is never downloaded or kept
    /// (nor its caption); show a placeholder like "📷 View once photo: open it on your phone".
    ViewOnce(ViewOnceKind),
    /// Media and other non-text messages we don't render yet; `label` is a short
    /// placeholder such as "🎥 Video" plus the caption if any.
    Unsupported { label: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewOnceKind {
    Photo,
    Video,
    Voice,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageMedia {
    /// Full-size image on disk once downloaded ([`Command::DownloadMedia`]); `None` before.
    pub path: Option<PathBuf>,
    /// Inline JPEG thumbnail from the message itself, for an instant blurred preview.
    pub thumbnail: Option<Vec<u8>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub caption: Option<String>,
    /// A download is in progress / failed.
    pub state: MediaState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceMedia {
    /// Decoded, directly playable audio (WAV, 16-bit PCM) once downloaded; `None` before.
    pub path: Option<PathBuf>,
    pub duration_secs: u32,
    /// Waveform from the message (WhatsApp sends 64 samples, 0..=100), if any.
    pub waveform: Vec<u8>,
    /// True for push-to-talk voice notes, false for forwarded audio files.
    pub ptt: bool,
    pub state: MediaState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoMedia {
    /// Downloaded file (as sent: usually MP4). Opened in the system player.
    pub path: Option<PathBuf>,
    pub thumbnail: Option<Vec<u8>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: u32,
    pub caption: Option<String>,
    /// Plays like a GIF (muted loop) in WhatsApp.
    pub gif: bool,
    /// Round "video note".
    pub round: bool,
    pub size_bytes: Option<u64>,
    pub state: MediaState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DocumentMedia {
    pub path: Option<PathBuf>,
    pub file_name: String,
    pub mimetype: Option<String>,
    pub size_bytes: Option<u64>,
    pub page_count: Option<u32>,
    /// First-page preview (JPEG), when the sender attached one.
    pub thumbnail: Option<Vec<u8>>,
    pub caption: Option<String>,
    pub state: MediaState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StickerMedia {
    /// Decoded to PNG (first frame for animated stickers) so the UI can show it directly.
    pub path: Option<PathBuf>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub animated: bool,
    pub state: MediaState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocationInfo {
    pub latitude: f64,
    pub longitude: f64,
    pub name: Option<String>,
    pub address: Option<String>,
    /// Map snapshot JPEG from the message, if any.
    pub thumbnail: Option<Vec<u8>>,
    pub live: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContactCard {
    pub display_name: String,
    /// Phone numbers parsed from the vCard (`TEL` lines), in display form.
    pub phones: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PollInfo {
    pub question: String,
    pub options: Vec<String>,
    /// Multiple answers allowed. (Vote tallies are end-to-end encrypted; not shown yet.)
    pub multiple: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RichText {
    pub header: Option<String>,
    pub body: String,
    pub footer: Option<String>,
    /// Button / list-row titles, display only.
    pub buttons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaState {
    /// Not downloaded yet (or path present = done).
    #[default]
    Idle,
    Downloading,
    Failed,
}

/// The message a message replies to, as shown in the reply preview above the bubble.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotedMessage {
    pub id: String,
    pub from_me: bool,
    pub sender_name: Option<String>,
    /// Short text: the quoted text, or "📷 Photo" / "🎤 Voice message" etc.
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub id: String,
    pub chat_jid: String,
    pub sender_jid: String,
    pub sender_name: Option<String>,
    pub from_me: bool,
    pub body: MessageBody,
    pub timestamp: Timestamp,
    /// Only meaningful for `from_me` messages.
    pub status: DeliveryStatus,
    /// Set when this message is a reply.
    pub quoted: Option<QuotedMessage>,
    /// Current reactions, one per sender (WhatsApp allows one emoji per person).
    pub reactions: Vec<Reaction>,
    /// The text/caption was edited after sending (shown as "Edited").
    pub edited: bool,
    /// Sent with WhatsApp's "Forwarded" mark.
    pub forwarded: bool,
    /// Deleted for everyone: `body` is only a placeholder ("This message was deleted").
    pub deleted: bool,
    /// People @mentioned in the text / caption (`@<digits>` tokens), resolved to names.
    pub mentions: Vec<Mention>,
}

/// An `@<digits>` token in a message's text and who it stands for.
#[derive(Clone, Debug, PartialEq)]
pub struct Mention {
    /// The digits after the `@`, as they appear in the text.
    pub user: String,
    pub jid: String,
    pub name: String,
}

/// One message of a chat matching [`Command::SearchMessages`].
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub timestamp: Timestamp,
}

/// A sticker I can send from the sticker panel. See [`Command::ListStickers`].
#[derive(Clone, Debug, PartialEq)]
pub struct StickerRef {
    /// Stable key (e.g. the sticker file's SHA-256, hex).
    pub id: String,
    /// PNG preview on disk once available (`None` while it downloads).
    pub preview: Option<PathBuf>,
    pub source: StickerSource,
    pub animated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StickerSource {
    /// Starred on the phone (synced from WhatsApp app state).
    Favorite,
    /// Recently sent by me.
    Recent,
}

/// Details for the profile / group info panel. See [`Command::FetchProfile`].
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub jid: String,
    pub name: String,
    /// Display form, e.g. "+92 300 1234567" (users only, when the number is known).
    pub phone: Option<String>,
    /// The user's "About" text (may be hidden by their privacy settings).
    pub about: Option<String>,
    /// Full-size profile photo on disk, if any.
    pub photo: Option<PathBuf>,
    pub is_group: bool,
    /// Group description.
    pub description: Option<String>,
    /// Group members, admins first, then by name.
    pub participants: Vec<Participant>,
    pub created_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Participant {
    pub jid: String,
    pub name: String,
    pub is_admin: bool,
    pub is_me: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reaction {
    pub emoji: String,
    pub sender_jid: String,
    pub sender_name: Option<String>,
    pub from_me: bool,
    pub timestamp: Timestamp,
}

/// How far a group member has read: the newest of my messages they've read.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupRead {
    pub jid: String,
    pub name: String,
    pub message_id: String,
    /// The message's timestamp.
    pub timestamp: Timestamp,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    /// Starting up / reconnecting.
    Connecting,
    /// Not paired: show the QR. `code` is the raw string to encode as a QR image.
    AwaitingQr { code: String, valid_for_secs: u64 },
    /// Paired and online.
    Connected { own_jid: String, push_name: Option<String> },
    Disconnected { reason: String },
    /// Session ended (unlinked from phone). The UI should go back to the QR screen.
    LoggedOut,
}

#[derive(Clone, Debug)]
// `MessageUpserted` is the common case; boxing it would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Connection(ConnectionState),
    /// History sync progress 0..=100, `None` when finished.
    SyncProgress(Option<u8>),
    /// Full chat list, sorted: pinned first, then most recent activity. Sent on startup
    /// (from the local store) and whenever it changes (debounced by the backend).
    Chats(Vec<ChatSummary>),
    /// Reply to [`Command::LoadMessages`], oldest first.
    Messages { chat_jid: String, messages: Vec<ChatMessage>, has_more: bool },
    /// Older history for `chat_jid` is being fetched from the phone (`loading`), or the
    /// fetch finished. `exhausted` means the phone has nothing older: stop asking.
    /// Fetched messages arrive as a regular [`Event::Messages`] page.
    HistoryLoading { chat_jid: String, loading: bool, exhausted: bool },
    /// A new or updated message (incoming, own message echo, or edit).
    MessageUpserted(ChatMessage),
    /// Messages deleted from the store (disappearing messages that expired): drop them
    /// from the transcript.
    MessagesRemoved { chat_jid: String, message_ids: Vec<String> },
    /// Stored messages of `chat_jid` changed in bulk (history sync). A UI that has this
    /// transcript loaded should reload it (`LoadMessages { before: None }` with a limit of at
    /// least what it shows); others can ignore it. Debounced per chat.
    TranscriptStale { chat_jid: String },
    /// Chat `from` (a LID chat) turned out to be the same person as `to` and was merged
    /// into it; `from` no longer exists. Re-key UI state (selection, loaded transcript).
    ChatMerged { from: String, to: String },
    StatusChanged { chat_jid: String, message_ids: Vec<String>, status: DeliveryStatus },
    /// How far a media download has got, 0..1 (only when the file's size is known).
    /// The finished download arrives as a `MessageUpserted` with the path set.
    DownloadProgress { chat_jid: String, message_id: String, fraction: f32 },
    /// Answer to [`Command::ListMedia`]: the chat's photos and videos, newest first.
    ChatMedia { chat_jid: String, items: Vec<ChatMessage> },
    /// Where each member of group `chat_jid` has read up to (all of them, by name). Sent
    /// after a group's [`Event::Messages`] page and whenever a read receipt moves someone.
    GroupReads { chat_jid: String, reads: Vec<GroupRead> },
    /// Someone is typing in `chat_jid`.
    Typing { chat_jid: String, sender_name: Option<String>, typing: bool, sender_jid: Option<String> },
    /// Answer to [`Command::ListStickers`] (and pushed again when it changes).
    Stickers(Vec<StickerRef>),
    /// Answer to [`Command::FetchProfile`].
    Profile(Profile),
    /// Profile picture for a chat or group participant, after [`Command::FetchAvatar`]
    /// (`None` = no picture / hidden by privacy settings).
    AvatarUpdated { jid: String, path: Option<PathBuf> },
    /// A live incoming message the user should be notified about: not from me, not from
    /// history sync / offline catch-up older than a minute, chat not muted.
    Notify { chat_jid: String, message_id: String, title: String, body: String },
    Error(String),
    /// Answer to [`Command::SearchMessages`]: every stored message of `chat_jid` whose
    /// text or caption contains `query`, oldest first.
    SearchResults { chat_jid: String, query: String, hits: Vec<SearchHit> },
    /// Answer to [`Command::ListContacts`], sorted by name.
    Contacts(Vec<Contact>),
    /// Answer to [`Command::LookupPhone`] (`phone` as asked). `jid` is the chat to open,
    /// `None` when the number isn't on WhatsApp; `error` when the check itself failed
    /// (offline, invalid number).
    PhoneLookup { phone: String, jid: Option<String>, error: Option<String> },
}

#[derive(Clone, Debug)]
pub enum Command {
    /// Load a page of messages. `before` = load older than this timestamp (None = newest).
    LoadMessages { chat_jid: String, before: Option<Timestamp>, limit: u32 },
    /// Send a text. The backend immediately emits `MessageUpserted` with `Pending`,
    /// then `StatusChanged` as the server acks.
    ///
    /// `client_id` becomes the WhatsApp message id if it is 8-64 ASCII alphanumerics
    /// (use [`crate::new_message_id`]); otherwise the backend generates one, and the
    /// `MessageUpserted` carries it.
    ///
    /// `mentions`: JIDs @mentioned in `text`, which carries them as `@<digits of the JID>`.
    SendText { chat_jid: String, text: String, client_id: String, reply_to: Option<String>, mentions: Vec<String> },
    /// Send a photo from disk (JPEG/PNG/HEIC/WebP). Same lifecycle as `SendText`.
    SendImage { chat_jid: String, path: PathBuf, caption: Option<String>, client_id: String, reply_to: Option<String> },
    /// Download an image/voice/video/document/sticker message's media; the backend emits `MessageUpserted`
    /// with `state: Downloading`, then with `path` set (or `Failed`). Idempotent.
    DownloadMedia { chat_jid: String, message_id: String },
    /// List my stickers (favorites first, then recently sent); answered by
    /// `Event::Stickers`, re-sent as previews finish downloading or favorites change.
    ListStickers,
    /// Send a sticker from the panel. Same lifecycle as `SendText` (Pending → Sent/Failed).
    SendSticker { chat_jid: String, sticker_id: String, client_id: String, reply_to: Option<String> },
    /// Load profile / group details; answered by `Event::Profile` (first from the local
    /// store immediately, then again once fresh data arrives from the server).
    FetchProfile { jid: String },
    /// Try again to send one of my messages that ended `Failed` (text, photo, sticker),
    /// keeping its id so it can't arrive twice. Same lifecycle as the original send:
    /// `MessageUpserted` Pending → `StatusChanged` Sent/Failed.
    RetrySend { chat_jid: String, message_id: String },
    /// Edit one of my own text messages (or a photo/video caption). WhatsApp allows this
    /// for 15 minutes after sending. The backend applies it locally right away
    /// (`MessageUpserted` with `edited: true`), and reverts + `Event::Error` on failure.
    EditMessage { chat_jid: String, message_id: String, text: String },
    /// React to a message (`Some(emoji)`) or remove my reaction (`None`). The backend
    /// re-emits the target message via `MessageUpserted` with updated `reactions`
    /// immediately (optimistic), and again if the send fails.
    React { chat_jid: String, message_id: String, emoji: Option<String> },
    /// The chat's photos and videos for the info panel (at most `limit`, newest first);
    /// answered by `ChatMedia`.
    ListMedia { chat_jid: String, limit: u32 },
    /// Fetch (or refresh, if stale) a profile picture; answered by `AvatarUpdated`.
    FetchAvatar { jid: String },
    MarkRead { chat_jid: String },
    SetTyping { chat_jid: String, typing: bool },
    /// Whether the app is in front. WhatsApp keeps the phone's notifications quiet while
    /// a linked device is online, so it's only online then, like WhatsApp Web.
    SetActive { active: bool },
    Logout,
    Shutdown,
    /// Delete a message. `for_everyone` revokes one of my own messages (WhatsApp allows
    /// this for about 2½ days, see [`REVOKE_WINDOW_SECS`]); the backend sends it, then
    /// emits `MessageUpserted` with `deleted: true` (or `Event::Error`). Otherwise it's
    /// removed from this account only (synced to my other devices): `MessagesRemoved`.
    DeleteMessage { chat_jid: String, message_id: String, for_everyone: bool },
    /// Send a copy of a stored message to `to_chat_jid`, marked "Forwarded". Same
    /// lifecycle as `SendText` in the target chat (media is reused or re-uploaded).
    ForwardMessage { chat_jid: String, message_id: String, to_chat_jid: String, client_id: String },
    /// Find stored messages of a chat by text (case-insensitive); answered by
    /// `Event::SearchResults`.
    SearchMessages { chat_jid: String, query: String },
    /// Pin / unpin a chat (synced through WhatsApp app state). WhatsApp allows 3 pins;
    /// a 4th is refused with `Event::Error`. Applied locally right away (`Chats`), and
    /// reverted + `Event::Error` if the phone can't be told.
    SetPinned { chat_jid: String, pinned: bool },
    /// Mute until a time (`i64::MAX` = always), or unmute with `0`. Same lifecycle as
    /// `SetPinned`. Muted chats don't notify and don't count towards the Dock badge.
    SetMuted { chat_jid: String, until: Timestamp },
    /// Archive / unarchive. Archiving a pinned chat unpins it, like WhatsApp.
    SetArchived { chat_jid: String, archived: bool },
    /// "Mark as Unread" (shows the unread dot; synced). `MarkRead` undoes it.
    MarkUnread { chat_jid: String },
    /// My contacts for the New Message picker; answered by `Event::Contacts`.
    ListContacts,
    /// Is this phone number (digits with country code, any formatting) on WhatsApp?
    /// Answered by `Event::PhoneLookup`.
    LookupPhone { phone: String },
    /// Make sure a 1:1 chat exists in the chat list (New Message to someone without a
    /// chat yet); a `Chats` update follows.
    OpenChat { jid: String },
    /// Send any file: videos (MP4, or converted to it) as videos, everything else as a
    /// document. Same lifecycle as `SendText`; the file is prepared and uploaded in order
    /// with other sends (the Pending message is shown right away and updated once its
    /// thumbnail is ready).
    SendFile { chat_jid: String, path: PathBuf, caption: Option<String>, client_id: String, reply_to: Option<String> },
    /// Send a recorded voice note (mono PCM), encoded as a WhatsApp PTT (Ogg/Opus).
    SendVoice { chat_jid: String, samples: Vec<i16>, sample_rate: u32, client_id: String, reply_to: Option<String> },
}

/// Someone I can start a chat with (New Message picker).
#[derive(Clone, Debug, PartialEq)]
pub struct Contact {
    /// Phone-number JID when known (`...@s.whatsapp.net`), else the LID.
    pub jid: String,
    /// Saved contact name, or their WhatsApp name.
    pub name: String,
    /// Display form, e.g. "+92 300 1234567".
    pub phone: Option<String>,
    /// In my address book (not just someone I've seen in a group).
    pub saved: bool,
}

/// How long after sending a message can still be deleted for everyone.
pub const REVOKE_WINDOW_SECS: i64 = 60 * 60 * 60;

#[derive(Clone, Debug)]
pub struct BackendConfig {
    /// Directory for the whatsapp-rust session DB and the app's message store,
    /// e.g. `~/Library/Application Support/WhatsRusty`.
    pub data_dir: PathBuf,
    /// Device name shown in the phone's "Linked devices" list.
    pub device_name: String,
}

/// Cheap to clone. Dropping all handles does not stop the backend; send `Command::Shutdown`.
#[derive(Clone)]
pub struct BackendHandle {
    pub commands: flume::Sender<Command>,
    pub events: flume::Receiver<Event>,
}
