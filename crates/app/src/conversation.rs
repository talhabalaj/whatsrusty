//! Transcript + floating header/composer, styled after Messages.app.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use crate::download::Fetch;
use wa_backend::{ChatMessage, DeliveryStatus, GroupRead, ImageMedia, MediaState, MessageBody, QuotedMessage, VoiceMedia};

use crate::audio::AudioPlayer;
use crate::avatar::avatar;
use crate::composer::{Composer, ComposerResized};
use crate::native_menu::Item;
use crate::glass;
use crate::find_bar::{FIND_BAR_HEIGHT, FindBar, FindEvent, match_ranges};
use crate::forward_picker::{ForwardEvent, ForwardPicker};
use crate::format::{short_time, transcript_time};
use crate::chrome::{frosted, surface};
use crate::icons::{Glyph, glyph};
use crate::state::{Change, MessageAction, Store, StoreEvent};
use crate::theme::Theme;

/// Messages within this window from the same sender share a bubble group.
const GROUP_GAP_SECS: i64 = 5 * 60;
/// A centered timestamp is shown when the conversation resumes after this gap.
const SEPARATOR_GAP_SECS: i64 = 60 * 60;
/// Photos sent one after another within this (and without captions) show as one album.
const ALBUM_GAP_SECS: i64 = 30;
use crate::kinds::BUBBLE_RADIUS;
const HEADER_HEIGHT: f32 = 52.;
/// The mini window's header, its traffic lights inside it.
pub const MINI_HEADER_HEIGHT: f32 = 40.;
/// Gap between the last message and the floating composer's field, beyond the
/// composer's 10pt top padding.
const COMPOSER_GAP: f32 = 6.;
/// The pinned-message bar under the header.
const PIN_BAR_HEIGHT: f32 = 44.;
const IMAGE_MAX_W: f32 = 260.;
/// Narrower than this, the info panel covers the conversation rather than sitting beside it.
const MIN_CHAT_WIDTH: f32 = 380.;
const IMAGE_MAX_H: f32 = 340.;
/// How long a message flashes after jumping to it.
const HIGHLIGHT: Duration = Duration::from_millis(1600);
/// Page size while paging back to a quoted message or search hit.
const JUMP_PAGE: u32 = 500;
/// Pages a jump may request before giving up (each may be a fetch from the phone).
const JUMP_MAX_PAGES: u32 = 12;

actions!(conversation, [FindInChat]);

pub fn init(cx: &mut App) {
    // Also over text fields' own ⌘F (bound later in the same context, so this one wins).
    cx.bind_keys([KeyBinding::new("cmd-f", FindInChat, None), KeyBinding::new("cmd-f", FindInChat, Some("Input"))]);
}

/// Which chat a conversation view shows: the main window follows the selection, a mini
/// window sticks to one chat.
#[derive(Clone, PartialEq)]
pub enum ChatBinding {
    Selected,
    Fixed(String),
}

impl ChatBinding {
    pub fn chat(&self, store: &Store) -> Option<String> {
        match self {
            ChatBinding::Selected => store.selected.clone(),
            ChatBinding::Fixed(jid) => Some(jid.clone()),
        }
    }

    /// The popped-out chat, for a mini window's view.
    pub fn fixed(&self) -> Option<&str> {
        match self {
            ChatBinding::Selected => None,
            ChatBinding::Fixed(jid) => Some(jid),
        }
    }

