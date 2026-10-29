//! The backend thread: a Tokio runtime running the whatsapp-rust client, a supervisor
//! that restarts it after logout / QR expiry, and the glue between protocol events,
//! the app store and the UI channels.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use tokio::sync::{Notify, Semaphore, mpsc};
use whatsapp_rust::download::MediaType;
use whatsapp_rust::http::HttpRequest;
use whatsapp_rust::prelude::{Bot, Client, MessageBuilderExt, MessageField, SqliteStore, wa};
use whatsapp_rust::types::events::{
    Event as WaEvent, EventHandler, EventInterest, EventKind, InboundMessage, LazyHistorySync,
};
use whatsapp_rust::types::presence::{ChatPresence, ReceiptType};
use whatsapp_rust::waproto::whatsapp::device_props::PlatformType;
use whatsapp_rust::waproto::whatsapp::history_sync::HistorySyncType;
use whatsapp_rust::waproto::whatsapp::web_message_info::{Status as WebStatus, StubType};
use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction;
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::{ErrorChainExt as _, Jid, SendOptions, UploadOptions};

use crate::extract::{
    DELETED_LABEL, Extracted, MediaKind, QuotedRef, StoredMedia, classify, document_name, expiration_of, fallback_name,
    is_forwarded, is_group_jid, is_hidden_chat, is_lid_jid, media_of, non_ad, preview_text, quoted_of,
};
use crate::media::{
    TempFile, avatar_file, create_private_dir_all, create_private_file, documents_dir, ext_for_mime, media_file,
    prepare_image_file, private_file, reserve_unique, restrict_file, restrict_tree, safe_file_name, sticker_to_png,
    temp_path, write_atomic,
};
use crate::media_retry;
use message_actions::{ForwardJob, delete_message, deleted_elsewhere, forward_message};
use crate::profile;
use crate::stickers::{self, Candidate, FetchError};
use crate::store::{ChatMeta, LocalProfile, NewMessage, NewQuote, NewReaction, OldestRef, OwnReaction, RawMessage, Store};
use crate::types::*;

mod chat_list;
mod message_actions;
mod outgoing;

const CHATS_DEBOUNCE: Duration = Duration::from_millis(250);
/// Consecutive QR sessions without a scan before backing off for `QR_BACKOFF`, so an
/// unattended QR screen doesn't reconnect every couple of minutes forever.
const MAX_QR_SESSIONS: u32 = 3;
const QR_BACKOFF: Duration = Duration::from_secs(300);
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Uploading a photo (encrypt + HTTPS upload).
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// If history sync goes quiet this long without reaching 100%, report it finished.
const SYNC_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Messages per on-demand history request to the phone (WA Web asks for 50).
const ON_DEMAND_COUNT: i32 = 50;
/// Give up on an on-demand request the phone never answered (phone offline, etc.).
const ON_DEMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Media downloads running at once.
const MAX_DOWNLOADS: usize = 3;
/// Profile pictures are re-fetched after this long (also "no picture" answers).
const AVATAR_TTL_SECS: i64 = 24 * 3600;
/// Pause between profile picture requests, so a freshly rendered chat list asking for
/// 50 pictures doesn't hammer the server.
const AVATAR_INTERVAL: Duration = Duration::from_millis(300);
/// Fetched profiles (About, group members) are reused this long.
const PROFILE_TTL: Duration = Duration::from_secs(5 * 60);
/// How long a download requested while (re)connecting waits for the connection.
const DOWNLOAD_CONNECT_WAIT: Duration = Duration::from_secs(30);
/// Incoming messages older than this (offline catch-up) don't notify.
const NOTIFY_MAX_AGE_SECS: i64 = 60;
/// How often expired disappearing messages are deleted.
const PURGE_INTERVAL: Duration = Duration::from_secs(60);
/// Bounds on the steps of `Command::Shutdown`, so the backend is gone well within the
/// UI's 3 s wait.
const SHUTDOWN_CLIENT_WAIT: Duration = Duration::from_millis(1200);
const SHUTDOWN_JOBS_WAIT: Duration = Duration::from_millis(800);
/// Blocking jobs that can run at once (effectively unbounded; `wipe` takes all permits).
const BLOCKING_PERMITS: u32 = 1 << 20;
/// Sticker panel previews downloading at once (separate from message media, so opening
/// the panel doesn't hold up a photo the user clicked).
const MAX_PREVIEW_DOWNLOADS: usize = 2;
/// Coalesces sticker list updates (previews arriving one by one).
const STICKERS_DEBOUNCE: Duration = Duration::from_millis(400);
/// Favorites are read again on `ListStickers` when older than this.
const FAVORITES_TTL: Duration = Duration::from_secs(120);
/// Waits before each attempt to read the favorites (the app state keys arrive a few
/// seconds after the first connection of a new link).
const FAVORITES_RETRY: [Duration; 4] =
    [Duration::ZERO, Duration::from_secs(10), Duration::from_secs(30), Duration::from_secs(90)];
/// A sticker preview that couldn't be made isn't tried again for this long.
const PREVIEW_RETRY_AFTER: Duration = Duration::from_secs(600);
/// How long sticker previews wait after WhatsApp says "rate-overlimit".
const PREVIEW_RATE_LIMIT_PAUSE: Duration = Duration::from_secs(15 * 60);
/// Sticker messages searched for one with a favorite's file (to re-upload it from).
const STICKER_ANCHOR_SCAN: u32 = 2000;

pub fn spawn(config: BackendConfig) -> Result<BackendHandle> {
    // Owner-only: messages, media and the session keys are private.
    create_private_dir_all(&config.data_dir)?;
    let store = Store::open(&config.data_dir.join("messages.db")).context("opening messages.db")?;
    let (cmd_tx, cmd_rx) = flume::unbounded();
    let (ev_tx, ev_rx) = flume::unbounded::<Event>();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("wa-backend-rt")
        .build()
        .context("building tokio runtime")?;
    std::thread::Builder::new().name("wa-backend".into()).spawn(move || {
        // Files written by older builds (default 0644 / 0755).
        restrict_tree(&config.data_dir);
        let shared = Arc::new(Shared::new(store, ev_tx, config));
        runtime.block_on(async {
            supervise(shared.clone(), cmd_rx).await;
            shared.close().await;
        });
        // Anything still running only finds a retired account and a closed store.
        runtime.shutdown_timeout(Duration::from_millis(500));
        log::info!("wa-backend stopped");
    })?;
    Ok(BackendHandle { commands: cmd_tx, events: ev_rx })
}

/// A WhatsApp-style message id (`3EB0` + 20 hex digits). `Command::SendText` uses the
/// caller's `client_id` as the message id when it is 8–64 ASCII alphanumerics, so a UI
/// can create its optimistic bubble with an id from here and have it match.
pub fn new_message_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let a = h.finish();
    h.write_u8(0x5a);
    let b = h.finish();
    format!("3EB0{:016X}{:04X}", a, b & 0xFFFF)
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// State that outlives individual client sessions.
struct Shared {
    store: Mutex<Store>,
    /// `None` once the backend has stopped: the UI sees the channel close.
    events: Mutex<Option<flume::Sender<Event>>>,
    chats_dirty: Notify,
    /// Chats whose stored transcript changed in bulk, announced with the next chat list.
    stale: Mutex<HashSet<String>>,
    /// Account generation, bumped (under the store lock) when the account's data is
    /// wiped or the backend stops. Work started for an older generation writes nothing.
    generation: AtomicU64,
    /// Every running blocking job holds a permit; `wipe` takes all of them to wait for
    /// the jobs to finish before deleting anything.
    blocking_jobs: Arc<Semaphore>,
    own_jid: Mutex<Option<String>>,
    own_lid: Mutex<Option<String>>,
    sync_generation: AtomicU64,
    qr_sessions: AtomicU32,
    session_db: PathBuf,
    config: BackendConfig,
    /// On-demand history state per canonical chat JID.
    history: Mutex<HashMap<String, HistoryFetch>>,
    /// Media downloads in flight: (canonical chat, message id).
    downloads: Mutex<HashSet<(String, String)>>,
    download_slots: Semaphore,
    /// Profile picture fetches in flight: canonical JID -> JIDs as the UI asked for them.
    avatars: Mutex<HashMap<String, HashSet<String>>>,
    /// Profile fetches in flight: canonical JID -> JIDs as the UI asked for them.
    profiles_in_flight: Mutex<HashMap<String, HashSet<String>>>,
    /// Recently fetched profiles by canonical JID (see [`PROFILE_TTL`]).
    profiles: Mutex<HashMap<String, (tokio::time::Instant, Profile)>>,
    /// Own reactions shown (stored) ahead of their send, by [`ReactJob::seq`]: undone if
    /// the send fails or never happens.
    reactions_in_flight: Mutex<HashMap<u64, OptimisticReaction>>,
    /// Own edits shown (stored) ahead of their send, by [`EditJob::seq`]: undone if the
    /// send fails or never happens.
    edits_in_flight: Mutex<HashMap<u64, OptimisticEdit>>,
    /// Sticker panel state (see `Command::ListStickers`).
    stickers: Mutex<StickerPanel>,
    /// The sticker list may have changed (debounced, sent if the UI asked for it).
    stickers_dirty: Notify,
    preview_slots: Semaphore,
    /// The app is in front (`Command::SetActive`): online to WhatsApp only then.
    active: AtomicBool,
}

#[derive(Default)]
struct StickerPanel {
    /// The UI asked for the list: changes are pushed to it.
    wanted: bool,
    last_sent: Option<Vec<StickerRef>>,
    /// When the favorites were last read from the app state.
    refreshed_at: Option<tokio::time::Instant>,
    refreshing: bool,
    previews_in_flight: HashSet<String>,
    /// Previews that couldn't be made, and when.
    previews_failed: HashMap<String, tokio::time::Instant>,
    /// Stickers whose file is gone from the CDN.
    expired: HashSet<String>,
    /// WhatsApp said "rate-overlimit": no preview fetches until then.
    paused_until: Option<tokio::time::Instant>,
}

/// An own reaction stored before it was sent.
#[derive(Clone, Debug)]
struct OptimisticReaction {
    /// Canonical chat.
    chat_jid: String,
    target_id: String,
    own_jid: String,
    ts_ms: i64,
    previous: Option<OwnReaction>,
}

static REACTION_SEQ: AtomicU64 = AtomicU64::new(0);

/// An edit of mine as stored, and what it replaced (to put back if it isn't sent).
struct OptimisticEdit {
    /// Canonical chat.
    chat_jid: String,
    id: String,
    /// The `edit_ts` it was stored with.
    applied_ts: i64,
    previous: MessageBody,
    /// `None` = not edited before.
    previous_ts: Option<i64>,
}

static EDIT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Our own identities, to recognise own messages in quotes.
#[derive(Clone, Debug, Default)]
struct Own {
    pn: Option<String>,
    lid: Option<String>,
}

impl Own {
    fn is_me(&self, jid: &str) -> bool {
        let jid = non_ad(jid);
        self.pn.as_deref() == Some(jid.as_str()) || self.lid.as_deref() == Some(jid.as_str())
    }

    fn quote(&self, q: QuotedRef) -> NewQuote {
        let from_me = q.participant.as_deref().is_some_and(|p| self.is_me(p));
        NewQuote { id: q.id, sender: q.participant.as_deref().map(non_ad), from_me, preview: q.preview }
    }
}

#[derive(Default)]
struct HistoryFetch {
    /// The request in flight.
    in_flight: Option<Anchor>,
    /// The phone answered with nothing older.
    exhausted: bool,
}

/// What an on-demand history request asked for: messages older than the oldest stored
/// one, which is at second `ts`. Older messages may share that second, so the ids stored
/// at `ts` when asking tell the new ones apart.
struct Anchor {
    ts: Timestamp,
    known: HashSet<String>,
    seq: u64,
}

static HISTORY_REQ_SEQ: AtomicU64 = AtomicU64::new(0);

impl Shared {
    fn new(store: Store, events: flume::Sender<Event>, config: BackendConfig) -> Self {
        Shared {
            store: Mutex::new(store),
            events: Mutex::new(Some(events)),
            chats_dirty: Notify::new(),
            stale: Mutex::new(HashSet::new()),
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            blocking_jobs: Arc::new(Semaphore::new(BLOCKING_PERMITS as usize)),
            own_jid: Mutex::new(None),
            own_lid: Mutex::new(None),
            sync_generation: AtomicU64::new(0),
            qr_sessions: AtomicU32::new(0),
            session_db: config.data_dir.join("session.db"),
            config,
            history: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashSet::new()),
            download_slots: Semaphore::new(MAX_DOWNLOADS),
            avatars: Mutex::new(HashMap::new()),
            profiles_in_flight: Mutex::new(HashMap::new()),
            profiles: Mutex::new(HashMap::new()),
            reactions_in_flight: Mutex::new(HashMap::new()),
            edits_in_flight: Mutex::new(HashMap::new()),
            stickers: Mutex::new(StickerPanel::default()),
            stickers_dirty: Notify::new(),
            preview_slots: Semaphore::new(MAX_PREVIEW_DOWNLOADS),
        }
    }

    fn emit(&self, event: Event) {
        if let Some(events) = &*self.events.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = events.send(event);
        }
    }

    fn mark_chats_dirty(&self) {
        self.chats_dirty.notify_one();
    }

    /// Run `f` against the store, logging (and swallowing) errors.
    fn store<R>(&self, f: impl FnOnce(&mut Store) -> Result<R>) -> Option<R> {
        self.store_inner(None, f)
    }

    /// [`Shared::store`] for work of account generation `generation`: does nothing
    /// (`None`) if the account has been wiped since.
    fn store_gen<R>(&self, generation: u64, f: impl FnOnce(&mut Store) -> Result<R>) -> Option<R> {
        self.store_inner(Some(generation), f)
    }

    fn store_inner<R>(&self, generation: Option<u64>, f: impl FnOnce(&mut Store) -> Result<R>) -> Option<R> {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if generation.is_some_and(|g| g != self.generation()) {
            log::debug!("dropping a store write for a wiped account");
            return None;
        }
        let result = f(&mut store);
        let merges = store.take_merges();
        drop(store);
        for (from, to) in merges {
            // In-memory state keyed by the old JID follows it.
            let mut stale = self.stale.lock().unwrap_or_else(|e| e.into_inner());
            if stale.remove(&from) {
                stale.insert(to.clone());
            }
            drop(stale);
            self.emit(Event::ChatMerged { from, to });
        }
        match result {
            Ok(r) => Some(r),
            Err(e) => {
                log::error!("store error: {e:#}");
                None
            }
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Run blocking work (file writes, decoding, history import) for account generation
    /// `generation` on the blocking pool. It doesn't start once that account is wiped, and
    /// a wipe waits for it to finish.
    async fn blocking<R: Send + 'static>(
        self: &Arc<Self>,
        generation: u64,
        f: impl FnOnce(&Shared) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let permit = self.blocking_jobs.clone().acquire_owned().await?;
        let shared = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if shared.generation() != generation {
                bail!("the account was wiped");
            }
            f(&shared)
        })
        .await
        .map_err(|e| anyhow!("{e}"))?
    }

    /// End the current account generation: from now on its work writes nothing. Then
    /// wait (up to `max`) for its blocking jobs. Returns false on timeout.
    async fn retire(&self, max: Option<Duration>) -> bool {
        {
            let _store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        let drained = self.blocking_jobs.acquire_many(BLOCKING_PERMITS);
        match max {
            Some(max) => tokio::time::timeout(max, drained).await.is_ok(),
            None => {
                let _ = drained.await;
                true
            }
        }
    }

    /// Final shutdown: retire the account, close `messages.db` cleanly and drop the event
    /// sender so the UI sees the channel close.
    async fn close(&self) {
        if !self.retire(Some(SHUTDOWN_JOBS_WAIT)).await {
            log::warn!("blocking jobs still running at shutdown");
        }
        self.store(|s| s.close());
        self.events.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Delete disappearing messages whose time is up, and tell the UI.
    fn purge_expired(&self) {
        let now = now_secs();
        let Some(removed) = self.store(|s| {
            s.prune_pending_edits(now)?;
            s.purge_expired(now)
        }) else {
            return;
        };
        let mut by_chat: HashMap<String, Vec<String>> = HashMap::new();
        for r in removed {
            if let Some(path) = &r.media_path
                && let Err(e) = std::fs::remove_file(path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("removing {}: {e}", path.display());
            }
            by_chat.entry(r.chat_jid).or_default().push(r.id);
        }
        if by_chat.is_empty() {
            return;
        }
        for (chat_jid, message_ids) in by_chat {
            self.emit(Event::MessagesRemoved { chat_jid, message_ids });
        }
        self.mark_chats_dirty();
    }

    /// Announce bulk transcript changes of `chats` (debounced, with the chat list).
    fn mark_stale(&self, chats: impl IntoIterator<Item = String>) {
        self.stale.lock().unwrap_or_else(|e| e.into_inner()).extend(chats);
        self.mark_chats_dirty();
    }

    fn emit_chats(&self) {
        if let Some(chats) = self.store(|s| s.list_chats(now_secs())) {
            self.emit(Event::Chats(chats));
        }
    }

    fn own_jid(&self) -> Option<String> {
        self.own_jid.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn own(&self) -> Own {
        Own { pn: self.own_jid(), lid: self.own_lid.lock().unwrap_or_else(|e| e.into_inner()).clone() }
    }

    fn set_own(&self, pn: Option<&Jid>, lid: Option<&Jid>) {
        *self.own_jid.lock().unwrap_or_else(|e| e.into_inner()) = pn.map(Jid::to_non_ad_string);
        *self.own_lid.lock().unwrap_or_else(|e| e.into_inner()) = lid.map(Jid::to_non_ad_string);
        let own: Vec<String> = [pn, lid].into_iter().flatten().map(Jid::to_non_ad_string).collect();
        self.store(|s| {
            s.set_own_jids(own);
            Ok(())
        });
    }

    /// Emit the stored state of a message.
    fn emit_message(&self, chat_jid: &str, id: &str) {
        if let Some(Some(msg)) = self.store(|s| s.get_message(chat_jid, id)) {
            self.emit(Event::MessageUpserted(msg));
        }
    }

    /// Forget the account: app store, media, avatars and whatsapp-rust session, deleting
    /// the files (not just their rows). Every `Client`/`SqliteStore` of the old session
    /// must already be dropped; its blocking jobs are waited for, and anything they or
    /// stray tasks try to write afterwards is dropped.
    async fn wipe(&self) {
        self.retire(None).await;
        self.store(|s| s.wipe());
        self.history.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.stale.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.profiles.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let panel = std::mem::take(&mut *self.stickers.lock().unwrap_or_else(|e| e.into_inner()));
        if panel.wanted {
            self.emit(Event::Stickers(Vec::new()));
        }
        *self.own_jid.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.own_lid.lock().unwrap_or_else(|e| e.into_inner()) = None;
        for dir in ["media", "avatars"] {
            let dir = self.config.data_dir.join(dir);
            if let Err(e) = std::fs::remove_dir_all(&dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::error!("removing {}: {e}", dir.display());
            }
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.session_db.clone().into_os_string();
            path.push(suffix);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::error!("removing {}: {e}", PathBuf::from(path).display()),
            }
        }
    }
}

enum Outcome {
    Shutdown,
    LoggedOut,
    QrExhausted,
    /// The client stopped and won't come back on its own (e.g. replaced by another session).
    Stopped(String),
}

async fn supervise(shared: Arc<Shared>, cmd_rx: flume::Receiver<Command>) {
    shared.emit(Event::Connection(ConnectionState::Connecting));
    shared.purge_expired();
    shared.emit_chats();
    let debouncer = {
        let shared = shared.clone();
        tokio::spawn(async move {
            loop {
                // Also wake when a timed mute ends, so the chat stops showing as muted.
                let mute_ends = shared.store(|s| s.next_mute_expiry(now_secs())).flatten();
                match mute_ends {
                    Some(t) => {
                        let wait = Duration::from_secs((t - now_secs()).clamp(0, 86_400) as u64 + 1);
                        tokio::select! {
                            _ = shared.chats_dirty.notified() => {}
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }
                    None => shared.chats_dirty.notified().await,
                }
                tokio::time::sleep(CHATS_DEBOUNCE).await;
                shared.emit_chats();
                let stale = std::mem::take(&mut *shared.stale.lock().unwrap_or_else(|e| e.into_inner()));
                for chat_jid in stale {
                    shared.emit(Event::TranscriptStale { chat_jid });
                }
            }
        })
    };
    let purger = {
        let shared = shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PURGE_INTERVAL).await;
                shared.purge_expired();
            }
        })
    };
    let sticker_debouncer = {
        let shared = shared.clone();
        tokio::spawn(async move {
            loop {
                shared.stickers_dirty.notified().await;
                tokio::time::sleep(STICKERS_DEBOUNCE).await;
                shared.emit_stickers(false);
            }
        })
    };

    loop {
        let outcome = match run_session(&shared, &cmd_rx).await {
            Ok(outcome) => outcome,
            Err(e) => Outcome::Stopped(format!("{e:#}")),
        };
        match outcome {
            Outcome::Shutdown => break,
            Outcome::LoggedOut => {
                shared.wipe().await;
                shared.qr_sessions.store(0, Ordering::Relaxed);
                shared.emit(Event::Connection(ConnectionState::LoggedOut));
                shared.emit(Event::Chats(Vec::new()));
                // Start pairing again right away so a fresh QR shows up.
            }
            Outcome::QrExhausted => {
                if shared.qr_sessions.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_QR_SESSIONS {
                    shared.qr_sessions.store(0, Ordering::Relaxed);
                    let reason = "QR code expired; a new one will be shown shortly.".to_owned();
                    if !idle(&shared, &cmd_rx, reason, Some(QR_BACKOFF)).await {
                        break;
                    }
                } else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            Outcome::Stopped(reason) => {
                // Not retried automatically (e.g. another instance took over this session);
                // the next app start reconnects.
                if !idle(&shared, &cmd_rx, reason, None).await {
                    break;
                }
            }
        }
        shared.emit(Event::Connection(ConnectionState::Connecting));
    }
    debouncer.abort();
    purger.abort();
    sticker_debouncer.abort();
    let _ = debouncer.await;
    let _ = purger.await;
    let _ = sticker_debouncer.await;
}

/// No client running: serve store-backed commands until `retry_after` elapses or `Logout`
/// (true: start a new session) or `Shutdown` (false).
async fn idle(
    shared: &Arc<Shared>,
    cmd_rx: &flume::Receiver<Command>,
    reason: String,
    retry_after: Option<Duration>,
) -> bool {
    shared.emit(Event::Connection(ConnectionState::Disconnected { reason }));
    let retry = async {
        match retry_after {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(retry);
    let mut commands_open = true;
    loop {
        let cmd = tokio::select! {
            _ = &mut retry => return true,
            cmd = cmd_rx.recv_async(), if commands_open => cmd,
        };
        match cmd {
            Ok(Command::Shutdown) => return false,
            Ok(Command::Logout) => {
                shared.wipe().await;
                shared.emit(Event::Connection(ConnectionState::LoggedOut));
                shared.emit(Event::Chats(Vec::new()));
                return true;
            }
            Ok(cmd) => handle_command(shared, None, cmd).await,
            Err(_) if retry_after.is_some() => commands_open = false,
            // Nobody can ever send a command again and nothing will retry.
            Err(_) => return false,
        }
    }
}

/// Why a session's client stopped.
#[derive(Debug, PartialEq)]
enum Terminal {
    LoggedOut,
    QrExhausted,
    Stopped(String),
}

/// The terminal reason of a session, recorded the moment the library reports it (its
/// event bus calls handlers synchronously, before the client stops), so it's known
/// whenever the client is seen to finish.
#[derive(Default)]
struct TerminalSlot(Mutex<Option<Terminal>>);

impl TerminalSlot {
    /// A logout wins over anything else; otherwise the first reason stays.
    fn record(&self, reason: Terminal) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if reason == Terminal::LoggedOut || slot.is_none() {
            *slot = Some(reason);
        }
    }

    fn outcome(&self) -> Outcome {
        match self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(Terminal::LoggedOut) => Outcome::LoggedOut,
            Some(Terminal::QrExhausted) => Outcome::QrExhausted,
            Some(Terminal::Stopped(reason)) => Outcome::Stopped(reason),
            None => Outcome::Stopped("Disconnected".to_owned()),
        }
    }
}

