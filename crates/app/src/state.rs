//! UI-side model: mirrors backend events into GPUI state that views observe.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Image, ImageFormat, SystemNotification, SystemNotificationAction};
use wa_backend::{
    BackendHandle, ChatMessage, ChatSummary, Command, ConnectionState, DeliveryStatus, Event, GroupRead, MessageBody, MessageReceipt,
    SearchHit,
};

/// Emitted when the visible transcript changes, so the conversation view can
/// rebuild its list (and decide whether to stick to the bottom).
pub enum StoreEvent {
    TranscriptChanged { chat_jid: String, change: Change },
    /// The account ended (logout / unlinked): drop drafts and anything account-specific.
    AccountReset,
    /// A chat's draft was saved, so other composers on the same chat can follow it.
    DraftChanged { chat_jid: String },
    /// A LID chat became a phone-number chat: views bound to `from` move to `to`.
    ChatMerged { from: String, to: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// New messages at the end (or the chat was just opened).
    Appended,
    /// An older page was inserted at the top: keep the user's scroll position.
    Prepended,
    /// Statuses, typing, edits: rows changed in place.
    Updated,
}

/// What a chat's composer holds while it's not on screen. Kept here rather than in the
/// composer so it survives closing the window.
#[derive(Default, Clone, PartialEq)]
pub struct Draft {
    pub text: String,
    /// Files waiting to be sent with the next message (pasted, dropped or chosen with +).
    pub attachments: Vec<PathBuf>,
    /// `@Name`s inserted into `text` and who they are: (name, JID).
    pub mentions: Vec<(String, String)>,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty()
    }
}

/// A chat shown in its own small window (see `mini.rs`).
pub struct Popped {
    pub window: AnyWindowHandle,
    /// Its transcript shows the newest messages (not scrolled up).
    pub at_latest: bool,
}

/// Paging state of one chat's transcript.
#[derive(Default, Clone, Copy)]
pub struct History {
    /// More (older) messages can be requested, locally or from the phone.
    pub has_more: bool,
    /// A `LoadMessages` page is outstanding.
    pub page_pending: bool,
    /// The phone is being asked for older messages.
    pub fetching_remote: bool,
    /// The outstanding page is an "older than" request (not the initial newest page).
    older_pending: bool,
    /// Don't ask again before this (an older-page request came back empty, or the phone
    /// didn't answer), so the render-driven prefetch can't spin.
    retry_after: Option<Instant>,
}

/// How long the info panel shows "Loading…" before falling back to "Not available".
const PROFILE_WAIT: Duration = Duration::from_secs(6);

/// Photos and videos the info panel shows at most.
const MEDIA_TILES: u32 = 60;

/// A photo or video in a chat's info panel, its thumbnail decoded once.
pub struct MediaTile {
    pub message: ChatMessage,
    pub thumb: Option<Arc<Image>>,
}

impl MediaTile {
    fn new(message: ChatMessage) -> Self {
        let thumb = match &message.body {
            MessageBody::Image(i) => i.thumbnail.as_ref(),
            MessageBody::Video(v) => v.thumbnail.as_ref(),
            _ => None,
        };
        let thumb = thumb.map(|t| Arc::new(Image::from_bytes(ImageFormat::Jpeg, t.clone())));
        Self { message, thumb }
    }
}

/// How long to wait before re-asking for older history after an empty answer.
const OLDER_RETRY: Duration = Duration::from_secs(20);

pub struct Store {
    backend: BackendHandle,
    pub connection: ConnectionState,
    pub sync_progress: Option<u8>,
    pub chats: Vec<ChatSummary>,
    pub messages: HashMap<String, Vec<ChatMessage>>,
    /// Downloads under way: how far each has got (0..1), by (chat, message id).
    download_progress: HashMap<(String, String), f32>,
    /// Photos and videos of a chat for the info panel, newest first (after `ListMedia`).
    chat_media: HashMap<String, Vec<MediaTile>>,
    /// How far each member of a group has read my messages, per group chat.
    pub group_reads: HashMap<String, Vec<GroupRead>>,
    pub typing: HashSet<String>,
    /// Who is typing in each chat (JID, name), for group typing indicators. `typing` holds
    /// the chats with anyone in here (or a 1:1 typing indicator without a sender).
    pub typists: HashMap<String, Vec<(String, Option<String>)>>,
    /// Latest answer to `search`: (chat, query, hits oldest first).
    pub search_results: Option<(String, String, Vec<SearchHit>)>,
    pub selected: Option<String>,
    pub last_error: Option<String>,
    pub history: HashMap<String, History>,
    /// Profile pictures by JID (`None` = known to have none). Decoded from bytes rather than
    /// shown by path: the backend rewrites `avatars/<jid>.jpg` in place when a picture
    /// changes, and GPUI caches path images forever.
    avatars: HashMap<String, Option<Arc<Image>>>,
    avatar_requested: HashSet<String>,
    avatar_loading: HashSet<String>,
    /// Downloads requested and not yet answered (cleared when the backend reports back,
    /// so "retry" works after a failure).
    media_requested: HashSet<(String, String)>,
    /// Message being replied to, per chat (shared by the main and mini windows).
    replying: HashMap<String, ChatMessage>,
    /// Photo shown full-window over the app, and the mini window's chat it opened in
    /// (`None`: the main window).
    pub lightbox: Option<PathBuf>,
    pub lightbox_in: Option<String>,
    /// The message whose photo the lightbox shows (for stepping to its neighbours).
    lightbox_id: Option<String>,
    /// Video playing over the app, and the mini window's chat it opened in (`None`: the
    /// main window).
    pub video: Option<OpenVideo>,
    pub video_in: Option<String>,
    /// JID whose info panel is open, and the profiles loaded so far.
    pub profile_open: Option<String>,
    pub profiles: HashMap<String, wa_backend::Profile>,
    /// My message (chat, id) whose Info panel is open (in place of the info panel), and
    /// who got and read it.
    pub message_info: Option<(String, String)>,
    pub receipts: HashMap<(String, String), Vec<MessageReceipt>>,
    /// My stickers for the sticker panel (favorites first, then recents).
    pub stickers: Vec<wa_backend::StickerRef>,
    /// When each profile was requested, so the panel can stop saying "Loading…".
    pub profile_requested_at: HashMap<String, Instant>,
    /// Messages sent from this window recently, so the transcript can animate them in once.
    pub just_sent: HashMap<String, Instant>,
    requested: HashSet<String>,
    /// Contacts for the New Message picker (after `request_contacts`).
    pub contacts: Vec<wa_backend::Contact>,
    /// Answers to phone-number checks: chat JID (`None` = not on WhatsApp) or the error.
    pub phone_lookups: HashMap<String, Result<Option<String>, String>>,
    /// The user closed the conversation (Mark as Unread on it): don't auto-select another.
    stay_deselected: bool,
    /// Unsent text and attachments per chat.
    pub drafts: HashMap<String, Draft>,
    /// A window of ours is in front, as last told to the backend: WhatsApp keeps the
    /// phone's notifications quiet while we're online, so we are only then.
    app_active: bool,
    /// Going inactive waits a moment (moving between our own windows passes through it).
    _inactive_after: Option<gpui_kit::Task<()>>,
    pub main_window: Option<AnyWindowHandle>,
    /// Chats popped out into mini windows.
    pub popped: HashMap<String, Popped>,
}

