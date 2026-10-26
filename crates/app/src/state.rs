//! UI-side model: mirrors backend events into GPUI state that views observe.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, Context, Entity, EventEmitter, Image, ImageFormat, SystemNotification};
use wa_backend::{
    BackendHandle, ChatMessage, ChatSummary, Command, ConnectionState, DeliveryStatus, Event, MessageBody,
};

/// Emitted when the visible transcript changes, so the conversation view can
/// rebuild its list (and decide whether to stick to the bottom).
pub enum StoreEvent {
    TranscriptChanged { chat_jid: String, change: Change },
    /// The account ended (logout / unlinked): drop drafts and anything account-specific.
    AccountReset,
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

/// How long to wait before re-asking for older history after an empty answer.
const OLDER_RETRY: Duration = Duration::from_secs(20);

pub struct Store {
    backend: BackendHandle,
    pub connection: ConnectionState,
    pub sync_progress: Option<u8>,
    pub chats: Vec<ChatSummary>,
    pub messages: HashMap<String, Vec<ChatMessage>>,
    pub typing: HashSet<String>,
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
    /// Message being replied to in the selected chat's composer.
    pub replying_to: Option<ChatMessage>,
    /// Photo shown full-window over the app.
    pub lightbox: Option<PathBuf>,
    /// JID whose info panel is open, and the profiles loaded so far.
    pub profile_open: Option<String>,
    pub profiles: HashMap<String, wa_backend::Profile>,
    /// My stickers for the sticker panel (favorites first, then recents).
    pub stickers: Vec<wa_backend::StickerRef>,
    /// When each profile was requested, so the panel can stop saying "Loading…".
    pub profile_requested_at: HashMap<String, Instant>,
    /// Messages sent from this window recently, so the transcript can animate them in once.
    pub just_sent: HashMap<String, Instant>,
    requested: HashSet<String>,
}

impl EventEmitter<StoreEvent> for Store {}

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
            typing: HashSet::new(),
            selected: None,
            last_error: None,
            history: HashMap::new(),
            avatars: HashMap::new(),
            avatar_requested: HashSet::new(),
            avatar_loading: HashSet::new(),
            media_requested: HashSet::new(),
            replying_to: None,
            lightbox: None,
            profile_open: None,
            profiles: HashMap::new(),
            stickers: Vec::new(),
            profile_requested_at: HashMap::new(),
            just_sent: HashMap::new(),
            requested: HashSet::new(),
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
                    self.typing.remove(&chat_jid);
                }
                if media_settled(&m.body) {
                    self.media_requested.remove(&(chat_jid.clone(), m.id.clone()));
                }
                // Only keep transcripts the user has opened; the chat list carries previews.
                if let Some(list) = self.messages.get_mut(&chat_jid) {
                    upsert(list, m);
                    cx.emit(StoreEvent::TranscriptChanged { chat_jid: chat_jid.clone(), change: Change::Appended });
                }
                // Only acknowledge what the user can actually see.
                if !from_me && self.selected.as_deref() == Some(chat_jid.as_str()) && cx.active_window().is_some() {
                    self.send(Command::MarkRead { chat_jid });
                }
            }
            Event::MessagesRemoved { chat_jid, message_ids } => {
                for id in &message_ids {
                    self.media_requested.remove(&(chat_jid.clone(), id.clone()));
                }
                if self.replying_to.as_ref().is_some_and(|m| message_ids.contains(&m.id)) {
                    self.replying_to = None;
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
            Event::Typing { chat_jid, typing, .. } => {
                if typing {
                    self.typing.insert(chat_jid.clone());
                } else {
                    self.typing.remove(&chat_jid);
                }
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
                // Like Messages: no banner for the conversation you're looking at.
                let focused = cx.active_window().is_some() && self.selected.as_deref() == Some(chat_jid.as_str());
                if !focused {
                    cx.show_system_notification(SystemNotification {
                        tag: chat_jid.into(),
                        title: title.into(),
                        body: body.into(),
                        actions: Vec::new(),
                    });
                }
                log::debug!("notify {message_id}");
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
        if self.requested.insert(jid.clone()) {
            self.messages.entry(jid.clone()).or_default();
            self.history.entry(jid.clone()).or_default().page_pending = true;
            self.send(Command::LoadMessages { chat_jid: jid.clone(), before: None, limit: 100 });
        }
        if cx.active_window().is_some() && self.chats.iter().any(|c| c.jid == jid && c.unread_count > 0) {
            self.send(Command::MarkRead { chat_jid: jid.clone() });
        }
        if self.selected.as_deref() != Some(jid.as_str()) {
            self.replying_to = None;
            // An open info panel follows the conversation, like Messages' details pane.
            if self.profile_open.is_some() && self.profile_open == self.selected {
                self.request_profile(&jid, cx);
                self.profile_open = Some(jid.clone());
            }
        }
        self.selected = Some(jid.clone());
        // Note: no `dismiss_system_notification` here. GPUI's macOS implementation makes a
        // synchronous XPC call to usernotificationsd on the main thread, which can block the
        // UI indefinitely (observed hanging at startup when the first chat is auto-selected).
        cx.emit(StoreEvent::TranscriptChanged { chat_jid: jid, change: Change::Appended });
        cx.notify();
    }

    /// Request the page before the oldest loaded message of the selected chat.
    /// Called when the transcript is scrolled near its top.
    pub fn load_older(&mut self, cx: &mut Context<Self>) {
        let Some(jid) = self.selected.clone() else { return };
        let Some(oldest) = self.messages.get(&jid).and_then(|m| m.first()).map(|m| m.timestamp) else { return };
        let h = self.history.entry(jid.clone()).or_default();
        if !h.has_more || h.page_pending || h.fetching_remote || h.retry_after.is_some_and(|t| Instant::now() < t) {
            return;
        }
        h.page_pending = true;
        h.older_pending = true;
        h.retry_after = None;
        self.send(Command::LoadMessages { chat_jid: jid, before: Some(oldest), limit: 100 });
        cx.notify();
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
        self.media_requested = std::mem::take(&mut self.media_requested)
            .into_iter()
            .map(|(c, id)| if c == from { (to.clone(), id) } else { (c, id) })
            .collect();
        if let Some(a) = self.avatars.remove(&from) {
            self.avatars.entry(to.clone()).or_insert(a);
        }
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
        self.typing.clear();
        self.avatars.clear();
        self.avatar_requested.clear();
        self.avatar_loading.clear();
        self.media_requested.clear();
        self.replying_to = None;
        self.lightbox = None;
        self.profile_open = None;
        self.profiles.clear();
        self.stickers.clear();
        self.just_sent.clear();
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
            let image = cx.background_executor().spawn(async move { load_image(&path) }).await;
            let _ = this.update(cx, |s, cx| {
                s.avatar_loading.remove(&jid);
                s.avatars.insert(jid, image);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn selected_history(&self) -> History {
        self.selected.as_ref().and_then(|j| self.history.get(j)).copied().unwrap_or_default()
    }

    pub fn selected_chat(&self) -> Option<&ChatSummary> {
        let jid = self.selected.as_deref()?;
        self.chats.iter().find(|c| c.jid == jid)
    }

    pub fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(chat_jid) = self.selected.clone() else { return };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let client_id = wa_backend::new_message_id();
        self.just_sent.retain(|_, t| t.elapsed() < Duration::from_secs(5));
        self.just_sent.insert(client_id.clone(), Instant::now());
        let reply_to = self.replying_to.take().map(|m| m.id);
        self.send(Command::SendText { chat_jid, text, client_id, reply_to });
        cx.notify();
    }

    pub fn request_stickers(&self) {
        self.send(Command::ListStickers);
    }

    pub fn send_sticker(&mut self, sticker_id: String, cx: &mut Context<Self>) {
        let Some(chat_jid) = self.selected.clone() else { return };
        let client_id = wa_backend::new_message_id();
        self.just_sent.insert(client_id.clone(), Instant::now());
        let reply_to = self.replying_to.take().map(|m| m.id);
        self.send(Command::SendSticker { chat_jid, sticker_id, client_id, reply_to });
        cx.notify();
    }

    pub fn send_image(&mut self, path: PathBuf, caption: Option<String>, cx: &mut Context<Self>) {
        let Some(chat_jid) = self.selected.clone() else { return };
        let client_id = wa_backend::new_message_id();
        self.just_sent.retain(|_, t| t.elapsed() < Duration::from_secs(5));
        self.just_sent.insert(client_id.clone(), Instant::now());
        let reply_to = self.replying_to.take().map(|m| m.id);
        self.send(Command::SendImage { chat_jid, path, caption, client_id, reply_to });
        cx.notify();
    }

    /// Show the info panel for a chat or person (toggles if it's already open).
    pub fn toggle_profile(&mut self, jid: String, cx: &mut Context<Self>) {
        if self.profile_open.as_deref() == Some(jid.as_str()) {
            self.profile_open = None;
        } else {
            self.request_profile(&jid, cx);
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

    pub fn open_lightbox(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.lightbox = Some(path);
        cx.notify();
    }

    pub fn close_lightbox(&mut self, cx: &mut Context<Self>) {
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

    /// My most recent message in the selected chat that can still be edited (↑ in an
    /// empty composer).
    pub fn last_editable(&self) -> Option<ChatMessage> {
        let jid = self.selected.as_ref()?;
        let list = self.messages.get(jid)?;
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
        if let Some(list) = self.messages.get_mut(&message.chat_jid)
            && let Some(m) = list.iter_mut().find(|m| m.id == message.id)
        {
            m.status = DeliveryStatus::Pending;
            cx.emit(StoreEvent::TranscriptChanged { chat_jid: message.chat_jid.clone(), change: Change::Updated });
        }
        self.send(Command::RetrySend { chat_jid: message.chat_jid.clone(), message_id: message.id.clone() });
        cx.notify();
    }

    pub fn set_replying_to(&mut self, message: Option<ChatMessage>, cx: &mut Context<Self>) {
        self.replying_to = message;
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

    pub fn set_typing(&self, typing: bool) {
        if let Some(chat_jid) = self.selected.clone() {
            self.send(Command::SetTyping { chat_jid, typing });
        }
    }

    pub fn set_typing_in(&self, chat_jid: &str, typing: bool) {
        self.send(Command::SetTyping { chat_jid: chat_jid.to_string(), typing });
    }

    pub fn logout(&self) {
        self.send(Command::Logout);
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
    if let Some(existing) = list.iter_mut().find(|x| x.id == m.id) {
        let status = merge_status(existing.status, m.status);
        *existing = m;
        existing.status = status;
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

fn load_image(path: &Path) -> Option<Arc<Image>> {
    let bytes = std::fs::read(path).ok()?;
    let format = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) { ImageFormat::Png } else { ImageFormat::Jpeg };
    Some(Arc::new(Image::from_bytes(format, bytes)))
}