fn ban_message(ban: &whatsapp_rust::types::events::TemporaryBan) -> String {
    format!("WhatsApp temporarily banned this account ({:?}), expires in {} s", ban.code, ban.expire.num_seconds())
}

/// Per-client-session state.
struct Session {
    shared: Arc<Shared>,
    client: Arc<Client>,
    /// Account generation the session belongs to (see [`Shared::blocking`]).
    generation: u64,
    group_names: mpsc::UnboundedSender<String>,
    /// Canonical JIDs whose profile picture to fetch, one at a time.
    avatar_queue: mpsc::UnboundedSender<String>,
    /// Canonical JIDs whose profile to fetch, in the same queue (ahead of pictures).
    profile_queue: mpsc::UnboundedSender<String>,
    /// Outgoing messages, sent one at a time in order (a photo upload holds back a text
    /// typed after it, like WhatsApp does).
    sends: mpsc::UnboundedSender<SendJob>,
    groups_requested: Mutex<HashSet<String>>,
    groups_fetched: AtomicBool,
    after_connect_started: AtomicBool,
    /// Detached tasks to abort and wait for at teardown (they hold the client).
    side_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    presence_subscribed: Mutex<HashSet<String>>,
}

/// Forwards bus events to the session's worker in arrival order, without dropping any,
/// and records terminal events right away.
struct Forwarder {
    events: mpsc::UnboundedSender<Arc<WaEvent>>,
    terminal: Arc<TerminalSlot>,
}

impl EventHandler for Forwarder {
    fn handle_event(&self, event: Arc<WaEvent>) {
        match &*event {
            WaEvent::LoggedOut(_) => self.terminal.record(Terminal::LoggedOut),
            WaEvent::PairingQrCodesExhausted(x) if x.disconnected => self.terminal.record(Terminal::QrExhausted),
            WaEvent::StreamReplaced(_) => {
                self.terminal.record(Terminal::Stopped("This session was opened somewhere else.".to_owned()))
            }
            WaEvent::TemporaryBan(ban) => self.terminal.record(Terminal::Stopped(ban_message(ban))),
            _ => {}
        }
        let _ = self.events.send(event);
    }

    fn interest(&self) -> EventInterest {
        EventInterest::of(&[
            EventKind::Connected,
            EventKind::Disconnected,
            EventKind::PairSuccess,
            EventKind::PairError,
            EventKind::LoggedOut,
            EventKind::PairingQrCode,
            EventKind::PairingQrCodesExhausted,
            EventKind::ClientOutdated,
            EventKind::Messages,
            EventKind::Receipt,
            EventKind::ChatPresence,
            EventKind::OfflineSyncCompleted,
            EventKind::ContactUpdate,
            EventKind::PushNameUpdate,
            EventKind::PinUpdate,
            EventKind::DeleteMessageForMeUpdate,
            EventKind::MuteUpdate,
            EventKind::ArchiveUpdate,
            EventKind::MarkChatAsReadUpdate,
            EventKind::GroupUpdate,
            EventKind::HistorySync,
            EventKind::PictureUpdate,
            EventKind::StreamReplaced,
            EventKind::TemporaryBan,
            EventKind::ConnectFailure,
        ])
    }
}

async fn run_session(shared: &Arc<Shared>, cmd_rx: &flume::Receiver<Command>) -> Result<Outcome> {
    let db_url = shared.session_db.to_str().context("session.db path is not UTF-8")?;
    // Owner-only from the start (SQLite gives -wal / -shm the database file's mode).
    create_private_file(&shared.session_db)?;
    let backend = SqliteStore::new(db_url).await.context("opening session.db")?;
    for suffix in ["", "-wal", "-shm"] {
        let mut path = shared.session_db.clone().into_os_string();
        path.push(suffix);
        restrict_file(Path::new(&path));
    }
    let (wa_tx, wa_rx) = mpsc::unbounded_channel();
    let terminal = Arc::new(TerminalSlot::default());
    let bot = Bot::builder()
        .with_backend(backend)
        .with_device_props(
            DevicePropsOverride::new()
                .with_os(shared.config.device_name.clone())
                .with_platform_type(PlatformType::DESKTOP)
                // Ask the phone for the full history at link time, like WhatsApp Desktop.
                // Only applies to new pairings.
                .with_require_full_sync(true),
        )
        .with_event_handler(Forwarder { events: wa_tx, terminal: terminal.clone() })
        .build()
        .await
        .context("building WhatsApp client")?;
    let client = bot.client();
    let snapshot = client.persistence_manager().get_device_snapshot();
    if snapshot.pn.is_some() {
        shared.set_own(snapshot.pn.as_ref(), snapshot.lid.as_ref());
    }

    let (group_tx, group_rx) = mpsc::unbounded_channel();
    let (avatar_tx, avatar_rx) = mpsc::unbounded_channel();
    let (profile_tx, profile_rx) = mpsc::unbounded_channel();
    let (send_tx, send_rx) = mpsc::unbounded_channel();
    let session = Arc::new(Session {
        shared: shared.clone(),
        client: client.clone(),
        generation: shared.generation(),
        group_names: group_tx,
        avatar_queue: avatar_tx,
        profile_queue: profile_tx,
        sends: send_tx,
        groups_requested: Mutex::new(HashSet::new()),
        groups_fetched: AtomicBool::new(false),
        after_connect_started: AtomicBool::new(false),
        side_tasks: Mutex::new(Vec::new()),
        presence_subscribed: Mutex::new(HashSet::new()),
    });
    let (session_cmd_tx, session_cmd_rx) = mpsc::unbounded_channel();
    let tasks = [
        tokio::spawn(session.clone().event_loop(wa_rx)),
        tokio::spawn(session.clone().command_loop(session_cmd_rx)),
        tokio::spawn(session.clone().group_name_loop(group_rx)),
        tokio::spawn(session.clone().lookup_loop(avatar_rx, profile_rx)),
        tokio::spawn(session.clone().send_loop(send_rx)),
    ];

    let mut handle = bot.spawn();
    let mut commands_open = true;

    enum Step {
        BotDone,
        Command(Option<Command>),
    }
    let outcome = loop {
        let step = tokio::select! {
            _ = &mut handle => Step::BotDone,
            c = cmd_rx.recv_async(), if commands_open => Step::Command(c.ok()),
        };
        match step {
            // Terminal events were recorded before the client stopped.
            Step::BotDone => break terminal.outcome(),
            // All handles dropped: keep running, there's just nobody to listen to.
            Step::Command(None) => commands_open = false,
            Step::Command(Some(Command::Shutdown)) => {
                if tokio::time::timeout(SHUTDOWN_CLIENT_WAIT, handle.shutdown()).await.is_err() {
                    log::warn!("client didn't stop in time");
                }
                break Outcome::Shutdown;
            }
            Step::Command(Some(Command::Logout)) => {
                terminal.record(Terminal::LoggedOut);
                client.logout().await;
            }
            Step::Command(Some(cmd)) => {
                let _ = session_cmd_tx.send(cmd);
            }
        }
    };

    // Stop every task of the session and wait for them, so nothing holds the client
    // (its store is closed before a possible wipe) or writes anything afterwards.
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
    loop {
        let side: Vec<_> = session.side_tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        if side.is_empty() {
            break;
        }
        for task in &side {
            task.abort();
        }
        for task in side {
            let _ = task.await;
        }
    }
    drop(session);
    drop(client);
    // Work the aborted tasks left half done: downloads go back to idle, history fetches
    // stop loading, queued or in-flight sends fail (the UI offers a retry).
    let fetching: Vec<String> = shared
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter_mut()
        .filter_map(|(chat, h)| h.in_flight.take().map(|_| chat.clone()))
        .collect();
    for chat_jid in fetching {
        shared.emit(Event::HistoryLoading { chat_jid, loading: false, exhausted: false });
    }
    shared.store(|s| s.reset_downloading());
    shared.downloads.lock().unwrap_or_else(|e| e.into_inner()).clear();
    shared.avatars.lock().unwrap_or_else(|e| e.into_inner()).clear();
    shared.profiles_in_flight.lock().unwrap_or_else(|e| e.into_inner()).clear();
    {
        let mut panel = shared.stickers.lock().unwrap_or_else(|e| e.into_inner());
        panel.refreshing = false;
        panel.previews_in_flight.clear();
    }
    if let Some(failed) = shared.store(|s| s.fail_pending()) {
        for (chat_jid, id) in failed {
            shared.emit(Event::StatusChanged { chat_jid, message_ids: vec![id], status: DeliveryStatus::Failed });
        }
    }
    let unsent: Vec<u64> =
        shared.reactions_in_flight.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    if !unsent.is_empty() {
        for seq in unsent {
            shared.finish_reaction(seq, false);
        }
        shared.emit(Event::Error("Couldn't send the reaction: disconnected".to_owned()));
    }
    let mut unsent: Vec<u64> = shared.edits_in_flight.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    if !unsent.is_empty() {
        unsent.sort_unstable();
        for seq in unsent {
            shared.finish_edit(seq, false);
        }
        shared.emit(Event::Error("Couldn't edit the message: disconnected".to_owned()));
    }
    Ok(outcome)
}

fn parse_jid(jid: &str) -> Option<Jid> {
    jid.parse().ok()
}