impl EventEmitter<StoreEvent> for Store {}

/// Ask the composer to take keyboard focus (a chat was just picked in New Message).
pub struct FocusComposer;

/// A video shown in the in-app player.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenVideo {
    pub path: PathBuf,
    pub width: Option<u32>,
    pub height: Option<u32>,
}
impl EventEmitter<FocusComposer> for Store {}

impl Store {
    pub fn new(backend: BackendHandle, cx: &mut Context<Self>) -> Self {
        let events = backend.events.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();

        Self {
            backend,
            connection: ConnectionState::Connecting,
            sync_progress: None,
            chats: Vec::new(),
            messages: HashMap::new(),
            group_reads: HashMap::new(),
            download_progress: HashMap::new(),
            chat_media: HashMap::new(),
            typing: HashSet::new(),
            typists: HashMap::new(),
            search_results: None,
            selected: None,
            last_error: None,
            history: HashMap::new(),
            avatars: HashMap::new(),
            avatar_requested: HashSet::new(),
            avatar_loading: HashSet::new(),
            media_requested: HashSet::new(),
            replying: HashMap::new(),
            lightbox: None,
            lightbox_in: None,
            lightbox_id: None,
            video: None,
            video_in: None,
            profile_open: None,
            profiles: HashMap::new(),
            message_info: None,
            receipts: HashMap::new(),
            stickers: Vec::new(),
            profile_requested_at: HashMap::new(),
            just_sent: HashMap::new(),
            requested: HashSet::new(),
            contacts: Vec::new(),
            phone_lookups: HashMap::new(),
            stay_deselected: false,
            drafts: HashMap::new(),
            app_active: false,
            _inactive_after: None,
            main_window: None,
            popped: HashMap::new(),
        }
    }

    pub fn create(backend: BackendHandle, cx: &mut gpui_kit::App) -> Entity<Self> {
        cx.new(|cx| Self::new(backend, cx))
    }