    /// A merged chat (LID → phone number) keeps its mini window.
    pub fn follow_merge(&mut self, from: &str, to: &str) -> bool {
        match self {
            ChatBinding::Fixed(jid) if jid == from => {
                *jid = to.to_string();
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone)]
enum Row {
    Separator(i64),
    /// `messages[ix..ix + span]`: more than one for an album of photos.
    ///
    /// `readers`: group members (JID, name) who have read up to this message.
    Message {
        ix: usize,
        span: usize,
        first: bool,
        last: bool,
        show_name: bool,
        status: Option<(String, bool)>,
        readers: Vec<(String, String)>,
    },
    Typing,
}

pub struct Conversation {
    store: Entity<Store>,
    binding: ChatBinding,
    composer: Entity<Composer>,
    list: ListState,
    rows: Vec<Row>,
    messages: Vec<ChatMessage>,
    /// Decoded inline thumbnails, keyed by message id.
    thumbs: HashMap<String, Arc<Image>>,
    /// Photos and stickers drawn in this view, within a memory budget.
    media_cache: Entity<crate::media_cache::MediaImageCache>,
    chat_jid: Option<String>,
    is_group: bool,
    /// Message whose tapback tray is open.
    picker_for: Option<String>,
    forward: Option<(Entity<ForwardPicker>, Subscription)>,
    find: Option<Find>,
    /// A quoted message / search hit to scroll to once it's loaded.
    pending_jump: Option<Jump>,
    /// Message flashing after a jump (id, animation generation).
    highlight: Option<(String, usize)>,
    highlight_gen: usize,
    /// Which of the chat's pins the pinned bar shows (clicking it moves on to the next).
    pin_ix: usize,
    /// Scrolled up from the end (kept by the list's scroll handler).
    away: Rc<Cell<bool>>,
    /// Messages that arrived while scrolled up (the scroll-to-bottom badge).
    unseen: usize,
    /// `away` as last told to the store (mini windows: read receipts need the newest
    /// messages on screen).
    reported_away: bool,
    /// A short-lived status pill ("Forwarded to …").
    notice: Option<String>,
    _timers: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

struct Jump {
    id: String,
    /// Known for search hits: page back until the transcript reaches it.
    timestamp: Option<i64>,
    flash: bool,
    pages: u32,
}

struct Find {
    bar: Entity<FindBar>,
    query: String,
    /// Matches oldest first: (id, timestamp).
    matches: Vec<(String, i64)>,
    current: Option<usize>,
    /// The store's answer for `query` is in (older messages included).
    searched: bool,
    /// Scrolled to a match for this query already.
    jumped: bool,
    _debounce: Option<Task<()>>,
    _sub: Subscription,
}

impl Conversation {
    pub fn new(store: Entity<Store>, binding: ChatBinding, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| Composer::new(store.clone(), binding.clone(), window, cx));
        let subs = vec![
            // The composer grew or shrank: the transcript's bottom inset follows.
            cx.subscribe(&composer, |_, _, _: &ComposerResized, cx| cx.notify()),
            cx.observe(&store, |this, _, cx| this.sync_header(cx)),
            cx.subscribe_in(&store, window, |this, _, ev: &StoreEvent, window, cx| match ev {
                StoreEvent::TranscriptChanged { chat_jid, change } => {
                    if this.binding.chat(this.store.read(cx)).as_deref() == Some(chat_jid.as_str()) {
                        this.rebuild(*change, cx);
                    }
                }
                StoreEvent::ChatMerged { from, to } => {
                    if this.binding.follow_merge(from, to) {
                        this.rebuild(Change::Appended, cx);
                    }
                }
                StoreEvent::AccountReset => {
                    this.find = None;
                    this.forward = None;
                    this.pending_jump = None;
                    this.rows.clear();
                    this.messages.clear();
                    this.thumbs.clear();
                    this.chat_jid = None;
                    this.list.reset(0);
                    cx.notify();
                }
                // The composer follows drafts itself (its resizes arrive as ComposerResized).
                StoreEvent::DraftChanged { .. } => {}
                StoreEvent::MessageAction { action, mini } if mini.as_deref() == this.binding.fixed() => {
                    this.do_action((**action).clone(), window, cx)
                }
                StoreEvent::MessageAction { .. } => {}
            }),
        ];
        let list = ListState::new(0, ListAlignment::Bottom, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        let away = Rc::new(Cell::new(false));
        let (flag, view) = (away.clone(), cx.entity_id());
        list.set_scroll_handler(move |ev, _, cx| {
            let now_away = ev.visible_range.end < ev.count && !ev.is_following_tail;
            if flag.replace(now_away) != now_away {
                cx.notify(view);
            }
        });
        let mut this = Self {
            store,
            binding,
            composer,
            list,
            rows: Vec::new(),
            messages: Vec::new(),
            thumbs: HashMap::new(),
            media_cache: crate::media_cache::MediaImageCache::new(cx),
            chat_jid: None,
            is_group: false,
            // Test hook: open the tapback tray for a message id.
            picker_for: None,
            forward: None,
            find: None,
            pending_jump: None,
            highlight: None,
            highlight_gen: 0,
            pin_ix: 0,
            away,
            unseen: 0,
            reported_away: false,
            notice: None,
            _timers: Vec::new(),
            _subs: subs,
        };
        if this.binding == ChatBinding::Selected {
            this.debug_hooks(window, cx);
        }
        // A reopened window starts on whatever chat the store already has selected.
        this.rebuild(Change::Appended, cx);
        this
    }

    fn sync_header(&mut self, cx: &mut Context<Self>) {
        let bound = self.binding.chat(self.store.read(cx));
        if bound != self.chat_jid {
            // `rebuild` notices the switch (and resets per-chat state) itself.
            self.rebuild(Change::Appended, cx);
        } else {
            if self.find.is_some() {
                // Search results from the store come in through here.
                self.refresh_matches(cx);
            }
            // A history fetch finishing (no transcript change) lets a jump page on.
            let h = self.chat_jid.as_ref().map(|c| self.store.read(cx).history_of(c)).unwrap_or_default();
            if self.pending_jump.is_some() && !h.page_pending && !h.fetching_remote {
                self.try_jump(cx);
            }
        }
        cx.notify();
    }

    fn rebuild(&mut self, change: Change, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let bound = self.binding.chat(store);
        let switched = bound != self.chat_jid;
        self.chat_jid = bound;
        self.is_group = self.chat_jid.as_ref().is_some_and(|j| store.chats.iter().any(|c| &c.jid == j && c.is_group));
        // Where the previously-first message sat, to splice older rows in above it.
        let old_first = self.messages.first().map(|m| m.id.clone());
        let old_first_row = self.rows.iter().position(|r| matches!(r, Row::Message { .. }));
        let old_keys = row_keys(&self.rows, &self.messages);
        let old_newest = self.messages.last().map(|m| m.timestamp);
        let old_messages: HashMap<String, ChatMessage> =
            if switched { HashMap::new() } else { self.messages.iter().map(|m| (m.id.clone(), m.clone())).collect() };
        self.messages = self.chat_jid.as_ref().and_then(|j| store.messages.get(j)).cloned().unwrap_or_default();
        let typing = self.chat_jid.as_ref().is_some_and(|j| store.typing.contains(j));
        let reads = match &self.chat_jid {
            Some(j) if self.is_group => store.group_reads.get(j).map(Vec::as_slice).unwrap_or_default(),
            _ => &[],
        };
        self.rows = build_rows(&self.messages, typing, reads);
        if switched {
            self.thumbs.clear();
            self.unseen = 0;
            self.away.set(false);
            self.pending_jump = None;
            self.highlight = None;
            self.picker_for = None;
            self.find = None;
            self.pin_ix = 0;
        }
        for m in &self.messages {
            let thumb = match &m.body {
                MessageBody::Image(ImageMedia { thumbnail: Some(t), .. }) => Some(t),
                MessageBody::Video(v) => v.thumbnail.as_ref(),
                MessageBody::Location(l) => l.thumbnail.as_ref(),
                _ => None,
            };
            if let Some(t) = thumb
                && !self.thumbs.contains_key(&m.id)
            {
                self.thumbs.insert(m.id.clone(), Arc::new(Image::from_bytes(ImageFormat::Jpeg, t.clone())));
            }
        }

        let old = self.list.item_count();
        let new = self.rows.len();
        // Unknown (rows not all measured yet) is *not* "at the end": jumping would yank a
        // reader who scrolled up.
        let following = self.list.is_following_tail() || self.list.is_scrolled_to_end() == Some(true);
        match change {
            _ if switched || old == 0 => self.list.reset(new),
            // Insert rows in front so the old first message keeps its row identity; the
            // list shifts its scroll anchor past an insertion that lands before it.
            // (Splicing over the anchor row would snap the view to the top instead.)
            Change::Prepended => {
                let new_first_row = old_first.as_ref().and_then(|id| {
                    self.rows.iter().position(|r| matches!(r, Row::Message { ix, .. } if self.messages[*ix].id == *id))
                });
                match (old_first_row, new_first_row) {
                    (Some(o), Some(n)) if n >= o => {
                        self.list.splice(0..0, n - o);
                        // Rows that changed shape (the old leading separator) re-measure lazily.
                        self.list.remeasure_items(n - o..(n + 1).min(new));
                    }
                    _ => self.list.reset(new),
                }
            }
            _ => {
                // Splice only the rows whose identity/shape changed (keyed by message id),
                // and re-measure rows whose content changed in place.
                let new_keys = row_keys(&self.rows, &self.messages);
                let prefix = old_keys.iter().zip(&new_keys).take_while(|(a, b)| a == b).count();
                let max_suffix = old_keys.len().min(new_keys.len()) - prefix;
                let suffix = old_keys
                    .iter()
                    .rev()
                    .zip(new_keys.iter().rev())
                    .take(max_suffix)
                    .take_while(|(a, b)| a == b)
                    .count();
                if old_keys.len() != old {
                    self.list.reset(new);
                } else {
                    if prefix + suffix < old || prefix + suffix < new {
                        self.list.splice(prefix..old - suffix, new - prefix - suffix);
                    }
                    for (row_ix, row) in self.rows.iter().enumerate() {
                        if let Row::Message { ix, span, .. } = row
                            && (row_ix < prefix || row_ix >= new - suffix)
                            && self.messages[*ix..*ix + *span]
                                .iter()
                                .any(|now| old_messages.get(&now.id).is_some_and(|m| m != now))
                        {
                            self.list.remeasure_items(row_ix..row_ix + 1);
                        }
                    }
                }
                if change == Change::Appended && following {
                    self.list.scroll_to_end();
                } else if change == Change::Appended {
                    // New arrivals only, not older messages a history reload filled in.
                    self.unseen += self
                        .messages
                        .iter()
                        .filter(|m| !m.from_me && !old_messages.contains_key(&m.id))
                        .filter(|m| old_newest.is_none_or(|t| m.timestamp >= t))
                        .count();
                }
                // Who's typing may have changed (group names).
                if matches!(self.rows.last(), Some(Row::Typing)) {
                    self.list.remeasure_items(new - 1..new);
                }
            }
        }
        if self.find.is_some() {
            self.refresh_matches(cx);
        }
        self.try_jump(cx);
        // Test hook: once the first page is in, jump to the top to exercise history
        // paging without a mouse.
        static DEBUG_SCROLLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let target = std::env::var("WA_DEBUG_SELECT").ok();
        if new > 5
            && std::env::var_os("WA_DEBUG_SCROLL_TOP").is_some()
            && (target.is_none() || target == self.chat_jid)
            && !DEBUG_SCROLLED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.list.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
        }
        cx.notify();
    }

    fn row_of(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(
            |r| matches!(r, Row::Message { ix, span, .. } if self.messages[*ix..*ix + *span].iter().any(|m| m.id == id)),
        )
    }

    /// Scroll to a message (a reply's original, a search hit), paging older history in
    /// until it's loaded.
    fn jump_to(&mut self, id: String, timestamp: Option<i64>, flash: bool, cx: &mut Context<Self>) {
        self.pending_jump = Some(Jump { id, timestamp, flash, pages: 0 });
        self.try_jump(cx);
    }

    fn try_jump(&mut self, cx: &mut Context<Self>) {
        let Some(jump) = &self.pending_jump else { return };
        if let Some(row) = self.row_of(&jump.id) {
            // Rows above may not be measured yet (just paged in): anchor on the row before
            // it, which keeps some context above, instead of revealing by height.
            self.list.scroll_to(ListOffset { item_ix: row.saturating_sub(1), offset_in_item: px(0.) });
            // Programmatic scrolls don't reach the scroll handler.
            if row + 3 < self.rows.len() {
                self.away.set(true);
            }
            if jump.flash {
                self.flash(jump.id.clone(), cx);
            }
            self.pending_jump = None;
            cx.notify();
            return;
        }
        // A search hit inside the loaded range that isn't here was deleted meanwhile.
        let oldest = self.messages.first().map(|m| m.timestamp);
        if jump.timestamp.zip(oldest).is_some_and(|(t, o)| t >= o) {
            self.pending_jump = None;
            return;
        }
        let Some(chat) = self.chat_jid.clone() else { return };
        let (store, view) = (self.store.clone(), cx.entity().downgrade());
        let exhausted = jump.pages >= JUMP_MAX_PAGES;
        cx.defer(move |cx| {
            if !exhausted && store.update(cx, |s, cx| s.load_older_by(&chat, JUMP_PAGE, true, cx)) {
                let _ = view.update(cx, |this, _| this.pending_jump.as_mut().map(|j| j.pages += 1));
                return;
            }
            // Still loading: the next store update tries again.
            let h = store.read(cx).history_of(&chat);
            if exhausted || (!h.page_pending && !h.fetching_remote) {
                let _ = view.update(cx, |this, cx| {
                    if this.pending_jump.take().is_some() {
                        this.show_notice("That message isn't available.", cx);
                    }
                });
            }
        });
    }

    /// The bar under the header showing a pinned message: the pins as segments on the
    /// left (the shown one bright), who sent it and what it says. A click jumps to it and
    /// moves on to the next pin; right-click (or ⋯) to unpin.
    fn pin_bar(&self, pins: &[ChatMessage], glass_on: bool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.pin_ix % pins.len();
        let shown = pins[ix].clone();
        let who = if shown.from_me {
            "You".to_string()
        } else if self.is_group {
            shown.sender_name.clone().unwrap_or_else(|| "Pinned message".into())
        } else {
            "Pinned message".into()
        };
        let preview = crate::message_info::preview(&shown).replace('\n', " ");
        let count = pins.len();
        let segments = div().flex_none().h(px(30.)).w(px(3.)).flex().flex_col().gap(px(2.)).children((0..count).map(|i| {
            div().flex_1().w_full().rounded_full().bg(if i == ix { theme.accent } else { theme.text_secondary.opacity(0.3) })
        }));
        let menu = {
            let (view, pins) = (cx.entity().downgrade(), pins.to_vec());
            move |at: Point<Pixels>, window: &mut Window, cx: &mut App| {
                let shown = pins[ix].clone();
                let mut items = vec![
                    {
                        let (view, m) = (view.clone(), shown.clone());
                        Item::action("Show in Chat", move |_, cx| {
                            let _ = view.update(cx, |this, cx| this.jump_to(m.id.clone(), Some(m.timestamp), true, cx));
                        })
                    },
                    {
                        let (view, m) = (view.clone(), shown.clone());
                        Item::action("Unpin", move |_, cx| {
                            let _ = view.update(cx, |this, cx| this.store.update(cx, |s, _| s.pin_message(&m, None)));
                        })
                    },
                ];
                if pins.len() > 1 {
                    items.push(Item::Separator);
                    items.push(Item::submenu(
                        "Unpin…",
                        pins.iter()
                            .map(|m| {
                                let (view, m) = (view.clone(), m.clone());
                                let title: String = crate::message_info::preview(&m).replace('\n', " ").chars().take(40).collect();
                                Item::action(title, move |_, cx| {
                                    let _ = view.update(cx, |this, cx| this.store.update(cx, |s, _| s.pin_message(&m, None)));
                                })
                            })
                            .collect(),
                    ));
                }
                crate::native_menu::show(items, at, window, cx);
            }
        };
        let more_menu = menu.clone();
        let bar = div()
            .id("pin-bar")
            .h(px(PIN_BAR_HEIGHT))
            .w_full()
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(10.))
            .relative()
            // A floating glass capsule under the header's; without glass, a frosted strip.
            .map(|d| {
                if glass_on {
                    d.rounded(px(PIN_BAR_HEIGHT / 2.)).pl(px(18.))
                } else {
                    d.child(crate::chrome::frosted(theme.bar_tint)).border_b_1().border_color(theme.separator)
                }
            })
            .cursor_pointer()
            .child(segments)
            .child(glyph(Glyph::Pin, 14., theme.text_secondary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(11.5)).line_height(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.accent).truncate().child(who))
                    .child(div().text_size(px(12.5)).line_height(px(16.)).text_color(theme.text).truncate().child(preview)),
            )
            // A photo or video: its thumbnail, like WhatsApp's.
            .children(self.thumbs.get(&shown.id).cloned().map(|thumb| {
                div().flex_none().size(px(30.)).rounded(px(4.)).overflow_hidden().child(
                    img(thumb).size_full().object_fit(ObjectFit::Cover),
                )
            }))
            .child(
                div()
                    .id("pin-bar-more")
                    .flex_none()
                    .size(px(26.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|s| s.bg(theme.search_bg))
                    .child(glyph(Glyph::More, 14., theme.text_secondary))
                    .on_click(move |ev, window, cx| {
                        cx.stop_propagation();
                        more_menu(ev.position(), window, cx)
                    }),
            )
            .on_mouse_down(MouseButton::Right, move |ev, window, cx| {
                cx.stop_propagation();
                menu(ev.position, window, cx)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.jump_to(shown.id.clone(), Some(shown.timestamp), true, cx);
                this.pin_ix = (ix + 1) % count;
                cx.notify();
            }));
        if glass_on {
            glass::glass("pin-bar", PIN_BAR_HEIGHT / 2., bar).into_any_element()
        } else {
            bar.into_any_element()
        }
    }

    /// An action asked for from the gallery or the photo viewer.
    fn do_action(&mut self, action: MessageAction, window: &mut Window, cx: &mut Context<Self>) {
        // The store just switched chats: catch up first, or the switch would cancel a jump.
        if self.binding.chat(self.store.read(cx)) != self.chat_jid {
            self.rebuild(Change::Appended, cx);
        }
        match action {
            MessageAction::Reveal(m) => self.jump_to(m.id, Some(m.timestamp), true, cx),
            MessageAction::Forward(m) => self.open_forward(m, window, cx),
        }
    }