impl Session {
    async fn event_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Arc<WaEvent>>) {
        while let Some(event) = rx.recv().await {
            self.handle_event(&event).await;
        }
    }

    async fn command_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Command>) {
        while let Some(cmd) = rx.recv().await {
            handle_command(&self.shared, Some(&self), cmd).await;
        }
    }

    /// Fetch subjects of groups we have no name for, one at a time.
    async fn group_name_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<String>) {
        while let Some(jid) = rx.recv().await {
            let still_unnamed = self.shared.store(|s| s.unnamed_groups()).unwrap_or_default().contains(&jid);
            let Some(group) = parse_jid(&jid).filter(|_| still_unnamed) else { continue };
            match self.client.groups().get_metadata(&group).await {
                Ok(meta) if !meta.subject.is_empty() => {
                    self.shared.store(|s| s.set_chat_name(&jid, &meta.subject));
                    self.shared.mark_chats_dirty();
                }
                Ok(_) => {}
                Err(e) => log::debug!("group metadata for {jid}: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Online while the app is in front, offline otherwise (see `Command::SetActive`).
    async fn apply_presence(&self) {
        let presence = self.client.presence();
        let active = self.shared.active.load(Ordering::Relaxed);
        let result = if active { presence.set_available().await } else { presence.set_unavailable().await };
        // While this device shows as online, the phone holds back its notifications:
        // keep a trace of what WhatsApp was last told.
        match result {
            Ok(()) => log::info!("presence: {}", if active { "online" } else { "offline" }),
            Err(e) => log::warn!("presence ({}): {e:?}", if active { "online" } else { "offline" }),
        }
    }

    /// The library sends "available" by itself after connecting (and after its offline
    /// catch-up): re-apply ours once that's out.
    fn correct_presence_soon(self: &Arc<Self>) {
        let session = self.clone();
        self.track(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            session.apply_presence().await;
        }));
    }

    fn request_group_name(&self, jid: &str) {
        let first = self.groups_requested.lock().unwrap_or_else(|e| e.into_inner()).insert(jid.to_owned());
        if first {
            let _ = self.group_names.send(jid.to_owned());
        }
    }

    /// Learn the PN for a LID JID from the library's LID-PN mapping if the store lacks it.
    async fn resolve_lid(&self, jid: &Jid) {
        if !jid.is_lid() {
            return;
        }
        let lid = jid.to_non_ad_string();
        let known = self.shared.store(|s| s.canonical(&lid)).is_some_and(|c| c != lid);
        if known {
            return;
        }
        if let Ok(Some(entry)) = self.client.get_lid_pn_entry(jid).await {
            let pn = format!("{}@s.whatsapp.net", entry.phone_number);
            if self.shared.store(|s| s.add_alias(&lid, &pn)) == Some(true) {
                self.shared.mark_chats_dirty();
            }
        }
    }

    fn learn_alias(&self, a: &Jid, b: &Jid) {
        let (lid, pn) = match (a.is_lid(), b.is_lid(), a.is_pn(), b.is_pn()) {
            (true, _, _, true) => (a, b),
            (_, true, true, _) => (b, a),
            _ => return,
        };
        if self.shared.store(|s| s.add_alias(&lid.to_non_ad_string(), &pn.to_non_ad_string())) == Some(true) {
            self.shared.mark_chats_dirty();
        }
    }

    async fn after_connect(self: Arc<Self>) {
        // Favorites live in app state: read them once per session (on a new link this
        // is how they first arrive).
        self.refresh_favorite_stickers(None);
        // One IQ for every group subject instead of one per group.
        match self.client.groups().get_participating().await {
            Ok(groups) => {
                // Communities themselves (their parent groups), for their names, in case the
                // groups list leaves them out.
                let communities = self.client.community().get_participating().await.unwrap_or_else(|e| {
                    log::debug!("fetching communities: {e}");
                    Default::default()
                });
                let mut links: HashMap<String, (String, Option<String>, bool, String)> = HashMap::new();
                for (jid, meta) in groups.iter().chain(&communities) {
                    let jid = jid.to_non_ad_string();
                    let parent = meta.parent_group_jid.as_ref().map(|p| p.to_non_ad_string());
                    links.insert(jid.clone(), (jid, parent, meta.is_parent_group, meta.subject.clone()));
                }
                let links: Vec<_> = links.into_values().collect();
                self.shared.store(|s| {
                    s.transaction(|s| {
                        for (jid, meta) in &groups {
                            if !meta.subject.is_empty() {
                                s.set_chat_name(&jid.to_non_ad_string(), &meta.subject)?;
                            }
                        }
                        s.set_group_communities(&links)
                    })
                });
                let mut requested = self.groups_requested.lock().unwrap_or_else(|e| e.into_inner());
                requested.extend(groups.keys().map(|j| j.to_non_ad_string()));
                drop(requested);
                self.shared.mark_chats_dirty();
            }
            Err(e) => log::warn!("fetching participating groups: {e}"),
        }
        self.groups_fetched.store(true, Ordering::Relaxed);
        for jid in self.shared.store(|s| s.unnamed_groups()).unwrap_or_default() {
            self.request_group_name(&jid);
        }
        for lid in self.shared.store(|s| s.lid_chats()).unwrap_or_default() {
            if let Some(jid) = parse_jid(&lid) {
                self.resolve_lid(&jid).await;
            }
        }
    }

    async fn handle_event(self: &Arc<Self>, event: &WaEvent) {
        let shared = &self.shared;
        use WaEvent as E;
        match event {
            E::Connected(_) => {
                shared.qr_sessions.store(0, Ordering::Relaxed);
                let snapshot = self.client.persistence_manager().get_device_snapshot();
                let own = snapshot.pn.as_ref().map(|j| j.to_non_ad_string());
                shared.set_own(snapshot.pn.as_ref(), snapshot.lid.as_ref());
                let push_name = Some(snapshot.push_name.clone()).filter(|p| !p.is_empty());
                shared.emit(Event::Connection(ConnectionState::Connected {
                    own_jid: own.unwrap_or_default(),
                    push_name,
                }));
                // `Connected` fires again on every reconnect; once per session is enough.
                if !self.after_connect_started.swap(true, Ordering::Relaxed) {
                    self.track(tokio::spawn(self.clone().after_connect()));
                }
                // Read receipts that couldn't be sent before.
                let session = self.clone();
                self.track(tokio::spawn(async move { send_receipts(&session.shared, &session, None).await }));
                self.correct_presence_soon();
            }
            // The library goes online after a connect's offline catch-up: undo that if the
            // app isn't in front.
            E::OfflineSyncCompleted(_) => self.correct_presence_soon(),
            E::Disconnected(d) => {
                shared.emit(Event::Connection(ConnectionState::Disconnected { reason: d.reason.to_string() }));
            }
            E::PairingQrCode(qr) => {
                shared.emit(Event::Connection(ConnectionState::AwaitingQr {
                    code: qr.code.clone(),
                    valid_for_secs: qr.timeout.as_secs(),
                }));
            }
            // Terminal events (logout, QR exhausted, replaced, ban) are recorded by the
            // `Forwarder` as they happen.
            E::PairSuccess(_) => shared.emit(Event::Connection(ConnectionState::Connecting)),
            E::PairError(e) => shared.emit(Event::Error(format!("Pairing failed: {}", e.error))),
            E::TemporaryBan(ban) => shared.emit(Event::Error(ban_message(ban))),
            E::ConnectFailure(f) => {
                shared.emit(Event::Error(format!(
                    "Connection refused: {:?} {}",
                    f.reason,
                    f.message.as_deref().unwrap_or("")
                )));
            }
            E::ClientOutdated(_) => {
                shared.emit(Event::Error("WhatsApp rejected this client version as outdated.".to_owned()))
            }
            E::Messages(batch) => {
                for m in batch.messages.iter() {
                    self.handle_message(m).await;
                }
            }
            E::Receipt(r) => {
                self.resolve_lid(&r.source.chat).await;
                let chat = r.source.chat.to_non_ad_string();
                let status = match r.r#type {
                    ReceiptType::Delivered => DeliveryStatus::Delivered,
                    ReceiptType::Read | ReceiptType::Played => DeliveryStatus::Read,
                    ReceiptType::ReadSelf | ReceiptType::PlayedSelf => {
                        // Read on another of our devices.
                        shared.store(|s| s.clear_unread(&chat));
                        shared.mark_chats_dirty();
                        return;
                    }
                    _ => return,
                };
                let result = shared.store(|s| Ok((s.canonical(&chat)?, s.upgrade_status(&chat, &r.message_ids, status)?)));
                if let Some((chat_jid, message_ids)) = &result
                    && !message_ids.is_empty()
                {
                    let (chat_jid, message_ids) = (chat_jid.clone(), message_ids.clone());
                    shared.emit(Event::StatusChanged { chat_jid, message_ids, status });
                    shared.mark_chats_dirty();
                }
                if r.source.is_from_me {
                    return;
                }
                // Who got or read it, and when, for its Info.
                if r.source.is_group {
                    self.resolve_lid(&r.source.sender).await;
                }
                let member = if r.source.is_group { r.source.sender.to_non_ad_string() } else { chat.clone() };
                let played = matches!(r.r#type, ReceiptType::Played);
                let at = r.timestamp.timestamp();
                if let Some(ids) = shared.store(|s| s.record_receipt(&chat, &member, &r.message_ids, status, played, at))
                    && !ids.is_empty()
                    && let Some((chat_jid, _)) = &result
                {
                    shared.emit(Event::ReceiptsChanged { chat_jid: chat_jid.clone(), message_ids: ids });
                }
                // In a group, who read it: their picture moves down to it.
                if r.source.is_group && status == DeliveryStatus::Read {
                    let sender = member;
                    if shared.store(|s| s.record_group_read(&chat, &sender, &r.message_ids)) == Some(true)
                        && let Some((chat_jid, _)) = result
                    {
                        emit_group_reads(shared, &chat_jid);
                    }
                }
            }
            E::ChatPresence(p) => {
                self.resolve_lid(&p.source.chat).await;
                let chat = p.source.chat.to_non_ad_string();
                let sender = p.source.sender.to_non_ad_string();
                let result = shared.store(|s| Ok((s.canonical(&chat)?, s.sender_display_name(&sender, None)?)));
                if let Some((chat_jid, name)) = result {
                    let sender_jid = shared.store(|s| s.canonical(&sender)).unwrap_or(sender);
                    shared.emit(Event::Typing {
                        chat_jid,
                        sender_name: Some(name),
                        typing: matches!(p.state, ChatPresence::Composing),
                        sender_jid: Some(sender_jid),
                    });
                }
            }
            E::PushNameUpdate(p) => {
                shared.store(|s| s.set_push_name(&p.jid.to_non_ad_string(), &p.new_push_name));
                shared.mark_chats_dirty();
            }
            E::ContactUpdate(c) => {
                let a = &c.action;
                if let (Some(lid), Some(pn)) = (a.lid_jid.as_deref(), a.pn_jid.as_deref())
                    && let (Some(lid), Some(pn)) = (parse_jid(lid), parse_jid(pn))
                {
                    self.learn_alias(&lid, &pn);
                }
                let name = a.full_name.as_deref().or(a.first_name.as_deref()).unwrap_or("");
                shared.store(|s| s.set_contact_name(&c.jid.to_non_ad_string(), name));
                shared.mark_chats_dirty();
            }
            E::DeleteMessageForMeUpdate(d) => deleted_elsewhere(shared, d),
            E::PinUpdate(p) => {
                let pinned_at = if p.action.pinned == Some(true) { p.timestamp.timestamp().max(1) } else { 0 };
                shared.store(|s| s.set_pinned(&p.jid.to_non_ad_string(), pinned_at));
                shared.mark_chats_dirty();
            }
            E::MuteUpdate(m) => {
                let until = if m.action.muted == Some(true) {
                    normalize_mute(m.action.mute_end_timestamp.unwrap_or(-1))
                } else {
                    0
                };
                shared.store(|s| s.set_muted_until(&m.jid.to_non_ad_string(), until));
                shared.mark_chats_dirty();
            }
            E::ArchiveUpdate(a) => {
                shared.store(|s| s.set_archived(&a.jid.to_non_ad_string(), a.action.archived == Some(true)));
                shared.mark_chats_dirty();
            }
            E::MarkChatAsReadUpdate(m) => {
                let jid = m.jid.to_non_ad_string();
                shared.store(|s| match m.action.read == Some(true) {
                    true => s.clear_unread(&jid),
                    false => s.set_marked_unread(&jid, true),
                });
                shared.mark_chats_dirty();
            }
            E::GroupUpdate(g) => {
                shared.forget_profile(&g.group_jid.to_non_ad_string());
                if let GroupNotificationAction::Subject { subject, .. } = &g.action {
                    shared.store(|s| s.set_chat_name(&g.group_jid.to_non_ad_string(), subject));
                    shared.mark_chats_dirty();
                }
            }
            E::HistorySync(lazy) => self.handle_history_sync((**lazy).clone()).await,
            E::PictureUpdate(p) => {
                let jid = p.jid.to_non_ad_string();
                shared.forget_profile(&jid);
                if shared.store(|s| s.invalidate_avatar(&jid)) != Some(true) {
                    // Never fetched: the UI asks when the chat becomes visible.
                    return;
                }
                if p.removed {
                    let Some(canonical) = shared.store(|s| s.canonical(&jid)) else { return };
                    let _ = std::fs::remove_file(avatar_file(&shared.config.data_dir, &canonical));
                    shared.store(|s| s.set_avatar(&canonical, None, None, now_secs()));
                    shared.emit(Event::AvatarUpdated { jid: canonical, path: None });
                    shared.mark_chats_dirty();
                } else {
                    self.queue_avatar(&jid);
                }
            }
            _ => {}
        }
    }

    async fn handle_message(self: &Arc<Self>, m: &InboundMessage) {
        let shared = &self.shared;
        let info = &m.info;
        let src = &info.source;
        let wire_chat = src.chat.to_string();
        let chat = src.chat.to_non_ad_string();
        if is_hidden_chat(&chat) {
            return;
        }

        // Keep LID and PN addressing of the same person in one chat.
        if !src.is_group {
            let alt = if src.is_from_me { src.recipient_alt.as_ref() } else { src.sender_alt.as_ref() };
            if let Some(alt) = alt {
                self.learn_alias(&src.chat, alt);
            }
            self.resolve_lid(&src.chat).await;
        } else if let Some(alt) = &src.sender_alt {
            self.learn_alias(&src.sender, alt);
        }

        let own = shared.own_jid();
        let sender = match (&own, src.is_from_me) {
            (Some(own), true) => own.clone(),
            _ => src.sender.to_non_ad_string(),
        };
        let push_name = Some(info.push_name.clone()).filter(|p| !p.is_empty() && !src.is_from_me);
        if let Some(p) = &push_name {
            shared.store(|s| s.set_push_name(&sender, p));
        }

        let mut notify = None;
        let timestamp = info.timestamp.timestamp();
        let updated = match classify(&m.message) {
            Extracted::Body(body) => {
                let expires_at = expiration_of(&m.message).map(|e| timestamp + i64::from(e));
                if expires_at.is_some_and(|t| t <= now_secs()) {
                    return;
                }
                let msg = NewMessage {
                    chat_jid: chat.clone(),
                    id: info.id.clone(),
                    sender_jid: sender,
                    wire_chat,
                    wire_sender: src.is_group.then(|| src.sender.to_non_ad_string()),
                    push_name,
                    from_me: src.is_from_me,
                    body,
                    timestamp,
                    status: if src.is_from_me { DeliveryStatus::Sent } else { DeliveryStatus::Delivered },
                    media: media_of(&m.message),
                    quoted: quoted_of(&m.message).map(|q| shared.own().quote(q)),
                    expires_at,
                };
                let forwarded = is_forwarded(&m.message);
                shared.store(|s| {
                    let inserted = s.insert_message(&msg, true)?;
                    if inserted && forwarded {
                        s.set_forwarded(&msg.chat_jid, &msg.id)?;
                    }
                    if msg.from_me {
                        // Replying from the phone reads the chat.
                        s.clear_unread(&msg.chat_jid)?;
                    }
                    // Not for one deleted before it arrived (its content is gone).
                    if inserted
                        && !msg.from_me
                        && now_secs() - msg.timestamp <= NOTIFY_MAX_AGE_SECS
                        && !s.is_revoked(&msg.chat_jid, &msg.id)?
                    {
                        notify = notification(s, &msg)?;
                    }
                    s.get_message(&msg.chat_jid, &msg.id)
                })
            }
            Extracted::Revoke { target_id } => shared.store(|s| s.revoke(&chat, &target_id, DELETED_LABEL)),
            Extracted::Edit { target_id, body } => shared.store(|s| s.apply_edit(&chat, &target_id, &body, timestamp)),
            Extracted::Reaction { target_id, emoji, sender_ts_ms } => {
                let reaction = NewReaction {
                    chat_jid: chat.clone(),
                    target_id,
                    sender_jid: sender,
                    from_me: src.is_from_me,
                    emoji,
                    ts_ms: sender_ts_ms.unwrap_or(timestamp * 1000),
                    push_name,
                };
                // Age by the server's time, not the sender's clock.
                let live = now_secs() - timestamp <= NOTIFY_MAX_AGE_SECS;
                let updated = shared.store(|s| {
                    if !s.upsert_reaction(&reaction)? {
                        return Ok(None);
                    }
                    let target = s.get_message(&reaction.chat_jid, &reaction.target_id)?;
                    if live && let Some(target) = &target {
                        notify = reaction_notification(s, &reaction, target)?;
                    }
                    Ok(target)
                });
                // Not a new message: the chat list (preview, unread) stays as it is.
                if let Some(Some(msg)) = updated {
                    shared.emit(Event::MessageUpserted(msg));
                }
                if let Some(n) = notify {
                    shared.emit(n);
                }
                return;
            }
            Extracted::Skip => return,
        };
        if let Some(Some(msg)) = updated {
            if msg.from_me && matches!(msg.body, MessageBody::Sticker(_)) {
                // A sticker I sent from another device: a new recent.
                shared.stickers_dirty.notify_one();
            }
            shared.emit(Event::MessageUpserted(msg));
            shared.mark_chats_dirty();
        }
        if let Some(n) = notify {
            shared.emit(n);
        }
        if is_group_jid(&chat)
            && self.groups_fetched.load(Ordering::Relaxed)
            && shared.store(|s| s.unnamed_groups()).unwrap_or_default().contains(&chat)
        {
            self.request_group_name(&chat);
        }
    }

    async fn handle_history_sync(self: &Arc<Self>, lazy: LazyHistorySync) {
        let sync_type = lazy.sync_type();
        let progress = lazy.progress();
        let own = self.shared.own();
        let generation = self.generation;
        let result = self
            .shared
            .blocking(generation, move |shared| {
                let Some(hs) = lazy.get() else { bail!("could not decode history sync (type {sync_type})") };
                shared
                    .store_gen(generation, |s| {
                        let with_messages = s.transaction(|s| apply_history_sync(s, hs, &own, now_secs()))?;
                        let all = hs.conversations.iter().map(|c| s.canonical(&c.id)).collect::<Result<Vec<_>>>()?;
                        let with_messages =
                            with_messages.iter().map(|c| s.canonical(c)).collect::<Result<HashSet<_>>>()?;
                        Ok((all, with_messages))
                    })
                    .context("storing history sync")
            })
            .await;
        let (chats, with_messages) = match result {
            Ok(r) => r,
            Err(e) => {
                log::error!("history sync: {e:#}");
                return;
            }
        };
        let shared = &self.shared;
        shared.mark_chats_dirty();
        if sync_type == HistorySyncType::ON_DEMAND as i32 {
            // Answered with a page of its own.
            for chat in chats {
                shared.finish_history_fetch(&chat, None);
            }
        } else {
            // Transcripts already open don't show what just arrived.
            shared.mark_stale(with_messages);
        }
        // Before `after_connect` has fetched all subjects at once, per-group requests
        // would only duplicate it.
        if self.groups_fetched.load(Ordering::Relaxed) {
            for jid in shared.store(|s| s.unnamed_groups()).unwrap_or_default() {
                self.request_group_name(&jid);
            }
        }

        let reports_progress = [HistorySyncType::INITIAL_BOOTSTRAP, HistorySyncType::RECENT, HistorySyncType::FULL]
            .iter()
            .any(|t| *t as i32 == sync_type);
        if let (true, Some(p)) = (reports_progress, progress) {
            let generation = shared.sync_generation.fetch_add(1, Ordering::Relaxed) + 1;
            if p >= 100 {
                shared.emit(Event::SyncProgress(None));
            } else {
                shared.emit(Event::SyncProgress(Some(p as u8)));
                let shared = shared.clone();
                self.track(tokio::spawn(async move {
                    tokio::time::sleep(SYNC_IDLE_TIMEOUT).await;
                    if shared.sync_generation.load(Ordering::Relaxed) == generation {
                        shared.emit(Event::SyncProgress(None));
                    }
                }));
            }
        }
    }
}

/// Mute end timestamps arrive in ms or s; -1 / 0-with-mute means "forever".
fn normalize_mute(t: i64) -> i64 {
    match t {
        t if t < 0 => i64::MAX,
        t if t > 100_000_000_000 => t / 1000,
        t => t,
    }
}

fn web_status(status: Option<WebStatus>) -> DeliveryStatus {
    match status {
        Some(WebStatus::ERROR) => DeliveryStatus::Failed,
        Some(WebStatus::DELIVERY_ACK) => DeliveryStatus::Delivered,
        Some(WebStatus::READ) | Some(WebStatus::PLAYED) => DeliveryStatus::Read,
        _ => DeliveryStatus::Sent,
    }
}

/// Store a history sync chunk. Returns the chats (as sent) that got messages.
fn apply_history_sync(s: &Store, hs: &wa::HistorySync, own: &Own, now: Timestamp) -> Result<Vec<String>> {
    let own_jid = own.pn.as_deref();
    let mut with_messages = Vec::new();
    for m in &hs.phone_number_to_lid_mappings {
        if let (Some(lid), Some(pn)) = (&m.lid_jid, &m.pn_jid) {
            s.add_alias(lid, pn)?;
        }
    }
    for conv in &hs.conversations {
        if let (Some(lid), Some(pn)) = (&conv.lid_jid, &conv.pn_jid) {
            s.add_alias(lid, pn)?;
        }
    }
    for p in &hs.pushnames {
        if let (Some(id), Some(name)) = (&p.id, &p.pushname)
            && Some(non_ad(id).as_str()) != own_jid
        {
            s.set_push_name(id, name)?;
        }
    }

    for conv in &hs.conversations {
        let chat = non_ad(&conv.id);
        if is_hidden_chat(&chat) {
            continue;
        }
        let mut unread = conv.unread_count;
        if conv.marked_as_unread == Some(true) && unread.unwrap_or(0) == 0 {
            unread = Some(1);
        }
        s.upsert_chat_meta(&ChatMeta {
            jid: chat.clone(),
            name: conv.name.clone().or_else(|| conv.display_name.clone()),
            unread,
            pinned_at: conv.pinned.map(i64::from),
            archived: conv.archived,
            muted_until: conv.mute_end_time.map(|t| normalize_mute(t as i64)),
            last_activity: conv.conversation_timestamp.or(conv.last_msg_timestamp).map(|t| t as i64),
        })?;
        if let Some(marked) = conv.marked_as_unread {
            s.set_marked_unread(&chat, marked)?;
        }

        if !conv.messages.is_empty() {
            with_messages.push(chat.clone());
        }
        for hm in &conv.messages {
            let Some(wmi) = hm.message.as_option() else { continue };
            let Some(key) = wmi.key.as_option() else { continue };
            let Some(id) = &key.id else { continue };
            let ts = wmi.message_timestamp.unwrap_or(0) as i64;
            let from_me = key.from_me == Some(true);
            let participant = key.participant.as_deref().or(wmi.participant.as_deref());
            let sender = if from_me {
                own_jid.map(str::to_owned).unwrap_or_default()
            } else if is_group_jid(&chat) {
                participant.map(non_ad).unwrap_or_default()
            } else {
                chat.clone()
            };
            let Some(msg) = wmi.message.as_option() else {
                // A message deleted for everyone is only a stub with its own id: keep
                // its place in the transcript, like a revoke received live.
                if matches!(wmi.message_stub_type, Some(StubType::REVOKE | StubType::ADMIN_REVOKE)) {
                    let stub = NewMessage {
                        chat_jid: chat.clone(),
                        id: id.clone(),
                        sender_jid: sender,
                        wire_chat: conv.id.clone(),
                        wire_sender: participant.map(str::to_owned),
                        push_name: wmi.push_name.clone(),
                        from_me,
                        body: MessageBody::Unsupported { label: DELETED_LABEL.to_owned() },
                        timestamp: ts,
                        status: if from_me { web_status(wmi.status) } else { DeliveryStatus::Delivered },
                        media: None,
                        quoted: None,
                        expires_at: None,
                    };
                    message_actions::store_revoked(s, &stub)?;
                }
                continue;
            };
            match classify(msg) {
                Extracted::Body(body) => {
                    let expires_at = history_expiry(wmi, msg, ts);
                    if expires_at.is_some_and(|t| t <= now) {
                        continue;
                    }
                    let inserted = s.insert_message(
                        &NewMessage {
                            chat_jid: chat.clone(),
                            id: id.clone(),
                            sender_jid: sender,
                            wire_chat: conv.id.clone(),
                            wire_sender: participant.map(str::to_owned),
                            push_name: wmi.push_name.clone(),
                            from_me,
                            body,
                            timestamp: ts,
                            status: if from_me { web_status(wmi.status) } else { DeliveryStatus::Delivered },
                            media: media_of(msg),
                            quoted: quoted_of(msg).map(|q| own.quote(q)),
                            expires_at,
                        },
                        false,
                    )?;
                    if inserted && is_forwarded(msg) {
                        s.set_forwarded(&chat, id)?;
                    }
                }
                Extracted::Edit { target_id, body } => {
                    s.apply_edit(&chat, &target_id, &body, ts)?;
                }
                Extracted::Revoke { target_id } => {
                    s.revoke(&chat, &target_id, DELETED_LABEL)?;
                }
                Extracted::Reaction { target_id, emoji, sender_ts_ms } => {
                    s.upsert_reaction(&NewReaction {
                        chat_jid: chat.clone(),
                        target_id,
                        sender_jid: sender,
                        from_me,
                        emoji,
                        ts_ms: sender_ts_ms.unwrap_or(ts * 1000),
                        push_name: wmi.push_name.clone(),
                    })?;
                }
                Extracted::Skip => {}
            }
            // The reactions this message had when the history was made.
            for r in &wmi.reactions {
                if let Some(reaction) = history_reaction(r, &chat, id, own_jid) {
                    s.upsert_reaction(&reaction)?;
                }
            }
        }
    }
    Ok(with_messages)
}

/// A `WebMessageInfo.reactions` entry on message `target_id`. Its key is the reaction's
/// own: `fromMe` = mine, `participant` = who reacted in a group (1:1: the other person).
fn history_reaction(r: &wa::Reaction, chat: &str, target_id: &str, own_jid: Option<&str>) -> Option<NewReaction> {
    let key = r.key.as_option()?;
    let from_me = key.from_me == Some(true);
    let sender = if from_me {
        own_jid?.to_owned()
    } else if is_group_jid(chat) {
        non_ad(key.participant.as_deref().filter(|p| !p.is_empty())?)
    } else {
        chat.to_owned()
    };
    Some(NewReaction {
        chat_jid: chat.to_owned(),
        target_id: target_id.to_owned(),
        sender_jid: sender,
        from_me,
        emoji: r.text.clone().unwrap_or_default(),
        // Undated: any dated copy beats it.
        ts_ms: r.sender_timestamp_ms.unwrap_or(0),
        push_name: None,
    })
}

/// When a disappearing message from history expires. Absolute times before the message
/// itself are ignored (not trusted to be absolute).
fn history_expiry(wmi: &wa::WebMessageInfo, msg: &wa::Message, ts: Timestamp) -> Option<Timestamp> {
    let after_sent = |t: Option<u64>| t.map(|t| t as i64).filter(|t| *t > ts);
    after_sent(wmi.ephemeral_expiration_timestamp)
        .or_else(|| {
            let duration = i64::from(wmi.ephemeral_duration.filter(|d| *d > 0)?);
            let start = wmi.ephemeral_start_timestamp.map(|t| t as i64).filter(|t| *t >= ts).unwrap_or(ts);
            Some(start + duration)
        })
        .or_else(|| expiration_of(msg).map(|e| ts + i64::from(e)))
}

/// Commands that work on the store, and talk to WhatsApp when a session is up.
async fn handle_command(shared: &Arc<Shared>, session: Option<&Arc<Session>>, cmd: Command) {
    match cmd {
        Command::LoadMessages { chat_jid, before, limit } => {
            if let Some((messages, has_more)) = shared.store(|s| s.load_messages(&chat_jid, before, limit)) {
                let reached_start = !has_more;
                let exhausted = shared.history_exhausted(&chat_jid);
                shared.emit(Event::Messages { chat_jid: chat_jid.clone(), messages, has_more: has_more || !exhausted });
                if is_group_jid(&chat_jid) && before.is_none() {
                    emit_group_reads(shared, &chat_jid);
                }
                // Scrolling up past what's stored locally: ask the phone for older messages.
                if before.is_some() && reached_start && !exhausted && let Some(session) = session {
                    request_older_history(shared, session, &chat_jid);
                }
            }
            // Incoming typing indicators in 1:1 chats need a presence subscription.
            if let Some(session) = session
                && !is_group_jid(&chat_jid)
                && session.client.is_connected()
                && session.presence_subscribed.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_jid.clone())
                && let Some(jid) = parse_jid(&chat_jid)
            {
                let client = session.client.clone();
                session.track(tokio::spawn(async move {
                    if let Err(e) = client.presence().subscribe(jid).await {
                        log::debug!("presence subscribe: {e}");
                    }
                }));
            }
        }
        Command::SendText { chat_jid, text, client_id, reply_to, mentions } => {
            let id = message_id_from(client_id);
            let body = MessageBody::Text(text.clone());
            let Some(canonical) = insert_pending(shared, &chat_jid, &id, body, None, reply_to.as_deref()) else {
                return;
            };
            let job = SendJob::Text { chat_jid, canonical, id, text, reply_to, mentions };
            queue_send(shared, session, job);
        }
        Command::SendImage { chat_jid, path, caption, client_id, reply_to } => {
            let id = message_id_from(client_id);
            let canonical = shared.store(|s| s.canonical(&chat_jid)).unwrap_or_else(|| chat_jid.clone());
            let generation = session.map_or_else(|| shared.generation(), |s| s.generation);
            let image = PendingImage { chat_jid, canonical, id, path, caption, reply_to };
            if let Some(job) = prepare_image_send(shared, generation, image).await {
                queue_send(shared, session, job);
            }
        }
        Command::DownloadMedia { chat_jid, message_id } => download_media(shared, session, &chat_jid, &message_id),
        Command::MessageInfo { chat_jid, message_id } => {
            if let Some(receipts) = shared.store(|s| s.receipts(&chat_jid, &message_id)) {
                shared.emit(Event::MessageInfo { chat_jid, message_id, receipts });
            }
        }
        Command::ListMedia { chat_jid, limit } => {
            if let Some(items) = shared.store(|s| s.load_media(&chat_jid, limit)) {
                shared.emit(Event::ChatMedia { chat_jid, items });
            }
        }
        Command::FetchAvatar { jid } => {
            let Some(canonical) = shared.store(|s| s.canonical(&jid)) else { return };
            let (fresh, cached_path) = shared.cached_avatar(&canonical);
            // Answer from the cache right away; a refresh follows if it's stale.
            if fresh || cached_path.is_some() || session.is_none() {
                shared.emit(Event::AvatarUpdated { jid: jid.clone(), path: cached_path });
            }
            if fresh || is_hidden_chat(&canonical) {
                return;
            }
            if let Some(session) = session {
                session.queue_avatar_for(&canonical, jid);
            }
        }
        Command::MarkRead { chat_jid } => {
            // Receipts for every unread message are kept until the server takes them
            // (retried on reconnect); the unread count clears as they're acknowledged.
            let queued = shared.store(|s| {
                let unread = s.unread_count(&chat_jid)?;
                let pending = s.queue_read_receipts(&chat_jid)?;
                if unread > 0 && pending == 0 {
                    // Nothing stored to acknowledge (e.g. marked unread by hand).
                    s.set_unread(&chat_jid, 0)?;
                }
                Ok((unread, pending))
            });
            chat_list::sync_marked_read(shared, session, &chat_jid);
            match (queued, session) {
                (Some((_, pending)), Some(session)) if pending > 0 => {
                    send_receipts(shared, session, Some(&chat_jid)).await
                }
                (Some((unread, 0)), _) if unread > 0 => shared.mark_chats_dirty(),
                _ => {}
            }
        }
        Command::SetActive { active } => {
            shared.active.store(active, Ordering::Relaxed);
            if let Some(session) = session {
                session.apply_presence().await;
            }
        }
        Command::SetTyping { chat_jid, typing } => {
            let (Some(session), Some(jid)) = (session, parse_jid(&chat_jid)) else { return };
            let chatstate = session.client.chatstate();
            let result = if typing { chatstate.send_composing(&jid).await } else { chatstate.send_paused(&jid).await };
            if let Err(e) = result {
                log::debug!("chat state: {e}");
            }
        }
        // Lifecycle commands are handled by the supervisor.
        Command::Logout | Command::Shutdown => {}
        Command::React { chat_jid, message_id, emoji } => react(shared, session, &chat_jid, &message_id, emoji),
        Command::EditMessage { chat_jid, message_id, text } => edit_message(shared, session, &chat_jid, &message_id, text),
        Command::RetrySend { chat_jid, message_id } => retry_send(shared, session, &chat_jid, &message_id).await,
        Command::ListStickers => {
            shared.stickers.lock().unwrap_or_else(|e| e.into_inner()).wanted = true;
            let list = shared.emit_stickers(true);
            if let Some(session) = session {
                session.load_sticker_previews(list);
                session.refresh_favorite_stickers(Some(FAVORITES_TTL));
            }
        }
        Command::SendSticker { chat_jid, sticker_id, client_id, reply_to } => {
            send_sticker(shared, session, chat_jid, sticker_id, client_id, reply_to).await
        }
        Command::FetchProfile { jid } => fetch_profile(shared, session, jid),
        Command::DeleteMessage { chat_jid, message_id, for_everyone } => {
            delete_message(shared, session, &chat_jid, &message_id, for_everyone)
        }
        Command::ForwardMessage { chat_jid, message_id, to_chat_jid, client_id } => {
            forward_message(shared, session, &chat_jid, &message_id, to_chat_jid, client_id).await
        }
        Command::SearchMessages { chat_jid, query } => {
            let hits = shared.store(|s| s.search_messages(&chat_jid, &query)).unwrap_or_default();
            shared.emit(Event::SearchResults { chat_jid, query, hits });
        }
        Command::SetPinned { .. }
        | Command::SetMuted { .. }
        | Command::SetArchived { .. }
        | Command::MarkUnread { .. }
        | Command::ListContacts
        | Command::LookupPhone { .. }
        | Command::OpenChat { .. } => chat_list::chat_command(shared, session, cmd).await,
        Command::SendFile { chat_jid, path, caption, client_id, reply_to } => {
            outgoing::send_file(shared, session, chat_jid, path, caption, client_id, reply_to).await
        }
        Command::SendVoice { chat_jid, samples, sample_rate, client_id, reply_to } => {
            outgoing::send_voice(shared, session, chat_jid, samples, sample_rate, client_id, reply_to).await
        }
    }
}


/// `Command::FetchProfile`: answer from the store (or a recent fetch) right away, then
/// queue a server refresh unless a recent one is cached.
fn fetch_profile(shared: &Arc<Shared>, session: Option<&Arc<Session>>, jid: String) {
    let Some(local) = shared.store(|s| s.local_profile(&jid)) else { return };
    let canonical = local.canonical.clone();
    let cached = shared.profiles.lock().unwrap_or_else(|e| e.into_inner()).get(&canonical).cloned();
    let fresh = cached.as_ref().is_some_and(|(at, _)| at.elapsed() < PROFILE_TTL);
    let mut profile = match cached {
        // Names and the picture may have changed since; the rest is from the server.
        Some((_, p)) => Profile { name: local.name.clone(), photo: local.avatar.clone(), ..p },
        None => local_profile(&local),
    };
    profile.jid = jid.clone();
    shared.emit(Event::Profile(profile));
    if fresh || is_hidden_chat(&canonical) {
        return;
    }
    if let Some(session) = session {
        session.queue_profile(&canonical, jid);
    }
}

/// A profile from what the store knows (no About / group details).
fn local_profile(local: &LocalProfile) -> Profile {
    Profile {
        jid: local.canonical.clone(),
        name: local.name.clone(),
        phone: if local.is_group { None } else { profile::display_phone(&local.canonical) },
        about: None,
        photo: local.avatar.clone(),
        is_group: local.is_group,
        description: None,
        participants: Vec::new(),
        created_at: None,
    }
}

impl Shared {
    /// Whether the cached profile picture of `canonical` is still fresh, and its file.
    fn cached_avatar(&self, canonical: &str) -> (bool, Option<PathBuf>) {
        let cached = self.store(|s| s.avatar(canonical)).flatten();
        let path = cached.as_ref().and_then(|a| a.path.clone()).filter(|p| p.is_file());
        let fresh = cached
            .as_ref()
            .is_some_and(|a| now_secs() - a.fetched_at < AVATAR_TTL_SECS && (a.path.is_none() || path.is_some()));
        (fresh, path)
    }

    /// Drop a cached profile (its picture or group details changed).
    fn forget_profile(&self, jid: &str) {
        if let Some(canonical) = self.store(|s| s.canonical(jid)) {
            self.profiles.lock().unwrap_or_else(|e| e.into_inner()).remove(&canonical);
        }
    }

    fn history_exhausted(&self, chat_jid: &str) -> bool {
        let Some(chat) = self.store(|s| s.canonical(chat_jid)) else { return true };
        self.history.lock().unwrap_or_else(|e| e.into_inner()).get(&chat).is_some_and(|h| h.exhausted)
    }

    fn finish_history_fetch(&self, chat: &str, seq: Option<u64>) {
        let anchor = {
            let mut map = self.history.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = map.get_mut(chat) else { return };
            match (&h.in_flight, seq) {
                (Some(a), Some(want)) if a.seq != want => return,
                (None, _) => return,
                _ => {}
            }
            h.in_flight.take()
        };
        let Some(anchor) = anchor else { return };
        // New: everything older than the anchor's second, and messages in that second
        // that weren't stored when asking.
        let page = self.store(|s| {
            let older = s.count_before(chat, anchor.ts)?;
            let boundary = s.messages_at(chat, anchor.ts, &anchor.known)?;
            let (mut messages, has_more) = if older > 0 {
                s.load_messages(chat, Some(anchor.ts), (older + 10).min(500))?
            } else {
                (Vec::new(), false)
            };
            messages.extend(boundary);
            Ok((messages, has_more))
        });
        let (messages, has_more) = page.unwrap_or_default();
        // A timeout (seq given) isn't proof the phone has nothing more; an empty answer is.
        let exhausted = seq.is_none() && messages.is_empty();
        if exhausted {
            self.history.lock().unwrap_or_else(|e| e.into_inner()).entry(chat.to_owned()).or_default().exhausted = true;
        }
        if !messages.is_empty() {
            self.emit(Event::Messages { chat_jid: chat.to_owned(), messages, has_more: has_more || !exhausted });
        }
        self.emit(Event::HistoryLoading { chat_jid: chat.to_owned(), loading: false, exhausted });
    }
}