    fn send(&self, cmd: Command) {
        let _ = self.backend.commands.send(cmd);
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connection(state) => {
                if matches!(state, ConnectionState::LoggedOut | ConnectionState::AwaitingQr { .. }) {
                    self.reset_account(cx);
                }
                self.connection = state;
            }
            Event::SyncProgress(p) => self.sync_progress = p,
            Event::Chats(chats) => {
                for c in &chats {
                    if let Some(p) = &c.avatar
                        && !matches!(self.avatars.get(&c.jid), Some(Some(_)))
                    {
                        self.load_avatar(c.jid.clone(), p.clone(), cx);
                    }
                }
                self.chats = chats;
                if self.selected.is_none()
                    && !self.stay_deselected
                    && let Some(first) = self.chats.first().map(|c| c.jid.clone())
                {
                    self.select(first, cx);
                }
            }
            Event::Messages { chat_jid, messages, has_more } => {
                let list = self.messages.entry(chat_jid.clone()).or_default();
                let empty = messages.is_empty();
                let was_older_request = self.history.get(&chat_jid).is_some_and(|h| h.older_pending);
                let older = match (list.first(), messages.first()) {
                    (Some(have), Some(new)) => {
                        new.timestamp < have.timestamp
                            // A page made only of messages from the oldest loaded second.
                            || (was_older_request && new.timestamp == have.timestamp && !list.iter().any(|m| m.id == new.id))
                    }
                    _ => false,
                };
                for m in messages {
                    upsert(list, m);
                }
                let h = self.history.entry(chat_jid.clone()).or_default();
                h.has_more = has_more;
                h.page_pending = false;
                if std::mem::take(&mut h.older_pending) && !older {
                    // Nothing older right now (offline, or the phone fetch is still out).
                    h.retry_after = Some(Instant::now() + OLDER_RETRY);
                }
                let change = if older {
                    Change::Prepended
                } else if empty {
                    Change::Updated
                } else {
                    Change::Appended
                };
                cx.emit(StoreEvent::TranscriptChanged { chat_jid, change });
            }
            Event::HistoryLoading { chat_jid, loading, exhausted } => {
                let h = self.history.entry(chat_jid).or_default();
                let finished = h.fetching_remote && !loading;
                h.fetching_remote = loading;
                if exhausted {
                    h.has_more = false;
                } else if finished {
                    // Allow the next scroll to the top to try again (after a pause).
                    h.retry_after = Some(Instant::now() + OLDER_RETRY);
                }
            }
            Event::MessageUpserted(m) => {
                let chat_jid = m.chat_jid.clone();
                let from_me = m.from_me;
                if !from_me {
                    self.set_typist(&chat_jid, Some(&m.sender_jid), None, false);
                }
                if media_settled(&m.body) {
                    let key = (chat_jid.clone(), m.id.clone());
                    self.download_progress.remove(&key);
                    self.media_requested.remove(&key);
                }
                // Keep an open info panel's media current (downloads, new photos).
                if matches!(m.body, MessageBody::Image(_) | MessageBody::Video(_))
                    && let Some(tiles) = self.chat_media.get_mut(&chat_jid)
                {
                    match tiles.iter_mut().find(|t| t.message.id == m.id) {
                        Some(t) if m.deleted => t.message.deleted = true,
                        Some(t) => *t = MediaTile::new(m.clone()),
                        None if !m.deleted => tiles.insert(0, MediaTile::new(m.clone())),
                        None => {}
                    }
                    tiles.retain(|t| !t.message.deleted);
                }
                // Only keep transcripts the user has opened; the chat list carries previews.
                if let Some(list) = self.messages.get_mut(&chat_jid) {
                    upsert(list, m);
                    cx.emit(StoreEvent::TranscriptChanged { chat_jid: chat_jid.clone(), change: Change::Appended });
                }
                // Only acknowledge what the user can actually see.
                if !from_me && self.chat_on_screen(&chat_jid, cx) {
                    self.send(Command::MarkRead { chat_jid });
                }
            }
            Event::MessagesRemoved { chat_jid, message_ids } => {
                for id in &message_ids {
                    self.media_requested.remove(&(chat_jid.clone(), id.clone()));
                }
                if self.replying.get(&chat_jid).is_some_and(|m| message_ids.contains(&m.id)) {
                    self.replying.remove(&chat_jid);
                }
                if let Some(list) = self.messages.get_mut(&chat_jid) {
                    list.retain(|m| !message_ids.contains(&m.id));
                    cx.emit(StoreEvent::TranscriptChanged { chat_jid, change: Change::Updated });
                }
            }
            Event::TranscriptStale { chat_jid } => {
                // Reload loaded transcripts after history sync wrote into them.
                if let Some(list) = self.messages.get(&chat_jid)
                    && self.requested.contains(&chat_jid)
                {
                    let limit = (list.len() as u32).max(100);
                    self.send(Command::LoadMessages { chat_jid, before: None, limit });
                }
            }
            Event::ChatMerged { from, to } => self.merge_chat(from, to, cx),
            Event::StatusChanged { chat_jid, message_ids, status } => {
                if let Some(list) = self.messages.get_mut(&chat_jid) {
                    for m in list.iter_mut().filter(|m| message_ids.contains(&m.id)) {
                        m.status = merge_status(m.status, status);
                    }
                    cx.emit(StoreEvent::TranscriptChanged { chat_jid, change: Change::Updated });
                }
            }
            Event::DownloadProgress { chat_jid, message_id, fraction } => {
                self.download_progress.insert((chat_jid, message_id), fraction);
                cx.notify();
            }
            Event::ChatMedia { chat_jid, items } => {
                let tiles = items.into_iter().map(MediaTile::new).collect();
                self.chat_media.insert(chat_jid, tiles);
                cx.notify();
            }
            Event::MessageInfo { chat_jid, message_id, receipts } => {
                self.receipts.insert((chat_jid, message_id), receipts);
                cx.notify();
            }
            Event::ReceiptsChanged { chat_jid, message_ids } => {
                if let Some((chat, id)) = &self.message_info
                    && *chat == chat_jid
                    && message_ids.contains(id)
                {
                    let (chat_jid, message_id) = (chat.clone(), id.clone());
                    self.send(Command::MessageInfo { chat_jid, message_id });
                }
            }
            Event::GroupReads { chat_jid, reads } => {
                self.group_reads.insert(chat_jid.clone(), reads);
                cx.emit(StoreEvent::TranscriptChanged { chat_jid, change: Change::Updated });
            }
            Event::Typing { chat_jid, sender_name, typing, sender_jid } => {
                self.set_typist(&chat_jid, sender_jid.as_deref(), sender_name, typing);
                let change = if typing { Change::Appended } else { Change::Updated };
                cx.emit(StoreEvent::TranscriptChanged { chat_jid, change });
            }
            Event::Stickers(list) => self.stickers = list,
            Event::Profile(p) => {
                if let Some(photo) = p.photo.clone() {
                    self.load_avatar(format!("{}#full", p.jid), photo, cx);
                }
                self.profiles.insert(p.jid.clone(), p);
            }
            Event::AvatarUpdated { jid, path } => match path {
                Some(path) => self.load_avatar(jid, path, cx),
                None => {
                    self.avatars.insert(jid, None);
                }
            },
            Event::Notify { chat_jid, message_id, title, body } => {
                // Like Messages: no banner for the conversation you're looking at (the app in
                // front and the chat showing in the main window, whichever of our windows is
                // key), or one popped out into a visible mini window.
                let visible = |w: AnyWindowHandle, cx: &mut Context<Self>| w.update(cx, |_, window, _| window.is_visible()).unwrap_or(false);
                let app_active = cx.active_window().is_some();
                let shown_in_main = self.selected.as_deref() == Some(chat_jid.as_str())
                    && app_active
                    && self.main_window.is_some_and(|w| visible(w, cx));
                let mini_visible = self.popped.get(&chat_jid).is_some_and(|p| visible(p.window, cx));
                // Archived chats stay quiet, like WhatsApp's "Keep chats archived".
                let archived = self.chats.iter().any(|c| c.jid == chat_jid && c.archived);
                let show = !shown_in_main && !mini_visible && !archived;
                log::info!(
                    "notify {message_id} in {chat_jid}: show={show} (in main={shown_in_main} mini={mini_visible} archived={archived})"
                );
                if show {
                    cx.show_system_notification(SystemNotification {
                        // One per message: reusing the chat's tag replaces its earlier
                        // notification, which macOS may do without a banner.
                        tag: notification_tag(&chat_jid, &message_id).into(),
                        title: crate::links::strip_bidi(&title).into(),
                        body: crate::links::strip_bidi(&body).into(),
                        actions: notification_actions(),
                    });
                }
            }
            Event::Contacts(list) => self.contacts = list,
            Event::PhoneLookup { phone, jid, error } => {
                self.phone_lookups.insert(phone, error.map_or(Ok(jid), Err));
            }
            Event::SearchResults { chat_jid, query, hits } => {
                self.search_results = Some((chat_jid, query, hits));
            }
            Event::Error(e) => {
                log::error!("backend: {e}");
                self.last_error = Some(e);
            }
        }
        cx.notify();
    }

    pub fn select(&mut self, jid: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(jid.as_str()) {
            return;
        }
        self.ensure_loaded(&jid);
        if self.main_active(cx) && self.chats.iter().any(|c| c.jid == jid && c.unread_count > 0) {
            self.send(Command::MarkRead { chat_jid: jid.clone() });
        }
        if self.selected.as_deref() != Some(jid.as_str()) {
            self.drop_reply_of_selected();
            self.message_info = None;
            // An open info panel follows the conversation, like Messages' details pane.
            if self.profile_open.is_some() && self.profile_open == self.selected {
                self.request_profile(&jid, cx);
                self.send(Command::ListMedia { chat_jid: jid.clone(), limit: MEDIA_TILES });
                self.profile_open = Some(jid.clone());
            }
        }
        self.selected = Some(jid.clone());
        self.stay_deselected = false;
        // Note: no `dismiss_system_notification` here. GPUI's macOS implementation makes a
        // synchronous XPC call to usernotificationsd on the main thread, which can block the
        // UI indefinitely (observed hanging at startup when the first chat is auto-selected).
        cx.emit(StoreEvent::TranscriptChanged { chat_jid: jid, change: Change::Appended });
        cx.notify();
    }

    /// Ask for a chat's newest messages the first time it's shown (selected or popped out).
    pub fn ensure_loaded(&mut self, jid: &str) {
        if self.requested.insert(jid.to_string()) {
            self.messages.entry(jid.to_string()).or_default();
            self.history.entry(jid.to_string()).or_default().page_pending = true;
            self.send(Command::LoadMessages { chat_jid: jid.to_string(), before: None, limit: 100 });
        }
    }

    /// Leaving the selected chat drops its reply, unless a mini window still shows it.
    fn drop_reply_of_selected(&mut self) {
        if let Some(old) = &self.selected
            && !self.popped.contains_key(old)
        {
            self.replying.remove(old);
        }
    }

    /// The main window is the key window.
    pub fn main_active(&self, cx: &App) -> bool {
        cx.active_window().is_some_and(|w| Some(w) == self.main_window)
    }

    /// `chat` is in front of the user: selected in the active main window, or in the key
    /// mini window showing its newest messages.
    pub fn chat_on_screen(&self, chat: &str, cx: &App) -> bool {
        (self.selected.as_deref() == Some(chat) && self.main_active(cx))
            || self.popped.get(chat).is_some_and(|p| p.at_latest && cx.active_window() == Some(p.window))
    }

    /// Acknowledge a chat that's on screen in a mini window (it became key, or was
    /// scrolled back to the newest messages).
    pub fn mark_read_if_unread(&self, chat: &str) {
        if self.chats.iter().any(|c| c.jid == chat && c.unread_count > 0) {
            self.send(Command::MarkRead { chat_jid: chat.to_string() });
        }
    }

    pub fn set_popped(&mut self, chat: &str, window: Option<AnyWindowHandle>) {
        match window {
            Some(window) => {
                self.ensure_loaded(chat);
                self.popped.insert(chat.to_string(), Popped { window, at_latest: true });
            }
            None => {
                self.popped.remove(chat);
                if self.selected.as_deref() != Some(chat) {
                    self.replying.remove(chat);
                }
            }
        }
    }

    pub fn set_mini_at_latest(&mut self, chat: &str, at_latest: bool, cx: &mut Context<Self>) {
        let Some(p) = self.popped.get_mut(chat) else { return };
        p.at_latest = at_latest;
        if self.chat_on_screen(chat, cx) {
            self.mark_read_if_unread(chat);
        }
    }

    /// Request the page before the oldest loaded message of `chat`.
    /// Called when the transcript is scrolled near its top.
    pub fn load_older(&mut self, chat: &str, cx: &mut Context<Self>) {
        self.load_older_by(chat, 100, false, cx);
    }

    /// Like `load_older`, with a page of `limit` messages (bigger when jumping back to a
    /// quoted message or search hit). `force` skips the pause after an empty answer (an
    /// explicit jump, not scrolling). Returns whether a page was requested.
    pub fn load_older_by(&mut self, chat: &str, limit: u32, force: bool, cx: &mut Context<Self>) -> bool {
        let jid = chat.to_string();
        let Some(oldest) = self.messages.get(&jid).and_then(|m| m.first()).map(|m| m.timestamp) else { return false };
        let h = self.history.entry(jid.clone()).or_default();
        if !h.has_more || h.page_pending || h.fetching_remote || (!force && h.retry_after.is_some_and(|t| Instant::now() < t)) {
            return false;
        }
        h.page_pending = true;
        h.older_pending = true;
        h.retry_after = None;
        self.send(Command::LoadMessages { chat_jid: jid, before: Some(oldest), limit });
        cx.notify();
        true
    }

    /// The window became active: acknowledge the conversation on screen.
    pub fn mark_selected_read(&self) {
        if let Some(jid) = &self.selected
            && self.chats.iter().any(|c| &c.jid == jid && c.unread_count > 0)
        {
            self.send(Command::MarkRead { chat_jid: jid.clone() });
        }
    }

    /// A LID chat turned out to be the same person as a phone-number chat: move UI state.
    fn merge_chat(&mut self, from: String, to: String, cx: &mut Context<Self>) {
        let was_loaded = self.messages.remove(&from).is_some() || self.messages.contains_key(&to);
        self.requested.remove(&from);
        if let Some(h) = self.history.remove(&from) {
            self.history.entry(to.clone()).or_insert(h);
        }
        if self.typing.remove(&from) {
            self.typing.insert(to.clone());
        }
        if let Some(t) = self.typists.remove(&from) {
            self.typists.insert(to.clone(), t);
        }
        self.media_requested = std::mem::take(&mut self.media_requested)
            .into_iter()
            .map(|(c, id)| if c == from { (to.clone(), id) } else { (c, id) })
            .collect();
        if let Some(a) = self.avatars.remove(&from) {
            self.avatars.entry(to.clone()).or_insert(a);
        }
        if let Some(d) = self.drafts.remove(&from) {
            self.drafts.entry(to.clone()).or_insert(d);
        }
        if let Some(r) = self.replying.remove(&from) {
            self.replying.entry(to.clone()).or_insert(r);
        }
        if let Some(p) = self.popped.remove(&from) {
            self.popped.entry(to.clone()).or_insert(p);
        }
        if self.lightbox_in.as_deref() == Some(from.as_str()) {
            self.lightbox_in = Some(to.clone());
        }
        cx.emit(StoreEvent::ChatMerged { from: from.clone(), to: to.clone() });
        if was_loaded {
            self.requested.insert(to.clone());
            self.messages.entry(to.clone()).or_default();
            self.send(Command::LoadMessages { chat_jid: to.clone(), before: None, limit: 100 });
        }
        if self.selected.as_deref() == Some(from.as_str()) {
            self.selected = Some(to.clone());
            cx.emit(StoreEvent::TranscriptChanged { chat_jid: to, change: Change::Appended });
        }
    }

    /// Forget everything tied to the account that just ended.
    fn reset_account(&mut self, cx: &mut Context<Self>) {
        self.chats.clear();
        self.messages.clear();
        self.history.clear();
        self.requested.clear();
        self.selected = None;
        self.stay_deselected = false;
        self.contacts.clear();
        self.phone_lookups.clear();
        self.typing.clear();
        self.typists.clear();
        self.group_reads.clear();
        self.download_progress.clear();
        self.chat_media.clear();
        self.search_results = None;
        self.avatars.clear();
        self.avatar_requested.clear();
        self.avatar_loading.clear();
        self.media_requested.clear();
        self.replying.clear();
        self.lightbox = None;
        self.lightbox_in = None;
        self.close_video(cx);
        self.profile_open = None;
        self.profiles.clear();
        self.message_info = None;
        self.receipts.clear();
        self.stickers.clear();
        self.just_sent.clear();
        self.drafts.clear();
        self.sync_progress = None;
        crate::audio::AudioPlayer::stop(cx);
        cx.emit(StoreEvent::AccountReset);
    }

    /// Decode an avatar file off the UI thread, then publish it.
    fn load_avatar(&mut self, jid: String, path: PathBuf, cx: &mut Context<Self>) {
        if !self.avatar_loading.insert(jid.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            // List avatars are drawn at 60 pt at most: keep them small. The profile's
            // large photo (`#full`) keeps its size.
            let max_px = if jid.ends_with("#full") { None } else { Some(AVATAR_PX) };
            let image = cx.background_executor().spawn(async move { load_image(&path, max_px) }).await;
            let _ = this.update(cx, |s, cx| {
                s.avatar_loading.remove(&jid);
                s.avatars.insert(jid, image);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn history_of(&self, chat: &str) -> History {
        self.history.get(chat).copied().unwrap_or_default()
    }

    /// `mentions`: JIDs whose `@<digits>` are in `text`.
    /// Reply typed into a notification: a plain message (not the reply the composer may
    /// have pending for that chat), and the chat counts as read, like WhatsApp.
    pub fn send_notification_reply(&mut self, chat_jid: String, text: &str, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let client_id = self.new_sent_id();
        self.send(Command::MarkRead { chat_jid: chat_jid.clone() });
        self.send(Command::SendText { chat_jid, text, client_id, reply_to: None, mentions: Vec::new() });
        cx.notify();
    }

    pub fn send_text(&mut self, chat_jid: String, text: String, mentions: Vec<String>, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let client_id = self.new_sent_id();
        let reply_to = self.replying.remove(&chat_jid).map(|m| m.id);
        self.send(Command::SendText { chat_jid, text, client_id, reply_to, mentions });
        cx.notify();
    }

    /// Any file but photos (videos, documents); see [`Command::SendFile`].
    pub fn send_file(&mut self, chat_jid: String, path: PathBuf, caption: Option<String>, cx: &mut Context<Self>) {
        let client_id = self.new_sent_id();
        let reply_to = self.replying.remove(&chat_jid).map(|m| m.id);
        self.send(Command::SendFile { chat_jid, path, caption, client_id, reply_to });
        cx.notify();
    }

    /// A recorded voice note (mono PCM).
    pub fn send_voice(&mut self, chat_jid: String, samples: Vec<i16>, sample_rate: u32, cx: &mut Context<Self>) {
        let client_id = self.new_sent_id();
        let reply_to = self.replying.remove(&chat_jid).map(|m| m.id);
        self.send(Command::SendVoice { chat_jid, samples, sample_rate, client_id, reply_to });
        cx.notify();
    }

    /// An id for a message sent from this window (animated in when it arrives).
    fn new_sent_id(&mut self) -> String {
        let client_id = wa_backend::new_message_id();
        self.just_sent.retain(|_, t| t.elapsed() < Duration::from_secs(5));
        self.just_sent.insert(client_id.clone(), Instant::now());
        client_id
    }

    /// Store (or drop, when empty) the composer's draft for `chat`. No notify (only
    /// composers read it); an event lets another window's composer on the chat follow.
    pub fn save_draft(&mut self, chat: &str, draft: Draft, cx: &mut Context<Self>) {
        let old = self.drafts.get(chat).cloned().unwrap_or_default();
        if draft.is_empty() {
            self.drafts.remove(chat);
        } else {
            self.drafts.insert(chat.to_string(), draft);
        }
        if self.drafts.get(chat).cloned().unwrap_or_default() != old {
            cx.emit(StoreEvent::DraftChanged { chat_jid: chat.to_string() });
        }
    }

    /// Members of a group I can @mention (not me), loading them once if needed.
    pub fn mentionable(&mut self, jid: &str, cx: &mut Context<Self>) -> Vec<wa_backend::Participant> {
        match self.profiles.get(jid) {
            Some(p) => p.participants.iter().filter(|p| !p.is_me).cloned().collect(),
            None => {
                if !self.profile_loading(jid) {
                    self.request_profile(jid, cx);
                }
                Vec::new()
            }
        }
    }

    pub fn request_stickers(&self) {
        self.send(Command::ListStickers);
    }

    pub fn send_sticker(&mut self, chat_jid: String, sticker_id: String, cx: &mut Context<Self>) {
        let client_id = wa_backend::new_message_id();
        self.just_sent.insert(client_id.clone(), Instant::now());
        let reply_to = self.replying.remove(&chat_jid).map(|m| m.id);
        self.send(Command::SendSticker { chat_jid, sticker_id, client_id, reply_to });
        cx.notify();
    }

    pub fn send_image(&mut self, chat_jid: String, path: PathBuf, caption: Option<String>, cx: &mut Context<Self>) {
        let client_id = self.new_sent_id();
        let reply_to = self.replying.remove(&chat_jid).map(|m| m.id);
        self.send(Command::SendImage { chat_jid, path, caption, client_id, reply_to });
        cx.notify();
    }

    /// Show the info panel for a chat or person (toggles if it's already open).
    pub fn toggle_profile(&mut self, jid: String, cx: &mut Context<Self>) {
        self.message_info = None;
        if self.profile_open.as_deref() == Some(jid.as_str()) {
            self.profile_open = None;
        } else {
            self.request_profile(&jid, cx);
            self.send(Command::ListMedia { chat_jid: jid.clone(), limit: MEDIA_TILES });
            self.profile_open = Some(jid);
        }
        cx.notify();
    }

    fn request_profile(&mut self, jid: &str, cx: &mut Context<Self>) {
        self.send(Command::FetchProfile { jid: jid.to_string() });
        self.profile_requested_at.insert(jid.to_string(), Instant::now());
        // Repaint once the wait is over so "Loading…" turns into "Not available".
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PROFILE_WAIT).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// How far the download of a message's media has got (0..1), while it runs.
    pub fn download_progress(&self, chat_jid: &str, id: &str) -> Option<f32> {
        self.download_progress.get(&(chat_jid.to_string(), id.to_string())).copied()
    }

    /// A chat's photos and videos for its info panel (`None` until they've loaded).
    pub fn chat_media(&self, chat_jid: &str) -> Option<&[MediaTile]> {
        self.chat_media.get(chat_jid).map(Vec::as_slice)
    }

    /// Still worth showing "Loading…" for `jid`'s profile details.
    pub fn profile_loading(&self, jid: &str) -> bool {
        !self.profiles.contains_key(jid)
            && self.profile_requested_at.get(jid).is_some_and(|t| t.elapsed() < PROFILE_WAIT)
    }

    pub fn close_profile(&mut self, cx: &mut Context<Self>) {
        if self.profile_open.take().is_some() {
            cx.notify();
        }
    }

    /// Show who got and read my message `id` (in place of the info panel).
    pub fn open_message_info(&mut self, chat_jid: String, id: String, cx: &mut Context<Self>) {
        self.profile_open = None;
        if chat_jid.ends_with("@g.us") && !self.profiles.contains_key(&chat_jid) {
            // Its members, for who it hasn't reached yet.
            self.request_profile(&chat_jid, cx);
        }
        self.send(Command::MessageInfo { chat_jid: chat_jid.clone(), message_id: id.clone() });
        self.message_info = Some((chat_jid, id));
        cx.notify();
    }

    pub fn close_message_info(&mut self, cx: &mut Context<Self>) {
        if self.message_info.take().is_some() {
            cx.notify();
        }
    }

    pub fn open_lightbox(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.open_lightbox_in(path, None, None, cx);
    }

    /// `id`: the photo's message. `mini`: the popped-out chat whose window shows it (`None`:
    /// the main window).
    pub fn open_lightbox_in(&mut self, path: PathBuf, id: Option<String>, mini: Option<String>, cx: &mut Context<Self>) {
        self.lightbox = Some(path);
        self.lightbox_id = id;
        self.lightbox_in = mini;
        cx.notify();
    }

    /// The open photo's chat's photos, oldest first: (message id, file once downloaded).
    fn lightbox_photos(&self) -> (Option<&str>, Vec<(&str, Option<&PathBuf>)>) {
        let chat = self.lightbox_in.as_deref().or(self.selected.as_deref());
        let photos = chat
            .and_then(|c| self.messages.get(c))
            .into_iter()
            .flatten()
            .filter(|m| !m.deleted)
            .filter_map(|m| match &m.body {
                MessageBody::Image(media) => Some((m.id.as_str(), media.path.as_ref())),
                _ => None,
            })
            .collect();
        (chat, photos)
    }

    fn lightbox_pos(&self, photos: &[(&str, Option<&PathBuf>)]) -> Option<usize> {
        match &self.lightbox_id {
            Some(id) => photos.iter().position(|(i, _)| i == id),
            None => photos.iter().position(|(_, p)| *p == self.lightbox.as_ref()),
        }
    }

    /// Whether the lightbox has a downloaded photo before (`-1`) / after (`1`) the open one.
    pub fn lightbox_can_step(&self, delta: isize) -> bool {
        let (_, photos) = self.lightbox_photos();
        let Some(pos) = self.lightbox_pos(&photos) else { return false };
        let mut ix = pos as isize + delta;
        while let Some((_, path)) = usize::try_from(ix).ok().and_then(|i| photos.get(i)) {
            if path.is_some() {
                return true;
            }
            ix += delta;
        }
        false
    }

    /// Show the previous (`-1`) / next (`1`) photo in the lightbox's chat, skipping ones not
    /// downloaded yet (the nearby ones are fetched, so stepping again reaches them).
    pub fn step_lightbox(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.lightbox.is_none() {
            return;
        }
        let (chat, photos) = self.lightbox_photos();
        let Some(chat) = chat.map(String::from) else { return };
        let Some(pos) = self.lightbox_pos(&photos) else { return };
        let missing: Vec<String> = photos
            .iter()
            .enumerate()
            .filter(|(i, (_, p))| p.is_none() && i.abs_diff(pos) <= 4)
            .map(|(_, (id, _))| id.to_string())
            .collect();
        let mut ix = pos as isize + delta;
        let mut next = None;
        while let Some((id, path)) = usize::try_from(ix).ok().and_then(|i| photos.get(i)) {
            if let Some(path) = path {
                next = Some(((*path).clone(), id.to_string()));
                break;
            }
            ix += delta;
        }
        for id in missing {
            self.request_media(&chat, &id);
        }
        if let Some((path, id)) = next {
            self.lightbox = Some(path);
            self.lightbox_id = Some(id);
            cx.notify();
        }
    }

    /// Play a video over the window it was opened from (`mini`: that chat's mini window).
    pub fn open_video_in(&mut self, video: OpenVideo, mini: Option<String>, cx: &mut Context<Self>) {
        self.close_video(cx);
        self.video = Some(video);
        self.video_in = mini;
        cx.notify();
    }

    pub fn close_video(&mut self, cx: &mut Context<Self>) {
        if self.video.take().is_some() {
            crate::video_player::close(&self.video_in);
            cx.notify();
        }
    }

    pub fn close_lightbox(&mut self, cx: &mut Context<Self>) {
        self.lightbox_id = None;
        if self.lightbox.take().is_some() {
            cx.notify();
        }
    }

    /// Toggle my reaction: picking the emoji I already reacted with removes it.
    pub fn react(&mut self, message: &ChatMessage, emoji: &str) {
        let mine = message.reactions.iter().find(|r| r.from_me).map(|r| r.emoji.as_str());
        let emoji = (mine != Some(emoji)).then(|| emoji.to_string());
        self.send(Command::React { chat_jid: message.chat_jid.clone(), message_id: message.id.clone(), emoji });
    }

    /// WhatsApp lets you edit your own text (or a caption) for 15 minutes.
    pub fn can_edit(m: &ChatMessage) -> bool {
        let editable_kind = match &m.body {
            MessageBody::Text(_) => true,
            MessageBody::Image(i) => i.caption.is_some(),
            MessageBody::Video(v) => v.caption.is_some(),
            MessageBody::Document(d) => d.caption.is_some(),
            _ => false,
        };
        let age = chrono::Utc::now().timestamp() - m.timestamp;
        m.from_me && editable_kind && m.status != DeliveryStatus::Failed && (0..15 * 60).contains(&age)
    }

    /// Text currently shown for an editable message (its text or caption).
    pub fn editable_text(m: &ChatMessage) -> String {
        match &m.body {
            MessageBody::Text(t) => t.clone(),
            MessageBody::Image(i) => i.caption.clone().unwrap_or_default(),
            MessageBody::Video(v) => v.caption.clone().unwrap_or_default(),
            MessageBody::Document(d) => d.caption.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// My most recent message in `chat` that can still be edited (↑ in an
    /// empty composer).
    pub fn last_editable(&self, chat: &str) -> Option<ChatMessage> {
        let list = self.messages.get(chat)?;
        list.iter().rev().find(|m| m.from_me).filter(|m| Self::can_edit(m)).cloned()
    }

    pub fn edit_message(&mut self, message: &ChatMessage, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() || text == Self::editable_text(message) {
            return;
        }
        self.send(Command::EditMessage { chat_jid: message.chat_jid.clone(), message_id: message.id.clone(), text });
        cx.notify();
    }

    /// "Try Again" on a message that wasn't delivered: resend it with the same id.
    pub fn retry_send(&mut self, message: &ChatMessage, cx: &mut Context<Self>) {
        if !message.from_me || message.status != DeliveryStatus::Failed {
            return;
        }
        // Show "Sending…" right away; the backend confirms with MessageUpserted/StatusChanged.
        // It goes to the end of the conversation, like a new message.
        if let Some(list) = self.messages.get_mut(&message.chat_jid)
            && let Some(pos) = list.iter().position(|m| m.id == message.id)
        {
            let mut m = list.remove(pos);
            m.status = DeliveryStatus::Pending;
            m.timestamp = m.timestamp.max(chrono::Utc::now().timestamp());
            upsert(list, m);
            cx.emit(StoreEvent::TranscriptChanged { chat_jid: message.chat_jid.clone(), change: Change::Appended });
        }
        self.send(Command::RetrySend { chat_jid: message.chat_jid.clone(), message_id: message.id.clone() });
        cx.notify();
    }

    pub fn replying_to(&self, chat: &str) -> Option<&ChatMessage> {
        self.replying.get(chat)
    }

    pub fn set_replying_to(&mut self, chat: &str, message: Option<ChatMessage>, cx: &mut Context<Self>) {
        match message {
            Some(m) => self.replying.insert(chat.to_string(), m),
            None => self.replying.remove(chat),
        };
        cx.notify();
    }

    /// Cached photo for a chat/participant; asks the backend once per session if unknown.
    pub fn avatar_for(&mut self, jid: &str) -> Option<Arc<Image>> {
        if let Some(known) = self.avatars.get(jid) {
            return known.clone();
        }
        if self.avatar_requested.insert(jid.to_string()) {
            self.send(Command::FetchAvatar { jid: jid.to_string() });
        }
        None
    }

    /// Peek without requesting (for render paths that only have `&self`).
    pub fn avatar_cached(&self, jid: &str) -> Option<Arc<Image>> {
        self.avatars.get(jid).cloned().flatten()
    }

    pub fn request_avatar(&mut self, jid: &str) {
        let _ = self.avatar_for(jid);
    }

    /// Ask the backend to download a message's media (deduplicated).
    pub fn request_media(&mut self, chat_jid: &str, message_id: &str) {
        if self.media_requested.insert((chat_jid.to_string(), message_id.to_string())) {
            log::debug!("requesting media {message_id} in {chat_jid}");
            self.send(Command::DownloadMedia { chat_jid: chat_jid.to_string(), message_id: message_id.to_string() });
        }
    }

    /// Animation start for a message this window just sent (cleared after a second).
    pub fn sent_at(&self, id: &str) -> Option<Instant> {
        self.just_sent.get(id).copied().filter(|t| t.elapsed().as_millis() < 1200)
    }

    /// One of our windows was activated or deactivated: online while one is in front.
    pub fn window_activation_changed(&mut self, active: bool, cx: &mut Context<Self>) {
        if active {
            self._inactive_after = None;
            if !self.app_active {
                self.app_active = true;
                self.send(Command::SetActive { active: true });
            }
            return;
        }
        if !self.app_active {
            return;
        }
        self._inactive_after = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |s, cx| {
                if s.app_active && cx.active_window().is_none() {
                    s.app_active = false;
                    s.send(Command::SetActive { active: false });
                }
            });
        }));
    }

    pub fn set_typing_in(&self, chat_jid: &str, typing: bool) {
        self.send(Command::SetTyping { chat_jid: chat_jid.to_string(), typing });
    }

    pub fn logout(&self) {
        self.send(Command::Logout);
    }

    pub fn set_pinned(&self, chat_jid: &str, pinned: bool) {
        self.send(Command::SetPinned { chat_jid: chat_jid.to_string(), pinned });
    }

    /// `until`: unix time, `i64::MAX` = always, `0` = unmute.
    pub fn set_muted(&self, chat_jid: &str, until: i64) {
        self.send(Command::SetMuted { chat_jid: chat_jid.to_string(), until });
    }

    pub fn set_archived(&self, chat_jid: &str, archived: bool) {
        self.send(Command::SetArchived { chat_jid: chat_jid.to_string(), archived });
    }

    pub fn mark_read(&self, chat_jid: &str) {
        self.send(Command::MarkRead { chat_jid: chat_jid.to_string() });
    }

    /// Like WhatsApp, marking the open conversation unread closes it (otherwise it would
    /// be read again right away).
    pub fn mark_unread(&mut self, chat_jid: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(chat_jid) {
            self.deselect(cx);
        }
        self.send(Command::MarkUnread { chat_jid: chat_jid.to_string() });
    }

    pub fn deselect(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.drop_reply_of_selected();
            self.selected = None;
            self.stay_deselected = true;
            cx.notify();
        }
    }

    pub fn request_contacts(&self) {
        self.send(Command::ListContacts);
    }

    /// Check a typed phone number; the answer lands in `phone_lookups[phone]`.
    pub fn lookup_phone(&mut self, phone: &str, cx: &mut Context<Self>) {
        self.phone_lookups.remove(phone);
        self.send(Command::LookupPhone { phone: phone.to_string() });
        cx.notify();
    }

    /// Open (or start) the chat with `jid` from New Message and focus the composer.
    pub fn open_chat(&mut self, jid: String, cx: &mut Context<Self>) {
        if !self.chats.iter().any(|c| c.jid == jid) {
            self.send(Command::OpenChat { jid: jid.clone() });
        }
        self.select(jid, cx);
        cx.emit(FocusComposer);
    }

    /// Track who is typing; `sender` is `None` for indicators without one (1:1 chats).
    fn set_typist(&mut self, chat_jid: &str, sender: Option<&str>, name: Option<String>, typing: bool) {
        let list = self.typists.entry(chat_jid.to_string()).or_default();
        match (sender, typing) {
            (Some(jid), true) => {
                list.retain(|(j, _)| j != jid);
                list.push((jid.to_string(), name));
            }
            (Some(jid), false) => list.retain(|(j, _)| j != jid),
            (None, _) => list.clear(),
        }
        let anyone = !list.is_empty() || (sender.is_none() && typing);
        if list.is_empty() {
            self.typists.remove(chat_jid);
        }
        if anyone {
            self.typing.insert(chat_jid.to_string());
        } else {
            self.typing.remove(chat_jid);
        }
    }

    /// Delete for everyone (my own, recent messages) or just for me.
    pub fn delete_message(&self, message: &ChatMessage, for_everyone: bool) {
        self.send(Command::DeleteMessage {
            chat_jid: message.chat_jid.clone(),
            message_id: message.id.clone(),
            for_everyone,
        });
    }

    /// Whether "Delete for Everyone" is still possible (WhatsApp's time limit).
    pub fn can_delete_for_everyone(m: &ChatMessage) -> bool {
        let age = chrono::Utc::now().timestamp() - m.timestamp;
        m.from_me
            && !m.deleted
            && matches!(m.status, DeliveryStatus::Sent | DeliveryStatus::Delivered | DeliveryStatus::Read)
            && age < wa_backend::REVOKE_WINDOW_SECS
    }

    /// Text and media messages can be forwarded (not deleted / view-once / other kinds).
    pub fn can_forward(m: &ChatMessage) -> bool {
        !m.deleted
            && matches!(
                m.body,
                MessageBody::Text(_)
                    | MessageBody::Image(_)
                    | MessageBody::Video(_)
                    | MessageBody::Document(_)
                    | MessageBody::Voice(_)
                    | MessageBody::Sticker(_)
            )
    }

    pub fn forward(&mut self, message: &ChatMessage, to_chat_jid: String, cx: &mut Context<Self>) {
        let client_id = wa_backend::new_message_id();
        self.send(Command::ForwardMessage {
            chat_jid: message.chat_jid.clone(),
            message_id: message.id.clone(),
            to_chat_jid,
            client_id,
        });
        cx.notify();
    }

    /// Search a chat's stored messages; the answer lands in `search_results`.
    pub fn search(&mut self, chat: &str, query: &str) {
        let chat_jid = chat.to_string();
        self.search_results = None;
        if !query.trim().is_empty() {
            self.send(Command::SearchMessages { chat_jid, query: query.to_string() });
        }
    }

    pub fn shutdown(&self) {
        self.send(Command::Shutdown);
    }

    /// Block (bounded) until the backend has stopped and dropped its event sender.
    /// GPUI gives async quit handlers only 200 ms, and the store's own event task can't
    /// run while we block, so this checks the channel directly.
    pub fn wait_for_backend_stop(&self, timeout: std::time::Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.backend.events.is_disconnected() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

/// Delivery progress only moves forward; `Failed` only replaces an unconfirmed send, and a
/// later server ack/receipt recovers a message that was marked failed.
fn merge_status(old: DeliveryStatus, new: DeliveryStatus) -> DeliveryStatus {
    use DeliveryStatus::*;
    let rank = |s: DeliveryStatus| match s {
        Pending => 0,
        Failed => 1,
        Sent => 2,
        Delivered => 3,
        Read => 4,
    };
    match (old, new) {
        (Pending, Failed) => Failed,
        // A retry ("Try Again") restarts a failed send.
        (Failed, Pending) => Pending,
        (_, Failed) => old,
        _ if rank(new) > rank(old) => new,
        _ => old,
    }
}

/// A media message whose download is no longer in flight (done or failed).
fn media_settled(body: &MessageBody) -> bool {
    use wa_backend::MediaState::Downloading;
    match body {
        MessageBody::Image(m) => m.state != Downloading,
        MessageBody::Voice(m) => m.state != Downloading,
        MessageBody::Video(m) => m.state != Downloading,
        MessageBody::Document(m) => m.state != Downloading,
        MessageBody::Sticker(m) => m.state != Downloading,
        _ => false,
    }
}

/// Insert keeping timestamp order; replace an existing message with the same id.
fn upsert(list: &mut Vec<ChatMessage>, m: ChatMessage) {
    if let Some(pos) = list.iter().position(|x| x.id == m.id) {
        let status = merge_status(list[pos].status, m.status);
        if list[pos].timestamp == m.timestamp {
            list[pos] = m;
            list[pos].status = status;
            return;
        }
        // Restamped (a resend): move it to its new place.
        list.remove(pos);
        let m = ChatMessage { status, ..m };
        let pos = list.partition_point(|x| x.timestamp <= m.timestamp);
        list.insert(pos, m);
        return;
    }
    let pos = list.partition_point(|x| x.timestamp <= m.timestamp);
    list.insert(pos, m);
}

/// One-line preview of a message body (reply bar, quotes, notifications).
pub fn body_text(body: &MessageBody) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    match body {
        MessageBody::Text(t) => Cow::Borrowed(t),
        MessageBody::Unsupported { label } => Cow::Borrowed(label),
        MessageBody::Image(i) => Cow::Borrowed(i.caption.as_deref().unwrap_or("📷 Photo")),
        MessageBody::Voice(_) => Cow::Borrowed("🎤 Voice message"),
        MessageBody::Video(v) => Cow::Borrowed(v.caption.as_deref().unwrap_or(if v.gif { "🎞️ GIF" } else { "🎥 Video" })),
        MessageBody::Document(d) => Cow::Owned(format!("📄 {}", d.file_name)),
        MessageBody::Sticker(_) => Cow::Borrowed("Sticker"),
        MessageBody::Location(l) => Cow::Owned(format!("📍 {}", l.name.as_deref().unwrap_or("Location"))),
        MessageBody::Contacts(c) => Cow::Owned(format!("👤 {}", c.first().map_or("Contact", |c| c.display_name.as_str()))),
        MessageBody::Poll(p) => Cow::Owned(format!("📊 {}", p.question)),
        MessageBody::Rich(r) => Cow::Borrowed(&r.body),
        MessageBody::System { text } => Cow::Borrowed(text),
        MessageBody::ViewOnce(k) => Cow::Borrowed(view_once_label(*k)),
    }
}

pub fn view_once_label(kind: wa_backend::ViewOnceKind) -> &'static str {
    match kind {
        wa_backend::ViewOnceKind::Photo => "📷 View once photo",
        wa_backend::ViewOnceKind::Video => "🎥 View once video",
        wa_backend::ViewOnceKind::Voice => "🎤 View once voice message",
    }
}