    fn flash(&mut self, id: String, cx: &mut Context<Self>) {
        self.highlight_gen += 1;
        let generation = self.highlight_gen;
        self.highlight = Some((id, generation));
        self._timers.retain(|t| !t.is_ready());
        self._timers.push(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT).await;
            let _ = this.update(cx, |this, cx| {
                if this.highlight.as_ref().is_some_and(|(_, g)| *g == generation) {
                    this.highlight = None;
                    cx.notify();
                }
            });
        }));
    }

    fn show_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        self.notice = Some(text.clone());
        self._timers.retain(|t| !t.is_ready());
        self._timers.push(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(2500)).await;
            let _ = this.update(cx, |this, cx| {
                if this.notice.as_deref() == Some(text.as_str()) {
                    this.notice = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Tail);
        self.away.set(false);
        self.unseen = 0;
        cx.notify();
    }

    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat_jid.is_none() {
            return;
        }
        if let Some(find) = &self.find {
            find.bar.update(cx, |b, cx| b.focus(window, cx));
            return;
        }
        let bar = cx.new(|cx| FindBar::new(window, cx));
        let sub = cx.subscribe_in(&bar, window, |this, _, ev: &FindEvent, window, cx| this.on_find(ev, window, cx));
        bar.update(cx, |b, cx| b.focus(window, cx));
        self.find = Some(Find {
            bar,
            query: String::new(),
            matches: Vec::new(),
            current: None,
            searched: false,
            jumped: false,
            _debounce: None,
            _sub: sub,
        });
        cx.notify();
    }

    fn on_find(&mut self, ev: &FindEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else { return };
        match ev {
            FindEvent::Query(q) => {
                let query = q.trim().to_string();
                if query == find.query {
                    return;
                }
                (find.query, find.searched, find.jumped, find.current) = (query.clone(), false, false, None);
                // The store scan decodes every message of the chat: not on every keystroke.
                let (store, chat) = (self.store.clone(), self.chat_jid.clone().unwrap_or_default());
                find._debounce = Some(cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(Duration::from_millis(200)).await;
                    store.update(cx, |s, _| s.search(&chat, &query));
                }));
                self.refresh_matches(cx);
            }
            FindEvent::Older | FindEvent::Newer => {
                let older = matches!(ev, FindEvent::Older);
                let n = find.matches.len();
                if n == 0 {
                    return;
                }
                let next = match find.current {
                    None => n - 1,
                    Some(i) if older => (i + n - 1) % n,
                    Some(i) => (i + 1) % n,
                };
                find.current = Some(next);
                find.jumped = true;
                let (id, ts) = find.matches[next].clone();
                self.update_find_status(cx);
                self.jump_to(id, Some(ts), false, cx);
            }
            FindEvent::Close => {
                self.find = None;
                Composer::focus(&self.composer, window, cx);
                cx.notify();
            }
        }
    }

    /// Recompute matches: the loaded transcript, plus the store's hits for older ones.
    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else { return };
        let mut matches: Vec<(String, i64)> = Vec::new();
        if !find.query.is_empty() {
            let mut seen = HashSet::new();
            for m in &self.messages {
                if find_text(m).is_some_and(|t| !match_ranges(t, &find.query).is_empty()) {
                    seen.insert(m.id.clone());
                    matches.push((m.id.clone(), m.timestamp));
                }
            }
            if let Some((chat, query, hits)) = &self.store.read(cx).search_results
                && Some(chat) == self.chat_jid.as_ref()
                && query.trim() == find.query
            {
                find.searched = true;
                matches.extend(hits.iter().filter(|h| !seen.contains(&h.id)).map(|h| (h.id.clone(), h.timestamp)));
            }
            matches.sort_by_key(|(_, ts)| *ts);
        }
        let current_id = find.current.and_then(|i| find.matches.get(i)).map(|(id, _)| id.clone());
        find.current = match current_id {
            Some(id) => matches.iter().position(|(m, _)| *m == id),
            None => None,
        };
        find.matches = matches;
        // Start from the newest match, like Messages.
        if find.current.is_none() && !find.jumped && !find.matches.is_empty() {
            let i = find.matches.len() - 1;
            find.current = Some(i);
            find.jumped = true;
            let (id, ts) = find.matches[i].clone();
            self.update_find_status(cx);
            self.jump_to(id, Some(ts), false, cx);
            return;
        }
        self.update_find_status(cx);
    }

    fn update_find_status(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &self.find else { return };
        let status = (find.searched || !find.matches.is_empty()).then(|| {
            let n = find.matches.len();
            // Counted from the newest, the one shown first.
            (find.current.map(|i| n - 1 - i), n)
        });
        find.bar.update(cx, |b, cx| b.set_status(status, cx));
        cx.notify();
    }

    /// The query to highlight in a message, and whether it's the current match.
    fn find_highlight(&self, id: &str) -> Option<(&str, bool)> {
        let find = self.find.as_ref().filter(|f| !f.query.is_empty())?;
        let current = find.current.and_then(|i| find.matches.get(i)).is_some_and(|(m, _)| m == id);
        Some((find.query.as_str(), current))
    }

    fn open_forward(&mut self, message: ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let picker = cx.new(|cx| ForwardPicker::new(store, message, window, cx));
        let sub = cx.subscribe_in(&picker, window, |this, picker, ev: &ForwardEvent, window, cx| {
            if let ForwardEvent::Picked(jid, name) = ev {
                let message = picker.read(cx).message.clone();
                let jid = jid.clone();
                this.store.update(cx, |s, cx| s.forward(&message, jid, cx));
                this.show_notice(format!("Forwarded to {name}"), cx);
            }
            this.forward = None;
            Composer::focus(&this.composer, window, cx);
            cx.notify();
        });
        self.picker_for = None;
        self.forward = Some((picker, sub));
        cx.notify();
    }

    /// Esc closes the topmost thing open over the conversation; false if nothing was.
    fn on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.picker_for.is_some() {
            self.close_menu(cx);
        } else if self.binding.fixed().is_none() && self.store.read(cx).message_info.is_some() {
            self.store.update(cx, |s, cx| s.close_message_info(cx));
        } else if self.binding.fixed().is_none() && self.store.read(cx).profile_open.is_some() {
            self.store.update(cx, |s, cx| s.close_profile(cx));
        } else {
            return false;
        }
        true
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.picker_for = None;
        cx.notify();
    }

    /// Test hooks: WA_DEBUG_FIND="<query>", WA_DEBUG_PICKER (tapback tray) / WA_DEBUG_MENU / WA_DEBUG_FORWARD / WA_DEBUG_DELETE /
    /// WA_DEBUG_JUMP="<message id>", WA_DEBUG_SCROLL_UP=1 (needs WA_DEBUG_SELECT).
    fn debug_hooks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hooks = [
            "WA_DEBUG_PICKER",
            "WA_DEBUG_MENU",
            "WA_DEBUG_FIND",
            "WA_DEBUG_FORWARD",
            "WA_DEBUG_DELETE",
            "WA_DEBUG_JUMP",
            "WA_DEBUG_SCROLL_UP",
            "WA_DEBUG_ACT",
        ];
        let set: Vec<(&str, String)> = hooks.iter().filter_map(|k| Some((*k, std::env::var(k).ok()?))).collect();
        if set.is_empty() {
            return;
        }
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(2500)).await;
            for (key, value) in set {
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        let find_message = |this: &Self| this.messages.iter().find(|m| m.id == value).cloned();
                        match key {
                            "WA_DEBUG_FIND" => {
                                this.open_find(window, cx);
                                if let Some(f) = &this.find {
                                    f.bar.update(cx, |b, cx| b.set_query(&value, window, cx));
                                }
                            }
                            "WA_DEBUG_FORWARD" => {
                                if let Some(m) = find_message(this) {
                                    this.open_forward(m, window, cx);
                                }
                            }
                            "WA_DEBUG_DELETE" => {
                                if let Some(m) = find_message(this) {
                                    this.confirm_delete(m, window, cx);
                                }
                            }
                            "WA_DEBUG_PICKER" => this.picker_for = Some(value.clone()),
                            // "<id>|everyone", "<id>|me", "<id>|retry", "<id>|fwd:<chat>", "<id>|vote:<option>+<option>"
                            "WA_DEBUG_ACT" => {
                                for act in value.split(',') {
                                    let Some((id, what)) = act.split_once('|') else { continue };
                                    let Some(m) = this.messages.iter().find(|m| m.id == id).cloned() else { continue };
                                    match what {
                                        "everyone" | "me" => this.store.read(cx).delete_message(&m, what == "everyone"),
                                        "retry" => this.store.update(cx, |s, cx| s.retry_send(&m, cx)),
                                        "star" => this.store.update(cx, |s, _| s.star_message(&m, true)),
                                        _ => {
                                            if let Some(to) = what.strip_prefix("fwd:") {
                                                this.store.update(cx, |s, cx| s.forward(&m, to.to_string(), cx));
                                            } else if let Some(options) = what.strip_prefix("vote:") {
                                                let options = options.split('+').filter(|o| !o.is_empty()).map(String::from).collect();
                                                this.store.update(cx, |s, _| s.vote_poll(&m, options));
                                            }
                                        }
                                    }
                                }
                            }
                            "WA_DEBUG_MENU" => {
                                // Right-click near the message's bubble.
                                if let (Some(row), Some(m)) = (this.row_of(&value), find_message(this)) {
                                    let at = this.list.bounds_for_item(row).map_or(point(px(640.), px(420.)), |b| {
                                        point(b.origin.x + px(260.), b.origin.y + b.size.height - px(10.))
                                    });
                                    this.open_message_menu(&m, at, window, cx);
                                }
                            }
                            "WA_DEBUG_JUMP" => this.jump_to(value.clone(), None, true, cx),
                            _ => {
                                this.list.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
                                this.away.set(true);
                            }
                        }
                        cx.notify();
                    });
                });
                cx.background_executor().timer(Duration::from_millis(400)).await;
            }
        })
        .detach();
    }

    /// Main window: compact centered avatar + name, like Messages (click for info), with
    /// Pop Out at the end. Mini window: avatar + name after the traffic lights, and a
    /// button that opens the chat in the main window. With Liquid Glass the main window's
    /// avatar + name sit in a glass capsule floating over the transcript instead of a bar.
    #[allow(clippy::too_many_arguments)]
    fn header(
        &self,
        chat: &wa_backend::ChatSummary,
        typing_label: Option<String>,
        photo: Option<Arc<Image>>,
        glass_on: bool,
        theme: &Theme,
    ) -> AnyElement {
        let (store, jid) = (self.store.clone(), chat.jid.clone());
        let icon_button = |id: &'static str, icon: Glyph, tip: &'static str| {
            div()
                .id(id)
                .absolute()
                .top_0()
                .bottom_0()
                .my_auto()
                .size(px(28.))
                .rounded(px(7.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(theme.search_bg))
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .child(glyph(icon, 15., theme.text_secondary))
        };
        // Fixed line heights: an emoji in the name (Apple Color Emoji's taller line) would
        // otherwise push "typing…" out of the 30pt / 38pt glass capsule.
        let names = div()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .text_size(px(13.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .truncate()
                    .child(chat.name.clone()),
            )
            .when_some(typing_label, |d, label| {
                d.child(div().text_size(px(11.)).line_height(px(13.)).text_color(theme.text_secondary).truncate().child(label))
            });
        let bar = div()
            .id("chat-header")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            // Over the transcript: clicks stop here (scrolling goes through).
            .block_mouse_except_scroll()
            .child(frosted(theme.bar_tint))
            .border_b_1()
            .border_color(theme.separator)
            .flex()
            .items_center()
            .gap(px(8.));
        if self.binding.fixed().is_some() && glass_on {
            // Like the main window's: the name in a glass capsule, a glass button at the end.
            let identity = div()
                .id("chat-header")
                .min_w_0()
                .h(px(30.))
                .pl(px(4.))
                .pr(px(12.))
                .flex()
                .items_center()
                .gap(px(6.))
                .child(avatar(&chat.name, photo, 22., theme))
                .child(names);
            let expand = div()
                .id("open-in-main")
                .absolute()
                .top_0()
                .bottom_0()
                .my_auto()
                .right(px(8.))
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Open in Main Window").build(window, cx))
                .child(glyph(Glyph::Expand, 13., theme.text))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    crate::mini::open_in_main(&store, jid.clone(), cx);
                });
            return div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                // Floating over the transcript: clicks stop here (scrolling goes through).
                .block_mouse_except_scroll()
                .h(px(MINI_HEADER_HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                // Clear of the traffic lights.
                .pl(px(70.))
                .pr(px(44.))
                .child(glass::glass("chat-header", 15., identity))
                .child(glass::circle_button("chat-open-in-main", 28., theme.search_bg, expand))
                .into_any_element();
        }
        if self.binding.fixed().is_some() {
            return bar
                .h(px(MINI_HEADER_HEIGHT))
                // Clear of the traffic lights.
                .pl(px(84.))
                .pr(px(40.))
                .child(avatar(&chat.name, photo, 22., theme))
                .child(names)
                .child(icon_button("open-in-main", Glyph::Expand, "Open in Main Window").right(px(8.)).on_click(
                    move |_, _, cx| {
                        cx.stop_propagation();
                        crate::mini::open_in_main(&store, jid.clone(), cx);
                    },
                ))
                .into_any_element();
        }
        let pop_jid = jid.clone();
        let pop_store = store.clone();
        let (glass_pop_store, glass_pop_jid) = (store.clone(), jid.clone());
        let pop_out = icon_button("pop-out", Glyph::PopOut, "Pop Out").right(px(12.)).on_click(move |_, _, cx| {
            cx.stop_propagation();
            crate::mini::pop_out(&pop_store, pop_jid.clone(), cx);
        });
        let (gallery_store, gallery_jid) = (store.clone(), jid.clone());
        let open_gallery = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            gallery_store.update(cx, |s, cx| s.open_gallery(gallery_jid.clone(), crate::state::GalleryTab::Media, cx));
        };
        let gallery = icon_button("open-gallery", Glyph::Grid, "Media, Docs and Links").right(px(48.)).on_click(open_gallery.clone());
        if glass_on {
            // A glass circle like the sidebar's New Message button.
            let pop_out = div()
                .id("pop-out")
                .absolute()
                .top_0()
                .bottom_0()
                .my_auto()
                .right(px(12.))
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Pop Out").build(window, cx))
                .child(glyph(Glyph::PopOut, 15., theme.text))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    crate::mini::pop_out(&glass_pop_store, glass_pop_jid.clone(), cx);
                });
            let pop_out = glass::circle_button("chat-pop-out", 32., theme.search_bg, pop_out);
            let gallery = div()
                .id("open-gallery")
                .absolute()
                .top_0()
                .bottom_0()
                .my_auto()
                .right(px(52.))
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Media, Docs and Links").build(window, cx))
                .child(glyph(Glyph::Grid, 14., theme.text))
                .on_click(open_gallery);
            let gallery = glass::circle_button("chat-gallery", 32., theme.search_bg, gallery);
            let identity = div()
                .id("chat-header")
                .cursor_pointer()
                .on_click(move |_, _, cx| store.update(cx, |s, cx| s.toggle_profile(jid.clone(), cx)))
                // Shrinks (the name truncates) rather than run under the pop-out button.
                .min_w_0()
                .h(px(38.))
                .pl(px(5.))
                .pr(px(16.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(avatar(&chat.name, photo, 28., theme))
                .child(names);
            return div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                // Floating over the transcript: clicks stop here (scrolling goes through).
                .block_mouse_except_scroll()
                .h(px(HEADER_HEIGHT))
                // Clear of the gallery and pop-out buttons on both sides, so the name stays centered.
                .px(px(96.))
                .flex()
                .items_center()
                .justify_center()
                .child(glass::glass("chat-header", 19., identity))
                .child(gallery)
                .child(pop_out)
                .into_any_element();
        }
        bar.h(px(HEADER_HEIGHT))
            .cursor_pointer()
            .on_click(move |_, _, cx| store.update(cx, |s, cx| s.toggle_profile(jid.clone(), cx)))
            .justify_center()
            .child(avatar(&chat.name, photo, 28., theme))
            .child(names)
            .child(gallery)
            .child(pop_out)
            .into_any_element()
    }

    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    fn render_row(&self, ix: usize, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(ix) {
            Some(Row::Separator(ts)) => div()
                .w_full()
                .pt(px(14.))
                .pb(px(6.))
                .flex()
                .justify_center()
                .text_size(px(11.))
                .text_color(theme.text_secondary)
                .font_weight(FontWeight::MEDIUM)
                .child(transcript_time(*ts))
                .into_any_element(),
            Some(Row::Message { ix, span, first, last, show_name, status, readers }) => {
                let album = &self.messages[*ix..*ix + *span];
                self.render_message(album, *first, *last, *show_name, status.as_ref(), readers, theme, window, cx)
            }
            Some(Row::Typing) => self.typing_row(theme, cx),
            None => div().into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// `album`: the row's message, or the photos of an album (actions apply to the first).
    fn render_message(
        &self,
        album: &[ChatMessage],
        first: bool,
        last: bool,
        show_name: bool,
        status: Option<&(String, bool)>,
        readers: &[(String, String)],
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let m = &album[0];
        let from_me = m.from_me;
        let show_sender = self.is_group && !from_me && show_name;
        let gutter = self.is_group && !from_me;
        let sender = m.sender_name.clone().unwrap_or_else(|| m.sender_jid.split('@').next().unwrap_or("").to_string());
        let sender_photo = if gutter && last { self.photo_for(&m.sender_jid, cx) } else { None };

        if let MessageBody::System { text } = &m.body {
            return crate::kinds::system_line(text, theme).into_any_element();
        }
        let thumb = self.thumbs.get(&m.id).cloned();
        let found = self.find_highlight(&m.id);
        // Under the bubble: "★ Starred", "Edited", or both.
        let starred = album.iter().any(|m| m.starred);
        let note = [starred.then_some("★ Starred"), m.edited.then_some("Edited")].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let content: AnyElement = match &m.body {
            _ if album.len() > 1 => self.album_content(album, theme, cx),
            _ if m.deleted => bubble(
                text_content(if from_me { "You deleted this message" } else { "This message was deleted" }, true),
                from_me,
                last,
                theme,
            )
            .into_any_element(),
            MessageBody::Image(media) => self.image_content(m, media, from_me, last, found, theme, cx),
            MessageBody::Video(v) => crate::kinds::video(&self.store, m, v, thumb, self.binding.fixed().map(String::from), theme, cx).into_any_element(),
            MessageBody::Document(d) => {
                bubble(div().child(crate::kinds::document(&self.store, m, d, from_me, theme, cx)), from_me, last, theme)
                    .into_any_element()
            }
            MessageBody::Sticker(s) => crate::kinds::sticker(&self.store, m, s, theme, cx).into_any_element(),
            MessageBody::Location(l) => crate::kinds::location(m, l, thumb, theme, from_me).into_any_element(),
            MessageBody::Contacts(c) => bubble(crate::kinds::contacts(c, from_me, theme), from_me, last, theme).into_any_element(),
            MessageBody::Poll(p) => bubble(crate::kinds::poll(&self.store, m, p, from_me, theme), from_me, last, theme).into_any_element(),
            MessageBody::Rich(r) => bubble(crate::kinds::rich(r, from_me, theme), from_me, last, theme).into_any_element(),
            MessageBody::System { .. } => unreachable!("handled above"),
            MessageBody::ViewOnce(k) => bubble(
                div()
                    .flex()
                    .flex_col()
                    .child(text_content(crate::state::view_once_label(*k), false))
                    .child(div().text_size(px(11.5)).opacity(0.75).child("Open it on your phone")),
                from_me,
                last,
                theme,
            )
            .into_any_element(),
            MessageBody::Voice(media) => voice_content(&self.store, m, media, from_me, last, theme, window, cx),
            // Just a few emoji: large, without a bubble, like WhatsApp.
            MessageBody::Text(t) if found.is_none() && let Some(n) = crate::emoji_complete::emoji_only(t) => {
                let size = match n {
                    1 => 52.,
                    2 | 3 => 42.,
                    _ => 34.,
                };
                div().px(px(2.)).text_size(px(size)).line_height(px(size * 1.2)).child(t.trim().to_string()).into_any_element()
            }
            MessageBody::Text(t) => bubble(found_text(format!("text-{}", m.id).into(), t, found, &m.mentions, from_me, theme), from_me, last, theme)
                .into_any_element(),
            MessageBody::Unsupported { label } => {
                bubble(text_content(label, true), from_me, last, theme).into_any_element()
            }
        };

        let group = SharedString::from(format!("msg-{}", m.id));
        let hover_button = |id: String, icon: Glyph| {
            div()
                .id(SharedString::from(id))
                .flex_none()
                .size(px(26.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .hover(|s| s.bg(theme.search_bg))
                .child(glyph(icon, 14., theme.text_secondary))
        };
        let reply_target = m.clone();
        let reply_button = hover_button(format!("reply-{}", m.id), Glyph::Reply).on_click(cx.listener(
            move |this, _, window, cx| {
                let target = reply_target.clone();
                this.store.update(cx, |s, cx| s.set_replying_to(&target.chat_jid.clone(), Some(target), cx));
                Composer::focus(&this.composer, window, cx);
            },
        ));
        let react_id = m.id.clone();
        let react_button = hover_button(format!("react-{}", m.id), Glyph::Smile).on_click(cx.listener(
            move |this, _, _, cx| {
                this.picker_for = Some(react_id.clone());
                cx.notify();
            },
        ));

        // One compact header line: "Sender  ↩ Replied-to", instead of a name line plus a
        // separate reply line.
        let quoted_who = m.quoted.as_ref().map(|q| {
            if q.from_me { "You".to_string() } else { q.sender_name.clone().unwrap_or_else(|| "message".into()) }
        });
        let header = (show_sender || quoted_who.is_some() || m.forwarded).then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(if gutter { 50. } else { 12. }))
                .pr(px(12.))
                .pb(px(2.))
                .text_size(px(11.))
                .text_color(theme.text_secondary)
                .when(show_sender, |d| d.child(div().font_weight(FontWeight::MEDIUM).child(sender.clone())))
                .when_some(quoted_who.clone(), |d, who| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(3.))
                            .child(glyph(Glyph::Reply, 10., theme.text_secondary))
                            .child(who),
                    )
                })
                .when(m.forwarded, |d| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(3.))
                            .italic()
                            .child(glyph(Glyph::Forward, 10., theme.text_secondary))
                            .child("Forwarded"),
                    )
                })
        });

        let reactions: Vec<_> = album.iter().flat_map(|m| m.reactions.iter().cloned()).collect();
        let picker_open = self.picker_for.as_deref() == Some(m.id.as_str());
        let flash = self.highlight.as_ref().filter(|(id, _)| *id == m.id).map(|(_, g)| *g);
        let bubble_area = div()
            .relative()
            // Shrinks in a narrow (mini) window so long text wraps instead of overflowing.
            .min_w_0()
            .when(!reactions.is_empty(), |d| d.pt(px(14.)))
            .child(content)
            .when(!reactions.is_empty(), |d| d.child(reaction_pill(&reactions, from_me, theme)))
            .when_some(flash, |d, generation| {
                // A brief glow over the bubble the transcript jumped to.
                let glow = theme.accent;
                d.child(
                    div()
                        .absolute()
                        .inset(px(-3.))
                        .rounded(px(BUBBLE_RADIUS + 3.))
                        .border_2()
                        .border_color(glow)
                        .bg(glow.opacity(0.18))
                        .with_animation(
                            SharedString::from(format!("flash-{}-{generation}", m.id)),
                            Animation::new(HIGHLIGHT).with_easing(ease_in_out),
                            |d, t| d.opacity(if t < 0.5 { 1. } else { 2. * (1. - t) }),
                        ),
                )
            })
            .when(picker_open && !m.deleted, |d| d.child(self.reaction_picker(m, from_me, theme, cx)));

        let right_clicked = m.clone();
        let row = div()
            .id(SharedString::from(format!("row-{}", m.id)))
            .group(group.clone())
            .relative()
            .w_full()
            .px(px(16.))
            .pt(px(if first { 8. } else { 2. }))
            .flex()
            .flex_col()
            .when(from_me, |d| d.items_end())
            .when_some(header, |d, h| d.child(h))
            .when_some(m.quoted.clone(), |d, q| d.child(self.quote_preview(&q, from_me, gutter, theme, cx)))
            .child(
                div()
                    .max_w_full()
                    .flex()
                    .items_end()
                    .gap(px(10.))
                    .when(from_me, |d| d.flex_row_reverse())
                    .when(gutter, |d| {
                        d.child(if last {
                            let (store, jid) = (self.store.clone(), m.sender_jid.clone());
                            div()
                                .id(SharedString::from(format!("sender-{}", m.id)))
                                .cursor_pointer()
                                .child(avatar(&sender, sender_photo.clone(), 28., theme))
                                .on_click(move |_, _, cx| store.update(cx, |s, cx| s.toggle_profile(jid.clone(), cx)))
                                .into_any_element()
                        } else {
                            div().size(px(28.)).flex_none().into_any_element()
                        })
                    })
                    .child(bubble_area)
                    .when(from_me && m.status == DeliveryStatus::Failed, |d| {
                        let target = m.clone();
                        d.child(
                            div()
                                .id(SharedString::from(format!("retry-{}", m.id)))
                                .flex_none()
                                .mb(px(6.))
                                .size(px(20.))
                                .rounded_full()
                                .bg(theme.failed)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(white())
                                .text_size(px(13.))
                                .font_weight(FontWeight::BOLD)
                                .cursor_pointer()
                                .child("!")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let target = target.clone();
                                    this.store.update(cx, |s, cx| s.retry_send(&target, cx));
                                })),
                        )
                    })
                    .when(!m.deleted, |d| {
                        d.child(div().flex_none().pb(px(4.)).flex().gap(px(2.)).child(react_button).child(reply_button))
                    }),
            )
            // "★ Starred · Edited"; with a status line below, it joins that ("Edited · Read 8:36 PM").
            .when(!note.is_empty() && status.is_none(), |d| {
                d.child(
                    div()
                        .pt(px(2.))
                        .px(px(if gutter { 50. } else { 6. }))
                        .text_size(px(11.))
                        .text_color(theme.text_secondary)
                        .child(note.clone()),
                )
            })
            .when_some(status, |d, (label, failed)| {
                let retry_target = m.clone();
                d.child(
                    div()
                        .id(SharedString::from(format!("status-{}", m.id)))
                        .pt(px(3.))
                        .pr(px(4.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if *failed { theme.failed } else { theme.text_secondary })
                        .child(if note.is_empty() { label.clone() } else { format!("{note} · {label}") })
                        .when(*failed, |d| {
                            d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                let target = retry_target.clone();
                                this.store.update(cx, |s, cx| s.retry_send(&target, cx));
                            }))
                        }),
                )
            })
            .when(!readers.is_empty(), |d| d.child(self.read_heads(&m.id, readers, theme, cx)))
            // Right-click opens the tapback tray and the message's menu, like Messages.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.open_message_menu(&right_clicked, ev.position, window, cx);
                }),
            );

        // Messages sent from this window rise into place from the composer.
        match self.store.read(cx).sent_at(&m.id) {
            Some(_) => row
                .with_animation(
                    SharedString::from(format!("sent-{}", m.id)),
                    Animation::new(Duration::from_millis(420)).with_easing(ease_out_quint()),
                    |d, t| d.top(px(46. * (1. - t))).opacity(0.3 + 0.7 * t),
                )
                .into_any_element(),
            None => row.into_any_element(),
        }
    }

    /// Messenger-style "seen" heads: a tiny picture of each group member who has read up
    /// to this message, their names on hover.
    fn read_heads(&self, id: &str, readers: &[(String, String)], theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        const SIZE: f32 = 15.;
        const MAX: usize = 6;
        // With more than fit, the last place says how many more.
        let shown = if readers.len() > MAX { MAX - 1 } else { readers.len() };
        let tip = format!("Read by {}", names_label(readers));
        div()
            .id(SharedString::from(format!("read-{id}")))
            .pt(px(3.))
            .pr(px(2.))
            .flex()
            .items_center()
            .gap(px(2.))
            .children(readers[..shown].iter().map(|(jid, name)| {
                div()
                    .rounded_full()
                    .border_1()
                    .border_color(theme.window_bg)
                    .child(avatar(name, self.photo_for(jid, cx), SIZE, theme))
            }))
            .when(readers.len() > shown, |d| {
                d.child(
                    div()
                        .pl(px(2.))
                        .text_size(px(10.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_secondary)
                        .child(format!("+{}", readers.len() - shown)),
                )
            })
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
            .into_any_element()
    }

    fn photo_for(&self, jid: &str, cx: &mut Context<Self>) -> Option<Arc<Image>> {
        if let Some(p) = self.store.read(cx).avatar_cached(jid) {
            return Some(p);
        }
        let (store, jid) = (self.store.clone(), jid.to_string());
        cx.defer(move |cx| store.update(cx, |s, _| s.request_avatar(&jid)));
        None
    }

    /// iMessage-style reply context: the original message, faded and shrunk to one line,
    /// just above the reply. (Who it replies to is shown on the header line.)
    fn quote_preview(
        &self,
        q: &QuotedMessage,
        reply_from_me: bool,
        gutter: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (bg, fg) = if q.from_me { (theme.bubble_out, theme.bubble_out_text) } else { (theme.bubble_in, theme.bubble_in_text) };
        let id = q.id.clone();
        div()
            .flex()
            .flex_col()
            .map(|d| if reply_from_me { d.items_end() } else { d.items_start() })
            .when(gutter, |d| d.pl(px(38.)))
            .pb(px(2.))
            .child(
                div()
                    .id(SharedString::from(format!("quote-{id}")))
                    .max_w(px(320.))
                    .px(px(10.))
                    .py(px(3.))
                    .rounded(px(12.))
                    .bg(bg)
                    .text_color(fg)
                    .opacity(0.5)
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .truncate()
                    .cursor_pointer()
                    .child(q.preview.replace('\n', " "))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(id.clone(), None, true, cx))),
            )
    }

    /// The tapback tray: six quick reactions and Reply, floating above the bubble.
    fn reaction_picker(&self, m: &ChatMessage, from_me: bool, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        const QUICK: [&str; 6] = ["❤️", "👍", "😂", "😮", "😢", "🙏"];
        let mine = m.reactions.iter().find(|r| r.from_me).map(|r| r.emoji.clone());
        let tray = crate::chrome::surface(div(), theme)
            .flex()
            .items_center()
            .gap(px(2.))
            .p(px(4.))
            .rounded_full()
            .shadow_md()
            .children(QUICK.iter().map(|emoji| {
                let picked = mine.as_deref() == Some(*emoji);
                let target = m.clone();
                div()
                    .id(SharedString::from(format!("pick-{}-{emoji}", m.id)))
                    .size(px(32.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(19.))
                    .when(picked, |d| d.bg(theme.accent.opacity(0.35)))
                    .hover(|s| s.bg(theme.search_bg))
                    .child(*emoji)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |s, _| s.react(&target, emoji));
                        this.close_menu(cx);
                    }))
            }))
            .child(div().w(px(1.)).h(px(20.)).mx(px(2.)).bg(theme.separator))
            .child({
                let target = m.clone();
                div()
                    .id(SharedString::from(format!("pick-reply-{}", m.id)))
                    .size(px(32.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|s| s.bg(theme.search_bg))
                    .child(glyph(Glyph::Reply, 15., theme.text_secondary))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let target = target.clone();
                        this.store.update(cx, |s, cx| s.set_replying_to(&target.chat_jid.clone(), Some(target), cx));
                        this.close_menu(cx);
                        Composer::focus(&this.composer, window, cx);
                    }))
            })
            .when(Store::can_edit(m), |tray| {
                let target = m.clone();
                tray.child(
                    div()
                        .id(SharedString::from(format!("pick-edit-{}", m.id)))
                        .size(px(32.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme.search_bg))
                        .child(glyph(Glyph::Pencil, 15., theme.text_secondary))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.close_menu(cx);
                            Composer::start_edit(&this.composer, target.clone(), window, cx);
                        })),
                )
            })
            .occlude();
        deferred(
            div()
                .absolute()
                .bottom_full()
                .mb(px(6.))
                .map(|d| if from_me { d.right_0() } else { d.left_0() })
                .child(tray),
        )
        .with_priority(1)
    }

    /// Right-click: the tapback tray over the bubble and the message's menu (native), like
    /// Messages. The tray stays after the menu closes, for a reaction.
    fn open_message_menu(&mut self, m: &ChatMessage, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_for = Some(m.id.clone());
        cx.notify();
        let this = cx.entity().downgrade();
        let act = move |f: fn(&mut Self, ChatMessage, &mut Window, &mut Context<Self>), target: &ChatMessage| {
            let (this, target) = (this.clone(), target.clone());
            move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| {
                    this.close_menu(cx);
                    f(this, target, window, cx);
                });
            }
        };
        let mut items = Vec::new();
        if !m.deleted {
            items.push(Item::action(
                "Reply",
                act(
                    |this, target, window, cx| {
                        this.store.update(cx, |s, cx| s.set_replying_to(&target.chat_jid.clone(), Some(target), cx));
                        Composer::focus(&this.composer, window, cx);
                    },
                    m,
                ),
            ));
        }
        if copy_text(m).is_some() {
            items.push(Item::action(
                "Copy",
                act(
                    |_, target, _, cx| {
                        if let Some(text) = copy_text(&target) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    },
                    m,
                ),
            ));
        }
        if !m.deleted {
            let files = crate::media_actions::file_items(m);
            if !files.is_empty() {
                if !items.is_empty() {
                    items.push(Item::Separator);
                }
                items.extend(files);
                items.push(Item::Separator);
            }
        }
        if Store::can_forward(m) {
            items.push(Item::action("Forward…", act(|this, target, window, cx| this.open_forward(target, window, cx), m)));
        }
        if !m.deleted {
            let starred = m.starred;
            items.push(Item::action(
                if starred { "Unstar" } else { "Star" },
                act(
                    if starred {
                        |this: &mut Self, target: ChatMessage, _: &mut Window, cx: &mut Context<Self>| {
                            this.store.update(cx, |s, _| s.star_message(&target, false))
                        }
                    } else {
                        |this: &mut Self, target: ChatMessage, _: &mut Window, cx: &mut Context<Self>| {
                            this.store.update(cx, |s, _| s.star_message(&target, true))
                        }
                    },
                    m,
                ),
            ));
        }
        if Store::can_pin(m) {
            let pinned = self.store.read(cx).is_pinned(m);
            if pinned {
                items.push(Item::action("Unpin", act(|this, target, _, cx| this.store.update(cx, |s, _| s.pin_message(&target, None)), m)));
            } else {
                let view = cx.entity().downgrade();
                let pin_for = |secs: u32| {
                    let (view, target) = (view.clone(), m.clone());
                    move |_: &mut Window, cx: &mut App| {
                        let _ = view.update(cx, |this, cx| {
                            this.close_menu(cx);
                            this.store.update(cx, |s, _| s.pin_message(&target, Some(secs)));
                        });
                    }
                };
                items.push(Item::submenu(
                    "Pin",
                    vec![
                        Item::action("For 24 Hours", pin_for(86_400)),
                        Item::action("For 7 Days", pin_for(604_800)),
                        Item::action("For 30 Days", pin_for(2_592_000)),
                    ],
                ));
            }
        }
        if Store::can_edit(m) {
            items.push(Item::action(
                "Edit",
                act(|this, target, window, cx| Composer::start_edit(&this.composer, target, window, cx), m),
            ));
        }
        if m.from_me && !m.deleted && self.binding.fixed().is_none() {
            items.push(Item::action(
                "Info",
                act(
                    |this, target, _, cx| {
                        this.store.update(cx, |s, cx| s.open_message_info(target.chat_jid.clone(), target.id.clone(), cx))
                    },
                    m,
                ),
            ));
        }
        // A queued send would still go out after its row is gone: no Delete while sending.
        let sending = m.from_me && m.status == DeliveryStatus::Pending;
        if !sending {
            if !items.is_empty() {
                items.push(Item::Separator);
            }
            items.push(Item::action("Delete…", act(|this, target, window, cx| this.confirm_delete(target, window, cx), m)));
        }
        if !items.is_empty() {
            crate::native_menu::show(items, at, window, cx);
        }
    }

    /// "Delete Message?" alert (native): for everyone (my recent messages) and/or just for me.
    fn confirm_delete(&mut self, m: ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
        let everyone = Store::can_delete_for_everyone(&m);
        let detail = if everyone {
            "Delete it for everyone in this chat, or only on your devices."
        } else {
            "It will be deleted on your devices. Others in the chat can still see it."
        };
        // Return picks the first: the one that can be undone least, so not that.
        let answers: &[PromptButton] = if everyone {
            &[PromptButton::new("Delete for Me"), PromptButton::new("Delete for Everyone"), PromptButton::cancel("Cancel")]
        } else {
            &[PromptButton::new("Delete for Me"), PromptButton::cancel("Cancel")]
        };
        let answer = window.prompt(PromptLevel::Warning, "Delete Message?", Some(detail), answers, cx);
        cx.spawn(async move |this, cx| {
            let for_everyone = match answer.await {
                Ok(0) => false,
                Ok(1) if everyone => true,
                _ => return,
            };
            let _ = this.update(cx, |this, cx| this.store.read(cx).delete_message(&m, for_everyone));
        })
        .detach();
    }

    /// Who's typing in this chat (JID, display name), for group typing labels.
    fn typists(&self, cx: &App) -> Vec<(String, String)> {
        let Some(jid) = &self.chat_jid else { return Vec::new() };
        self.store
            .read(cx)
            .typists
            .get(jid)
            .map(|list| {
                list.iter()
                    .map(|(j, n)| (j.clone(), n.clone().unwrap_or_else(|| j.split('@').next().unwrap_or("").to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn typing_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let typists = if self.is_group { self.typists(cx) } else { Vec::new() };
        let first = typists.first().cloned();
        let photo = first.as_ref().and_then(|(jid, _)| self.photo_for(jid, cx));
        div()
            .pt(px(8.))
            .px(px(16.))
            .flex()
            .flex_col()
            .when(!typists.is_empty(), |d| {
                d.child(
                    div()
                        .pl(px(50.))
                        .pb(px(2.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_secondary)
                        .child(names_label(&typists)),
                )
            })
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(10.))
                    .when(self.is_group, |d| {
                        d.child(match &first {
                            Some((_, name)) => avatar(name, photo, 28., theme),
                            None => div().size(px(28.)).flex_none().into_any_element(),
                        })
                    })
                    .child(typing_bubble(theme)),
            )
            .into_any_element()
    }

    /// Photos sent together: a grid of up to four (the last one counting the rest),
    /// each opening in the photo viewer.
    fn album_content(&self, album: &[ChatMessage], theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        const GAP: f32 = 3.;
        // Rounded containers don't clip their children, so each photo rounds its own
        // corners: the album's outer ones like a bubble, the inner ones slightly.
        let r = |outer: bool| px(if outer { BUBBLE_RADIUS } else { 4. });
        let half = (IMAGE_MAX_W - GAP) / 2.;
        let more = album.len().saturating_sub(4);
        let mini = self.binding.fixed().map(String::from);
        // `corners`: which of top-left, top-right, bottom-right, bottom-left are the album's.
        let mut tile = |m: &ChatMessage, w: f32, h: f32, more: usize, [tl, tr, br, bl]: [bool; 4]| {
            let round = |d: Div| d.rounded_tl(r(tl)).rounded_tr(r(tr)).rounded_br(r(br)).rounded_bl(r(bl));
            let MessageBody::Image(media) = &m.body else { return div().into_any_element() };
            if media.path.is_none() && media.state == MediaState::Idle {
                let (store, chat, id) = (self.store.clone(), m.chat_jid.clone(), m.id.clone());
                cx.defer(move |cx| store.update(cx, |s, _| s.request_media(&chat, &id)));
            }
            // The tile's aspect ratio, not the photo's: an image's own ratio otherwise wins
            // and a wide or tall photo spills out of its tile instead of being cropped.
            let photo = |src: ImageSource| {
                img(src).w(px(w)).h(px(h)).aspect_ratio(w / h).object_fit(ObjectFit::Cover).rounded_tl(r(tl)).rounded_tr(r(tr)).rounded_br(r(br)).rounded_bl(r(bl))
            };
            let picture: AnyElement = match (&media.path, self.thumbs.get(&m.id)) {
                (Some(path), _) => photo(path.clone().into()).into_any_element(),
                (None, Some(thumb)) => photo(thumb.clone().into()).into_any_element(),
                (None, None) => round(div().size_full().bg(theme.bubble_in)).into_any_element(),
            };
            let (path, store, mini, id) = (media.path.clone(), self.store.clone(), mini.clone(), m.id.clone());
            let fetch = Fetch::of(media.path.is_some(), media.state, self.store.read(cx).download_progress(&m.chat_jid, &id));
            let chat = m.chat_jid.clone();
            div()
                .id(SharedString::from(format!("img-{}", m.id)))
                .relative()
                .flex_none()
                .w(px(w))
                .h(px(h))
                .cursor_pointer()
                .child(picture)
                .when(!fetch.ready() && more == 0, |d| d.child(round(crate::download::not_downloaded(fetch, 36.))))
                .when(more > 0, |d| {
                    d.child(
                        round(div())
                            .absolute()
                            .inset_0()
                            .bg(black().opacity(0.45))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(white())
                            .text_size(px(24.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!("+{more}")),
                    )
                })
                .on_click(move |_, _, cx| match path.clone() {
                    Some(path) => store.update(cx, |s, cx| s.open_lightbox_in(path, Some(id.clone()), mini.clone(), cx)),
                    None if fetch == Fetch::Failed => store.update(cx, |s, _| s.request_media(&chat, &id)),
                    None => {}
                })
                .into_any_element()
        };
        let row = || div().flex().gap(px(GAP));
        let grid = match album {
            [a, b] => vec![row()
                .child(tile(a, half, half * 1.3, 0, [true, false, false, true]))
                .child(tile(b, half, half * 1.3, 0, [false, true, true, false]))],
            [a, b, c] => vec![
                row().child(tile(a, IMAGE_MAX_W, half, 0, [true, true, false, false])),
                row()
                    .child(tile(b, half, half, 0, [false, false, false, true]))
                    .child(tile(c, half, half, 0, [false, false, true, false])),
            ],
            _ => vec![
                row()
                    .child(tile(&album[0], half, half, 0, [true, false, false, false]))
                    .child(tile(&album[1], half, half, 0, [false, true, false, false])),
                row()
                    .child(tile(&album[2], half, half, 0, [false, false, false, true]))
                    .child(tile(&album[3], half, half, more, [false, false, true, false])),
            ],
        };
        div()
            .w(px(IMAGE_MAX_W))
            .flex()
            .flex_col()
            .gap(px(GAP))
            .children(grid)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn image_content(
        &self,
        m: &ChatMessage,
        media: &ImageMedia,
        from_me: bool,
        last: bool,
        found: Option<(&str, bool)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (w, h) = image_box(media.width, media.height);
        if media.path.is_none() && media.state == MediaState::Idle {
            let (store, chat, id) = (self.store.clone(), m.chat_jid.clone(), m.id.clone());
            cx.defer(move |cx| store.update(cx, |s, _| s.request_media(&chat, &id)));
        }
        let picture: AnyElement = match (&media.path, self.thumbs.get(&m.id)) {
            (Some(path), _) => {
                img(path.clone()).size_full().rounded(px(BUBBLE_RADIUS)).object_fit(ObjectFit::Cover).into_any_element()
            }
            (None, Some(thumb)) => {
                img(thumb.clone()).size_full().rounded(px(BUBBLE_RADIUS)).object_fit(ObjectFit::Cover).into_any_element()
            }
            (None, None) => div().size_full().bg(theme.bubble_in).into_any_element(),
        };
        let (path, store, mini, id) = (media.path.clone(), self.store.clone(), self.binding.fixed().map(String::from), m.id.clone());
        let fetch = Fetch::of(media.path.is_some(), media.state, self.store.read(cx).download_progress(&m.chat_jid, &id));
        let chat = m.chat_jid.clone();
        let frame = div()
            .id(SharedString::from(format!("img-{}", m.id)))
            .relative()
            .w(px(w))
            .h(px(h))
            .rounded(px(BUBBLE_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.separator)
            .cursor_pointer()
            .child(picture)
            .when(!fetch.ready(), |d| d.child(crate::download::not_downloaded(fetch, 44.)))
            .on_click(move |_, _, cx| match path.clone() {
                Some(path) => store.update(cx, |s, cx| s.open_lightbox_in(path, Some(id.clone()), mini.clone(), cx)),
                None if fetch == Fetch::Failed => store.update(cx, |s, _| s.request_media(&chat, &id)),
                None => {}
            });
        match media.caption.as_deref().filter(|c| !c.trim().is_empty()) {
            None => frame.into_any_element(),
            Some(caption) => div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .when(from_me, |d| d.items_end())
                .child(frame)
                .child(bubble(
                    found_text(format!("caption-{}", m.id).into(), caption, found, &m.mentions, from_me, theme),
                    from_me,
                    last,
                    theme,
                ))
                .into_any_element(),
        }
    }
}

/// iMessage tapback badge on the bubble's top corner: up to three distinct emojis plus a
/// count; blue when one of them is mine.
fn reaction_pill(reactions: &[wa_backend::Reaction], from_me: bool, theme: &Theme) -> impl IntoElement {
    let mut distinct: Vec<&str> = Vec::new();
    for r in reactions {
        if !distinct.contains(&r.emoji.as_str()) {
            distinct.push(&r.emoji);
        }
    }
    let mine = reactions.iter().any(|r| r.from_me);
    let (bg, fg) = if mine { (theme.accent, white()) } else { (theme.bubble_in, theme.text_secondary) };
    div()
        .absolute()
        .top_0()
        .map(|d| if from_me { d.left(px(-10.)) } else { d.right(px(-10.)) })
        .h(px(24.))
        .px(px(6.))
        .rounded_full()
        .bg(bg)
        .border_2()
        .border_color(theme.window_bg)
        .flex()
        .items_center()
        .gap(px(1.))
        .text_size(px(13.))
        .children(distinct.into_iter().take(3).map(|e| div().child(e.to_string())))
        .when(reactions.len() > 1, |d| {
            d.child(div().pl(px(2.)).text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(fg).child(reactions.len().to_string()))
        })
}

/// Text (or a caption) to copy: what the bubble shows in words.
/// What's open beside the conversation.
enum SidePanel {
    Profile(String),
    MessageInfo(String, String),
}

pub fn copy_text(m: &ChatMessage) -> Option<String> {
    if m.deleted {
        return None;
    }
    let text = match &m.body {
        MessageBody::Rich(r) => Some(r.body.as_str()),
        _ => find_text(m),
    };
    text.filter(|t| !t.trim().is_empty()).map(str::to_string)
}

/// The words find-in-chat searches: text and captions (like the store's search).
fn find_text(m: &ChatMessage) -> Option<&str> {
    if m.deleted {
        return None;
    }
    match &m.body {
        MessageBody::Text(t) => Some(t),
        MessageBody::Image(i) => i.caption.as_deref(),
        MessageBody::Video(v) => v.caption.as_deref(),
        MessageBody::Document(d) => d.caption.as_deref(),
        MessageBody::Rich(r) => Some(&r.body),
        _ => None,
    }
}

/// "Omar", "Omar and Sara", "Omar and 2 others".
fn names_label(typists: &[(String, String)]) -> String {
    match typists {
        [] => String::new(),
        [(_, a)] => a.clone(),
        [(_, a), (_, b)] => format!("{a} and {b}"),
        [(_, a), rest @ ..] => format!("{a} and {} others", rest.len()),
    }
}

/// @mentions: bold, tinted on incoming bubbles (white on my blue ones), like Messages.
fn mention_color(from_me: bool, theme: &Theme) -> Hsla {
    if from_me { theme.bubble_out_text } else { theme.accent }
}

/// Bubble text with @mentions shown by name, links clickable (see `links`) and
/// find-in-chat matches highlighted (the current match stronger). Matches are found in
/// the text as shown.
fn found_text(
    id: SharedString,
    text: &str,
    found: Option<(&str, bool)>,
    mentions: &[wa_backend::Mention],
    from_me: bool,
    theme: &Theme,
) -> Div {
    let (text, mentioned) = crate::mentions::display(text, mentions);
    let linked = crate::links::linkify(&text, &mentioned);
    let mentioned: Vec<_> = mentioned.iter().map(|r| linked.map_range(r)).collect();
    let (text, links) = (linked.text, linked.links);
    let matches = found.map(|(query, _)| match_ranges(&text, query)).unwrap_or_default();
    if matches.is_empty() && mentioned.is_empty() && links.is_empty() {
        // Plain text can be selected (drag, ⌘C). Links and mentions need clicks, which
        // the selectable text doesn't take; their menu's Copy covers them.
        let selection = if from_me { white().opacity(0.35) } else { theme.accent.opacity(0.35) };
        return div()
            .text_size(px(14.))
            .line_height(px(19.))
            .child(gpui_kit::base::SelectableText::new(id, text).selection_color(selection));
    }
    let current = found.is_some_and(|(_, current)| current);
    let find_style = HighlightStyle {
        background_color: Some(if current { hsla(48. / 360., 1., 0.55, 1.) } else { hsla(52. / 360., 1., 0.7, 0.75) }),
        color: Some(black()),
        ..Default::default()
    };
    // Links: white on my blue bubbles, tinted on gray ones, like Messages; the warning
    // mark of a suspicious one in amber.
    let link_color = if from_me { theme.bubble_out_text } else { theme.accent };
    let warn_color = if from_me { hsla(45. / 360., 1., 0.72, 1.) } else { hsla(32. / 360., 0.95, 0.48, 1.) };
    let link_ranges: Vec<_> = links.iter().map(|l| l.range.clone()).collect();
    let mark_ranges: Vec<_> = links.iter().filter_map(|l| l.mark.clone()).collect();
    // Cut at every boundary so a match inside a mention or link gets both looks.
    let mut cuts: Vec<usize> =
        matches.iter().chain(&mentioned).chain(&link_ranges).chain(&mark_ranges).flat_map(|r| [r.start, r.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let inside = |ranges: &[std::ops::Range<usize>], seg: &std::ops::Range<usize>| {
        ranges.iter().any(|r| r.start <= seg.start && seg.end <= r.end)
    };
    let highlights: Vec<_> = cuts
        .windows(2)
        .map(|w| w[0]..w[1])
        .filter_map(|seg| {
            let link = links.iter().find(|l| l.range.start <= seg.start && seg.end <= l.range.end);
            let mut style = match (inside(&matches, &seg), inside(&mentioned, &seg)) {
                (true, m) => HighlightStyle { font_weight: m.then_some(FontWeight::SEMIBOLD), ..find_style },
                (false, true) => HighlightStyle {
                    color: Some(mention_color(from_me, theme)),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
                (false, false) if link.is_some() => HighlightStyle { color: Some(link_color), ..Default::default() },
                (false, false) if inside(&mark_ranges, &seg) => {
                    HighlightStyle { color: Some(warn_color), ..Default::default() }
                }
                (false, false) => return None,
            };
            if let Some(link) = link {
                let color = style.color.unwrap_or(link_color);
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(if link.suspicious() { warn_color } else { color }),
                    wavy: link.suspicious(),
                });
            }
            Some((seg, style))
        })
        .collect();
    let styled = StyledText::new(text).with_highlights(highlights);
    let body = div().text_size(px(14.)).line_height(px(19.));
    if links.is_empty() {
        return body.child(styled);
    }
    body.child(crate::links::interactive(id, styled, links))
}

fn text_content(text: &str, italic: bool) -> Div {
    div()
        .text_size(px(14.))
        .line_height(px(19.))
        .when(italic, |d| d.italic().opacity(0.85))
        .child(text.to_string())
}

/// Display size for an image bubble, keeping aspect ratio inside the max box.
pub fn image_box(w: Option<u32>, h: Option<u32>) -> (f32, f32) {
    let (w, h) = match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f32, h as f32),
        _ => (4., 3.),
    };
    let scale = (IMAGE_MAX_W / w).min(IMAGE_MAX_H / h);
    ((w * scale).max(120.), (h * scale).max(80.))
}

#[allow(clippy::too_many_arguments)]
fn voice_content(
    store: &Entity<Store>,
    m: &ChatMessage,
    media: &VoiceMedia,
    from_me: bool,
    last: bool,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<Conversation>,
) -> AnyElement {
    let playback = AudioPlayer::state(cx, &m.id);
    let playing = playback.is_some_and(|(_, p)| p);
    if playing || AudioPlayer::busy(cx) {
        window.request_animation_frame();
    }
    let duration = media.duration_secs.max(1) as f32;
    let progress = playback.map_or(0., |(pos, _)| (pos.as_secs_f32() / duration).clamp(0., 1.));
    let shown_secs = match playback {
        Some((pos, _)) => pos.as_secs() as u32,
        None => media.duration_secs,
    };
    let (fg, played, unplayed) = if from_me {
        (theme.bubble_out_text, white(), white().opacity(0.45))
    } else {
        (theme.bubble_in_text, theme.accent, theme.waveform_in)
    };

    // As many bars as fit beside the play button and time: fewer in a narrow (mini) window.
    // Row padding, hover buttons, group avatar, bubble padding, play button, gaps, time.
    let gutter = if !from_me && m.chat_jid.ends_with("@g.us") { 38. } else { 0. };
    let room = f32::from(window.viewport_size().width) - 96. - gutter - 24. - 28. - 18. - 34.;
    let bars = waveform_bars(&media.waveform, ((room / 4.5) as usize).clamp(12, 30));
    let fetch = Fetch::of(media.path.is_some(), media.state, store.read(cx).download_progress(&m.chat_jid, &m.id));
    let (path, id, chat) = (media.path.clone(), m.id.clone(), m.chat_jid.clone());
    // Play once it's here; until then a down arrow, ringed by the progress while it comes.
    let icon = match fetch {
        Fetch::Ready if playing => glyph(Glyph::Pause, 13., white()),
        Fetch::Ready => glyph(Glyph::Play, 13., white()),
        _ => glyph(Glyph::ArrowDown, 14., white()),
    };
    let button = div()
        .id(SharedString::from(format!("play-{}", m.id)))
        .relative()
        .flex_none()
        .size(px(28.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .bg(if from_me { white().opacity(0.22) } else { theme.accent })
        .child(icon)
        .when_some(match fetch { Fetch::Downloading(p) => Some(p), _ => None }, |d, p| {
            d.child(crate::download::ring(p, 2., white()).inset(px(1.5)))
        })
        .on_click(cx.listener(move |this, _, _, cx| match &path {
            Some(path) => AudioPlayer::toggle(cx, &id, path),
            None if !matches!(fetch, Fetch::Downloading(_)) => this.store.update(cx, |s, _| s.request_media(&chat, &id)),
            None => {}
        }));

    let content = div()
        .flex()
        .items_center()
        .gap(px(9.))
        .py(px(2.))
        .child(button)
        .child(
            div().flex_none().flex().items_center().gap(px(2.)).h(px(26.)).children(bars.iter().enumerate().map(|(i, v)| {
                let done = (i as f32 + 0.5) / bars.len() as f32 <= progress;
                div()
                    .flex_none()
                    .w(px(2.5))
                    .h(px(4. + 20. * v))
                    .rounded_full()
                    .bg(if done { played } else { unplayed })
            })),
        )
        .child(
            div()
                .flex_none()
                .min_w(px(30.))
                .text_size(px(12.))
                .text_color(fg.opacity(0.8))
                .child(format!("{}:{:02}", shown_secs / 60, shown_secs % 60)),
        );
    if fetch == Fetch::Missing {
        // Fetch voice notes as they scroll into view so play starts instantly.
        let store = store.clone();
        let (chat, id) = (m.chat_jid.clone(), m.id.clone());
        cx.defer(move |cx| store.update(cx, |s, _| s.request_media(&chat, &id)));
    }
    bubble(content, from_me, last, theme).into_any_element()
}

/// Resample WhatsApp's waveform (0..=100 samples) to `n` bars in 0..=1; a gentle
/// placeholder shape when the message has none.
fn waveform_bars(waveform: &[u8], n: usize) -> Vec<f32> {
    if waveform.is_empty() {
        return (0..n).map(|i| 0.25 + 0.2 * ((i as f32 * 0.9).sin().abs())).collect();
    }
    (0..n)
        .map(|i| {
            let a = i * waveform.len() / n;
            let b = ((i + 1) * waveform.len() / n).max(a + 1).min(waveform.len());
            let max = waveform[a..b].iter().copied().max().unwrap_or(0);
            (max as f32 / 100.).clamp(0.08, 1.)
        })
        .collect()
}

/// A Messages bubble. The tail is drawn with the classic two-shape trick: a bubble-colored
/// curl at the bottom corner, then a background-colored shape that carves its inner edge.
fn bubble(content: Div, from_me: bool, tail: bool, theme: &Theme) -> Div {
    let (bg, fg) = if from_me { (theme.bubble_out, theme.bubble_out_text) } else { (theme.bubble_in, theme.bubble_in_text) };
    div()
        .relative()
        .max_w(px(520.))
        .px(px(12.))
        .py(px(7.))
        .rounded(px(BUBBLE_RADIUS))
        .bg(bg)
        .text_color(fg)
        .child(content)
        .when(tail, |d| {
            // The carve only needs to reach as far as the curl does (6px); anything wider
            // paints window background over neighbours such as group-sender avatars.
            let curl = div().absolute().bottom_0().h(px(18.)).w(px(18.)).bg(bg);
            let carve = div().absolute().bottom_0().h(px(18.)).w(px(10.)).bg(theme.window_bg);
            if from_me {
                d.child(curl.right(px(-6.)).rounded_bl(px(16.)))
                    .child(carve.right(px(-10.)).rounded_bl(px(10.)))
            } else {
                d.child(curl.left(px(-6.)).rounded_br(px(16.)))
                    .child(carve.left(px(-10.)).rounded_br(px(10.)))
            }
        })
}

/// Messages' typing indicator: a small gray bubble with three dots pulsing in turn,
/// plus the little "thought" circles trailing off its corner.
fn typing_bubble(theme: &Theme) -> impl IntoElement {
    let dot = |i: usize| {
        div().size(px(7.)).rounded_full().bg(theme.text_secondary).with_animation(
            ("typing-dot", i),
            Animation::new(Duration::from_millis(1200)).repeat(),
            move |d, t| {
                // Each dot peaks a third of a cycle after the previous one.
                let phase = (t - i as f32 * 0.18).rem_euclid(1.);
                let pulse = (phase * std::f32::consts::TAU).sin().max(0.);
                d.opacity(0.35 + 0.65 * pulse)
            },
        )
    };
    div()
        .relative()
        .w(px(54.))
        // Same height and radius as a one-line text bubble (7pt padding × 2 + 19pt line).
        .h(px(33.))
        .rounded(px(BUBBLE_RADIUS))
        .bg(theme.bubble_in)
        .flex()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .child(dot(0))
        .child(dot(1))
        .child(dot(2))
        .child(div().absolute().left(px(-2.)).bottom(px(-2.)).size(px(11.)).rounded_full().bg(theme.bubble_in))
        .child(div().absolute().left(px(-7.)).bottom(px(-8.)).size(px(5.)).rounded_full().bg(theme.bubble_in))
}

/// A dimmed layer over the conversation for a dialog; clicking it calls `dismiss`.
/// With glass, `el` goes into a surface above the glass panes (it covers them).
pub fn above_glass(glass_on: bool, id: &'static str, el: impl IntoElement) -> AnyElement {
    if glass_on { glass::above_glass(id, el).into_any_element() } else { el.into_any_element() }
}

/// Messages' soft scroll edge under the glass bars: the transcript fades into the window
/// background towards the top (or bottom) edge, so the glass reads over calm content.
pub fn edge_fade(top: bool, height: f32, theme: &Theme) -> impl IntoElement {
    let (solid, clear) = (theme.window_bg.opacity(0.92), theme.window_bg.opacity(0.));
    let fade = if top {
        linear_gradient(180., linear_color_stop(solid, 0.), linear_color_stop(clear, 1.))
    } else {
        linear_gradient(0., linear_color_stop(solid, 0.), linear_color_stop(clear, 1.))
    };
    div().absolute().left_0().right_0().h(px(height)).map(|d| if top { d.top_0() } else { d.bottom_0() }).bg(fade)
}

fn modal_backdrop(id: &'static str, dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .bg(black().opacity(0.18))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, dismiss)
}

/// "Jump to the latest", centered over the transcript just above the composer: a glass
/// circle with an arrow, which widens into a "3 new messages" pill when messages arrived
/// while scrolled up. Rises into place when it appears.
fn scroll_button(unseen: usize, bottom: f32, glass_on: bool, theme: &Theme, cx: &mut Context<Conversation>) -> impl IntoElement {
    const SIZE: f32 = 36.;
    let label = match unseen {
        0 => None,
        1 => Some("1 new message".to_string()),
        n if n > 99 => Some("99+ new messages".to_string()),
        n => Some(format!("{n} new messages")),
    };
    let arrow = if label.is_some() { theme.accent } else { theme.text };
    let hover = theme.search_bg;
    let content = div()
        .id("scroll-to-bottom")
        // Clicks stop here; the strip around the pill still reaches the transcript.
        .block_mouse_except_scroll()
        .h(px(SIZE))
        .when(label.is_none(), |d| d.w(px(SIZE)))
        .when(label.is_some(), |d| d.pl(px(12.)).pr(px(15.)))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(glyph(Glyph::ArrowDown, 15., arrow))
        .when_some(label, |d, label| {
            d.child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(label))
        })
        .on_click(cx.listener(|this, _, _, cx| this.scroll_to_bottom(cx)));
    let pill = if glass_on {
        glass::glass("scroll-to-bottom", SIZE / 2., content).into_any_element()
    } else {
        surface(content, theme).shadow_md().into_any_element()
    };
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom(px(bottom + 12.))
        .flex()
        .justify_center()
        .child(pill)
        .with_animation(
            "scroll-to-bottom-in",
            Animation::new(Duration::from_millis(280)).with_easing(ease_out_quint()),
            // Absolutely positioned, so moving it shifts nothing else.
            move |d, t| d.bottom(px(bottom + 4. + 8. * t)).opacity(t),
        )
}

/// Identity of each row for diffing: rows with equal keys are the same list item.
#[derive(PartialEq)]
enum RowKey {
    Separator(i64),
    Message { id: String, span: usize, first: bool, last: bool, show_name: bool, status: bool, readers: Vec<String> },
    Typing,
}

fn row_keys(rows: &[Row], messages: &[ChatMessage]) -> Vec<RowKey> {
    rows.iter()
        .map(|r| match r {
            Row::Separator(ts) => RowKey::Separator(*ts),
            Row::Message { ix, span, first, last, show_name, status, readers } => RowKey::Message {
                id: messages[*ix].id.clone(),
                span: *span,
                first: *first,
                last: *last,
                show_name: *show_name,
                status: status.is_some(),
                readers: readers.iter().map(|(jid, _)| jid.clone()).collect(),
            },
            Row::Typing => RowKey::Typing,
        })
        .collect()
}

/// `reads`: how far each group member has read (empty outside groups).
fn build_rows(messages: &[ChatMessage], typing: bool, reads: &[GroupRead]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(messages.len() + 8);
    let last_mine = messages.iter().rposition(|m| m.from_me && !m.deleted);
    let mut ix = 0;
    while ix < messages.len() {
        let span = album_span(messages, ix);
        // An album's first photo starts the row, its last one ends it.
        let (m, end) = (&messages[ix], &messages[ix + span - 1]);
        let prev = ix.checked_sub(1).map(|p| &messages[p]);
        let next = messages.get(ix + span);
        let separator = prev.is_none_or(|p| m.timestamp - p.timestamp > SEPARATOR_GAP_SECS);
        if separator {
            rows.push(Row::Separator(m.timestamp));
        }
        let same = |a: &ChatMessage, b: &ChatMessage| {
            a.from_me == b.from_me && a.sender_jid == b.sender_jid && (b.timestamp - a.timestamp).abs() <= GROUP_GAP_SECS
        };
        let new_sender = separator || prev.is_none_or(|p| !same(p, m));
        let first = new_sender || m.quoted.is_some();
        let next_separates = next.is_some_and(|n| n.timestamp - end.timestamp > SEPARATOR_GAP_SECS);
        let last = next.is_none_or(|n| next_separates || !same(end, n) || n.quoted.is_some());
        let mine = last_mine.is_some_and(|l| (ix..ix + span).contains(&l));
        let readers: Vec<(String, String)> = reads
            .iter()
            .filter(|r| messages[ix..ix + span].iter().any(|m| m.id == r.message_id))
            .map(|r| (r.jid.clone(), r.name.clone()))
            .collect();
        // In a group the heads say who read it; "Read" alone would only mean someone did.
        let status = (mine || (end.from_me && end.status == DeliveryStatus::Failed))
            .then(|| status_label(end))
            .filter(|_| readers.is_empty() || end.status == DeliveryStatus::Failed);
        let last = last || status.is_some() || !readers.is_empty();
        rows.push(Row::Message { ix, span, first, last, show_name: new_sender, status, readers });
        ix += span;
    }
    if typing {
        rows.push(Row::Typing);
    }
    rows
}

/// How many photos from `ix` on were sent together: one after another from the same
/// sender, each within [`ALBUM_GAP_SECS`] of the last, none captioned, replying or failed.
fn album_span(messages: &[ChatMessage], ix: usize) -> usize {
    let photo = |m: &ChatMessage| {
        !m.deleted
            && m.status != DeliveryStatus::Failed
            && matches!(&m.body, MessageBody::Image(i) if i.caption.as_deref().is_none_or(|c| c.trim().is_empty()))
    };
    if !photo(&messages[ix]) {
        return 1;
    }
    let mut end = ix;
    while let Some(n) = messages.get(end + 1) {
        let p = &messages[end];
        if !photo(n)
            || n.quoted.is_some()
            || n.from_me != p.from_me
            || n.sender_jid != p.sender_jid
            || n.timestamp - p.timestamp > ALBUM_GAP_SECS
        {
            break;
        }
        end += 1;
    }
    end - ix + 1
}

fn status_label(m: &ChatMessage) -> (String, bool) {
    match m.status {
        DeliveryStatus::Pending => ("Sending…".into(), false),
        DeliveryStatus::Sent => ("Sent".into(), false),
        DeliveryStatus::Delivered => ("Delivered".into(), false),
        DeliveryStatus::Read => (format!("Read {}", short_time(m.timestamp)), false),
        // Red, beside the bubble's red "!": that says it didn't go.
        DeliveryStatus::Failed => ("Try Again".into(), true),
    }
}

impl Render for Conversation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let store = self.store.read(cx);
        let bound = self.binding.chat(store);
        let Some(chat) = bound.as_ref().and_then(|j| store.chats.iter().find(|c| &c.jid == j)).cloned() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.window_bg)
                .text_color(theme.text_secondary)
                .child(if store.chats.is_empty() { "Waiting for chats…" } else { "Select a conversation" })
                .into_any_element();
        };
        let typing = store.typing.contains(&chat.jid);
        let typists = if self.is_group { self.typists(cx) } else { Vec::new() };
        let typing_label = match typists.as_slice() {
            _ if !typing => None,
            [] => Some("typing…".to_string()),
            t @ [_] => Some(format!("{} is typing…", names_label(t))),
            t => Some(format!("{} are typing…", names_label(t))),
        };
        let store = self.store.read(cx);
        let mini = self.binding.fixed().is_some();
        // A mini window has no room for the info panel.
        let side_panel = if mini {
            None
        } else if let Some((chat, id)) = store.message_info.clone() {
            Some(SidePanel::MessageInfo(chat, id))
        } else {
            store.profile_open.clone().map(SidePanel::Profile)
        };
        let history = store.history_of(&chat.jid);
        let loading_older = history.page_pending || history.fetching_remote;
        let header_photo = store.avatar_cached(&chat.jid);
        if header_photo.is_none() {
            let (s, jid) = (self.store.clone(), chat.jid.clone());
            cx.defer(move |cx| s.update(cx, |s, _| s.request_avatar(&jid)));
        }
        let view = cx.entity();
        let (store_handle, chat_jid) = (self.store.clone(), chat.jid.clone());
        let find_bar = self.find.as_ref().map(|f| f.bar.clone());
        let header_height = if mini { MINI_HEADER_HEIGHT } else { HEADER_HEIGHT };
        let pins: Vec<ChatMessage> = self
            .store
            .read(cx)
            .pins
            .get(&chat.jid)
            .into_iter()
            .flatten()
            // As the transcript has it now, if it's loaded (edits).
            .map(|p| self.messages.iter().find(|m| m.id == p.id).unwrap_or(p).clone())
            .filter(|p| !p.deleted)
            .collect();
        let find_height = if find_bar.is_some() { FIND_BAR_HEIGHT } else { 0. };
        let glass_on = glass::enabled(window);
        let pin_bar = (!pins.is_empty()).then(|| self.pin_bar(&pins, glass_on, &theme, cx));
        // On glass the bar floats with a little room around it.
        let pin_height = if glass_on { PIN_BAR_HEIGHT + 8. } else { PIN_BAR_HEIGHT };
        let top = header_height + find_height + if pin_bar.is_some() { pin_height } else { 0. };
        let bottom = self.composer.read(cx).height() + COMPOSER_GAP;
        let away = self.away.get();
        if !away {
            self.unseen = 0;
        }
        let unseen = self.unseen;
        if let Some(jid) = self.binding.fixed()
            && self.reported_away != away
        {
            self.reported_away = away;
            let (store, jid) = (self.store.clone(), jid.to_string());
            cx.defer(move |cx| store.update(cx, |s, cx| s.set_mini_at_latest(&jid, !away, cx)));
        }
        let header = self.header(&chat, typing_label, header_photo, glass_on, &theme);
        let forward = self.forward.as_ref().map(|(p, _)| p.clone());

        let pane = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .image_cache(self.media_cache.clone())
            // After the composer (whose own popups come first) lets it through.
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && this.on_escape(cx) {
                    cx.stop_propagation();
                }
            }))
            .bg(theme.window_bg)
            // Transcript fills the pane and scrolls under the floating header and composer.
            .child(
                list(self.list.clone(), move |ix, window, cx| {
                    // The first rows are laid out once they come within the list's
                    // overdraw of the viewport: prefetch the previous page.
                    if ix < 3 {
                        let (store, chat) = (store_handle.clone(), chat_jid.clone());
                        cx.defer(move |cx| store.update(cx, |s, cx| s.load_older(&chat, cx)));
                    }
                    let theme = Theme::for_window(window);
                    view.update(cx, |this, cx| this.render_row(ix, &theme, window, cx))
                })
                .size_full()
                .pt(px(top))
                .pb(px(bottom)),
            )
            .when(loading_older, |d| {
                d.child(
                    div().absolute().top(px(top + 10.)).left_0().right_0().flex().justify_center().child(
                        surface(div().px(px(10.)).py(px(4.)).rounded_full(), &theme)
                            .text_size(px(11.))
                            .text_color(theme.text_secondary)
                            .child(if history.fetching_remote {
                                "Loading earlier messages from your phone…"
                            } else {
                                "Loading earlier messages…"
                            }),
                    ),
                )
            })
            .when(glass_on, |d| d.child(edge_fade(true, top + 12., &theme)))
            .child(header)
            .when_some(find_bar, |d, bar| {
                d.child(div().absolute().top(px(header_height)).left_0().right_0().block_mouse_except_scroll().child(bar))
            })
            .when_some(pin_bar, |d, bar| {
                d.child(
                    div()
                        .absolute()
                        .top(px(header_height + find_height))
                        .left_0()
                        .right_0()
                        .when(glass_on, |d| d.px(px(12.)))
                        .block_mouse_except_scroll()
                        .child(bar),
                )
            })
            .when_some(self.notice.clone(), |d, text| {
                d.child(
                    div().absolute().top(px(top + 10.)).left_0().right_0().flex().justify_center().child(
                        surface(div().px(px(12.)).py(px(5.)).rounded_full(), &theme)
                            .shadow_sm()
                            .text_size(px(12.))
                            .text_color(theme.text)
                            .child(text),
                    ),
                )
            })
            .when(away, |d| d.child(scroll_button(unseen, bottom, glass_on, &theme, cx)))
            .when(glass_on, |d| d.child(edge_fade(false, bottom, &theme)))
            // Floating over the transcript: clicks on it stop there (scrolling goes through).
            .child(div().absolute().bottom_0().left_0().right_0().block_mouse_except_scroll().child(self.composer.clone()))
            // Files dragged over the conversation: a soft tint and outline; dropped, they're
            // attached. Doesn't occlude, so the transcript still scrolls and takes clicks.
            .child({
                let (tint, outline) = (theme.accent.opacity(0.07), theme.accent.opacity(0.75));
                div()
                    .id("drop-zone")
                    .absolute()
                    .inset(px(6.))
                    .rounded(px(14.))
                    .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(tint).border_2().border_color(outline))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        Composer::attach_files(&this.composer, paths.paths().to_vec(), cx);
                        Composer::focus(&this.composer, window, cx);
                    }))
            })
            // Clicking anywhere else closes the tapback tray / menu.
            .when(self.picker_for.is_some(), |d| {
                let close = |this: &mut Self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| this.close_menu(cx);
                d.child(
                    deferred(
                        div()
                            .id("menu-backdrop")
                            .absolute()
                            .inset_0()
                            .occlude()
                            .on_mouse_down(MouseButton::Left, cx.listener(close))
                            .on_mouse_down(MouseButton::Right, cx.listener(close))
                            .on_scroll(cx.listener(|this, _, _, cx| this.close_menu(cx))),
                    )
                    .with_priority(0),
                )
            })
            .when_some(forward, |d, picker| {
                d.child(above_glass(glass_on, "forward-backdrop", modal_backdrop("forward-backdrop", cx.listener(|this, _, window, cx| {
                    this.forward = None;
                    Composer::focus(&this.composer, window, cx);
                    cx.notify();
                })).child(picker)))
            });
        // Beside the conversation while that keeps a usable width; in a narrower window
        // the info panel slides over it instead of squeezing it.
        let room = f32::from(window.viewport_size().width) - crate::SIDEBAR_WIDTH - crate::profile::PANEL_WIDTH;
        let overlay_panel = room < MIN_CHAT_WIDTH;
        div()
            .size_full()
            .relative()
            .flex()
            .child(pane)
            .when_some(side_panel, |d, open| {
                let panel = match open {
                    SidePanel::Profile(jid) => crate::profile::panel(&jid, &self.store, glass_on, &theme, cx).into_any_element(),
                    SidePanel::MessageInfo(chat, id) => {
                        crate::message_info::panel(&chat, &id, &self.store, glass_on, &theme, cx).into_any_element()
                    }
                };
                if overlay_panel {
                    let sheet = div().absolute().top_0().bottom_0().right_0().occlude().shadow_lg().child(panel);
                    d.child(above_glass(glass_on, "profile-sheet", sheet))
                } else {
                    d.child(panel)
                }
            })
            .into_any_element()
    }
}