fn emit_group_reads(shared: &Arc<Shared>, chat_jid: &str) {
    if let Some(reads) = shared.store(|s| s.group_reads(chat_jid)) {
        shared.emit(Event::GroupReads { chat_jid: chat_jid.to_owned(), reads });
    }
}

/// Send pending read receipts (of one chat, or all) and forget the ones the server took.
async fn send_receipts(shared: &Arc<Shared>, session: &Arc<Session>, chat_jid: Option<&str>) {
    let Some(pending) = shared.store(|s| s.pending_receipts(chat_jid)) else { return };
    let mut batches: HashMap<(String, String, Option<String>), Vec<String>> = HashMap::new();
    for (chat, r) in pending {
        batches.entry((chat, r.wire_chat, r.wire_sender)).or_default().push(r.id);
    }
    let mut acked = false;
    for ((canonical, wire_chat, sender), ids) in batches {
        let sent = match parse_jid(&wire_chat) {
            Some(chat) => {
                let sender = sender.as_deref().and_then(parse_jid);
                let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                match session.client.mark_as_read(&chat, sender.as_ref(), &refs).await {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!("sending read receipts (will retry): {e}");
                        false
                    }
                }
            }
            // Can never be sent.
            None => true,
        };
        if sent {
            shared.store(|s| s.ack_receipts(&canonical, &ids));
            acked = true;
        }
    }
    if acked {
        shared.mark_chats_dirty();
    }
}

/// Ask the primary phone for messages older than the oldest one stored for `chat_jid`.
/// The answer arrives as an ON_DEMAND history sync, handled in `handle_history_sync`.
fn request_older_history(shared: &Arc<Shared>, session: &Arc<Session>, chat_jid: &str) {
    let Some((chat, oldest, known)) = shared.store(|s| {
        let oldest = s.oldest_message(chat_jid)?;
        let known = match &oldest {
            Some(o) => s.ids_at(chat_jid, o.ts)?,
            None => HashSet::new(),
        };
        Ok((s.canonical(chat_jid)?, oldest, known))
    }) else {
        return;
    };
    let seq = HISTORY_REQ_SEQ.fetch_add(1, Ordering::Relaxed);
    {
        let mut map = shared.history.lock().unwrap_or_else(|e| e.into_inner());
        let h = map.entry(chat.clone()).or_default();
        if h.exhausted || h.in_flight.is_some() {
            return;
        }
        let Some(OldestRef { ts, .. }) = &oldest else {
            // Nothing stored to anchor the request on.
            h.exhausted = true;
            drop(map);
            shared.emit(Event::HistoryLoading { chat_jid: chat, loading: false, exhausted: true });
            return;
        };
        h.in_flight = Some(Anchor { ts: *ts, known, seq });
    }
    let Some(oldest) = oldest else { return };
    shared.emit(Event::HistoryLoading { chat_jid: chat.clone(), loading: true, exhausted: false });

    let client = session.client.clone();
    let shared = shared.clone();
    session.track(tokio::spawn(async move {
        let sent = match parse_jid(&oldest.wire_chat) {
            Some(jid) => client
                .fetch_message_history(&jid, &oldest.id, oldest.from_me, oldest.ts * 1000, ON_DEMAND_COUNT)
                .await
                .map_err(|e| e.to_string()),
            None => Err(format!("invalid chat JID {}", oldest.wire_chat)),
        };
        if let Err(e) = sent {
            log::warn!("on-demand history request failed: {e}");
            shared.finish_history_fetch(&chat, Some(seq));
            return;
        }
        log::info!("requested older history (anchor ts {})", oldest.ts);
        tokio::time::sleep(ON_DEMAND_TIMEOUT).await;
        shared.finish_history_fetch(&chat, Some(seq));
    }));
}

/// Notification for a newly stored incoming message, unless its chat is muted.
fn notification(s: &Store, msg: &NewMessage) -> Result<Option<Event>> {
    let Some((name, is_group, muted)) = s.chat_info(&msg.chat_jid, now_secs())? else { return Ok(None) };
    if muted {
        return Ok(None);
    }
    let text = s.with_mention_names(preview_text(&msg.body).into_owned())?;
    let (title, body) = if is_group {
        let sender = s.sender_display_name(&msg.sender_jid, msg.push_name.as_deref())?;
        (name, format!("{sender}: {text}"))
    } else {
        (name, text)
    };
    Ok(Some(Event::Notify { chat_jid: s.canonical(&msg.chat_jid)?, message_id: msg.id.clone(), title, body }))
}

/// Notification for someone else's reaction to one of my messages (WhatsApp shows
/// those), unless the chat is muted.
fn reaction_notification(s: &Store, r: &NewReaction, target: &ChatMessage) -> Result<Option<Event>> {
    if r.from_me || r.emoji.is_empty() || !target.from_me || s.is_revoked(&target.chat_jid, &target.id)? {
        return Ok(None);
    }
    let Some((name, _, muted)) = s.chat_info(&target.chat_jid, now_secs())? else { return Ok(None) };
    if muted {
        return Ok(None);
    }
    let sender = s.sender_display_name(&r.sender_jid, r.push_name.as_deref())?;
    let preview = preview_text(&target.body);
    let snippet: String = preview.chars().take(REACTION_SNIPPET_CHARS).collect();
    let ellipsis = if preview.chars().count() > REACTION_SNIPPET_CHARS { "…" } else { "" };
    Ok(Some(Event::Notify {
        chat_jid: target.chat_jid.clone(),
        message_id: target.id.clone(),
        title: name,
        body: format!("{sender} reacted {} to “{snippet}{ellipsis}”", r.emoji),
    }))
}

/// How much of the message a reaction notification quotes.
const REACTION_SNIPPET_CHARS: usize = 40;

/// `Command::React`: store my reaction right away, show it, then send it (in order with
/// the messages queued before it). A failed send puts the previous one back.
fn react(shared: &Arc<Shared>, session: Option<&Arc<Session>>, chat_jid: &str, id: &str, emoji: Option<String>) {
    let emoji = emoji.unwrap_or_default();
    let own = shared.own();
    let Some(own_jid) = own.pn.clone().or_else(|| own.lid.clone()) else {
        shared.emit(Event::Error("Couldn't react: not logged in".to_owned()));
        return;
    };
    let stored = shared.store(|s| {
        let Some(raw) = s.raw_message(chat_jid, id)? else { return Ok(None) };
        let Some((previous, ts_ms)) = s.set_own_reaction(&raw.chat_jid, &raw.id, &own_jid, &emoji, now_ms())? else {
            return Ok(None);
        };
        Ok(Some((raw, previous, ts_ms)))
    });
    // Not stored, or deleted for everyone: nothing to react to.
    let Some(Some((raw, previous, ts_ms))) = stored else { return };
    shared.emit_message(&raw.chat_jid, &raw.id);
    let seq = REACTION_SEQ.fetch_add(1, Ordering::Relaxed);
    shared.reactions_in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(
        seq,
        OptimisticReaction { chat_jid: raw.chat_jid.clone(), target_id: raw.id.clone(), own_jid, ts_ms, previous },
    );
    let job = ReactJob { seq, wire_chat: raw.wire_chat.clone(), key: reaction_key(&raw), emoji };
    queue_send(shared, session, SendJob::React(Box::new(job)));
}

/// How long after sending a message WhatsApp offers to edit it (WA Web's
/// `message_edit_client_entry_point_limit_seconds`; receivers take edits for 20 minutes).
const EDIT_WINDOW_SECS: i64 = 15 * 60;

const NOT_EDITABLE: &str = "This message can't be edited";
const EDIT_EXPIRED: &str = "This message can no longer be edited";

/// Why I can't edit `raw` (at `now`), if I can't: only my own sent (or sending) text
/// messages and captioned photos / videos / documents, within [`EDIT_WINDOW_SECS`].
fn edit_refusal(raw: &RawMessage, now: i64) -> Option<&'static str> {
    let captioned = |c: &Option<String>| c.as_deref().is_some_and(|c| !c.is_empty()) && raw.media.is_some();
    let editable_kind = match &raw.body {
        MessageBody::Text(_) => true,
        MessageBody::Image(m) => captioned(&m.caption),
        MessageBody::Video(m) => captioned(&m.caption),
        MessageBody::Document(m) => captioned(&m.caption),
        _ => false,
    };
    if !raw.from_me || raw.revoked || raw.status == DeliveryStatus::Failed || !editable_kind {
        Some(NOT_EDITABLE)
    } else if now - raw.timestamp > EDIT_WINDOW_SECS {
        Some(EDIT_EXPIRED)
    } else {
        None
    }
}

/// The new content of an edit of `raw` (WA's `editedMessage`): the text, or the photo /
/// video / document as sent with the new caption. `None` if it isn't editable.
fn edited_content(raw: &RawMessage, text: &str) -> Option<wa::Message> {
    match (&raw.body, &raw.media) {
        (MessageBody::Text(_), _) => Some(wa::Message::text(text.to_owned())),
        (MessageBody::Image(_) | MessageBody::Video(_) | MessageBody::Document(_), Some(media)) => {
            Some(media.to_message(Some(text)))
        }
        _ => None,
    }
}

/// Whether `raw` already shows `text`.
fn shows_text(raw: &RawMessage, text: &str) -> bool {
    match &raw.body {
        MessageBody::Text(t) => t == text,
        MessageBody::Image(m) => m.caption.as_deref() == Some(text.trim()),
        MessageBody::Video(m) => m.caption.as_deref() == Some(text.trim()),
        MessageBody::Document(m) => m.caption.as_deref() == Some(text.trim()),
        _ => false,
    }
}

/// `Command::EditMessage`: store and show the edit right away (as a newer edit, so
/// replays of the original can't undo it), then send it in order with the messages
/// queued before it. A failed send puts the previous content back.
fn edit_message(shared: &Arc<Shared>, session: Option<&Arc<Session>>, chat_jid: &str, id: &str, text: String) {
    if text.trim().is_empty() {
        shared.emit(Event::Error("Couldn't edit the message: it can't be empty".to_owned()));
        return;
    }
    let applied = shared.store(|s| {
        let Some(raw) = s.raw_message(chat_jid, id)? else { return Ok(Err(NOT_EDITABLE)) };
        if let Some(refusal) = edit_refusal(&raw, now_secs()) {
            return Ok(Err(refusal));
        }
        if shows_text(&raw, &text) {
            return Ok(Ok(None));
        }
        // Never older than the edit shown, or it wouldn't apply (e.g. one from my phone
        // with its clock ahead).
        let edit_ts = now_secs().max(raw.edit_ts.unwrap_or(0));
        match s.apply_edit(&raw.chat_jid, &raw.id, &MessageBody::Text(text.clone()), edit_ts)? {
            Some(msg) => Ok(Ok(Some((raw, msg, edit_ts)))),
            None => Ok(Err(NOT_EDITABLE)),
        }
    });
    let (raw, msg, applied_ts) = match applied {
        Some(Ok(Some(applied))) => applied,
        Some(Ok(None)) => return,
        Some(Err(refusal)) => {
            log::info!("not editing {id}: {refusal}");
            shared.emit(Event::Error(refusal.to_owned()));
            return;
        }
        // Store error (logged by `store`).
        None => return,
    };
    shared.emit(Event::MessageUpserted(msg));
    shared.mark_chats_dirty();
    let seq = EDIT_SEQ.fetch_add(1, Ordering::Relaxed);
    shared.edits_in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(
        seq,
        OptimisticEdit {
            chat_jid: raw.chat_jid.clone(),
            id: raw.id.clone(),
            applied_ts,
            previous: raw.body,
            previous_ts: raw.edit_ts,
        },
    );
    let job = EditJob { seq, chat_jid: raw.chat_jid, id: raw.id, text };
    queue_send(shared, session, SendJob::Edit(Box::new(job)));
}

/// The key of the message a reaction is for, as WhatsApp clients build it (Baileys
/// `react.key`, WA Web `MsgKey`): the chat as the server addressed it, `fromMe`, the id,
/// and in groups the author as they appeared on the wire. For my own group messages the
/// author depends on the group's addressing mode; `None` here, filled in when sending.
fn reaction_key(raw: &RawMessage) -> wa::MessageKey {
    let participant = if !is_group_jid(&raw.chat_jid) {
        None
    } else if raw.from_me {
        raw.wire_sender.as_deref().map(non_ad)
    } else {
        Some(raw.wire_sender.as_deref().map_or_else(|| raw.sender_jid.clone(), non_ad))
    };
    wa::MessageKey {
        remote_jid: Some(raw.wire_chat.clone()),
        from_me: Some(raw.from_me),
        id: Some(raw.id.clone()),
        participant,
    }
}

impl Shared {
    /// A reaction send ended: forget it, and if it failed undo it (unless my reaction
    /// changed again since) and show the message as it is now.
    fn finish_reaction(&self, seq: u64, sent: bool) {
        let Some(r) = self.reactions_in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&seq) else {
            return;
        };
        if sent {
            return;
        }
        let reverted = self.store(|s| {
            s.revert_own_reaction(&r.chat_jid, &r.target_id, &r.own_jid, r.ts_ms, r.previous.as_ref())
        });
        if reverted == Some(true) {
            self.emit_message(&r.chat_jid, &r.target_id);
        }
    }
}

impl Shared {
    /// An edit send ended: forget it, and if it failed put back what it replaced (unless
    /// the message changed again since) and show the message as it is now. A later edit
    /// of mine still on its way carries the content shown; it takes over what to put
    /// back should it fail too.
    fn finish_edit(&self, seq: u64, sent: bool) {
        let e = {
            let mut in_flight = self.edits_in_flight.lock().unwrap_or_else(|e| e.into_inner());
            let Some(e) = in_flight.remove(&seq) else { return };
            if sent {
                return;
            }
            let later = in_flight
                .iter_mut()
                .filter(|(s, l)| **s > seq && l.chat_jid == e.chat_jid && l.id == e.id)
                .min_by_key(|(s, _)| **s);
            if let Some((_, later)) = later {
                later.previous = e.previous;
                later.previous_ts = e.previous_ts;
                return;
            }
            e
        };
        let reverted = self.store(|s| s.revert_edit(&e.chat_jid, &e.id, e.applied_ts, &e.previous, e.previous_ts));
        if let Some(Some(msg)) = reverted {
            log::info!("edit of {} reverted", e.id);
            self.emit(Event::MessageUpserted(msg));
            self.mark_chats_dirty();
        }
    }
}

/// An edit to send. Its optimistic state is in [`Shared::edits_in_flight`].
struct EditJob {
    seq: u64,
    /// Canonical chat.
    chat_jid: String,
    id: String,
    text: String,
}

/// A reaction to send. Its optimistic state is in [`Shared::reactions_in_flight`].
struct ReactJob {
    seq: u64,
    wire_chat: String,
    key: wa::MessageKey,
    /// Empty = remove my reaction.
    emoji: String,
}

/// An outgoing message waiting in the session's send queue. Its `Pending` row is stored
/// (and shown) before it's queued.
enum SendJob {
    Text {
        chat_jid: String,
        canonical: String,
        id: String,
        text: String,
        reply_to: Option<String>,
        /// JIDs @mentioned in the text.
        mentions: Vec<String>,
    },
    Image {
        chat_jid: String,
        canonical: String,
        id: String,
        jpeg: Vec<u8>,
        /// Thumbnail, dimensions, caption; the upload fills in the rest.
        proto: Box<wa::message::ImageMessage>,
        reply_to: Option<String>,
    },
    Sticker {
        chat_jid: String,
        canonical: String,
        id: String,
        sticker: Box<StickerJob>,
        reply_to: Option<String>,
    },
    /// A copy of a stored message, marked forwarded.
    Forward { chat_jid: String, canonical: String, id: String, forward: Box<ForwardJob> },
    /// A file, video or voice note (see [`outgoing`]).
    Upload { chat_jid: String, canonical: String, id: String, upload: Box<outgoing::UploadJob> },
    /// Not a message: no status of its own (see [`Shared::finish_reaction`]).
    React(Box<ReactJob>),
    /// Not a message either (see [`Shared::finish_edit`]).
    Edit(Box<EditJob>),
}

/// A sticker to send: the panel entry and the message built from it.
struct StickerJob {
    candidate: Candidate,
    proto: wa::message::StickerMessage,
}

impl SendJob {
    /// The message a message send is for; `None` for reactions and edits.
    fn target(&self) -> Option<(&str, &str, &str)> {
        match self {
            SendJob::Text { chat_jid, canonical, id, .. }
            | SendJob::Image { chat_jid, canonical, id, .. }
            | SendJob::Sticker { chat_jid, canonical, id, .. }
            | SendJob::Forward { chat_jid, canonical, id, .. }
            | SendJob::Upload { chat_jid, canonical, id, .. } => Some((chat_jid, canonical, id)),
            SendJob::React(_) | SendJob::Edit(_) => None,
        }
    }
}

/// The caller's `client_id` if it's usable as a WhatsApp message id, else a fresh one.
fn message_id_from(client_id: String) -> String {
    let valid = (8..=64).contains(&client_id.len()) && client_id.chars().all(|c| c.is_ascii_alphanumeric());
    if valid { client_id } else { new_message_id() }
}

/// Store and show an own message as `Pending`. Returns the canonical chat JID.
fn insert_pending(
    shared: &Arc<Shared>,
    chat_jid: &str,
    id: &str,
    body: MessageBody,
    media: Option<(StoredMedia, Option<PathBuf>)>,
    reply_to: Option<&str>,
) -> Option<String> {
    let stored = shared.store(|s| {
        let quoted = match reply_to {
            Some(qid) => s.raw_message(chat_jid, qid)?.map(|q| NewQuote {
                id: q.id,
                sender: Some(q.sender_jid),
                from_me: q.from_me,
                preview: preview_text(&q.body).into_owned(),
            }),
            None => None,
        };
        let msg = NewMessage {
            chat_jid: chat_jid.to_owned(),
            id: id.to_owned(),
            sender_jid: shared.own_jid().unwrap_or_default(),
            wire_chat: chat_jid.to_owned(),
            wire_sender: None,
            push_name: None,
            from_me: true,
            body,
            timestamp: now_secs(),
            status: DeliveryStatus::Pending,
            media: media.as_ref().map(|(m, _)| m.clone()),
            quoted,
            expires_at: None,
        };
        s.insert_message(&msg, false)?;
        if let Some((_, Some(path))) = &media {
            s.set_media_path(chat_jid, id, Some(path))?;
        }
        s.set_unread(chat_jid, 0)?;
        Ok((s.canonical(chat_jid)?, s.get_message(chat_jid, id)?))
    });
    let (canonical, Some(stored)) = stored? else { return None };
    shared.emit(Event::MessageUpserted(stored));
    shared.mark_chats_dirty();
    Some(canonical)
}

/// A `Command::SendImage` to prepare.
struct PendingImage {
    chat_jid: String,
    canonical: String,
    id: String,
    path: PathBuf,
    caption: Option<String>,
    reply_to: Option<String>,
}

/// Prepare a photo (decode, orient, thumbnail, copy into the media dir), store it as
/// `Pending`, and build its send job. A photo that can't be read is stored as failed.
async fn prepare_image_send(shared: &Arc<Shared>, generation: u64, image: PendingImage) -> Option<SendJob> {
    let PendingImage { chat_jid, canonical, id, path, caption, reply_to } = image;
    let caption = caption.map(|c| c.trim().to_owned()).filter(|c| !c.is_empty());
    let dest = media_file(&shared.config.data_dir, &canonical, &id, "jpg");
    let prepared = {
        let dest = dest.clone();
        shared
            .blocking(generation, move |_| {
                let p = prepare_image_file(&path)?;
                write_atomic(&dest, &p.jpeg)?;
                Ok(p)
            })
            .await
    };
    let mut proto = wa::message::ImageMessage {
        mimetype: Some("image/jpeg".to_owned()),
        caption: caption.clone(),
        ..Default::default()
    };
    let body = |p: &wa::message::ImageMessage| {
        MessageBody::Image(ImageMedia {
            path: None,
            thumbnail: p.jpeg_thumbnail.clone(),
            width: p.width,
            height: p.height,
            caption: caption.clone(),
            state: MediaState::Idle,
        })
    };
    match prepared {
        Ok(p) => {
            proto.width = Some(p.width);
            proto.height = Some(p.height);
            proto.jpeg_thumbnail = Some(p.thumbnail);
            let proto = Box::new(proto);
            let media = Some((StoredMedia::Image(proto.clone()), Some(dest)));
            insert_pending(shared, &chat_jid, &id, body(&proto), media, reply_to.as_deref())?;
            Some(SendJob::Image { chat_jid, canonical, id, jpeg: p.jpeg, proto, reply_to })
        }
        Err(e) => {
            log::warn!("preparing photo: {e:#}");
            shared.emit(Event::Error(format!("Couldn't send the photo: {e:#}")));
            let media = StoredMedia::Image(Box::new(proto.clone()));
            let stored = shared.store(|s| {
                s.insert_message(
                    &NewMessage {
                        chat_jid: chat_jid.clone(),
                        id: id.clone(),
                        sender_jid: shared.own_jid().unwrap_or_default(),
                        wire_chat: chat_jid.clone(),
                        wire_sender: None,
                        push_name: None,
                        from_me: true,
                        body: body(&proto),
                        timestamp: now_secs(),
                        status: DeliveryStatus::Pending,
                        media: Some(media),
                        quoted: None,
                        expires_at: None,
                    },
                    false,
                )?;
                s.mark_failed(&chat_jid, &id)?;
                s.get_message(&chat_jid, &id)
            });
            if let Some(Some(msg)) = stored {
                shared.emit(Event::MessageUpserted(msg));
                shared.mark_chats_dirty();
            }
            None
        }
    }
}

/// Hand a send to the session's queue, or fail it right away when offline.
fn queue_send(shared: &Arc<Shared>, session: Option<&Arc<Session>>, job: SendJob) {
    match (session, job.target()) {
        // If the send loop is already gone (session ending), the teardown fails it.
        (Some(session), _) => {
            let _ = session.sends.send(job);
        }
        (None, Some((chat_jid, canonical, id))) => {
            let (chat_jid, canonical, id) = (chat_jid.to_owned(), canonical.to_owned(), id.to_owned());
            finish_send(shared, &chat_jid, canonical, id, Err(anyhow!("not connected")))
        }
        (None, None) => match job {
            SendJob::React(r) => {
                shared.finish_reaction(r.seq, false);
                shared.emit(Event::Error("Couldn't send the reaction: not connected".to_owned()));
            }
            SendJob::Edit(e) => {
                shared.finish_edit(e.seq, false);
                shared.emit(Event::Error("Couldn't edit the message: not connected".to_owned()));
            }
            _ => {}
        },
    }
}

/// Record a send's outcome and tell the UI.
fn finish_send(shared: &Arc<Shared>, chat_jid: &str, canonical: String, id: String, result: Result<()>) {
    let status = match result {
        Ok(()) => {
            shared.store(|s| s.upgrade_status(chat_jid, std::slice::from_ref(&id), DeliveryStatus::Sent));
            DeliveryStatus::Sent
        }
        Err(e) => {
            log::warn!("sending message failed: {e:#}");
            shared.store(|s| s.mark_failed(chat_jid, &id));
            DeliveryStatus::Failed
        }
    };
    // Re-read: a delivery receipt may already have overtaken the send result.
    let status = shared.store(|s| s.get_message(chat_jid, &id)).flatten().map_or(status, |m| m.status);
    shared.emit(Event::StatusChanged { chat_jid: canonical, message_ids: vec![id], status });
}