/// List avatars' size in pixels (60 pt at 2×, with room to spare).
const AVATAR_PX: u32 = 160;

/// GPUI keeps every image it draws decoded for the app's lifetime, so a 640 px avatar
/// costs 1.6 MB however small it's shown: `max_px` scales it down first.
fn load_image(path: &Path, max_px: Option<u32>) -> Option<Arc<Image>> {
    let bytes = std::fs::read(path).ok()?;
    let format = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) { ImageFormat::Png } else { ImageFormat::Jpeg };
    let small = max_px.and_then(|max| {
        let decoded = image::load_from_memory(&bytes).ok()?;
        if decoded.width() <= max && decoded.height() <= max {
            return None;
        }
        let mut png = Vec::new();
        decoded.thumbnail(max, max).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
        Some(png)
    });
    Some(Arc::new(match small {
        Some(png) => Image::from_bytes(ImageFormat::Png, png),
        None => Image::from_bytes(format, bytes),
    }))
}

/// A notification's tag: the chat, then the message (JIDs never contain `#`).
fn notification_tag(chat_jid: &str, message_id: &str) -> String {
    format!("{chat_jid}#{message_id}")
}

/// What a notification's response asks for: [`NOTIFY_REPLY`] (with the typed text) or
/// [`NOTIFY_READ`]; no action means its body was clicked.
pub const NOTIFY_REPLY: &str = "reply";
pub const NOTIFY_READ: &str = "read";

/// Reply inline, or mark the chat read, from the notification.
fn notification_actions() -> Vec<SystemNotificationAction> {
    vec![
        SystemNotificationAction { id: NOTIFY_REPLY.into(), label: "Reply".into(), text_input: Some("Message".into()) },
        SystemNotificationAction { id: NOTIFY_READ.into(), label: "Mark as Read".into(), text_input: None },
    ]
}

/// The chat a notification's tag belongs to.
pub fn notification_chat(tag: &str) -> &str {
    tag.split_once('#').map_or(tag, |(chat, _)| chat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_tags_lead_back_to_the_chat() {
        let tag = notification_tag("120363000000000001@g.us", "3EB0ABC");
        assert_eq!(notification_chat(&tag), "120363000000000001@g.us");
        // Tags from before they carried the message.
        assert_eq!(notification_chat("923001234567@s.whatsapp.net"), "923001234567@s.whatsapp.net");
    }

    #[test]
    fn avatars_load_small() {
        let dir = std::env::temp_dir().join(format!("wa-avatar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.jpg");
        image::RgbImage::from_pixel(640, 640, image::Rgb([200, 80, 40])).save(&path).unwrap();
        let small = load_image(&path, Some(AVATAR_PX)).unwrap();
        let decoded = image::load_from_memory(small.bytes()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (AVATAR_PX, AVATAR_PX));
        // The profile's large photo keeps its size.
        let full = load_image(&path, None).unwrap();
        assert_eq!(image::load_from_memory(full.bytes()).unwrap().width(), 640);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