/// `contextInfo` quoting a stored message, the way WhatsApp clients build it: stanza id,
/// the quoted sender's JID, and the quoted content without its own context.
fn reply_context(shared: &Shared, chat_jid: &str, reply_to: &str) -> Option<wa::ContextInfo> {
    let raw: RawMessage = shared.store(|s| s.raw_message(chat_jid, reply_to)).flatten()?;
    let own = shared.own();
    let participant = if raw.from_me {
        let lid_addressed = is_lid_jid(&non_ad(&raw.wire_chat)) || raw.wire_sender.as_deref().is_some_and(|w| is_lid_jid(&non_ad(w)));
        if lid_addressed { own.lid.or(own.pn) } else { own.pn.or(own.lid) }
    } else if is_group_jid(&raw.chat_jid) {
        Some(raw.wire_sender.as_deref().map_or_else(|| raw.sender_jid.clone(), non_ad))
    } else {
        Some(non_ad(&raw.wire_chat))
    };
    let quoted = match (&raw.media, &raw.body) {
        (Some(m), MessageBody::Image(i)) => m.to_message(i.caption.as_deref()),
        (Some(m), MessageBody::Video(v)) => m.to_message(v.caption.as_deref()),
        (Some(m), MessageBody::Document(d)) => m.to_message(d.caption.as_deref()),
        (Some(m), MessageBody::Voice(_) | MessageBody::Sticker(_)) => m.to_message(None),
        (_, MessageBody::Text(t)) => wa::Message::text(t.clone()),
        (_, other) => wa::Message::text(preview_text(other).into_owned()),
    };
    Some(wa::ContextInfo {
        stanza_id: Some(raw.id),
        participant,
        quoted_message: MessageField::some(quoted),
        ..Default::default()
    })
}

/// Start downloading a message's media, unless it's on disk already or in flight.
fn download_media(shared: &Arc<Shared>, session: Option<&Arc<Session>>, chat_jid: &str, id: &str) {
    let Some(Some(raw)) = shared.store(|s| s.raw_message(chat_jid, id)) else { return };
    if matches!(raw.body, MessageBody::ViewOnce(_)) {
        // Never downloaded: view-once media is for the phone only.
        return;
    }
    if raw.media_path.is_some() {
        shared.emit_message(&raw.chat_jid, &raw.id);
        return;
    }
    let key = (raw.chat_jid.clone(), raw.id.clone());
    let session = match (session, &raw.media) {
        (Some(session), Some(_)) => session,
        _ => {
            // Offline, or stored before media metadata was kept (nothing to download with).
            log::info!("can't download media of {id}: {}", if raw.media.is_none() { "no media info" } else { "offline" });
            shared.store(|s| s.set_media_state(&raw.chat_jid, &raw.id, MediaState::Failed));
            shared.emit_message(&raw.chat_jid, &raw.id);
            return;
        }
    };
    if !shared.downloads.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
        return;
    }
    shared.store(|s| s.set_media_state(&raw.chat_jid, &raw.id, MediaState::Downloading));
    shared.emit_message(&raw.chat_jid, &raw.id);

    let task_session = session.clone();
    let task = tokio::spawn(async move {
        let session = task_session;
        let shared = session.shared.clone();
        let _guard = DownloadGuard { shared: shared.clone(), key };
        let result = match shared.download_slots.acquire().await {
            // Requests made while (re)connecting wait for the connection a little.
            Ok(_permit) if session.wait_connected(Some(DOWNLOAD_CONNECT_WAIT)).await => session.fetch_media(&raw).await,
            Ok(_permit) => Err(anyhow!("not connected")),
            Err(e) => Err(e.into()),
        };
        match result {
            Ok(path) => shared.store(|s| s.set_media_path(&raw.chat_jid, &raw.id, Some(&path))),
            Err(e) => {
                log::warn!("downloading media of {}: {e:#}", raw.id);
                shared.store(|s| s.set_media_state(&raw.chat_jid, &raw.id, MediaState::Failed))
            }
        };
        shared.emit_message(&raw.chat_jid, &raw.id);
    });
    session.track(task);
}

/// Reports a download's progress to the UI, a few times a second at most.
struct Progress {
    shared: Arc<Shared>,
    chat_jid: String,
    message_id: String,
    total: u64,
    last: Option<(std::time::Instant, f32)>,
}

impl Progress {
    fn new(shared: Arc<Shared>, raw: &RawMessage, total: u64) -> Self {
        Self { shared, chat_jid: raw.chat_jid.clone(), message_id: raw.id.clone(), total, last: None }
    }

    fn at(&mut self, written: u64) {
        let fraction = (written as f64 / self.total as f64).clamp(0., 1.) as f32;
        let due = self.last.is_none_or(|(when, shown)| {
            (fraction - shown).abs() >= 0.01 && when.elapsed() >= Duration::from_millis(120)
        });
        if due {
            self.last = Some((std::time::Instant::now(), fraction));
            let (chat_jid, message_id) = (self.chat_jid.clone(), self.message_id.clone());
            self.shared.emit(Event::DownloadProgress { chat_jid, message_id, fraction });
        }
    }
}

/// The download's file, counting what's been written (a retry rewinds it).
struct ProgressWriter {
    file: std::fs::File,
    progress: Progress,
}

impl std::io::Write for ProgressWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = std::io::Write::write(&mut self.file, buf)?;
        let at = std::io::Seek::stream_position(&mut self.file)?;
        self.progress.at(at);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

impl std::io::Seek for ProgressWriter {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        std::io::Seek::seek(&mut self.file, pos)
    }
}

impl whatsapp_rust::download::DownloadWriter for ProgressWriter {
    fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }
}

/// Drops a download from the in-flight set however its task ends.
struct DownloadGuard {
    shared: Arc<Shared>,
    key: (String, String),
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        self.shared.downloads.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.key);
    }
}

impl Session {
    /// Keep a detached task's handle for teardown (and forget finished ones).
    fn track(&self, handle: tokio::task::JoinHandle<()>) {
        let mut tasks = self.side_tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle);
    }

    async fn send_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<SendJob>) {
        while let Some(job) = rx.recv().await {
            let Some((chat_jid, canonical, id)) = job.target().map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned()))
            else {
                match job {
                    SendJob::React(r) => {
                        let result = self.send_reaction(&r).await;
                        if let Err(e) = &result {
                            log::warn!("sending reaction failed: {e:#}");
                            self.shared.emit(Event::Error(format!("Couldn't send the reaction: {e:#}")));
                        }
                        self.shared.finish_reaction(r.seq, result.is_ok());
                    }
                    SendJob::Edit(e) => {
                        let result = self.send_edit(&e).await;
                        match &result {
                            Ok(()) => log::info!("edit of {} sent", e.id),
                            Err(err) => {
                                log::warn!("sending edit of {} failed: {err:#}", e.id);
                                self.shared.emit(Event::Error(format!("Couldn't edit the message: {err:#}")));
                            }
                        }
                        self.shared.finish_edit(e.seq, result.is_ok());
                    }
                    _ => {}
                }
                continue;
            };
            let result = self.send_job(job).await;
            finish_send(&self.shared, &chat_jid, canonical, id, result);
        }
    }

    async fn send_job(&self, job: SendJob) -> Result<()> {
        match job {
            SendJob::Text { chat_jid, id, text, reply_to, mentions, .. } => {
                let mut context = reply_to.as_deref().and_then(|r| reply_context(&self.shared, &chat_jid, r));
                if !mentions.is_empty() {
                    context.get_or_insert_with(Default::default).mentioned_jid = mentions;
                }
                let message = match context {
                    Some(ctx) => wa::Message {
                        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                            text: Some(text),
                            context_info: MessageField::some(ctx),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None => wa::Message::text(text),
                };
                self.send(&chat_jid, message, id).await
            }
            SendJob::Image { chat_jid, id, jpeg, mut proto, reply_to, .. } => {
                let upload = self.client.upload(jpeg, MediaType::Image, UploadOptions::new());
                let up = tokio::time::timeout(UPLOAD_TIMEOUT, upload).await.context("upload timed out")??;
                proto.url = Some(up.url);
                proto.direct_path = Some(up.direct_path);
                proto.media_key = Some(up.media_key.to_vec());
                proto.file_enc_sha256 = Some(up.file_enc_sha256.to_vec());
                proto.file_sha256 = Some(up.file_sha256.to_vec());
                proto.file_length = Some(up.file_length);
                proto.media_key_timestamp = Some(up.media_key_timestamp);
                // Keep what's needed to re-download / quote it later.
                self.shared.store(|s| s.set_media(&chat_jid, &id, &StoredMedia::Image(proto.clone())));
                if let Some(ctx) = reply_to.as_deref().and_then(|r| reply_context(&self.shared, &chat_jid, r)) {
                    proto.context_info = MessageField::some(ctx);
                }
                let message = wa::Message { image_message: MessageField::some(*proto), ..Default::default() };
                self.send(&chat_jid, message, id).await
            }
            SendJob::Sticker { chat_jid, id, sticker, reply_to, .. } => {
                let StickerJob { candidate, mut proto } = *sticker;
                let how = self.prepare_sticker(&candidate, &mut proto).await?;
                log::info!("stickers: sending {} ({:?}) {how}", candidate.id, candidate.source);
                // Keep what's needed to re-download / quote it later; a fresh link also
                // makes this the sticker's best recent.
                self.shared.store(|s| s.set_media(&chat_jid, &id, &StoredMedia::Sticker(Box::new(proto.clone()))));
                self.shared.stickers_dirty.notify_one();
                if let Some(ctx) = reply_to.as_deref().and_then(|r| reply_context(&self.shared, &chat_jid, r)) {
                    proto.context_info = MessageField::some(ctx);
                }
                let message = wa::Message { sticker_message: MessageField::some(proto), ..Default::default() };
                self.send(&chat_jid, message, id).await
            }
            SendJob::Forward { chat_jid, id, forward, .. } => {
                let message = self.forward_content(&chat_jid, &id, *forward).await?;
                self.send(&chat_jid, message, id).await
            }
            SendJob::Upload { chat_jid, id, upload, .. } => self.send_upload(&chat_jid, id, *upload).await,
            SendJob::React(_) => unreachable!("reactions are sent by `send_reaction`"),
            SendJob::Edit(_) => unreachable!("edits are sent by `send_edit`"),
        }
    }

    /// Send a reaction (`reactionMessage` with the target key, the emoji or "" to remove,
    /// and `senderTimestampMs`; the library encrypts it for community announcement groups).
    async fn send_reaction(&self, r: &ReactJob) -> Result<()> {
        let chat = parse_jid(&r.wire_chat).with_context(|| format!("invalid chat JID {}", r.wire_chat))?;
        let mut key = r.key.clone();
        if key.participant.is_none() && is_group_jid(&r.wire_chat) {
            // My own group message: I'm its author under the group's addressing mode.
            use whatsapp_rust::wacore::types::message::AddressingMode;
            let routing = self.client.groups().query_info(&chat).await.map_err(|e| anyhow!("{e}"))?;
            let own = self.client.persistence_manager().get_device_snapshot();
            let me = match routing.addressing_mode {
                AddressingMode::Lid => own.lid.as_ref().or(own.pn.as_ref()),
                _ => own.pn.as_ref(),
            };
            key.participant = Some(me.context("not logged in")?.to_non_ad_string());
        }
        let send = self.client.send_reaction(chat, key, &r.emoji);
        match tokio::time::timeout(SEND_TIMEOUT, send).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => bail!("timed out"),
        }
    }

    /// Send an edit (`protocolMessage` MESSAGE_EDIT with the original's key and the new
    /// content; the library adds my author JID in groups). Built from the message as
    /// stored now, so a photo sent just before has its upload details. The original went
    /// first (the queue is ordered); one that failed isn't edited.
    async fn send_edit(&self, e: &EditJob) -> Result<()> {
        let raw = self.shared.store(|s| s.raw_message(&e.chat_jid, &e.id)).flatten().context("message not found")?;
        if raw.revoked {
            bail!("the message was deleted");
        }
        if raw.status == DeliveryStatus::Failed {
            bail!("the message wasn't sent");
        }
        let content = edited_content(&raw, &e.text).context("message can't be edited")?;
        let chat = parse_jid(&raw.wire_chat).with_context(|| format!("invalid chat JID {}", raw.wire_chat))?;
        log::info!("sending edit of {}", e.id);
        let send = self.client.edit_message(chat, raw.id.clone(), content);
        match tokio::time::timeout(SEND_TIMEOUT, send).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => bail!("timed out"),
        }
    }

    async fn send(&self, chat_jid: &str, message: wa::Message, id: String) -> Result<()> {
        let to = parse_jid(chat_jid).with_context(|| format!("invalid chat JID {chat_jid}"))?;
        let send = self.client.send_message_with_options(to, message, SendOptions::default().with_message_id(id));
        match tokio::time::timeout(SEND_TIMEOUT, send).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => bail!("timed out"),
        }
    }

    /// Download a message's media and make it usable: audio decoded to WAV, stickers
    /// converted to PNG, documents saved under their own (sanitized, deduplicated) name.
    /// Returns the file to show / play / open. Re-uses files already on disk.
    async fn fetch_media(&self, raw: &RawMessage) -> Result<PathBuf> {
        let mut media = raw.media.clone().context("no media info")?;
        let data_dir = &self.shared.config.data_dir;
        let file = |default_ext: &'static str| {
            media_file(data_dir, &raw.chat_jid, &raw.id, ext_for_mime(media.mimetype(), default_ext))
        };
        match media.kind() {
            MediaKind::Image | MediaKind::Video | MediaKind::Ptv => {
                let file = file(if media.kind() == MediaKind::Image { "jpg" } else { "mp4" });
                if !file.is_file() {
                    self.download_to_file(raw, &mut media, &file).await?;
                }
                Ok(file)
            }
            MediaKind::Audio => {
                let file = file("ogg");
                let target = file.with_extension("wav");
                if target.is_file() {
                    return Ok(target);
                }
                if !file.is_file() {
                    self.download_to_file(raw, &mut media, &file).await?;
                }
                let (src, dst) = (file, target.clone());
                self.shared.blocking(self.generation, move |_| crate::audio::to_wav(&src, &dst)).await?;
                Ok(target)
            }
            MediaKind::Document => {
                let StoredMedia::Document(doc) = &media else { unreachable!() };
                let name = safe_file_name(&document_name(doc), doc.mimetype.as_deref());
                let dir = documents_dir(data_dir, &raw.chat_jid);
                // The stored `media_path` makes this idempotent; the name is reserved
                // so two documents called the same don't overwrite each other.
                let dest = self.shared.blocking(self.generation, move |_| reserve_unique(&dir, &name)).await?;
                // The reserved (empty) file goes away unless the download fills it, also
                // if this task is cancelled.
                let dest = TempFile::new(dest);
                self.download_to_file(raw, &mut media, dest.path()).await?;
                Ok(dest.keep())
            }
            MediaKind::Sticker => {
                // Animated stickers keep their original WebP (GPUI plays animated WebP);
                // still ones are stored as PNG.
                let file = media_file(data_dir, &raw.chat_jid, &raw.id, "png");
                let webp = media_file(data_dir, &raw.chat_jid, &raw.id, "webp");
                if webp.is_file() {
                    return Ok(webp);
                }
                let known_animated = !media.is_lottie()
                    && matches!(&media, StoredMedia::Sticker(p) if p.is_animated == Some(true));
                // A PNG from before animated stickers were kept is replaced by the WebP.
                if file.is_file() && !known_animated {
                    return Ok(file);
                }
                let StoredMedia::Sticker(proto) = &media else { unreachable!() };
                let data = if media.is_lottie() {
                    // A zipped Lottie animation: only its PNG preview is showable.
                    proto.png_thumbnail.clone().filter(|t| !t.is_empty()).context("Lottie sticker without preview")?
                } else {
                    self.download_bytes(raw, &mut media).await?
                };
                let (dest, dest_webp) = (file.clone(), webp.clone());
                let sticker = self
                    .shared
                    .blocking(self.generation, move |_| {
                        let sticker = sticker_to_png(&data)?;
                        if sticker.animated {
                            write_atomic(&dest_webp, &data)?;
                            let _ = std::fs::remove_file(&dest);
                        } else {
                            write_atomic(&dest, &sticker.png)?;
                        }
                        Ok(sticker)
                    })
                    .await?;
                // Remember what the file showed us (senders don't always say).
                if let StoredMedia::Sticker(proto) = &mut media {
                    let animated = sticker.animated || proto.is_animated == Some(true);
                    let changed = proto.is_animated != Some(animated) || proto.width.is_none_or(|w| w == 0);
                    proto.is_animated = Some(animated);
                    proto.width = proto.width.filter(|w| *w > 0).or(Some(sticker.width));
                    proto.height = proto.height.filter(|h| *h > 0).or(Some(sticker.height));
                    if changed {
                        self.shared.store(|s| s.set_media(&raw.chat_jid, &raw.id, &media));
                    }
                }
                Ok(if sticker.animated { webp } else { file })
            }
        }
    }

    /// Stream the media into `dest` (via a temporary file, never all in memory), asking
    /// the sender's phone for a re-upload if it expired on the CDN. The temporary file is
    /// removed on failure or cancellation.
    async fn download_to_file(&self, raw: &RawMessage, media: &mut StoredMedia, dest: &Path) -> Result<()> {
        let mut tmp = TempFile::new(temp_path(dest));
        match self.stream_to(raw, media, tmp.path()).await {
            // 403 too: history-synced media carries signed links that expire; the CDN
            // rejects them with 403 and only a re-upload from the phone brings them back.
            Err(e) if matches!(e.http_status(), Some(403 | 404 | 410)) => {
                log::info!("media of {} expired ({e}); requesting a re-upload", raw.id);
                self.refresh_media(raw, media).await?;
                // A fresh file: the failed attempt's writer may not be done with its own.
                tmp = TempFile::new(temp_path(dest));
                self.stream_to(raw, media, tmp.path()).await?;
            }
            other => other?,
        }
        std::fs::rename(tmp.path(), dest).with_context(|| format!("renaming to {}", dest.display()))?;
        tmp.keep();
        Ok(())
    }

    async fn stream_to(&self, raw: &RawMessage, media: &StoredMedia, part: &Path) -> Result<()> {
        let file = private_file(part)?;
        let file = match media.file_length().filter(|n| *n > 0) {
            Some(total) => {
                let progress = Progress::new(self.shared.clone(), raw, total);
                self.client.download_to_writer(media.downloadable(), ProgressWriter { file, progress }).await?.file
            }
            None => self.client.download_to_writer(media.downloadable(), file).await?,
        };
        file.sync_all()?;
        Ok(())
    }

    /// Small media (stickers) into memory, with the same re-upload handling.
    async fn download_bytes(&self, raw: &RawMessage, media: &mut StoredMedia) -> Result<Vec<u8>> {
        match self.client.download(media.downloadable()).await {
            // 403 too: history-synced media carries signed links that expire; the CDN
            // rejects them with 403 and only a re-upload from the phone brings them back.
            Err(e) if matches!(e.http_status(), Some(403 | 404 | 410)) => {
                log::info!("media of {} expired ({e}); requesting a re-upload", raw.id);
                self.refresh_media(raw, media).await?;
                self.client.download(media.downloadable()).await
            }
            other => other,
        }
    }

    /// Get a fresh direct path from the sender's phone and remember it.
    async fn refresh_media(&self, raw: &RawMessage, media: &mut StoredMedia) -> Result<()> {
        let direct_path = self.request_reupload(raw, media).await?;
        media.set_direct_path(direct_path);
        self.shared.store(|s| s.set_media(&raw.chat_jid, &raw.id, media));
        Ok(())
    }

    async fn request_reupload(&self, raw: &RawMessage, media: &StoredMedia) -> Result<String> {
        let chat = parse_jid(&raw.wire_chat).with_context(|| format!("invalid chat JID {}", raw.wire_chat))?;
        let participant = raw.wire_sender.as_deref().and_then(parse_jid);
        let request = media_retry::Request {
            msg_id: &raw.id,
            chat: &chat,
            participant: participant.as_ref(),
            from_me: raw.from_me,
            media_key: media.media_key().context("no media key")?,
        };
        match media_retry::request(&self.client, &request).await? {
            media_retry::Outcome::Success(direct_path) => Ok(direct_path),
            other => bail!("media re-upload failed: {}", other.code()),
        }
    }

    /// Queue a profile picture fetch for `canonical`, answering `AvatarUpdated` for
    /// `asked` (the JID as the UI asked for it). Deduplicated while in flight.
    fn queue_avatar_for(&self, canonical: &str, asked: String) {
        let first = {
            let mut inflight = self.shared.avatars.lock().unwrap_or_else(|e| e.into_inner());
            let waiting = inflight.entry(canonical.to_owned()).or_default();
            let first = waiting.is_empty();
            waiting.insert(asked);
            first
        };
        if first {
            let _ = self.avatar_queue.send(canonical.to_owned());
        }
    }

    /// Queue a profile fetch for `canonical`, answering `Profile` for `asked`.
    /// Deduplicated while in flight.
    fn queue_profile(&self, canonical: &str, asked: String) {
        let first = {
            let mut inflight = self.shared.profiles_in_flight.lock().unwrap_or_else(|e| e.into_inner());
            let waiting = inflight.entry(canonical.to_owned()).or_default();
            let first = waiting.is_empty();
            waiting.insert(asked);
            first
        };
        if first {
            let _ = self.profile_queue.send(canonical.to_owned());
        }
    }

    fn queue_avatar(&self, jid: &str) {
        if let Some(canonical) = self.shared.store(|s| s.canonical(jid)) {
            self.queue_avatar_for(&canonical, canonical.clone());
        }
    }

    /// Wait until the client is connected (`max: None` = as long as the session lives).
    async fn wait_connected(&self, max: Option<Duration>) -> bool {
        let start = tokio::time::Instant::now();
        while !self.client.is_connected() {
            if max.is_some_and(|max| start.elapsed() >= max) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        true
    }

    /// Profile pictures and profiles for the UI, one server lookup at a time so that a
    /// burst of them can't flood the server. Profiles (an open panel) go first.
    async fn lookup_loop(
        self: Arc<Self>,
        mut avatars: mpsc::UnboundedReceiver<String>,
        mut profiles: mpsc::UnboundedReceiver<String>,
    ) {
        loop {
            let (jid, is_profile) = tokio::select! {
                biased;
                Some(jid) = profiles.recv() => (jid, true),
                Some(jid) = avatars.recv() => (jid, false),
                else => break,
            };
            // The UI asks for pictures as soon as the stored chat list shows, usually
            // before we're online; hold the queue until we are.
            self.wait_connected(None).await;
            if is_profile {
                let profile = self.load_profile(&jid).await;
                let waiting = self.shared.profiles_in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&jid);
                for asked in waiting.unwrap_or_default() {
                    self.shared.emit(Event::Profile(Profile { jid: asked, ..profile.clone() }));
                }
            } else {
                self.refresh_avatar(&jid).await;
            }
            tokio::time::sleep(AVATAR_INTERVAL).await;
        }
    }

    /// Fetch the profile picture of canonical `jid` and tell whoever asked for it.
    async fn refresh_avatar(&self, jid: &str) -> Option<PathBuf> {
        let before = self.shared.store(|s| s.avatar(jid)).flatten().and_then(|a| a.path);
        let path = self.fetch_avatar(jid).await;
        let waiting = self.shared.avatars.lock().unwrap_or_else(|e| e.into_inner()).remove(jid);
        for asked in waiting.unwrap_or_default() {
            self.shared.emit(Event::AvatarUpdated { jid: asked, path: path.clone() });
        }
        // `ChatSummary.avatar` only changes when a picture appears or goes away
        // (a new picture overwrites the same file).
        if path != before {
            self.shared.mark_chats_dirty();
        }
        path
    }

    /// Fresh profile of canonical `jid`: the store's view plus whatever the server
    /// answers. Failures (e.g. 403 for a group we left) keep the stored parts. Cached
    /// for [`PROFILE_TTL`] when everything was fetched.
    async fn load_profile(&self, jid: &str) -> Profile {
        let shared = &self.shared;
        let local = shared.store(|s| s.local_profile(jid)).unwrap_or_else(|| LocalProfile {
            canonical: jid.to_owned(),
            name: fallback_name(jid),
            is_group: is_group_jid(jid),
            avatar: None,
        });
        let mut profile = local_profile(&local);
        // The avatar is already the full-size picture; re-fetch it only when stale.
        let (fresh, cached) = shared.cached_avatar(jid);
        profile.photo = if fresh { cached } else { self.refresh_avatar(jid).await };
        let Some(target) = parse_jid(jid) else { return profile };
        let complete = if local.is_group {
            match self.client.groups().get_metadata(&target).await {
                Ok(meta) => {
                    self.apply_group_metadata(&mut profile, meta);
                    true
                }
                Err(e) => {
                    log::info!("group info of {jid}: {e}");
                    false
                }
            }
        } else {
            match self.client.contacts().get_user_info(std::slice::from_ref(&target)).await {
                Ok(info) => {
                    // Hidden by privacy settings: an error for the status, or nothing.
                    profile.about = info
                        .into_values()
                        .next()
                        .filter(|i| i.status_error.is_none())
                        .and_then(|i| i.status)
                        .filter(|s| !s.trim().is_empty());
                    true
                }
                Err(e) => {
                    log::info!("about of {jid}: {e}");
                    false
                }
            }
        };
        log::info!(
            "profile of {jid} fetched{} (about: {}, participants: {}, photo: {})",
            if complete { "" } else { " partially" },
            profile.about.is_some(),
            profile.participants.len(),
            profile.photo.is_some(),
        );
        if complete {
            let entry = (tokio::time::Instant::now(), profile.clone());
            shared.profiles.lock().unwrap_or_else(|e| e.into_inner()).insert(jid.to_owned(), entry);
        }
        profile
    }

    fn apply_group_metadata(&self, profile: &mut Profile, meta: whatsapp_rust::GroupMetadata) {
        let shared = &self.shared;
        if !meta.subject.is_empty() && meta.subject != profile.name {
            shared.store(|s| s.set_chat_name(&profile.jid, &meta.subject));
            shared.mark_chats_dirty();
            profile.name = meta.subject.clone();
        }
        profile.description = meta.description.filter(|d| !d.trim().is_empty());
        profile.created_at = meta.creation_time.map(|t| t as i64);
        let own = shared.own();
        let members: Vec<(String, Option<String>, Option<String>, bool)> = meta
            .participants
            .iter()
            .map(|m| {
                let pn = m.phone_number.as_ref().map(Jid::to_non_ad_string);
                let lid = m.lid.as_ref().map(Jid::to_non_ad_string);
                (m.jid.to_non_ad_string(), pn, lid, m.is_admin())
            })
            .collect();
        let mut participants = shared
            .store(|s| {
                members
                    .iter()
                    .map(|(jid, pn, lid, is_admin)| {
                        let (canonical, name) = s.member(jid, pn.as_deref())?;
                        let is_me = [Some(jid), pn.as_ref(), lid.as_ref()].into_iter().flatten().any(|j| own.is_me(j));
                        Ok(Participant { jid: canonical, name, is_admin: *is_admin, is_me })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap_or_default();
        profile::sort_participants(&mut participants);
        profile.participants = participants;
    }

    /// Fetch `jid`'s profile picture into the avatar cache. Errors keep the cached one.
    async fn fetch_avatar(&self, jid: &str) -> Option<PathBuf> {
        let shared = &self.shared;
        let cached = shared.store(|s| s.avatar(jid)).flatten();
        let cached_path = cached.as_ref().and_then(|a| a.path.clone()).filter(|p| p.is_file());
        let target = parse_jid(jid)?;
        match self.client.contacts().get_profile_picture(&target, false).await {
            Ok(None) => {
                // No picture, or hidden from us by privacy settings.
                let _ = std::fs::remove_file(avatar_file(&shared.config.data_dir, jid));
                shared.store(|s| s.set_avatar(jid, None, None, now_secs()));
                None
            }
            Ok(Some(pic)) => {
                if let Some(path) = &cached_path
                    && cached.as_ref().and_then(|c| c.picture_id.as_deref()) == Some(pic.id.as_str())
                {
                    shared.store(|s| s.set_avatar(jid, Some(&pic.id), Some(path), now_secs()));
                    return cached_path;
                }
                match self.download_avatar(jid, &pic.url).await {
                    Ok(path) => {
                        shared.store(|s| s.set_avatar(jid, Some(&pic.id), Some(&path), now_secs()));
                        Some(path)
                    }
                    Err(e) => {
                        log::debug!("downloading profile picture of {jid}: {e:#}");
                        cached_path
                    }
                }
            }
            Err(e) => {
                log::debug!("profile picture of {jid}: {e}");
                cached_path
            }
        }
    }

    async fn download_avatar(&self, jid: &str, url: &str) -> Result<PathBuf> {
        let response = self.client.http_client.execute(HttpRequest::get(url)).await?;
        if response.status_code != 200 {
            bail!("HTTP {}", response.status_code);
        }
        let path = avatar_file(&self.shared.config.data_dir, jid);
        let dest = path.clone();
        self.shared.blocking(self.generation, move |_| write_atomic(&dest, &response.body)).await?;
        Ok(path)
    }
}

/// `Command::SendSticker`: store and show the sticker as `Pending` (with a copy of its
/// preview as the message's own file), then queue it.
async fn send_sticker(
    shared: &Arc<Shared>,
    session: Option<&Arc<Session>>,
    chat_jid: String,
    sticker_id: String,
    client_id: String,
    reply_to: Option<String>,
) {
    let Some(candidate) = shared.sticker_candidates().into_iter().find(|c| c.id == sticker_id) else {
        shared.emit(Event::Error("Couldn't send the sticker: it's no longer in your stickers".to_owned()));
        return;
    };
    let id = message_id_from(client_id);
    let canonical = shared.store(|s| s.canonical(&chat_jid)).unwrap_or_else(|| chat_jid.clone());
    let generation = session.map_or_else(|| shared.generation(), |s| s.generation);
    // The message gets its own file: deleting / expiring it removes that file.
    let path = match shared.sticker_preview(&candidate) {
        Some(src) => {
            let dest = media_file(&shared.config.data_dir, &canonical, &id, "png");
            shared
                .blocking(generation, move |_| {
                    write_atomic(&dest, &std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?)?;
                    Ok(dest)
                })
                .await
                .inspect_err(|e| log::warn!("copying sticker preview: {e:#}"))
                .ok()
        }
        None => None,
    };
    let proto = stickers::outgoing_sticker(&candidate.proto, candidate.animated.unwrap_or(false));
    let media = StoredMedia::Sticker(Box::new(proto.clone()));
    let body = media.body(path.clone(), MediaState::Idle);
    let Some(canonical) = insert_pending(shared, &chat_jid, &id, body, Some((media, path)), reply_to.as_deref())
    else {
        return;
    };
    shared.stickers_dirty.notify_one();
    let sticker = Box::new(StickerJob { candidate, proto });
    queue_send(shared, session, SendJob::Sticker { chat_jid, canonical, id, sticker, reply_to });
}

const NOT_RESENDABLE: &str = "This message can't be resent";
const PHOTO_GONE: &str = "The photo is no longer available";

/// How to send a failed message again (see [`retry_plan`]).
#[derive(Debug, PartialEq)]
enum RetryPlan {
    Text { text: String, reply_to: Option<String> },
    /// The photo as prepared for the first send (our JPEG copy in the media dir).
    Image { file: PathBuf, proto: Box<wa::message::ImageMessage>, reply_to: Option<String> },
    /// `sticker_id`: hex SHA-256 of the sticker file.
    Sticker { sticker_id: String, proto: Box<wa::message::StickerMessage>, reply_to: Option<String> },
    /// A file, video or voice note, from our copy of it.
    Upload(Box<outgoing::UploadJob>),
}

/// Why a message isn't resent: `Ignore` (not a failed send of mine; logged only) or
/// `Refuse` (a failed send that can't be redone; told to the user).
#[derive(Debug, PartialEq)]
enum RetryRefusal {
    Ignore(&'static str),
    Refuse(&'static str),
}

/// What resending `raw` takes: only my own failed, unrevoked texts, photos (whose local
/// copy is still there) and stickers.
fn retry_plan(raw: &RawMessage) -> Result<RetryPlan, RetryRefusal> {
    if !raw.from_me {
        return Err(RetryRefusal::Ignore("not mine"));
    }
    if raw.revoked {
        return Err(RetryRefusal::Ignore("revoked"));
    }
    if raw.status != DeliveryStatus::Failed {
        return Err(RetryRefusal::Ignore("not failed"));
    }
    let reply_to = raw.quoted_id.clone();
    if let Some(upload) = outgoing::retry_upload(raw)? {
        return Ok(RetryPlan::Upload(Box::new(upload)));
    }
    match (&raw.body, &raw.media) {
        (MessageBody::Text(text), _) => Ok(RetryPlan::Text { text: text.clone(), reply_to }),
        (MessageBody::Image(image), Some(StoredMedia::Image(stored))) => {
            let file = raw.media_path.clone().ok_or(RetryRefusal::Refuse(PHOTO_GONE))?;
            // As first sent; the upload replaces any link from an earlier attempt.
            let mut proto = stored.clone();
            proto.caption = image.caption.clone();
            proto.context_info = MessageField::none();
            Ok(RetryPlan::Image { file, proto, reply_to })
        }
        (MessageBody::Image(_), _) => Err(RetryRefusal::Refuse(PHOTO_GONE)),
        (MessageBody::Sticker(_), Some(StoredMedia::Sticker(stored))) => {
            let sha = stored.file_sha256.as_deref().filter(|h| h.len() == 32).ok_or(RetryRefusal::Refuse(NOT_RESENDABLE))?;
            let mut proto = stored.clone();
            proto.context_info = MessageField::none();
            Ok(RetryPlan::Sticker { sticker_id: stickers::hex(sha), proto, reply_to })
        }
        _ => Err(RetryRefusal::Refuse(NOT_RESENDABLE)),
    }
}

/// `Command::RetrySend`: rebuild a failed send from the store and queue it again under
/// the same id (the server drops a duplicate if the first attempt got through after all).
async fn retry_send(shared: &Arc<Shared>, session: Option<&Arc<Session>>, chat_jid: &str, id: &str) {
    let Some(Some(raw)) = shared.store(|s| s.raw_message(chat_jid, id)) else {
        log::debug!("not resending {id}: not stored");
        return;
    };
    let plan = match retry_plan(&raw) {
        Ok(plan) => plan,
        Err(RetryRefusal::Ignore(why)) => {
            log::debug!("not resending {id}: {why}");
            return;
        }
        Err(RetryRefusal::Refuse(reason)) => {
            log::info!("not resending {id}: {reason}");
            shared.emit(Event::Error(reason.to_owned()));
            return;
        }
    };
    // Addressed as the first attempt was (same remote JID for the same id).
    let (wire, canonical, id) = (raw.wire_chat.clone(), raw.chat_jid.clone(), raw.id.clone());
    let (kind, job) = match plan {
        RetryPlan::Text { text, reply_to } => {
            // Mentions aren't stored; the `@<digits>` in the text say who they were.
            let mentions = shared.store(|s| s.resolve_mentions(&text)).unwrap_or_default();
            let mentions = mentions.into_iter().map(|m| m.jid).collect();
            let job = SendJob::Text { chat_jid: wire, canonical: canonical.clone(), id: id.clone(), text, reply_to, mentions };
            ("text", job)
        }
        RetryPlan::Upload(upload) => {
            ("file", SendJob::Upload { chat_jid: wire, canonical: canonical.clone(), id: id.clone(), upload })
        }
        RetryPlan::Image { file, proto, reply_to } => {
            let generation = session.map_or_else(|| shared.generation(), |s| s.generation);
            let read = shared
                .blocking(generation, move |_| std::fs::read(&file).with_context(|| format!("reading {}", file.display())))
                .await;
            let jpeg = match read {
                Ok(jpeg) => jpeg,
                Err(e) => {
                    log::info!("not resending {id}: {e:#}");
                    shared.emit(Event::Error(PHOTO_GONE.to_owned()));
                    return;
                }
            };
            let job = SendJob::Image { chat_jid: wire, canonical: canonical.clone(), id: id.clone(), jpeg, proto, reply_to };
            ("photo", job)
        }
        RetryPlan::Sticker { sticker_id, proto, reply_to } => {
            let candidate = retry_sticker_candidate(shared, &raw, sticker_id, &proto);
            let sticker = Box::new(StickerJob { candidate, proto: *proto });
            ("sticker", SendJob::Sticker { chat_jid: wire, canonical: canonical.clone(), id: id.clone(), sticker, reply_to })
        }
    };
    // No await from here to the queue: edits / reactions to it (refused while it's
    // failed) can only be queued after it.
    let reset = shared.store(|s| {
        if !s.reset_failed(&canonical, &id, now_secs())? {
            return Ok(None);
        }
        // A resend goes to the end of the conversation, like a new message.
        s.restamp(&canonical, &id, now_secs())?;
        s.get_message(&canonical, &id)
    });
    let Some(Some(msg)) = reset else {
        log::debug!("not resending {id}: no longer failed");
        return;
    };
    log::info!("retrying send of {id} ({kind})");
    shared.emit(Event::MessageUpserted(msg));
    shared.mark_chats_dirty();
    queue_send(shared, session, job);
}

/// The sticker to resend `raw` as: its panel entry (for its anchor, whose sender's phone
/// can re-upload an expired file) with the stored message's proto, else one made from it.
fn retry_sticker_candidate(
    shared: &Shared,
    raw: &RawMessage,
    sticker_id: String,
    proto: &wa::message::StickerMessage,
) -> Candidate {
    let panel = shared.sticker_candidates().into_iter().find(|c| c.id == sticker_id);
    let mut candidate = panel.unwrap_or_else(|| Candidate {
        id: sticker_id,
        source: StickerSource::Recent,
        proto: proto.clone(),
        at: raw.timestamp.saturating_mul(1000),
        animated: proto.is_animated,
        anchor: None,
        message_png: raw.media_path.clone(),
    });
    // This message never reached anyone: no phone can re-upload it.
    if candidate.anchor.as_ref().is_some_and(|(chat, id)| *chat == raw.chat_jid && *id == raw.id) {
        candidate.anchor = None;
    }
    candidate.proto = proto.clone();
    candidate
}

impl Shared {
    /// The sticker panel's entries, from the store (favorites, then my recent sends).
    fn sticker_candidates(&self) -> Vec<Candidate> {
        let data = self.store(|s| {
            Ok((
                s.favorite_stickers()?,
                s.sticker_messages(true, stickers::RECENT_SCAN)?,
                s.removed_recent_stickers()?,
            ))
        });
        let Some((favorites, sent, removed)) = data else { return Vec::new() };
        let favorites = favorites
            .into_iter()
            .filter_map(|f| {
                let sha = stickers::unhex(&f.id).filter(|h| h.len() == 32)?;
                Some(Candidate {
                    proto: stickers::favorite_proto(&sha, &f.action),
                    id: f.id,
                    source: StickerSource::Favorite,
                    at: f.ts_ms,
                    animated: f.animated,
                    anchor: None,
                    message_png: None,
                })
            })
            .collect();
        let recents = sent
            .into_iter()
            .filter_map(|r| {
                let id = stickers::hex(r.proto.file_sha256.as_deref().filter(|h| h.len() == 32)?);
                Some(Candidate {
                    id,
                    source: StickerSource::Recent,
                    animated: r.proto.is_animated,
                    proto: *r.proto,
                    at: r.ts.saturating_mul(1000),
                    anchor: Some((r.chat_jid, r.id)),
                    message_png: r.media_path.filter(|p| p.is_file()),
                })
            })
            .collect();
        let (list, lottie_skipped) = stickers::assemble(favorites, recents, &removed);
        if lottie_skipped > 0 {
            log::debug!("stickers: {lottie_skipped} Lottie stickers without a preview left out");
        }
        list
    }

    /// The image to show for a sticker: the animated original (GPUI plays animated WebP)
    /// when it moves, else the panel's PNG, else its message's file.
    fn sticker_preview(&self, c: &Candidate) -> Option<PathBuf> {
        let dir = &self.config.data_dir;
        let original = stickers::sticker_file(dir, &c.id, "webp");
        if c.animated == Some(true) && !stickers::is_lottie(&c.proto) && original.is_file() {
            return Some(original);
        }
        let cached = stickers::sticker_file(dir, &c.id, "png");
        if cached.is_file() { Some(cached) } else { c.message_png.clone().filter(|p| p.is_file()) }
    }

    /// Send the sticker list: always when `force` (an explicit `ListStickers`), else only
    /// if the UI asked for it before and it changed. Returns the entries.
    fn emit_stickers(&self, force: bool) -> Vec<Candidate> {
        if !force && !self.stickers.lock().unwrap_or_else(|e| e.into_inner()).wanted {
            return Vec::new();
        }
        let list = self.sticker_candidates();
        let data_dir = &self.config.data_dir;
        let refs: Vec<StickerRef> = list
            .iter()
            // Gone for good: there's nothing to show or send (the list for previews keeps
            // them, in case a message with one turns up).
            .filter(|c| !stickers::gone_marker(data_dir, &c.id).is_file())
            .map(|c| StickerRef {
                id: c.id.clone(),
                preview: self.sticker_preview(c),
                source: c.source,
                animated: c.animated == Some(true) && !stickers::is_lottie(&c.proto),
            })
            .collect();
        let mut panel = self.stickers.lock().unwrap_or_else(|e| e.into_inner());
        if force || panel.last_sent.as_ref() != Some(&refs) {
            panel.last_sent = Some(refs.clone());
            drop(panel);
            self.emit(Event::Stickers(refs));
        }
        list
    }

    fn sticker_expired(&self, id: &str) -> bool {
        self.stickers.lock().unwrap_or_else(|e| e.into_inner()).expired.contains(id)
    }

    fn set_sticker_expired(&self, id: &str, expired: bool) {
        let mut panel = self.stickers.lock().unwrap_or_else(|e| e.into_inner());
        if expired {
            panel.expired.insert(id.to_owned());
        } else {
            panel.expired.remove(id);
        }
    }
}

/// Takes a sticker out of the previews in flight however its task ends.
struct PreviewGuard {
    shared: Arc<Shared>,
    id: String,
}

impl Drop for PreviewGuard {
    fn drop(&mut self) {
        self.shared.stickers.lock().unwrap_or_else(|e| e.into_inner()).previews_in_flight.remove(&self.id);
    }
}

impl Session {
    /// Read the favorites from the app state in the background, unless that's running or
    /// was done less than `max_age` ago (`None`: regardless of age).
    fn refresh_favorite_stickers(self: &Arc<Self>, max_age: Option<Duration>) {
        {
            let mut panel = self.shared.stickers.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = panel.refreshed_at.zip(max_age).is_some_and(|(at, max)| at.elapsed() < max);
            if panel.refreshing || fresh {
                return;
            }
            panel.refreshing = true;
        }
        let session = self.clone();
        self.track(tokio::spawn(async move {
            let done = session.read_favorite_stickers().await;
            let mut panel = session.shared.stickers.lock().unwrap_or_else(|e| e.into_inner());
            panel.refreshing = false;
            if done {
                panel.refreshed_at = Some(tokio::time::Instant::now());
            }
        }));
    }

    /// Read the `regular_low` collection and store the favorites it has, retrying while
    /// the app state keys haven't arrived yet. Returns true once stored.
    async fn read_favorite_stickers(self: &Arc<Self>) -> bool {
        for (attempt, wait) in FAVORITES_RETRY.iter().enumerate() {
            tokio::time::sleep(*wait).await;
            self.wait_connected(None).await;
            let fetched = match stickers::fetch_regular_low(&self.client).await {
                Ok(f) => f,
                Err(e) => {
                    let last = attempt + 1 == FAVORITES_RETRY.len();
                    match &e {
                        FetchError::MissingKey(_) => log::warn!(
                            "stickers: can't read favorites (attempt {}): {e}{}",
                            attempt + 1,
                            if last { "; giving up until the next refresh" } else { "; retrying" }
                        ),
                        FetchError::Other(_) => log::warn!("stickers: reading favorites failed (attempt {}): {e}", attempt + 1),
                    }
                    continue;
                }
            };
            let sync = stickers::fold_mutations(&fetched.mutations);
            let stored = self.shared.store_gen(self.generation, |s| s.transaction(|s| s.replace_sticker_sync(&sync)));
            if stored.is_none() {
                return false;
            }
            log::info!(
                "stickers: regular_low snapshot v{} + {} patches (now v{}, {} mutations) -> {} favorites, {} hidden recents",
                fetched.snapshot_version,
                fetched.patches,
                fetched.version,
                fetched.mutations.len(),
                sync.favorites.len(),
                sync.removed_recents.len(),
            );
            if self.shared.stickers.lock().unwrap_or_else(|e| e.into_inner()).wanted {
                self.shared.stickers_dirty.notify_one();
                self.load_sticker_previews(self.shared.sticker_candidates());
            }
            return true;
        }
        false
    }

    /// Make the missing previews of `list` in the background, a few at a time.
    fn load_sticker_previews(self: &Arc<Self>, list: Vec<Candidate>) {
        let data_dir = &self.shared.config.data_dir;
        let mut todo: Vec<Candidate> =
            list.into_iter().filter(|c| !stickers::sticker_file(data_dir, &c.id, "png").is_file()).collect();
        // Favorites have no message of their own: find one with the same file, whose PNG
        // may be on disk already and whose sender's phone can re-upload it.
        if todo.iter().any(|c| c.anchor.is_none()) {
            let mut by_hash = self.sticker_messages_by_hash();
            for c in todo.iter_mut().filter(|c| c.anchor.is_none()) {
                if let Some(row) = by_hash.remove(&c.id) {
                    c.message_png = row.media_path.filter(|p| p.is_file());
                    c.anchor = Some((row.chat_jid, row.id));
                }
            }
        }
        for c in todo {
            let gone = stickers::gone_marker(data_dir, &c.id);
            if gone.is_file() {
                if c.anchor.is_none() {
                    continue;
                }
                // A message with it turned up: its sender's phone can re-upload it.
                let _ = std::fs::remove_file(&gone);
            }
            {
                let mut panel = self.shared.stickers.lock().unwrap_or_else(|e| e.into_inner());
                if panel.paused_until.is_some_and(|until| tokio::time::Instant::now() < until) {
                    return;
                }
                let recently_failed =
                    panel.previews_failed.get(&c.id).is_some_and(|at| at.elapsed() < PREVIEW_RETRY_AFTER);
                if recently_failed || !panel.previews_in_flight.insert(c.id.clone()) {
                    continue;
                }
            }
            let session = self.clone();
            self.track(tokio::spawn(async move {
                let shared = session.shared.clone();
                let _guard = PreviewGuard { shared: shared.clone(), id: c.id.clone() };
                let result = match shared.preview_slots.acquire().await {
                    // Rate-limited while this one waited for a slot: leave it for later.
                    Ok(_) if shared.stickers.lock().unwrap_or_else(|e| e.into_inner()).paused_until.is_some_and(|until| tokio::time::Instant::now() < until) => return,
                    Ok(_permit) => session.make_sticker_preview(&c).await,
                    Err(e) => Err(e.into()),
                };
                match result {
                    Ok(()) => {
                        shared.stickers.lock().unwrap_or_else(|e| e.into_inner()).previews_failed.remove(&c.id);
                        shared.stickers_dirty.notify_one();
                    }
                    Err(e) => {
                        let now = tokio::time::Instant::now();
                        let mut panel = shared.stickers.lock().unwrap_or_else(|e| e.into_inner());
                        if format!("{e:#}").contains("rate-overlimit") {
                            // Each failed download costs WhatsApp a media-connection
                            // request: stop them all for a while, then retry.
                            log::info!("stickers: rate-limited; previews paused for {PREVIEW_RATE_LIMIT_PAUSE:?}");
                            panel.paused_until = Some(now + PREVIEW_RATE_LIMIT_PAUSE);
                        } else {
                            log::info!("stickers: no preview for {} ({:?}): {e:#}", c.id, c.source);
                            panel.previews_failed.insert(c.id, now);
                        }
                    }
                }
            }));
        }
    }

    /// Write `media/stickers/<id>.png` (and the original file as `<id>.webp`, the copy a
    /// re-upload sends): from a message's PNG already on disk, the Lottie thumbnail, or a
    /// download (through a stored message when there is one, so expired media can be
    /// re-uploaded by its sender's phone).
    async fn make_sticker_preview(&self, c: &Candidate) -> Result<()> {
        let data_dir = &self.shared.config.data_dir;
        let png = stickers::sticker_file(data_dir, &c.id, "png");
        let original = stickers::sticker_file(data_dir, &c.id, "webp");
        if let Some(src) = c.message_png.clone() {
            // A message's sticker file is a PNG, or the original WebP for animated ones.
            return self
                .shared
                .blocking(self.generation, move |_| {
                    let data = std::fs::read(&src)?;
                    if src.extension().is_some_and(|e| e == "webp") {
                        write_atomic(&original, &data)?;
                        write_atomic(&png, &sticker_to_png(&data)?.png)
                    } else {
                        write_atomic(&png, &data)
                    }
                })
                .await;
        }
        let (data, lottie) = if stickers::is_lottie(&c.proto) {
            (c.proto.png_thumbnail.clone().filter(|t| !t.is_empty()).context("Lottie sticker without preview")?, true)
        } else {
            if !self.wait_connected(Some(DOWNLOAD_CONNECT_WAIT)).await {
                bail!("not connected");
            }
            (self.sticker_bytes(c).await?, false)
        };
        let sticker = self
            .shared
            .blocking(self.generation, move |_| {
                let sticker = sticker_to_png(&data)?;
                if !lottie {
                    write_atomic(&original, &data)?;
                }
                write_atomic(&png, &sticker.png)?;
                Ok(sticker)
            })
            .await?;
        if c.source == StickerSource::Favorite {
            let animated = sticker.animated && !lottie;
            self.shared.store_gen(self.generation, |s| s.set_favorite_sticker_animated(&c.id, animated));
        }
        Ok(())
    }

    /// A sticker's file, decrypted.
    async fn sticker_bytes(&self, c: &Candidate) -> Result<Vec<u8>> {
        let anchor = c.anchor.as_ref().and_then(|(chat, id)| self.shared.store(|s| s.raw_message(chat, id)).flatten());
        if let Some(raw) = anchor.filter(|r| r.media.is_some()) {
            let mut media = raw.media.clone().context("no media info")?;
            let bytes = self.download_bytes(&raw, &mut media).await;
            if bytes.is_ok() {
                self.shared.set_sticker_expired(&c.id, false);
            }
            return bytes;
        }
        let media = StoredMedia::Sticker(Box::new(c.proto.clone()));
        match self.client.download(media.downloadable()).await {
            Err(e) if matches!(e.http_status(), Some(403 | 404 | 410)) => {
                self.shared.set_sticker_expired(&c.id, true);
                let marker = stickers::gone_marker(&self.shared.config.data_dir, &c.id);
                let _ = marker.parent().map(std::fs::create_dir_all);
                let _ = std::fs::write(&marker, b"");
                Err(e.context("expired on the CDN, and no stored message has it to ask for a re-upload"))
            }
            other => other,
        }
    }

    /// A stored message with this sticker: the candidate's own (my latest send), else
    /// any sent or received one with the same file.
    fn sticker_anchor(&self, c: &Candidate) -> Option<RawMessage> {
        let (chat, id) = match &c.anchor {
            Some(anchor) => anchor.clone(),
            None => {
                let row = self.sticker_messages_by_hash().remove(&c.id)?;
                (row.chat_jid, row.id)
            }
        };
        self.shared.store(|s| s.raw_message(&chat, &id)).flatten().filter(|r| r.media.is_some())
    }

    /// Recent stored sticker messages (anyone's) by file hash, the newest of each.
    fn sticker_messages_by_hash(&self) -> HashMap<String, crate::store::StickerRow> {
        let rows = self.shared.store(|s| s.sticker_messages(false, STICKER_ANCHOR_SCAN)).unwrap_or_default();
        let mut by_hash = HashMap::new();
        for row in rows {
            if let Some(h) = row.proto.file_sha256.as_deref().filter(|h| h.len() == 32) {
                by_hash.entry(stickers::hex(h)).or_insert(row);
            }
        }
        by_hash
    }

    /// Make the sticker's media usable by the recipient before sending. Its link is
    /// expired (known from a failed download, or by the link's own `oe=` expiry): upload
    /// our copy of the file again (same file, same id), or else have a stored message's
    /// sender phone re-upload it. Returns how it goes out, for the log.
    async fn prepare_sticker(&self, c: &Candidate, proto: &mut wa::message::StickerMessage) -> Result<&'static str> {
        let link_expired = stickers::cdn_expiry(proto).is_some_and(|t| t <= now_secs());
        if stickers::has_media_fields(proto) && !link_expired && !self.shared.sticker_expired(&c.id) {
            return Ok("as-is");
        }
        let original = stickers::sticker_file(&self.shared.config.data_dir, &c.id, "webp");
        let bytes = if original.is_file() {
            Some(std::fs::read(&original).with_context(|| format!("reading {}", original.display()))?)
        } else {
            None
        };
        let Some(bytes) = bytes else {
            let raw = self.sticker_anchor(c).context("its file expired, and there's no copy of it to send again")?;
            let mut media = raw.media.clone().context("no media info")?;
            self.refresh_media(&raw, &mut media).await?;
            let StoredMedia::Sticker(fresh) = &media else { bail!("not a sticker") };
            // That message's own key and hashes go with its link (it may be another
            // upload of the same file, e.g. someone else's message for a favorite).
            stickers::adopt_media(proto, fresh);
            let direct_path = proto.direct_path.clone().context("re-upload without a path")?;
            proto.url = Some(format!("https://mmg.whatsapp.net{direct_path}"));
            self.shared.set_sticker_expired(&c.id, false);
            return Ok("with a link refreshed by the phone");
        };
        let upload = self.client.upload(bytes, MediaType::Sticker, UploadOptions::new());
        let up = tokio::time::timeout(UPLOAD_TIMEOUT, upload).await.context("upload timed out")??;
        proto.url = Some(up.url);
        proto.direct_path = Some(up.direct_path);
        proto.media_key = Some(up.media_key.to_vec());
        proto.file_enc_sha256 = Some(up.file_enc_sha256.to_vec());
        proto.file_sha256 = Some(up.file_sha256.to_vec());
        proto.file_length = Some(up.file_length);
        proto.media_key_timestamp = Some(up.media_key_timestamp);
        if stickers::hex(&up.file_sha256) != c.id {
            log::warn!("stickers: re-uploaded copy of {} hashes differently", c.id);
        }
        self.shared.set_sticker_expired(&c.id, false);
        Ok("re-uploaded from our copy")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_me_by_pn_or_lid() {
        let own = Own { pn: Some("923001234567@s.whatsapp.net".into()), lid: Some("100000012345678@lid".into()) };
        assert!(own.is_me("923001234567@s.whatsapp.net"));
        assert!(own.is_me("923001234567:33@s.whatsapp.net"));
        assert!(own.is_me("100000012345678@lid"));
        assert!(own.is_me("100000012345678:33@lid"));
        assert!(!own.is_me("923331112222@s.whatsapp.net"));
        assert!(!Own::default().is_me("923001234567@s.whatsapp.net"));
    }

    #[test]
    fn local_profile_formats_the_phone() {
        let local = LocalProfile {
            canonical: "923001234567@s.whatsapp.net".into(),
            name: "Ali".into(),
            is_group: false,
            avatar: None,
        };
        let p = local_profile(&local);
        assert_eq!(p.phone.as_deref(), Some("+92 300 1234567"));
        let group = LocalProfile { canonical: "120363040237990503@g.us".into(), is_group: true, ..local };
        assert_eq!(local_profile(&group).phone, None);
    }

    #[test]
    fn message_ids_look_like_whatsapp_ids() {
        let a = new_message_id();
        let b = new_message_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 24);
        assert!(a.starts_with("3EB0") && a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn expired_media_status_is_visible_through_context() {
        // `Client::download` wraps the CDN status like this; 404/410 trigger a re-upload.
        let e = anyhow::Error::new(whatsapp_rust::http::HttpStatusError { status: 410 }).context("expired");
        assert_eq!(e.http_status(), Some(410));
        assert_eq!(anyhow!("other").http_status(), None);
    }

    #[test]
    fn own_ids_recognise_quotes() {
        let own = Own { pn: Some("1@s.whatsapp.net".into()), lid: Some("9@lid".into()) };
        let q = |p: &str| QuotedRef { id: "X".into(), participant: Some(p.into()), preview: String::new() };
        assert!(own.quote(q("9:4@lid")).from_me);
        assert!(own.quote(q("1@s.whatsapp.net")).from_me);
        let other = own.quote(q("2:1@s.whatsapp.net"));
        assert!(!other.from_me);
        assert_eq!(other.sender.as_deref(), Some("2@s.whatsapp.net"));
        assert_eq!(message_id_from("bad id!".into()).len(), 24);
        assert_eq!(message_id_from("ABCDEF123456".into()), "ABCDEF123456");
    }

    #[test]
    fn notifications() {
        let s = Store::open_in_memory().unwrap();
        let incoming = |chat: &str, sender: &str, body: MessageBody| NewMessage {
            chat_jid: chat.into(),
            id: "M1".into(),
            sender_jid: sender.into(),
            wire_chat: chat.into(),
            wire_sender: None,
            push_name: Some("ally".into()),
            from_me: false,
            body,
            timestamp: now_secs(),
            status: DeliveryStatus::Delivered,
            media: None,
            quoted: None,
            expires_at: None,
        };
        let alice = "15550000001@s.whatsapp.net";
        let group = "120363000000000001@g.us";
        let m = incoming(alice, alice, MessageBody::Text("hi".into()));
        s.insert_message(&m, true).unwrap();
        s.set_contact_name(alice, "Alice").unwrap();
        let Some(Event::Notify { chat_jid, message_id, title, body }) = notification(&s, &m).unwrap() else {
            panic!("no notification")
        };
        assert_eq!((chat_jid.as_str(), message_id.as_str()), (alice, "M1"));
        assert_eq!((title.as_str(), body.as_str()), ("Alice", "hi"));

        let photo = MessageBody::Image(ImageMedia {
            path: None,
            thumbnail: None,
            width: None,
            height: None,
            caption: None,
            state: MediaState::Idle,
        });
        let g = incoming(group, alice, photo);
        s.insert_message(&g, true).unwrap();
        s.set_chat_name(group, "Family").unwrap();
        let Some(Event::Notify { title, body, .. }) = notification(&s, &g).unwrap() else { panic!("no notification") };
        assert_eq!((title.as_str(), body.as_str()), ("Family", "Alice: 📷 Photo"));

        s.set_muted_until(group, i64::MAX).unwrap();
        assert!(notification(&s, &g).unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_wipe_leaves_no_old_data() {
        let data_dir = std::env::temp_dir().join(format!("wa-wipe-{}", new_message_id()));
        create_private_dir_all(&data_dir).unwrap();
        let marker = format!("marker-{}", new_message_id());
        let store = Store::open(&data_dir.join("messages.db")).unwrap();
        let alice = "15550000001@s.whatsapp.net";
        for i in 0..50 {
            store
                .insert_message(
                    &NewMessage {
                        chat_jid: alice.into(),
                        id: format!("M{i}"),
                        sender_jid: alice.into(),
                        wire_chat: alice.into(),
                        wire_sender: None,
                        push_name: Some(marker.clone()),
                        from_me: false,
                        body: MessageBody::Text(format!("{marker} {i}")),
                        timestamp: i,
                        status: DeliveryStatus::Delivered,
                        media: None,
                        quoted: None,
                        expires_at: None,
                    },
                    true,
                )
                .unwrap();
        }
        let session_db = data_dir.join("session.db");
        for (path, data) in [
            (media_file(&data_dir, alice, "M1", "jpg"), b"jpeg".as_slice()),
            (documents_dir(&data_dir, alice).join("a.pdf"), b"pdf"),
            (avatar_file(&data_dir, alice), b"jpeg"),
            (session_db.clone(), b"keys"),
            (data_dir.join("session.db-wal"), b"keys"),
        ] {
            write_atomic(&path, data).unwrap();
        }
        let (events, _rx) = flume::unbounded();
        let shared = Shared::new(store, events, BackendConfig { data_dir: data_dir.clone(), device_name: "test".into() });
        assert_eq!(shared.session_db, session_db);
        *shared.own_jid.lock().unwrap() = Some(alice.into());
        shared.history.lock().unwrap().insert(alice.into(), HistoryFetch { in_flight: None, exhausted: true });
        shared.wipe().await;
        assert!(!shared.history_exhausted(alice), "history state survived the wipe");

        let mut left: Vec<String> = std::fs::read_dir(&data_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        // Only the fresh, empty message store remains.
        assert!(left.iter().all(|f| f.starts_with("messages.db")), "{left:?}");
        for f in &left {
            let bytes = std::fs::read(data_dir.join(f)).unwrap();
            assert!(!bytes.windows(marker.len()).any(|w| w == marker.as_bytes()), "{f} has old data");
        }
        assert!(shared.store(|s| s.list_chats(0)).unwrap().is_empty());
        assert_eq!(shared.own_jid(), None);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    fn test_shared(name: &str) -> (Arc<Shared>, PathBuf, flume::Receiver<Event>) {
        let data_dir = std::env::temp_dir().join(format!("wa-{name}-{}", new_message_id()));
        create_private_dir_all(&data_dir).unwrap();
        let store = Store::open(&data_dir.join("messages.db")).unwrap();
        let (events, rx) = flume::unbounded();
        let config = BackendConfig { data_dir: data_dir.clone(), device_name: "test".into() };
        (Arc::new(Shared::new(store, events, config)), data_dir, rx)
    }

    /// Blocking work of an account (photo preparation, history import, conversions)
    /// can't recreate its data after a wipe: a wipe waits for running jobs, and jobs or
    /// store writes of the old account that come later do nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn blocking_work_cannot_outlive_a_wipe() {
        let (shared, data_dir, _rx) = test_shared("gate");
        let alice = "15550000001@s.whatsapp.net";
        let generation = shared.generation();
        let file = media_file(&data_dir, alice, "M1", "jpg");
        // Running when the wipe starts: finishes, then the wipe deletes what it wrote.
        let started = Arc::new(std::sync::Barrier::new(2));
        let running = {
            let (shared, file, started) = (shared.clone(), file.clone(), started.clone());
            tokio::spawn(async move {
                shared
                    .blocking(generation, move |_| {
                        started.wait();
                        std::thread::sleep(Duration::from_millis(300));
                        write_atomic(&file, b"jpeg")
                    })
                    .await
            })
        };
        tokio::task::spawn_blocking(move || started.wait()).await.unwrap();
        shared.wipe().await;
        assert!(running.is_finished(), "the wipe didn't wait for the running job");
        assert!(running.await.unwrap().is_ok());
        assert!(!data_dir.join("media").exists(), "a running job's file survived the wipe");

        // Started (or writing to the store) for the old account after the wipe: nothing.
        let late = {
            let file = file.clone();
            shared.blocking(generation, move |_| write_atomic(&file, b"jpeg")).await
        };
        assert!(late.is_err());
        let msg = NewMessage {
            chat_jid: alice.into(),
            id: "M1".into(),
            sender_jid: alice.into(),
            wire_chat: alice.into(),
            wire_sender: None,
            push_name: None,
            from_me: false,
            body: MessageBody::Text("old account".into()),
            timestamp: 1,
            status: DeliveryStatus::Delivered,
            media: None,
            quoted: None,
            expires_at: None,
        };
        assert!(shared.store_gen(generation, |s| s.insert_message(&msg, true)).is_none());
        assert!(!data_dir.join("media").exists());
        assert!(shared.store(|s| s.list_chats(0)).unwrap().is_empty());
        // The new account's work runs normally.
        let now = shared.generation();
        assert!(shared.blocking(now, move |_| write_atomic(&file, b"new")).await.is_ok());
        assert!(shared.store_gen(now, |s| s.insert_message(&msg, true)).is_some());
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    /// The logout is known when the client stops, however late the event worker is: the
    /// handler records it synchronously, and it beats other terminal reasons.
    #[test]
    fn logout_is_recorded_before_the_client_stops() {
        use whatsapp_rust::types::events::{LoggedOut, StreamReplaced};
        let (tx, mut rx) = mpsc::unbounded_channel();
        let terminal = Arc::new(TerminalSlot::default());
        let forwarder = Forwarder { events: tx, terminal: terminal.clone() };
        forwarder.handle_event(Arc::new(WaEvent::StreamReplaced(StreamReplaced::builder().build())));
        let logged_out = LoggedOut::builder()
            .on_connect(false)
            .reason(whatsapp_rust::types::events::ConnectFailureReason::LoggedOut)
            .build();
        forwarder.handle_event(Arc::new(WaEvent::LoggedOut(logged_out)));
        // Nothing consumed the forwarded events yet.
        assert!(matches!(terminal.outcome(), Outcome::LoggedOut));
        assert!(matches!(terminal.outcome(), Outcome::Stopped(r) if r == "Disconnected"));
        assert_eq!(std::iter::from_fn(|| rx.try_recv().ok()).count(), 2);

        let slot = TerminalSlot::default();
        slot.record(Terminal::LoggedOut);
        slot.record(Terminal::QrExhausted);
        assert!(matches!(slot.outcome(), Outcome::LoggedOut));
    }

    /// An on-demand history answer whose messages share the anchor's second (in part or
    /// all of them) is delivered, and isn't taken as "nothing older".
    #[test]
    fn history_answers_in_the_anchor_second_are_not_lost() {
        let (shared, data_dir, rx) = test_shared("anchor");
        let alice = "15550000001@s.whatsapp.net";
        let msg = |id: &str, ts: i64| NewMessage {
            chat_jid: alice.into(),
            id: id.into(),
            sender_jid: alice.into(),
            wire_chat: alice.into(),
            wire_sender: None,
            push_name: None,
            from_me: false,
            body: MessageBody::Text(id.into()),
            timestamp: ts,
            status: DeliveryStatus::Delivered,
            media: None,
            quoted: None,
            expires_at: None,
        };
        let ask = |seq: u64| {
            let (oldest, known) = shared
                .store(|s| {
                    let o = s.oldest_message(alice)?.unwrap();
                    Ok((o.ts, s.ids_at(alice, o.ts)?))
                })
                .unwrap();
            shared.history.lock().unwrap().entry(alice.into()).or_default().in_flight =
                Some(Anchor { ts: oldest, known, seq });
        };
        let page = |rx: &flume::Receiver<Event>| {
            let mut got = None;
            let mut exhausted = None;
            for e in rx.try_iter() {
                match e {
                    Event::Messages { messages, .. } => got = Some(messages.into_iter().map(|m| m.id).collect::<Vec<_>>()),
                    Event::HistoryLoading { exhausted: x, .. } => exhausted = Some(x),
                    _ => {}
                }
            }
            (got, exhausted.unwrap())
        };
        shared.store(|s| s.insert_message(&msg("anchor", 100), false)).unwrap();
        let _ = rx.try_iter().count();

        // Everything the phone sent shares the anchor's second.
        ask(1);
        shared.store(|s| s.insert_message(&msg("same1", 100), false)).unwrap();
        shared.store(|s| s.insert_message(&msg("same2", 100), false)).unwrap();
        shared.finish_history_fetch(alice, None);
        assert_eq!(page(&rx), (Some(vec!["same1".into(), "same2".into()]), false));
        assert!(!shared.history_exhausted(alice));

        // Older ones plus one more in the boundary second.
        ask(2);
        shared.store(|s| s.insert_message(&msg("older", 90), false)).unwrap();
        shared.store(|s| s.insert_message(&msg("same3", 100), false)).unwrap();
        shared.finish_history_fetch(alice, None);
        assert_eq!(page(&rx), (Some(vec!["older".into(), "same3".into()]), false));

        // A really empty answer is.
        ask(3);
        shared.finish_history_fetch(alice, None);
        assert_eq!(page(&rx), (None, true));
        assert!(shared.history_exhausted(alice));
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    fn raw(chat: &str, id: &str, sender: &str, from_me: bool, wire_chat: &str, wire_sender: Option<&str>) -> RawMessage {
        RawMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: sender.into(),
            wire_chat: wire_chat.into(),
            wire_sender: wire_sender.map(Into::into),
            from_me,
            body: MessageBody::Text("hi".into()),
            media: None,
            media_path: None,
            timestamp: 1_000,
            status: DeliveryStatus::Sent,
            edit_ts: None,
            revoked: false,
            quoted_id: None,
        }
    }

    fn photo(caption: Option<&str>) -> (MessageBody, StoredMedia) {
        let proto = wa::message::ImageMessage {
            caption: caption.map(Into::into),
            direct_path: Some("/v/x".into()),
            media_key: Some(vec![1; 32]),
            mimetype: Some("image/jpeg".into()),
            ..Default::default()
        };
        let media = StoredMedia::Image(Box::new(proto));
        (media.body(None, MediaState::Idle), media)
    }

    /// Only my own failed, unrevoked texts / photos / stickers are resent, rebuilt as
    /// first sent (same quote, caption and media).
    #[test]
    fn retry_eligibility_and_plans() {
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        let mine = RawMessage {
            status: DeliveryStatus::Failed,
            quoted_id: Some("Q".into()),
            body: MessageBody::Text("hello".into()),
            ..raw(alice, "M", me, true, alice, None)
        };
        assert_eq!(retry_plan(&mine), Ok(RetryPlan::Text { text: "hello".into(), reply_to: Some("Q".into()) }));
        let ignored = |r: &RawMessage| matches!(retry_plan(r), Err(RetryRefusal::Ignore(_)));
        assert!(ignored(&RawMessage { from_me: false, ..mine.clone() }));
        assert!(ignored(&RawMessage { revoked: true, ..mine.clone() }));
        for status in [DeliveryStatus::Pending, DeliveryStatus::Sent, DeliveryStatus::Delivered, DeliveryStatus::Read] {
            assert!(ignored(&RawMessage { status, ..mine.clone() }), "{status:?}");
        }
        let refused = |r: RawMessage| retry_plan(&r).err();
        for body in [MessageBody::System { text: "x".into() }, MessageBody::Unsupported { label: "x".into() }] {
            assert_eq!(refused(RawMessage { body, ..mine.clone() }), Some(RetryRefusal::Refuse(NOT_RESENDABLE)));
        }

        // Photo: from its local copy, with the caption shown and no stale quote context.
        let (body, mut media) = photo(Some("old"));
        if let StoredMedia::Image(p) = &mut media {
            p.context_info = MessageField::some(wa::ContextInfo { stanza_id: Some("X".into()), ..Default::default() });
        }
        let body = match body {
            MessageBody::Image(i) => MessageBody::Image(ImageMedia { caption: Some("sunset".into()), ..i }),
            b => b,
        };
        let file = PathBuf::from("/tmp/M.jpg");
        let photo_msg = RawMessage { body, media: Some(media), media_path: Some(file.clone()), ..mine.clone() };
        let Ok(RetryPlan::Image { file: got, proto, reply_to }) = retry_plan(&photo_msg) else { panic!("no photo plan") };
        assert_eq!((got, reply_to), (file, Some("Q".into())));
        assert_eq!(proto.caption.as_deref(), Some("sunset"));
        assert_eq!(proto.mimetype.as_deref(), Some("image/jpeg"));
        assert!(proto.context_info.is_unset());
        assert_eq!(refused(RawMessage { media_path: None, ..photo_msg.clone() }), Some(RetryRefusal::Refuse(PHOTO_GONE)));
        assert_eq!(refused(RawMessage { media: None, ..photo_msg }), Some(RetryRefusal::Refuse(PHOTO_GONE)));

        // Sticker: from its stored media, keyed by the file hash.
        let proto = wa::message::StickerMessage {
            file_sha256: Some(vec![7; 32]),
            direct_path: Some("/s".into()),
            ..Default::default()
        };
        let media = StoredMedia::Sticker(Box::new(proto.clone()));
        let sticker = RawMessage { body: media.body(None, MediaState::Idle), media: Some(media), ..mine.clone() };
        assert_eq!(
            retry_plan(&sticker),
            Ok(RetryPlan::Sticker { sticker_id: stickers::hex(&[7; 32]), proto: Box::new(proto.clone()), reply_to: Some("Q".into()) })
        );
        let unhashed = StoredMedia::Sticker(Box::new(wa::message::StickerMessage { file_sha256: None, ..proto }));
        assert_eq!(refused(RawMessage { media: Some(unhashed), ..sticker }), Some(RetryRefusal::Refuse(NOT_RESENDABLE)));
    }

    /// Offline resends: shown as `Pending` again (same id and quote), then failed again;
    /// a photo whose copy is gone, or a message that isn't failed, isn't touched.
    #[tokio::test(flavor = "multi_thread")]
    async fn retry_send_offline() {
        let (shared, data_dir, rx) = test_shared("retry");
        let alice = "15550000001@s.whatsapp.net";
        let mine = |id: &str, body: MessageBody, media: Option<StoredMedia>, quoted: Option<&str>| NewMessage {
            chat_jid: alice.into(),
            id: id.into(),
            sender_jid: "15550000009@s.whatsapp.net".into(),
            wire_chat: alice.into(),
            wire_sender: None,
            push_name: None,
            from_me: true,
            body,
            timestamp: 100,
            status: DeliveryStatus::Pending,
            media,
            quoted: quoted.map(|q| NewQuote { id: q.into(), sender: Some(alice.into()), from_me: false, preview: "q".into() }),
            expires_at: None,
        };
        let insert_failed = |m: NewMessage, path: Option<&Path>| {
            shared
                .store(|s| {
                    s.insert_message(&m, false)?;
                    if let Some(p) = path {
                        s.set_media_path(alice, &m.id, Some(p))?;
                    }
                    s.mark_failed(alice, &m.id)
                })
                .unwrap();
        };
        let retry = |id: &str| Command::RetrySend { chat_jid: alice.into(), message_id: id.into() };
        let status = |id: &str| shared.store(|s| s.get_message(alice, id)).flatten().unwrap().status;
        let assert_resent = |events: Vec<Event>, id: &str| {
            let Some(Event::MessageUpserted(m)) = events.first() else { panic!("no pending upsert: {events:?}") };
            assert_eq!((m.id.as_str(), m.status), (id, DeliveryStatus::Pending));
            assert!(
                events.iter().any(|e| matches!(e, Event::StatusChanged { message_ids, status: DeliveryStatus::Failed, .. } if message_ids == &[id.to_owned()])),
                "{events:?}"
            );
            m.clone()
        };

        insert_failed(mine("T1", MessageBody::Text("hello".into()), None, Some("Q")), None);
        handle_command(&shared, None, retry("T1")).await;
        let m = assert_resent(rx.try_iter().collect(), "T1");
        assert_eq!(m.quoted.map(|q| q.id).as_deref(), Some("Q"));
        assert_eq!(status("T1"), DeliveryStatus::Failed);

        let (body, media) = photo(Some("sunset"));
        let jpg = media_file(&data_dir, alice, "P1", "jpg");
        write_atomic(&jpg, b"jpeg").unwrap();
        insert_failed(mine("P1", body.clone(), Some(media.clone()), None), Some(&jpg));
        handle_command(&shared, None, retry("P1")).await;
        assert_resent(rx.try_iter().collect(), "P1");

        // The local copy is gone: an error, still failed, no upsert.
        std::fs::remove_file(&jpg).unwrap();
        handle_command(&shared, None, retry("P1")).await;
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(matches!(events.as_slice(), [Event::Error(e)] if e == PHOTO_GONE), "{events:?}");
        assert_eq!(status("P1"), DeliveryStatus::Failed);

        let proto = wa::message::StickerMessage { file_sha256: Some(vec![7; 32]), ..Default::default() };
        let media = StoredMedia::Sticker(Box::new(proto));
        insert_failed(mine("S1", media.body(None, MediaState::Idle), Some(media), None), None);
        handle_command(&shared, None, retry("S1")).await;
        assert_resent(rx.try_iter().collect(), "S1");

        // Not failed (sent, or unknown): nothing happens.
        shared.store(|s| s.upgrade_status(alice, &["T1".to_owned()], DeliveryStatus::Sent)).unwrap();
        handle_command(&shared, None, retry("T1")).await;
        handle_command(&shared, None, retry("nope")).await;
        assert!(rx.try_iter().next().is_none());
        assert_eq!(status("T1"), DeliveryStatus::Sent);

        // Can't be resent: an error.
        insert_failed(mine("X1", MessageBody::Unsupported { label: "poll".into() }, None, None), None);
        handle_command(&shared, None, retry("X1")).await;
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(matches!(events.as_slice(), [Event::Error(e)] if e == NOT_RESENDABLE), "{events:?}");
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    /// Only my own sent / sending texts and captioned media, not deleted, within 15
    /// minutes of sending.
    #[test]
    fn edit_eligibility() {
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        let mine = raw(alice, "M", me, true, alice, None);
        let t = mine.timestamp;
        assert_eq!(edit_refusal(&mine, t), None);
        assert_eq!(edit_refusal(&mine, t + EDIT_WINDOW_SECS), None);
        assert_eq!(edit_refusal(&mine, t + EDIT_WINDOW_SECS + 1), Some(EDIT_EXPIRED));
        assert_eq!(EDIT_EXPIRED, "This message can no longer be edited");
        let pending = RawMessage { status: DeliveryStatus::Pending, ..mine.clone() };
        assert_eq!(edit_refusal(&pending, t), None);
        for not_editable in [
            raw(alice, "M", alice, false, alice, None),
            RawMessage { revoked: true, ..mine.clone() },
            RawMessage { status: DeliveryStatus::Failed, ..mine.clone() },
            RawMessage { body: MessageBody::System { text: "x".into() }, ..mine.clone() },
            RawMessage { body: MessageBody::Unsupported { label: "x".into() }, ..mine.clone() },
        ] {
            assert_eq!(edit_refusal(&not_editable, t), Some(NOT_EDITABLE));
        }
        // Someone else's old message: not mine is the reason.
        assert_eq!(edit_refusal(&raw(alice, "M", alice, false, alice, None), t + 10_000), Some(NOT_EDITABLE));
        let (body, media) = photo(Some("beach"));
        let captioned = RawMessage { body, media: Some(media), ..mine.clone() };
        assert_eq!(edit_refusal(&captioned, t), None);
        let (body, media) = photo(None);
        assert_eq!(edit_refusal(&RawMessage { body, media: Some(media), ..mine.clone() }, t), Some(NOT_EDITABLE));
        // No media info to resend it with.
        assert_eq!(edit_refusal(&RawMessage { media: None, ..captioned }, t), Some(NOT_EDITABLE));
    }

    /// The `editedMessage`: plain text, or the photo as sent with the new caption.
    #[test]
    fn edited_content_message() {
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        let mine = raw(alice, "M", me, true, alice, None);
        let text = edited_content(&mine, "fixed").unwrap();
        assert_eq!(text.conversation.as_deref(), Some("fixed"));
        let (body, media) = photo(Some("beach"));
        let photo = edited_content(&RawMessage { body, media: Some(media), ..mine.clone() }, "sunset").unwrap();
        let image = photo.image_message.as_option().unwrap();
        assert_eq!(image.caption.as_deref(), Some("sunset"));
        assert_eq!(image.direct_path.as_deref(), Some("/v/x"));
        assert_eq!(image.media_key.as_deref(), Some(&[1u8; 32][..]));
        assert!(photo.conversation.is_none());
        assert!(edited_content(&RawMessage { body: MessageBody::System { text: "x".into() }, ..mine }, "x").is_none());
    }

    #[test]
    fn reaction_keys() {
        let alice = "15550000001@s.whatsapp.net";
        let alice_lid = "900000000001@lid";
        let me = "15550000009@s.whatsapp.net";
        let group = "120363000000000001@g.us";
        let key = |k: wa::MessageKey| (k.remote_jid, k.from_me, k.id, k.participant);
        // 1:1, theirs: addressed like the server did (here: their LID), no participant.
        assert_eq!(
            key(reaction_key(&raw(alice, "A1", alice, false, alice_lid, None))),
            (Some(alice_lid.into()), Some(false), Some("A1".into()), None)
        );
        // 1:1, mine.
        assert_eq!(
            key(reaction_key(&raw(alice, "A2", me, true, alice, None))),
            (Some(alice.into()), Some(true), Some("A2".into()), None)
        );
        // Group, someone else's: the author as on the wire, without the device.
        assert_eq!(
            key(reaction_key(&raw(group, "G1", alice, false, group, Some("900000000001:12@lid")))),
            (Some(group.into()), Some(false), Some("G1".into()), Some(alice_lid.into()))
        );
        // ... falling back to the stored sender.
        assert_eq!(reaction_key(&raw(group, "G2", alice, false, group, None)).participant.as_deref(), Some(alice));
        // Group, mine: as echoed by the server, else resolved when sending.
        assert_eq!(
            reaction_key(&raw(group, "G3", me, true, group, Some("900000000009:3@lid"))).participant.as_deref(),
            Some("900000000009@lid")
        );
        assert_eq!(reaction_key(&raw(group, "G4", me, true, group, None)).participant, None);
    }

    #[test]
    fn history_reactions() {
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        let group = "120363000000000001@g.us";
        let r = |from_me: bool, participant: Option<&str>, text: &str| wa::Reaction {
            key: MessageField::some(wa::MessageKey {
                remote_jid: Some(group.into()),
                from_me: Some(from_me),
                participant: participant.map(Into::into),
                ..Default::default()
            }),
            text: Some(text.into()),
            sender_timestamp_ms: Some(5000),
            ..Default::default()
        };
        let got = history_reaction(&r(false, Some("15550000001:2@s.whatsapp.net"), "😂"), group, "G1", Some(me)).unwrap();
        assert_eq!((got.sender_jid.as_str(), got.from_me, got.emoji.as_str(), got.ts_ms), (alice, false, "😂", 5000));
        let mine = history_reaction(&r(true, None, "👍"), group, "G1", Some(me)).unwrap();
        assert_eq!((mine.sender_jid.as_str(), mine.from_me), (me, true));
        // Who reacted is unknown: dropped rather than attributed to nobody.
        assert!(history_reaction(&r(false, None, "👍"), group, "G1", Some(me)).is_none());
        assert!(history_reaction(&r(true, None, "👍"), group, "G1", None).is_none());
        // 1:1: the other person.
        assert_eq!(history_reaction(&r(false, None, "👍"), alice, "A1", Some(me)).unwrap().sender_jid, alice);
    }

    #[test]
    fn reaction_notifications_only_for_my_messages() {
        let s = Store::open_in_memory().unwrap();
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        let mut mine = NewMessage {
            chat_jid: alice.into(),
            id: "M1".into(),
            sender_jid: me.into(),
            wire_chat: alice.into(),
            wire_sender: None,
            push_name: None,
            from_me: true,
            body: MessageBody::Text("a rather long message that will be cut short in the notification".into()),
            timestamp: now_secs(),
            status: DeliveryStatus::Sent,
            media: None,
            quoted: None,
            expires_at: None,
        };
        s.insert_message(&mine, false).unwrap();
        mine.id = "M2".into();
        mine.from_me = false;
        mine.sender_jid = alice.into();
        s.insert_message(&mine, false).unwrap();
        s.set_contact_name(alice, "Ali").unwrap();
        let reaction = |target: &str, emoji: &str, from_me: bool| NewReaction {
            chat_jid: alice.into(),
            target_id: target.into(),
            sender_jid: if from_me { me.into() } else { alice.into() },
            from_me,
            emoji: emoji.into(),
            ts_ms: now_ms(),
            push_name: None,
        };
        let notify = |r: &NewReaction| {
            let target = s.get_message(alice, &r.target_id).unwrap().unwrap();
            reaction_notification(&s, r, &target).unwrap()
        };
        let Some(Event::Notify { chat_jid, message_id, title, body }) = notify(&reaction("M1", "❤️", false)) else {
            panic!("no notification")
        };
        assert_eq!((chat_jid.as_str(), message_id.as_str(), title.as_str()), (alice, "M1", "Ali"));
        assert_eq!(body, "Ali reacted ❤️ to “a rather long message that will be cut s…”");
        assert!(notify(&reaction("M2", "❤️", false)).is_none(), "not my message");
        assert!(notify(&reaction("M1", "", false)).is_none(), "a removal");
        assert!(notify(&reaction("M1", "❤️", true)).is_none(), "my own reaction");
        s.set_muted_until(alice, i64::MAX).unwrap();
        assert!(notify(&reaction("M1", "❤️", false)).is_none(), "muted");
    }

    /// A reaction shows right away; if it can't be sent, the previous one comes back.
    #[test]
    fn unsent_reaction_is_reverted() {
        let (shared, data_dir, rx) = test_shared("react");
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        *shared.own_jid.lock().unwrap() = Some(me.into());
        let msg = NewMessage {
            chat_jid: alice.into(),
            id: "M1".into(),
            sender_jid: alice.into(),
            wire_chat: alice.into(),
            wire_sender: None,
            push_name: None,
            from_me: false,
            body: MessageBody::Text("hi".into()),
            timestamp: 100,
            status: DeliveryStatus::Delivered,
            media: None,
            quoted: None,
            expires_at: None,
        };
        shared.store(|s| s.insert_message(&msg, false)).unwrap();
        let reactions = |rx: &flume::Receiver<Event>| {
            rx.try_iter()
                .filter_map(|e| match e {
                    Event::MessageUpserted(m) => Some(Some(m.reactions.into_iter().map(|r| r.emoji).collect::<Vec<_>>())),
                    Event::Error(_) => Some(None),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // Offline: shown, then put back, with an error.
        react(&shared, None, alice, "M1", Some("👍".into()));
        assert_eq!(reactions(&rx), vec![Some(vec!["👍".to_owned()]), Some(vec![]), None]);
        assert!(shared.reactions_in_flight.lock().unwrap().is_empty());
        assert!(shared.store(|s| s.get_message(alice, "M1")).flatten().unwrap().reactions.is_empty());
        // Unknown message: nothing happens.
        react(&shared, None, alice, "nope", Some("👍".into()));
        assert!(reactions(&rx).is_empty());
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    /// An edit is shown at once; unsent (offline) it's put back with an error. Refusals
    /// change nothing. Two failed edits in a row end at the content before both.
    #[test]
    fn unsent_edit_is_reverted() {
        let (shared, data_dir, rx) = test_shared("edit");
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        let now = now_secs();
        let mine = |id: &str, ts: i64| NewMessage {
            chat_jid: alice.into(),
            id: id.into(),
            sender_jid: me.into(),
            wire_chat: alice.into(),
            wire_sender: None,
            push_name: None,
            from_me: true,
            body: MessageBody::Text("helo".into()),
            timestamp: ts,
            status: DeliveryStatus::Sent,
            media: None,
            quoted: None,
            expires_at: None,
        };
        shared.store(|s| s.insert_message(&mine("M1", now), false)).unwrap();
        shared.store(|s| s.insert_message(&mine("OLD", now - EDIT_WINDOW_SECS - 5), false)).unwrap();
        let events = |rx: &flume::Receiver<Event>| {
            rx.try_iter()
                .filter_map(|e| match e {
                    Event::MessageUpserted(m) => Some(Ok((m.body, m.edited))),
                    Event::Error(e) => Some(Err(e)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let text = |t: &str| MessageBody::Text(t.into());
        edit_message(&shared, None, alice, "M1", "hello".into());
        assert_eq!(
            events(&rx),
            vec![
                Ok((text("hello"), true)),
                Ok((text("helo"), false)),
                Err("Couldn't edit the message: not connected".into())
            ]
        );
        assert!(shared.edits_in_flight.lock().unwrap().is_empty());
        let stored = shared.store(|s| s.get_message(alice, "M1")).flatten().unwrap();
        assert_eq!((stored.body, stored.edited), (text("helo"), false));
        // Too late, unchanged, empty, unknown.
        edit_message(&shared, None, alice, "OLD", "hello".into());
        assert_eq!(events(&rx), vec![Err(EDIT_EXPIRED.into())]);
        edit_message(&shared, None, alice, "M1", "helo".into());
        assert!(events(&rx).is_empty());
        edit_message(&shared, None, alice, "M1", "  ".into());
        assert_eq!(events(&rx).len(), 1);
        edit_message(&shared, None, alice, "nope", "x".into());
        assert_eq!(events(&rx), vec![Err(NOT_EDITABLE.into())]);

        // Two edits on their way, both failing: back to before the first.
        let apply = |t: &str, ts: i64| shared.store(|s| s.apply_edit(alice, "M1", &text(t), ts)).flatten().unwrap();
        apply("one", now + 1);
        apply("two", now + 2);
        {
            let mut in_flight = shared.edits_in_flight.lock().unwrap();
            let entry = |applied_ts, previous: &str, previous_ts| OptimisticEdit {
                chat_jid: alice.into(),
                id: "M1".into(),
                applied_ts,
                previous: text(previous),
                previous_ts,
            };
            in_flight.insert(1001, entry(now + 1, "helo", None));
            in_flight.insert(1002, entry(now + 2, "one", Some(now + 1)));
        }
        shared.finish_edit(1001, false);
        assert_eq!(shared.store(|s| s.get_message(alice, "M1")).flatten().unwrap().body, text("two"));
        shared.finish_edit(1002, false);
        let stored = shared.store(|s| s.get_message(alice, "M1")).flatten().unwrap();
        assert_eq!((stored.body, stored.edited), (text("helo"), false));
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    /// The panel lists favorites (from app state, added and removed as it changes) and
    /// then my sent stickers, one per file; a sticker sent offline shows as pending with
    /// its preview, then fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn sticker_panel() {
        use crate::stickers::{StickerSync, hex};
        let (shared, data_dir, rx) = test_shared("stickers");
        let alice = "15550000001@s.whatsapp.net";
        let me = "15550000009@s.whatsapp.net";
        *shared.own_jid.lock().unwrap() = Some(me.into());
        let sticker = |sha: u8, path: &str| wa::message::StickerMessage {
            file_sha256: Some(vec![sha; 32]),
            file_enc_sha256: Some(vec![2; 32]),
            media_key: Some(vec![3; 32]),
            direct_path: Some(path.into()),
            mimetype: Some("image/webp".into()),
            ..Default::default()
        };
        let insert = |id: &str, from_me: bool, ts: i64, proto: wa::message::StickerMessage| {
            let media = StoredMedia::Sticker(Box::new(proto));
            let msg = NewMessage {
                chat_jid: alice.into(),
                id: id.into(),
                sender_jid: if from_me { me.into() } else { alice.into() },
                wire_chat: alice.into(),
                wire_sender: None,
                push_name: None,
                from_me,
                body: media.body(None, MediaState::Idle),
                timestamp: ts,
                status: DeliveryStatus::Sent,
                media: Some(media),
                quoted: None,
                expires_at: None,
            };
            shared.store(|s| s.insert_message(&msg, false)).unwrap();
        };
        insert("S1", true, 100, sticker(1, "/old"));
        insert("S2", true, 200, sticker(2, "/two"));
        insert("S3", true, 300, sticker(1, "/new"));
        insert("R1", false, 400, sticker(4, "/theirs"));
        // A preview already downloaded for my latest send of sticker 1.
        let png = media_file(&data_dir, alice, "S3", "png");
        write_atomic(&png, b"png").unwrap();
        shared.store(|s| s.set_media_path(alice, "S3", Some(&png))).unwrap();

        let fav = |sha: u8, ts: i64| {
            let a = wa::sync_action_value::StickerAction {
                direct_path: Some(format!("/fav{sha}")),
                media_key: Some(vec![5; 32]),
                file_enc_sha256: Some(vec![6; 32]),
                is_favorite: Some(true),
                ..Default::default()
            };
            (hex(&[sha; 32]), (a, ts))
        };
        let mut sync = StickerSync { favorites: HashMap::from([fav(2, 10), fav(9, 20)]), ..Default::default() };
        shared.store(|s| s.transaction(|s| s.replace_sticker_sync(&sync))).unwrap();
        shared.store(|s| s.set_favorite_sticker_animated(&hex(&[9; 32]), true)).unwrap();

        let listed = |rx: &flume::Receiver<Event>| {
            rx.try_iter()
                .filter_map(|e| match e {
                    Event::Stickers(list) => Some(list),
                    _ => None,
                })
                .last()
                .unwrap()
        };
        handle_command(&shared, None, Command::ListStickers).await;
        let list = listed(&rx);
        let got: Vec<(String, StickerSource, bool)> = list.iter().map(|r| (r.id.clone(), r.source, r.animated)).collect();
        assert_eq!(
            got,
            vec![
                (hex(&[9; 32]), StickerSource::Favorite, true),
                (hex(&[2; 32]), StickerSource::Favorite, false),
                (hex(&[1; 32]), StickerSource::Recent, false),
            ],
            "favorites first; recents deduplicated and without favorites or others' stickers"
        );
        assert_eq!(list[2].preview.as_deref(), Some(png.as_path()));
        assert_eq!(list[0].preview, None);
        // Favorite 2, which I sent as S2, goes out with S2's media (and can be re-uploaded
        // through it).
        let fav2 = shared.sticker_candidates().into_iter().find(|c| c.id == hex(&[2; 32])).unwrap();
        assert_eq!(fav2.anchor, Some((alice.to_owned(), "S2".to_owned())));
        assert_eq!(fav2.proto.direct_path.as_deref(), Some("/two"));
        assert_eq!(fav2.proto.media_key, Some(vec![3; 32]));

        // Unfavorited on the phone (and the animated flag survives a re-read).
        sync.favorites.remove(&hex(&[2; 32]));
        shared.store(|s| s.transaction(|s| s.replace_sticker_sync(&sync))).unwrap();
        let favs = shared.store(|s| s.favorite_stickers()).unwrap();
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].animated, Some(true));
        let ids: Vec<String> = shared.sticker_candidates().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![hex(&[9; 32]), hex(&[1; 32]), hex(&[2; 32])]);

        // Offline: stored as pending with its own copy of the preview, then failed.
        handle_command(
            &shared,
            None,
            Command::SendSticker {
                chat_jid: alice.into(),
                sticker_id: hex(&[1; 32]),
                client_id: "3EB0STICKER00000000001".into(),
                reply_to: Some("S2".into()),
            },
        )
        .await;
        let events: Vec<Event> = rx.try_iter().collect();
        let Some(Event::MessageUpserted(m)) = events.iter().find(|e| matches!(e, Event::MessageUpserted(_))) else {
            panic!("no optimistic message: {events:?}")
        };
        assert_eq!((m.id.as_str(), m.status, m.from_me), ("3EB0STICKER00000000001", DeliveryStatus::Pending, true));
        assert_eq!(m.quoted.as_ref().map(|q| q.id.as_str()), Some("S2"));
        let MessageBody::Sticker(body) = &m.body else { panic!("not a sticker: {:?}", m.body) };
        let own_copy = media_file(&data_dir, alice, "3EB0STICKER00000000001", "png");
        assert_eq!(body.path.as_deref(), Some(own_copy.as_path()));
        assert_eq!(std::fs::read(&own_copy).unwrap(), b"png");
        assert!(events.iter().any(|e| matches!(e, Event::StatusChanged { status: DeliveryStatus::Failed, .. })));
        let raw = shared.store(|s| s.raw_message(alice, "3EB0STICKER00000000001")).flatten().unwrap();
        let Some(StoredMedia::Sticker(sent)) = raw.media else { panic!("no media") };
        assert_eq!(sent.direct_path.as_deref(), Some("/new"), "my latest send's link");

        // Unknown sticker: an error, nothing stored.
        handle_command(
            &shared,
            None,
            Command::SendSticker { chat_jid: alice.into(), sticker_id: "nope".into(), client_id: String::new(), reply_to: None },
        )
        .await;
        assert!(matches!(rx.try_iter().last(), Some(Event::Error(_))));

        // Logout forgets them (and tells the UI).
        shared.wipe().await;
        assert!(shared.sticker_candidates().is_empty());
        assert!(rx.try_iter().any(|e| matches!(e, Event::Stickers(l) if l.is_empty())));
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn mute_normalization() {
        assert_eq!(normalize_mute(-1), i64::MAX);
        assert_eq!(normalize_mute(1_700_000_000_000), 1_700_000_000);
        assert_eq!(normalize_mute(1_700_000_000), 1_700_000_000);
    }
}
