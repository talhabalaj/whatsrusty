//! Conversation list: search field, pinned chats, Messages-style rows, archived chats,
//! New Message picker.

use std::time::{Duration, Instant};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use crate::native_menu::Item;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use wa_backend::ChatSummary;

use crate::avatar::avatar;
use crate::format::sidebar_time;
use crate::icons::{Glyph, glyph};
use crate::new_chat::NewChat;
use crate::state::{FocusComposer, Store};
use crate::theme::Theme;

const ROW_HEIGHT: f32 = 68.;
/// WhatsApp allows this many pinned chats.
const MAX_PINNED: usize = 3;
const ERROR_SHOWN_FOR: Duration = Duration::from_secs(6);
/// How long typing in the search field pauses before messages are searched too.
const SEARCH_ALL_DELAY: Duration = Duration::from_millis(250);
/// With Liquid Glass: the New Message circle and the search capsule.
const COMPOSE_GLASS_SIZE: f32 = 32.;
const SEARCH_GLASS_HEIGHT: f32 = 32.;
/// The traffic-light row (52) and the search field with its bottom margin (32 + 8): with
/// glass they float over the list, which starts below them.
const SIDEBAR_TOP: f32 = 52. + SEARCH_GLASS_HEIGHT + 8.;

actions!(sidebar, [NewMessage, SearchChats, NextChat, PreviousChat]);

pub fn init(cx: &mut App) {
    // Also in text fields (the composer, search), which would otherwise take them.
    for context in [None, Some("Input")] {
        cx.bind_keys([
            KeyBinding::new("cmd-n", NewMessage, context),
            KeyBinding::new("alt-cmd-f", SearchChats, context),
            KeyBinding::new("ctrl-tab", NextChat, context),
            KeyBinding::new("ctrl-shift-tab", PreviousChat, context),
        ]);
    }
}

/// What the list's keyboard cursor can rest on.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    ArchivedBack,
    Pin(String),
    ArchivedEntry,
    StarredEntry,
    StarredBack,
    Community(String),
    Chat(String),
    /// A starred message or a search hit: (chat, message id).
    Message(String, String),
}

/// A row of the sidebar list.
#[derive(Clone, PartialEq)]
enum Row {
    /// "‹ Archived", back to the main list.
    ArchivedBack,
    Pinned,
    /// The "Archived" entry above the chats.
    ArchivedEntry,
    Chat(String),
    /// A community's row (its jid): stands for its groups, which it shows when expanded.
    Community(String),
    /// A group shown under its expanded community.
    CommunityChat(String),
    /// The "Starred" entry under "Archived".
    StarredEntry,
    /// "‹ Starred", back to the main list.
    StarredBack,
    /// "Messages", over the messages matching a search.
    MessagesHeader,
    /// A starred message, or a message matching the search: (chat, message id).
    Message(String, String),
}

/// With glass: the soft scroll edge under the floating search field, like the
/// conversation's under its header. Fades into the sidebar glass's own shade.
fn top_fade(theme: &Theme) -> impl IntoElement {
    let fade = linear_gradient(
        180.,
        linear_color_stop(theme.sidebar_glass.opacity(0.96), 0.),
        linear_color_stop(theme.sidebar_glass.opacity(0.), 1.),
    );
    div().absolute().top_0().left_0().right_0().h(px(SIDEBAR_TOP + 10.)).bg(fade)
}

pub struct Sidebar {
    store: Entity<Store>,
    search: Entity<InputState>,
    query: String,
    /// Everything below the search field scrolls as one list: the archive's back row,
    /// pinned chats, the Archived entry and the chats.
    list: ListState,
    rows: Vec<Row>,
    /// The selection we last revealed in the list.
    last_selected: Option<String>,
    /// Showing archived chats instead of the main list.
    archived_view: bool,
    /// Showing starred messages instead of the main list.
    starred_view: bool,
    /// Searching all chats' messages waits for a pause in typing.
    _search_all: Option<Task<()>>,
    /// Communities whose groups are shown under them.
    expanded: std::collections::HashSet<String>,
    /// Key focus for the list itself (↑/↓ move `cursor`, Enter opens).
    list_focus: FocusHandle,
    cursor: Option<Target>,
    /// Whether the list had key focus this frame (rows draw the cursor's ring then).
    list_focused: bool,
    /// Liquid Glass is on in this window (as of this frame).
    glass_on: bool,
    /// The New Message picker, with its dismiss subscription.
    new_chat: Option<(Entity<NewChat>, Subscription)>,
    /// When the picker was last dismissed: the click on the compose button that closes it
    /// (as a click outside) mustn't reopen it.
    new_chat_closed_at: Option<Instant>,
    shown_error: Option<String>,
    _subs: Vec<Subscription>,
}

impl Sidebar {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subs = vec![
            cx.observe_in(&store, window, |this, store, window, cx| {
                this.watch_error(store.read(cx).last_error.clone(), cx);
                this.reveal_selection(window, cx);
                // Nothing left in the archive: back to the main list.
                if this.archived_view && !store.read(cx).chats.iter().any(|c| c.archived) {
                    this.archived_view = false;
                }
                cx.notify();
            }),
            cx.subscribe_in(&search, window, |this, state, ev: &InputEvent, _, cx| {
                match ev {
                    InputEvent::Change => {
                        this.query = state.read(cx).value().to_lowercase();
                        // Messages too, once typing pauses.
                        let query = this.query.clone();
                        this._search_all = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(SEARCH_ALL_DELAY).await;
                            let _ = this.update(cx, |this, cx| this.store.update(cx, |s, cx| s.search_all(&query, cx)));
                        }));
                        cx.notify();
                    }
                    // Enter: open the first match.
                    InputEvent::PressEnter { .. } if !this.query.is_empty() => {
                        let first = this.targets(cx).into_iter().find_map(|(t, _)| match t {
                            Target::Pin(j) | Target::Chat(j) => Some(j),
                            _ => None,
                        });
                        if let Some(jid) = first {
                            this.store.update(cx, |s, cx| s.open_chat(jid, cx));
                        }
                    }
                    _ => {}
                }
            }),
        ];
        let mut this = Self {
            store,
            search,
            query: String::new(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            rows: Vec::new(),
            last_selected: None,
            archived_view: false,
            starred_view: false,
            _search_all: None,
            expanded: Default::default(),
            list_focus: cx.focus_handle(),
            cursor: None,
            list_focused: false,
            glass_on: false,
            new_chat: None,
            new_chat_closed_at: None,
            shown_error: None,
            _subs: subs,
        };
        // Test hook: open New Message with this text typed.
        if let Ok(q) = std::env::var("WA_DEBUG_NEW_CHAT") {
            this.open_new_chat(window, cx);
            if let Some((picker, _)) = &this.new_chat {
                picker.update(cx, |p, cx| p.set_query(&q, window, cx));
            }
        }
        if std::env::var_os("WA_DEBUG_ARCHIVED").is_some() {
            this.archived_view = true;
        }
        if std::env::var_os("WA_DEBUG_STARRED").is_some() {
            this.starred_view = true;
        }
        // Test hook: type this into the search field.
        if let Ok(q) = std::env::var("WA_DEBUG_SEARCH") {
            // After startup's own selection (which clears a search). Setting the value
            // doesn't send `Change`: search as typing would.
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.query = q.to_lowercase();
                    this.search.update(cx, |s, cx| s.set_value(q.clone(), window, cx));
                    this.store.update(cx, |s, cx| s.search_all(&q, cx));
                    cx.notify();
                });
            })
            .detach();
        }
        debug_ui_script(window, cx);
        this
    }

    fn open_new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.new(|cx| NewChat::new(self.store.clone(), window, cx));
        let sub = cx.subscribe(&picker, |this, _, _: &DismissEvent, cx| {
            this.new_chat = None;
            this.new_chat_closed_at = Some(Instant::now());
            cx.notify();
        });
        let input_owner = picker.clone();
        input_owner.update(cx, |p, cx| p.focus(window, cx));
        self.new_chat = Some((picker, sub));
        cx.notify();
    }

    /// Errors from the backend (a 4th pin, offline, the phone refusing a change) show
    /// for a few seconds at the bottom of the list.
    fn watch_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        if error == self.shown_error {
            return;
        }
        self.shown_error = error.clone();
        let Some(error) = error else { return };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ERROR_SHOWN_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.store.update(cx, |s, cx| {
                    if s.last_error.as_ref() == Some(&error) {
                        s.last_error = None;
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// A chat was selected (a click, a search result, a notification): picked from a
    /// search, it clears the search and focuses the composer; either way the list scrolls
    /// so the chat is in view.
    fn reveal_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        if selected == self.last_selected {
            return;
        }
        self.last_selected = selected.clone();
        let Some(jid) = selected else { return };
        let from_search = !self.query.is_empty();
        if from_search {
            self.query.clear();
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
            let store = self.store.clone();
            cx.defer(move |cx| store.update(cx, |_, cx| cx.emit(FocusComposer)));
        }
        if let Some(chat) = self.store.read(cx).chats.iter().find(|c| c.jid == jid) {
            self.archived_view = chat.archived;
            if let Some(community) = &chat.community {
                self.expanded.insert(community.jid.clone());
            }
        }
        self.sync_rows(cx);
        if let Some(ix) = self.rows.iter().position(|r| matches!(r, Row::Chat(j) | Row::CommunityChat(j) if *j == jid)) {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    /// The list's rows for the current view and query.
    fn current_rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let chats = &store.chats;
        if self.starred_view {
            let mut rows = vec![Row::StarredBack];
            rows.extend(store.starred.iter().map(|m| Row::Message(m.chat_jid.clone(), m.id.clone())));
            return rows;
        }
        let main_view = self.query.is_empty() && !self.archived_view;
        let mut rows = Vec::new();
        if self.archived_view {
            rows.push(Row::ArchivedBack);
        }
        if main_view && chats.iter().any(|c| c.pinned && !c.archived) {
            rows.push(Row::Pinned);
        }
        if main_view && chats.iter().any(|c| c.archived) {
            rows.push(Row::ArchivedEntry);
        }
        if main_view && !store.starred.is_empty() {
            rows.push(Row::StarredEntry);
        }
        let jids = self.visible_jids(cx);
        if !main_view {
            rows.extend(jids.into_iter().map(Row::Chat));
            // Searching: the messages that match, under the chats.
            if let Some((query, hits)) = &store.search_all
                && !self.query.is_empty()
                && query.to_lowercase() == self.query.trim()
                && !hits.is_empty()
            {
                rows.push(Row::MessagesHeader);
                rows.extend(hits.iter().map(|m| Row::Message(m.chat_jid.clone(), m.id.clone())));
            }
            return rows;
        }
        // A community's groups gather under one row, where its most recent group would be.
        let community_of = |jid: &str| chats.iter().find(|c| c.jid == jid).and_then(|c| c.community.as_ref()).map(|c| c.jid.clone());
        let mut seen = std::collections::HashSet::new();
        for jid in &jids {
            match community_of(jid) {
                None => rows.push(Row::Chat(jid.clone())),
                Some(community) if seen.insert(community.clone()) => {
                    rows.push(Row::Community(community.clone()));
                    if self.expanded.contains(&community) {
                        let members = jids.iter().filter(|j| community_of(j).as_ref() == Some(&community));
                        rows.extend(members.cloned().map(Row::CommunityChat));
                    }
                }
                Some(_) => {}
            }
        }
        rows
    }

    /// Brings the list in line with [`Self::current_rows`], splicing only the rows that
    /// changed so the scroll position stays put when chats move.
    fn sync_rows(&mut self, cx: &App) {
        let new = self.current_rows(cx);
        if new == self.rows {
            return;
        }
        let old = std::mem::replace(&mut self.rows, new);
        let new = &self.rows;
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|(a, b)| a == b).count();
        self.list.splice(prefix..old.len() - suffix, new.len() - prefix - suffix);
    }

    fn render_row(&self, ix: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix) else { return div().into_any_element() };
        let chats = self.store.read(cx).chats.clone();
        match row {
            Row::ArchivedBack => {
                let el = self.back_header(false, theme, cx).into_any_element();
                self.ring(&Target::ArchivedBack, el, 10., 10., theme)
            }
            Row::Pinned => self.pinned_grid(&chats, theme, cx).unwrap_or_else(|| div().into_any_element()),
            Row::ArchivedEntry => {
                let el = self.archived_row(&chats, theme, cx).unwrap_or_else(|| div().into_any_element());
                self.ring(&Target::ArchivedEntry, el, 10., 10., theme)
            }
            Row::StarredEntry => {
                let el = self.starred_row(theme, cx);
                self.ring(&Target::StarredEntry, el, 10., 10., theme)
            }
            Row::StarredBack => {
                let el = self.back_header(true, theme, cx).into_any_element();
                self.ring(&Target::StarredBack, el, 10., 10., theme)
            }
            Row::MessagesHeader => div()
                .px(px(24.))
                .pt(px(12.))
                .pb(px(4.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child("Messages")
                .into_any_element(),
            Row::Message(chat, id) => {
                let el = self.message_row(chat, id, theme, cx);
                self.ring(&Target::Message(chat.clone(), id.clone()), el, 10., 10., theme)
            }
            Row::Community(jid) => {
                let el = self.community_row(jid, &chats, theme, cx);
                self.ring(&Target::Community(jid.clone()), el, 10., 10., theme)
            }
            Row::Chat(jid) | Row::CommunityChat(jid) => {
                let member = matches!(row, Row::CommunityChat(_));
                let state = self.store.read(cx);
                let Some(chat) = chats.iter().find(|c| &c.jid == jid) else { return div().into_any_element() };
                let photo = state.avatar_cached(jid);
                let selected = state.selected.as_deref() == Some(jid.as_str());
                let typing = state.typing.contains(jid);
                // Ask for photos of rows as they scroll into view.
                if photo.is_none() {
                    let (store, jid) = (self.store.clone(), jid.clone());
                    cx.defer(move |cx| store.update(cx, |s, _| s.request_avatar(&jid)));
                }
                let pin_count = chats.iter().filter(|c| c.pinned && !c.archived).count();
                let el = div()
                    .px(px(10.))
                    // Under its community, indented past the community's avatar.
                    .when(member, |d| d.pl(px(34.)))
                    .child(chat_row(chat, photo, selected, typing, pin_count, theme, self.store.clone()))
                    .into_any_element();
                self.ring(&Target::Chat(jid.clone()), el, if member { 34. } else { 10. }, 10., theme)
            }
        }
    }

    /// Where the cursor can go, in order, with the list row each sits in.
    fn targets(&self, cx: &App) -> Vec<(Target, usize)> {
        let chats = &self.store.read(cx).chats;
        let mut out = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            match row {
                Row::ArchivedBack => out.push((Target::ArchivedBack, ix)),
                Row::Pinned => out.extend(
                    chats.iter().filter(|c| c.pinned && !c.archived).map(|c| (Target::Pin(c.jid.clone()), ix)),
                ),
                Row::ArchivedEntry => out.push((Target::ArchivedEntry, ix)),
                Row::Community(jid) => out.push((Target::Community(jid.clone()), ix)),
                Row::Chat(jid) | Row::CommunityChat(jid) => out.push((Target::Chat(jid.clone()), ix)),
                Row::StarredEntry => out.push((Target::StarredEntry, ix)),
                Row::StarredBack => out.push((Target::StarredBack, ix)),
                Row::MessagesHeader => {}
                Row::Message(chat, id) => out.push((Target::Message(chat.clone(), id.clone()), ix)),
            }
        }
        out
    }

    /// Put the cursor on `target` (if it's in the list) and scroll it into view.
    fn move_cursor(&mut self, target: Option<Target>, cx: &mut Context<Self>) {
        let targets = self.targets(cx);
        let Some((target, row)) = target.and_then(|t| targets.into_iter().find(|(x, _)| *x == t)) else { return };
        self.cursor = Some(target);
        self.list.scroll_to_reveal_item(row);
        cx.notify();
    }

    /// Key focus into the list: the cursor on the open chat if it's listed, else the first row.
    fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets(cx);
        let selected = self.store.read(cx).selected.clone();
        let start = selected
            .and_then(|j| targets.iter().find(|(t, _)| matches!(t, Target::Pin(x) | Target::Chat(x) if *x == j)))
            .or_else(|| targets.iter().find(|(t, _)| matches!(t, Target::Pin(_) | Target::Chat(_))))
            .or(targets.first())
            .map(|(t, _)| t.clone());
        self.list_focus.focus(window, cx);
        self.move_cursor(start, cx);
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// The community a chat belongs to, when it's listed under it.
    fn community_of(&self, jid: &str, cx: &App) -> Option<String> {
        let chat = self.store.read(cx).chats.iter().find(|c| c.jid == jid)?;
        chat.community.as_ref().map(|c| c.jid.clone()).filter(|c| self.expanded.contains(c))
    }

    /// Enter / Space on the cursor: open the chat (and type in it), toggle the community,
    /// go into or out of the archive.
    fn activate(&mut self, target: Target, cx: &mut Context<Self>) {
        match target {
            Target::Pin(jid) | Target::Chat(jid) => self.store.update(cx, |s, cx| s.open_chat(jid, cx)),
            Target::Community(jid) => {
                if !self.expanded.remove(&jid) {
                    self.expanded.insert(jid);
                }
            }
            Target::ArchivedEntry => self.archived_view = true,
            Target::ArchivedBack => self.archived_view = false,
            Target::StarredEntry => self.starred_view = true,
            Target::StarredBack => self.starred_view = false,
            Target::Message(chat, id) => self.open_message(&chat, &id, cx),
        }
        cx.notify();
    }

    /// Keys while the list has focus. False: not ours (let it through).
    fn on_list_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let targets = self.targets(cx);
        let pos = self.cursor.as_ref().and_then(|c| targets.iter().position(|(t, _)| t == c));
        let at = |i: usize| targets.get(i).map(|(t, _)| t.clone());
        match key {
            "down" => {
                let next = pos.map_or(0, |p| (p + 1).min(targets.len().saturating_sub(1)));
                self.move_cursor(at(next), cx);
            }
            "up" => match pos {
                // Above the first row: back up into the search field.
                Some(0) | None => self.focus_search(window, cx),
                Some(p) => self.move_cursor(at(p - 1), cx),
            },
            "home" => self.move_cursor(at(0), cx),
            "end" => self.move_cursor(at(targets.len().saturating_sub(1)), cx),
            "enter" | "space" => {
                if let Some(target) = self.cursor.clone() {
                    self.activate(target, cx);
                    self.sync_rows(cx);
                    let cursor = self.cursor.clone();
                    self.move_cursor(cursor, cx);
                }
            }
            "right" => match self.cursor.clone() {
                Some(Target::Community(jid)) if !self.expanded.contains(&jid) => {
                    self.expanded.insert(jid);
                    cx.notify();
                }
                _ => {}
            },
            "left" => match self.cursor.clone() {
                Some(Target::Community(jid)) if self.expanded.contains(&jid) => {
                    self.expanded.remove(&jid);
                    cx.notify();
                }
                // From a community's group up to the community.
                Some(Target::Chat(jid)) => {
                    if let Some(community) = self.community_of(&jid, cx) {
                        self.move_cursor(Some(Target::Community(community)), cx);
                    }
                }
                _ => {}
            },
            "escape" => {
                let community = match &self.cursor {
                    Some(Target::Chat(jid)) => self.community_of(jid, cx),
                    _ => None,
                };
                if let Some(community) = community {
                    // Out of a community's groups: fold it up.
                    self.expanded.remove(&community);
                    self.sync_rows(cx);
                    self.move_cursor(Some(Target::Community(community)), cx);
                } else if self.starred_view {
                    self.starred_view = false;
                    self.sync_rows(cx);
                    self.move_cursor(Some(Target::StarredEntry), cx);
                } else if self.archived_view {
                    self.archived_view = false;
                    self.sync_rows(cx);
                    self.move_cursor(Some(Target::ArchivedEntry), cx);
                } else if !self.query.is_empty() {
                    self.focus_search(window, cx);
                } else {
                    // Back to typing in the open chat.
                    self.cursor = None;
                    self.store.update(cx, |_, cx| cx.emit(FocusComposer));
                }
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    /// ⌃Tab / ⌃⇧Tab: the next / previous chat in the list's order (pinned first).
    pub fn step_chat(&mut self, delta: isize, cx: &mut Context<Self>) {
        let chats: Vec<String> = self
            .targets(cx)
            .into_iter()
            .filter_map(|(t, _)| match t {
                Target::Pin(j) | Target::Chat(j) => Some(j),
                _ => None,
            })
            .collect();
        if chats.is_empty() {
            return;
        }
        let selected = self.store.read(cx).selected.clone();
        let pos = selected.and_then(|s| chats.iter().position(|j| *j == s));
        let next = match pos {
            Some(p) => (p as isize + delta).rem_euclid(chats.len() as isize) as usize,
            None => 0,
        };
        let jid = chats[next].clone();
        self.store.update(cx, |s, cx| s.select(jid, cx));
    }

    /// ⌘N.
    pub fn new_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_chat.is_none() {
            self.open_new_chat(window, cx);
        }
    }

    /// ⌥⌘F: into the search field (out of the archive).
    pub fn search_chats(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_search(window, cx);
    }

    /// The cursor's ring, over a row's element (`inset`: its horizontal margins).
    fn ring(&self, target: &Target, el: AnyElement, left: f32, right: f32, theme: &Theme) -> AnyElement {
        let on = self.list_focused && self.cursor.as_ref() == Some(target);
        div()
            .relative()
            .child(el)
            .when(on, |d| {
                d.child(
                    div().absolute().top_0().bottom_0().left(px(left)).right(px(right)).rounded(px(8.)).border_2().border_color(theme.accent),
                )
            })
            .into_any_element()
    }

    fn matches(&self, c: &ChatSummary) -> bool {
        self.query.is_empty() || c.name.to_lowercase().contains(&self.query)
    }

    /// Chat rows of the list. Pinned chats sit in the grid above them, except while
    /// searching.
    fn visible_jids(&self, cx: &App) -> Vec<String> {
        let searching = !self.query.is_empty();
        self.store
            .read(cx)
            .chats
            .iter()
            .filter(|c| c.archived == self.archived_view)
            // A community's own chat holds nothing; its groups stand for it.
            .filter(|c| !c.is_community)
            .filter(|c| searching || self.archived_view || !c.pinned)
            .filter(|c| self.matches(c))
            .map(|c| c.jid.clone())
            .collect()
    }

    /// Traffic-light row with New Message at the end: a glass circle with Liquid Glass, like
    /// macOS 26+ toolbar buttons, otherwise a plain icon button.
    fn header(&self, glass_on: bool, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let button = div()
            .id("new-message")
            .size(px(if glass_on { COMPOSE_GLASS_SIZE } else { 30. }))
            .when(glass_on, |d| d.rounded_full())
            .when(!glass_on, |d| d.rounded(px(6.)));
        let button = button
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(theme.search_bg))
            .tooltip(|window, cx| Tooltip::new("New Message").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| {
                let just_closed = this.new_chat_closed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300));
                match this.new_chat.take() {
                    Some(_) => cx.notify(),
                    None if just_closed => {}
                    None => this.open_new_chat(window, cx),
                }
            }))
            // On glass the icon reads like a toolbar symbol: primary color, a little smaller.
            .child(if glass_on { glyph(Glyph::Compose, 16., theme.text) } else { glyph(Glyph::Compose, 18., theme.text_secondary) });
        // Room for the traffic lights, which sit in our transparent titlebar.
        div().h(px(52.)).flex_none().flex().items_center().justify_end().pr(px(12.)).child(if glass_on {
            crate::glass::glass("sidebar-compose", COMPOSE_GLASS_SIZE / 2., button).into_any_element()
        } else {
            button.into_any_element()
        })
    }

    /// The search field: a glass capsule with a magnifier with Liquid Glass, otherwise a
    /// filled rounded field.
    fn search_field(&self, glass_on: bool, theme: &Theme) -> impl IntoElement {
        let input = Input::new(&self.search).appearance(false).cleanable(true);
        let field = if glass_on {
            let field = div()
                .h(px(SEARCH_GLASS_HEIGHT))
                .pl(px(10.))
                .pr(px(4.))
                .flex()
                .items_center()
                .gap(px(4.))
                .text_size(px(13.))
                .child(glyph(Glyph::Magnifier, 13., theme.text_secondary))
                .child(div().flex_1().min_w_0().child(input));
            crate::glass::glass("sidebar-search", SEARCH_GLASS_HEIGHT / 2., field).into_any_element()
        } else {
            div().rounded(px(8.)).bg(theme.search_bg).text_size(px(13.)).child(input).into_any_element()
        };
        div().px(px(10.)).pb(px(8.)).child(field)
    }

    /// "‹ Archived" / "‹ Starred" over the archive / the starred messages: back to the
    /// main list.
    fn back_header(&self, starred: bool, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(if starred { "starred-back" } else { "archived-back" })
            .mx(px(10.))
            .px(px(6.))
            .mb(px(4.))
            .h(px(32.))
            .flex()
            .items_center()
            .gap(px(4.))
            .rounded(px(6.))
            .cursor_pointer()
            // On glass the circle shows the hover.
            .when(!self.glass_on, |d| d.hover(|s| s.bg(theme.search_bg)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if starred {
                    this.starred_view = false;
                } else {
                    this.archived_view = false;
                }
                cx.notify();
            }))
            .map(|d| {
                if self.glass_on {
                    // Back as a glass circle, like the toolbar's New Message button.
                    let (id, pane) = if starred {
                        ("starred-back-glass", "sidebar-starred-back")
                    } else {
                        ("archived-back-glass", "sidebar-archived-back")
                    };
                    let back = div().id(id).child(glyph(Glyph::ChevronLeft, 13., theme.text));
                    d.gap(px(8.)).child(crate::glass::circle_button(pane, 28., theme.search_bg, back))
                } else {
                    d.child(glyph(Glyph::ChevronLeft, 15., theme.accent))
                }
            })
            .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child(if starred { "Starred" } else { "Archived" }))
    }

    /// The "Starred" entry under "Archived", when anything is starred.
    fn starred_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let count = self.store.read(cx).starred.len();
        let row = div()
            .id("starred-row")
            .w_full()
            .h(px(44.))
            .flex()
            .items_center()
            .gap(px(8.))
            .pr(px(12.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.search_bg))
            .on_click(cx.listener(|this, _, _, cx| {
                this.starred_view = true;
                cx.notify();
            }))
            .child(div().w(px(14.)).flex_none())
            .child(div().w(px(44.)).flex_none().flex().justify_center().child(glyph(Glyph::Star, 19., theme.text_secondary)))
            .child(div().flex_1().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).child("Starred"))
            .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(theme.text_secondary).child(count.to_string()))
            .child(glyph(Glyph::ChevronRight, 11., theme.text_secondary));
        div().px(px(10.)).pb(px(2.)).child(row).into_any_element()
    }

    /// Open a starred message / search hit: its chat, scrolled to it.
    fn open_message(&mut self, chat: &str, id: &str, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let found = store
            .starred
            .iter()
            .chain(store.search_all.iter().flat_map(|(_, hits)| hits))
            .find(|m| m.chat_jid == chat && m.id == id)
            .cloned();
        if let Some(m) = found {
            self.store.update(cx, |s, cx| s.message_action(crate::state::MessageAction::Reveal(m), None, cx));
        }
    }

    /// A starred message or a search hit: the chat's photo and name (with the sender in
    /// a group), when, and what it says.
    fn message_row(&self, chat_jid: &str, id: &str, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let source: &[wa_backend::ChatMessage] =
            if self.starred_view { &store.starred } else { store.search_all.as_ref().map_or(&[], |(_, h)| h.as_slice()) };
        let Some(m) = source.iter().find(|m| m.chat_jid == chat_jid && m.id == id).cloned() else {
            return div().into_any_element();
        };
        let chat = store.chats.iter().find(|c| c.jid == chat_jid);
        let chat_name = chat.map_or_else(|| chat_jid.split('@').next().unwrap_or_default().to_string(), |c| c.name.clone());
        let is_group = chat.is_some_and(|c| c.is_group);
        let photo = store.avatar_cached(chat_jid);
        let who = match &m.sender_name {
            _ if m.from_me => Some("You".to_string()),
            Some(name) if is_group => Some(name.clone()),
            _ => None,
        };
        let title = match &who {
            Some(who) if is_group => format!("{who} · {chat_name}"),
            _ => chat_name.clone(),
        };
        let mut text = crate::message_info::preview(&m).replace('\n', " ");
        // "@<digits>" as the names they stand for, like the bubble shows them.
        for mention in &m.mentions {
            text = text.replace(&format!("@{}", mention.user), &format!("@{}", mention.name));
        }
        let text = match who {
            Some(who) if !is_group => format!("{who}: {text}"),
            _ => text,
        };
        let (open_chat, open_id) = (chat_jid.to_string(), id.to_string());
        let (menu_store, menu_message, starred_view) = (self.store.clone(), m.clone(), self.starred_view);
        let row = div()
            .id(SharedString::from(format!("hit-{chat_jid}-{id}")))
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.search_bg))
            .child(avatar(&chat_name, photo, 44., theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().flex_1().min_w_0().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                            .child(div().flex_none().text_size(px(12.)).text_color(theme.text_secondary).child(sidebar_time(m.timestamp))),
                    )
                    .child(div().text_size(px(12.5)).text_color(theme.text_secondary).line_clamp(2).child(text)),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.open_message(&open_chat, &open_id, cx)))
            // Right-click a starred message to unstar it.
            .when(starred_view, |d| {
                d.on_mouse_down(MouseButton::Right, move |ev, window, cx| {
                    let (store, m) = (menu_store.clone(), menu_message.clone());
                    let items = vec![Item::action("Unstar", move |_, cx| store.update(cx, |s, _| s.star_message(&m, false)))];
                    crate::native_menu::show(items, ev.position, window, cx);
                })
            });
        div().px(px(10.)).child(row).into_any_element()
    }

    /// The "Archived" entry above the chats, when anything is archived.
    fn archived_row(&self, chats: &[ChatSummary], theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let archived: Vec<_> = chats.iter().filter(|c| c.archived).collect();
        if archived.is_empty() {
            return None;
        }
        let unread = archived.iter().filter(|c| c.unread_count > 0).count();
        // A full-width row lined up with the chats (icon over the avatars, label over the
        // names), the unread count and a chevron at its end.
        let row = div()
            .id("archived-row")
            .w_full()
            .h(px(44.))
            .flex()
            .items_center()
            .gap(px(8.))
            .pr(px(12.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.search_bg))
            .on_click(cx.listener(|this, _, _, cx| {
                this.archived_view = true;
                cx.notify();
            }))
            .child(div().w(px(14.)).flex_none())
            .child(div().w(px(44.)).flex_none().flex().justify_center().child(glyph(Glyph::Archive, 19., theme.text_secondary)))
            .child(div().flex_1().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).child("Archived"))
            .when(unread > 0, |d| {
                d.child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(theme.text_secondary).child(unread.to_string()))
            })
            .child(glyph(Glyph::ChevronRight, 11., theme.text_secondary));
        let row = div().px(px(10.)).pb(px(2.)).child(row);
        Some(row.into_any_element())
    }

    /// A community: its name, its groups' unread total and latest message; clicking shows
    /// or hides its groups beneath it.
    fn community_row(&self, jid: &str, chats: &[ChatSummary], theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let members: Vec<&ChatSummary> =
            chats.iter().filter(|c| c.community.as_ref().is_some_and(|k| k.jid == jid) && !c.archived).collect();
        let Some(name) = members.iter().find_map(|c| c.community.as_ref()).map(|k| k.name.clone()) else {
            return div().into_any_element();
        };
        let expanded = self.expanded.contains(jid);
        let unread = members.iter().any(|c| c.unread_count > 0);
        let all_muted = members.iter().all(|c| c.muted);
        let latest = members.iter().filter_map(|c| c.last_message.as_ref().map(|m| (*c, m))).max_by_key(|(_, m)| m.timestamp);
        let preview = latest
            .map(|(c, m)| {
                let text = match (&m.sender_name, m.from_me) {
                    (_, true) => format!("You: {}", m.text),
                    (Some(s), false) => format!("{s}: {}", m.text),
                    _ => m.text.clone(),
                };
                crate::links::strip_bidi(&format!("{} · {text}", c.name))
            })
            .unwrap_or_else(|| format!("{} groups", members.len()));
        let time = latest.map(|(_, m)| sidebar_time(m.timestamp)).unwrap_or_default();
        let photo = self.store.read(cx).avatar_cached(jid);
        if photo.is_none() {
            let (store, jid) = (self.store.clone(), jid.to_string());
            cx.defer(move |cx| store.update(cx, |s, _| s.request_avatar(&jid)));
        }
        let secondary = if unread { theme.text } else { theme.text_secondary };
        let toggle = jid.to_string();
        let row = div()
            .id(SharedString::from(format!("community-{jid}")))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .pr(px(10.))
            .rounded(px(8.))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded.remove(&toggle) {
                    this.expanded.insert(toggle.clone());
                }
                cx.notify();
            }))
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    // Collapsed, the community carries its groups' unread dot.
                    .when(unread && !expanded, |d| d.child(unread_dot(all_muted, theme))),
            )
            .child(avatar(&name, photo, 44., theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(1.))
                    .border_b_1()
                    .border_color(theme.separator)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.5))
                                    .font_weight(if unread { FontWeight::BOLD } else { FontWeight::MEDIUM })
                                    .child(name),
                            )
                            .child(div().flex_1())
                            .child(div().flex_none().pl(px(2.)).text_size(px(12.)).text_color(theme.text_secondary).child(time))
                            .child(glyph(if expanded { Glyph::ChevronDown } else { Glyph::ChevronRight }, 10., theme.text_secondary)),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .line_height(px(16.))
                            .text_color(secondary)
                            .line_clamp(2)
                            .text_ellipsis()
                            .child(preview),
                    ),
            );
        div().px(px(10.)).child(row).into_any_element()
    }

    fn pinned_grid(&self, chats: &[ChatSummary], theme: &Theme, cx: &App) -> Option<AnyElement> {
        let pinned: Vec<&ChatSummary> = chats.iter().filter(|c| c.pinned && !c.archived).collect();
        if pinned.is_empty() {
            return None;
        }
        let state = self.store.read(cx);
        let pin_count = pinned.len();
        let tiles = pinned.into_iter().map(|chat| {
            let selected = state.selected.as_deref() == Some(chat.jid.as_str());
            let focused = self.list_focused && self.cursor == Some(Target::Pin(chat.jid.clone()));
            pinned_tile(chat, state.avatar_cached(&chat.jid), selected, focused, pin_count, theme, self.store.clone())
        });
        Some(div().px(px(10.)).pb(px(6.)).flex().children(tiles).into_any_element())
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        self.sync_rows(cx);
        let error = self.store.read(cx).last_error.clone();
        let glass_on = crate::glass::enabled(window);
        self.list_focused = self.list_focus.is_focused(window);
        self.glass_on = glass_on;
        if !self.list_focused {
            self.cursor = None;
        }
        let view = cx.entity();
        let list = list(self.list.clone(), move |ix, window, cx| {
            let theme = Theme::for_window(window);
            view.update(cx, |this, cx| this.render_row(ix, &theme, cx))
        })
        .flex_1()
        // With glass the list runs under the floating search field and New Message button.
        .when(glass_on, |d| d.size_full().pt(px(SIDEBAR_TOP)));
        let top = div()
            .flex_none()
            .flex()
            .flex_col()
            // With glass it floats over the list: clicks stop here (scrolling goes through).
            .when(glass_on, |d| d.absolute().top_0().left_0().right_0().block_mouse_except_scroll())
            .child(self.header(glass_on, &theme, cx))
            .child(self.search_field(glass_on, &theme));
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .track_focus(&self.list_focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                if this.list_focus.is_focused(window) {
                    if this.on_list_key(key, window, cx) {
                        cx.stop_propagation();
                    }
                    return;
                }
                // In the search field: Esc clears the search, then leaves the archive.
                if key != "escape" {
                    return;
                }
                if !this.query.is_empty() {
                    this.query.clear();
                    this.search.update(cx, |s, cx| s.set_value("", window, cx));
                } else if this.archived_view {
                    this.archived_view = false;
                } else {
                    return;
                }
                cx.notify();
                cx.stop_propagation();
            }))
            // ↓ in the search field moves into the list (the field takes it as a caret move).
            .capture_action(cx.listener(|this, _: &gpui_kit::component::input::MoveDown, window, cx| {
                if this.search.read(cx).focus_handle(cx).is_focused(window) {
                    this.focus_list(window, cx);
                    cx.stop_propagation();
                }
            }))
            // With glass the glass edge is the divider, like a native sidebar.
            .when(!glass_on, |d| d.bg(theme.sidebar_bg).border_r_1().border_color(theme.sidebar_border))
            .map(|d| if glass_on { d.child(list).child(top_fade(&theme)).child(top) } else { d.child(top).child(list) })
            .when_some(error, |d, e| d.child(error_toast(e, &theme, self.store.clone())))
            .when_some(self.new_chat.as_ref().map(|(p, _)| p.clone()), |d, picker| {
                d.child(deferred(div().absolute().top(px(46.)).left(px(8.)).right(px(8.)).child(picker)).with_priority(1))
            });
        if !glass_on {
            return content.into_any_element();
        }
        // Liquid Glass: like a macOS 27 sidebar, edge-to-edge regular glass (not floating),
        // sampling the window background painted beneath it. The list draws inside the glass.
        div()
            .size_full()
            .bg(theme.window_bg)
            .child(crate::glass::sidebar_glass("sidebar", content))
            .into_any_element()
    }
}

fn error_toast(text: String, theme: &Theme, store: Entity<Store>) -> impl IntoElement {
    div()
        .id("sidebar-error")
        .absolute()
        .occlude()
        .bottom(px(12.))
        .left(px(12.))
        .right(px(12.))
        .px(px(12.))
        .py(px(8.))
        .rounded(px(10.))
        .bg(theme.header_bg)
        .border_1()
        .border_color(theme.separator)
        .shadow_md()
        .text_size(px(12.))
        .text_color(theme.text)
        .on_click(move |_, _, cx| {
            store.update(cx, |s, cx| {
                s.last_error = None;
                cx.notify();
            })
        })
        .child(text)
}

/// Right-click menu of a chat (row or pinned tile): a native menu.
fn with_chat_menu<E>(element: E, chat: &ChatSummary, pin_count: usize, store: Entity<Store>) -> impl IntoElement
where
    E: InteractiveElement + ParentElement + Styled + IntoElement + 'static,
{
    let chat = chat.clone();
    element.on_mouse_down(MouseButton::Right, move |ev: &MouseDownEvent, window, cx| {
        cx.stop_propagation();
        crate::native_menu::show(chat_menu(&chat, pin_count, &store), ev.position, window, cx);
    })
}

fn chat_menu(chat: &ChatSummary, pin_count: usize, store: &Entity<Store>) -> Vec<Item> {
    let jid = chat.jid.clone();
    let mut items = Vec::new();
    let j = jid.clone();
    items.push(menu_item(store, "Pop Out", move |s, cx| crate::mini::pop_out(s, j.clone(), cx)));
    items.push(Item::Separator);
    if !chat.archived {
        let pinned = chat.pinned;
        let j = jid.clone();
        items.push(
            menu_item(store, if pinned { "Unpin" } else { "Pin" }, move |s, cx| s.read(cx).set_pinned(&j, !pinned))
                .enabled(pinned || pin_count < MAX_PINNED),
        );
    }
    let j = jid.clone();
    items.push(match chat.unread_count > 0 {
        true => menu_item(store, "Mark as Read", move |s, cx| s.read(cx).mark_read(&j)),
        false => menu_item(store, "Mark as Unread", move |s, cx| s.update(cx, |s, cx| s.mark_unread(&j, cx))),
    });
    items.push(Item::Separator);
    if chat.muted {
        let j = jid.clone();
        items.push(menu_item(store, "Unmute", move |s, cx| s.read(cx).set_muted(&j, 0)));
    } else {
        let option = |label: &str, secs: Option<i64>| {
            let j = jid.clone();
            menu_item(store, label, move |s, cx| {
                let until = secs.map_or(i64::MAX, |s| chrono::Utc::now().timestamp() + s);
                s.read(cx).set_muted(&j, until);
            })
        };
        items.push(Item::submenu(
            "Mute",
            vec![option("For 8 Hours", Some(8 * 3600)), option("For 1 Week", Some(7 * 86_400)), option("Always", None)],
        ));
    }
    let archived = chat.archived;
    items.push(menu_item(store, if archived { "Unarchive" } else { "Archive" }, move |s, cx| {
        s.read(cx).set_archived(&jid, !archived)
    }));
    items
}

fn menu_item(store: &Entity<Store>, label: &str, f: impl FnOnce(&Entity<Store>, &mut App) + 'static) -> Item {
    let store = store.clone();
    Item::action(label, move |_, cx| f(&store, cx))
}

/// Unread marker: blue dot, like Messages (gray when the chat is muted).
fn unread_dot(muted: bool, theme: &Theme) -> Div {
    div().size(px(9.)).rounded_full().bg(if muted { theme.text_secondary } else { theme.unread_dot })
}

fn pinned_tile(
    chat: &ChatSummary,
    photo: Option<std::sync::Arc<Image>>,
    selected: bool,
    focused: bool,
    pin_count: usize,
    theme: &Theme,
    store: Entity<Store>,
) -> impl IntoElement {
    let jid = chat.jid.clone();
    let unread = chat.unread_count > 0 && !selected;
    let click_store = store.clone();
    let tile = div()
        .id(SharedString::from(format!("pin-{}", chat.jid)))
        .flex_1()
        .min_w_0()
        .py(px(6.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(5.))
        .rounded(px(10.))
        .relative()
        .when(focused, |d| d.child(div().absolute().inset_0().rounded(px(10.)).border_2().border_color(theme.accent)))
        .on_click(move |_, _, cx| {
            let jid = jid.clone();
            click_store.update(cx, |s, cx| s.select(jid, cx));
        })
        .child(
            div()
                .relative()
                .child(avatar(&chat.name, photo, 60., theme))
                .when(unread, |d| d.child(unread_dot(chat.muted, theme).absolute().top(px(1.)).left(px(-2.)).size(px(12.)))),
        )
        .child(
            div()
                .max_w_full()
                .px(px(7.))
                .py(px(1.))
                .rounded_full()
                .flex()
                .items_center()
                .gap(px(3.))
                .when(selected, |d| d.bg(theme.row_selected))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.5))
                        .when(unread, |d| d.font_weight(FontWeight::BOLD))
                        .text_color(if selected { theme.row_selected_text } else { theme.text })
                        .child(chat.name.clone()),
                )
                .when(chat.muted, |d| {
                    let color = if selected { theme.row_selected_text } else { theme.text_secondary };
                    d.child(div().flex_none().child(glyph(Glyph::BellSlash, 10., color)))
                }),
        );
    with_chat_menu(tile, chat, pin_count, store)
}

fn chat_row(
    chat: &ChatSummary,
    photo: Option<std::sync::Arc<Image>>,
    selected: bool,
    typing: bool,
    pin_count: usize,
    theme: &Theme,
    store: Entity<Store>,
) -> AnyElement {
    let unread = chat.unread_count > 0 && !selected;
    let (title, secondary) = match (selected, unread) {
        (true, _) => (theme.row_selected_text, theme.row_selected_text.opacity(0.8)),
        (false, true) => (theme.text, theme.text),
        (false, false) => (theme.text, theme.text_secondary),
    };
    let preview = if typing {
        "typing…".to_string()
    } else {
        chat.last_message
            .as_ref()
            .map(|m| match (&m.sender_name, chat.is_group, m.from_me) {
                (_, _, true) => format!("You: {}", m.text),
                (Some(s), true, false) => format!("{s}: {}", m.text),
                _ => m.text.clone(),
            })
            .map(|p| crate::links::strip_bidi(&p))
            .unwrap_or_default()
    };
    let time = chat.last_message.as_ref().map(|m| sidebar_time(m.timestamp)).unwrap_or_default();
    let jid = chat.jid.clone();
    let click_store = store.clone();

    let row = div()
        .id(SharedString::from(format!("chat-{}", chat.jid)))
        .h(px(ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap(px(8.))
        .pr(px(10.))
        .rounded(px(8.))
        .when(selected, |d| d.bg(theme.row_selected))
        .on_click(move |_, _, cx| {
            let jid = jid.clone();
            click_store.update(cx, |s, cx| s.select(jid, cx));
        })
        // Unread indicator lives in the left gutter, like Messages.
        .child(
            div()
                .w(px(14.))
                .flex_none()
                .flex()
                .justify_center()
                .when(unread, |d| d.child(unread_dot(chat.muted, theme))),
        )
        .child(avatar(&chat.name, photo, 44., theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(1.))
                .when(!selected, |d| d.border_b_1().border_color(theme.separator))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.5))
                                .font_weight(if unread { FontWeight::BOLD } else { FontWeight::MEDIUM })
                                .text_color(title)
                                .child(chat.name.clone()),
                        )
                        .when(chat.muted, |d| d.child(div().flex_none().child(glyph(Glyph::BellSlash, 11., secondary))))
                        .child(div().flex_1())
                        .child(
                            div()
                                .flex_none()
                                .pl(px(2.))
                                .text_size(px(12.))
                                .text_color(if unread { theme.text_secondary } else { secondary })
                                .child(time),
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(16.))
                        .text_color(secondary)
                        .line_clamp(2)
                        .text_ellipsis()
                        .child(preview),
                ),
        );
    with_chat_menu(row, chat, pin_count, store).into_any_element()
}

/// Test hook: WA_DEBUG_UI="wait 1500; click 300,26; rclick 120,200; key down; type abc"
/// drives the window with real mouse/key events (window coordinates in points).
/// `drag x,y a.png|b.pdf` / `drop x,y` simulate dragging files from Finder.
/// `scroll x,y dy` scrolls by dy points at x,y; `scrollbench x,y secs` scrolls there for
/// secs seconds in a fixed pattern (one event per 8 ms, 1.5 s each way at 1200 pt/s),
/// so profiling runs compare.
/// A reopened window (see `WA_DEBUG_REOPEN`) runs `WA_DEBUG_UI_REOPENED` instead.
fn parse_point(at: &str) -> Point<Pixels> {
    let (x, y) = at.split_once(',').unwrap_or(("0", "0"));
    point(px(x.trim().parse().unwrap_or(0.)), px(y.trim().parse().unwrap_or(0.)))
}

fn debug_scroll(window: &mut Window, cx: &mut App, position: Point<Pixels>, dy: f32) {
    window.dispatch_event(
        PlatformInput::Scroll(ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: Default::default(),
        }),
        cx,
    );
}

fn debug_ui_script(window: &mut Window, cx: &mut Context<Sidebar>) {
    static WINDOWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let var = if WINDOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 { "WA_DEBUG_UI" } else { "WA_DEBUG_UI_REOPENED" };
    run_debug_ui_script(var, window, cx);
}

/// Runs the `WA_DEBUG_UI`-style script in env var `var` against `window` (mini windows
/// use `WA_DEBUG_UI_MINI`).
pub fn run_debug_ui_script<T: 'static>(var: &str, window: &mut Window, cx: &mut Context<T>) {
    let Ok(script) = std::env::var(var) else { return };
    cx.spawn_in(window, async move |_, cx| {
        cx.background_executor().timer(Duration::from_millis(2000)).await;
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (verb, arg) = step.split_once(' ').unwrap_or((step, ""));
            if verb == "wait" {
                let ms = arg.parse().unwrap_or(500);
                cx.background_executor().timer(Duration::from_millis(ms)).await;
                continue;
            }
            if verb == "scrollbench" {
                let (at, secs) = arg.split_once(' ').unwrap_or((arg, "10"));
                let position = parse_point(at);
                let end = std::time::Instant::now() + Duration::from_secs_f32(secs.trim().parse().unwrap_or(10.));
                let (mut tick, start) = (0u64, std::time::Instant::now());
                log::info!("WA_DEBUG_UI: scrollbench start");
                while std::time::Instant::now() < end {
                    // 1.5 s up (towards older messages), 1.5 s back down; 9.6 pt per 8 ms.
                    let up = (start.elapsed().as_millis() / 1500) % 2 == 0;
                    let _ = cx.update(|window, cx| debug_scroll(window, cx, position, if up { 9.6 } else { -9.6 }));
                    tick += 1;
                    cx.background_executor().timer(Duration::from_millis(8)).await;
                }
                log::info!("WA_DEBUG_UI: scrollbench done ({tick} events)");
                continue;
            }
            // Keystroke parsing wants 'static strings; a test hook can afford the leak.
            let arg: &'static str = Box::leak(arg.to_string().into_boxed_str());
            let _ = cx.update(|window, cx| match verb {
                // "press" / "rpress": a frame between down and up, which InteractiveText needs.
                "click" | "rclick" | "press" | "rpress" => {
                    // Hit testing uses the last drawn frame, which is stale with no screen.
                    #[cfg(feature = "snapshots")]
                    {
                        window.refresh();
                        window.draw(cx).clear(cx);
                    }
                    let Some((x, y)) = arg.split_once(',') else { return };
                    let position = point(px(x.trim().parse().unwrap_or(0.)), px(y.trim().parse().unwrap_or(0.)));
                    let button = if verb.starts_with('r') { MouseButton::Right } else { MouseButton::Left };
                    let modifiers = Modifiers::default();
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent { position, pressed_button: None, modifiers }),
                        cx,
                    );
                    window.dispatch_event(
                        PlatformInput::MouseDown(MouseDownEvent {
                            button,
                            position,
                            modifiers,
                            click_count: 1,
                            first_mouse: false,
                        }),
                        cx,
                    );
                    #[cfg(feature = "snapshots")]
                    if verb.ends_with("press") {
                        window.refresh();
                        window.draw(cx).clear(cx);
                    }
                    window.dispatch_event(
                        PlatformInput::MouseUp(MouseUpEvent { button, position, modifiers, click_count: 1 }),
                        cx,
                    );
                }
                // "select x1,y1 x2,y2": press, move with the button down, release.
                "select" => {
                    #[cfg(feature = "snapshots")]
                    {
                        window.refresh();
                        window.draw(cx).clear(cx);
                    }
                    let (from, to) = arg.split_once(' ').unwrap_or((arg, arg));
                    let (from, to) = (parse_point(from), parse_point(to));
                    let modifiers = Modifiers::default();
                    let button = MouseButton::Left;
                    window.dispatch_event(
                        PlatformInput::MouseDown(MouseDownEvent { button, position: from, modifiers, click_count: 1, first_mouse: false }),
                        cx,
                    );
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent { position: to, pressed_button: Some(button), modifiers }),
                        cx,
                    );
                    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button, position: to, modifiers, click_count: 1 }), cx);
                }
                "drag" | "drop" => {
                    let (at, files) = arg.split_once(' ').unwrap_or((arg, ""));
                    let Some((x, y)) = at.split_once(',') else { return };
                    let position = point(px(x.trim().parse().unwrap_or(0.)), px(y.trim().parse().unwrap_or(0.)));
                    let event = if verb == "drag" {
                        let paths: Vec<std::path::PathBuf> = files.split('|').filter(|p| !p.is_empty()).map(Into::into).collect();
                        window.dispatch_event(
                            PlatformInput::FileDrop(FileDropEvent::Entered { position, paths: ExternalPaths(paths.into()) }),
                            cx,
                        );
                        FileDropEvent::Pending { position }
                    } else {
                        FileDropEvent::Submit { position }
                    };
                    window.dispatch_event(PlatformInput::FileDrop(event), cx);
                }
                "scroll" => {
                    #[cfg(feature = "snapshots")]
                    {
                        window.refresh();
                        window.draw(cx).clear(cx);
                    }
                    let (at, dy) = arg.split_once(' ').unwrap_or((arg, "0"));
                    debug_scroll(window, cx, parse_point(at), dy.trim().parse().unwrap_or(0.));
                }
                "key" => {
                    if let Ok(ks) = Keystroke::parse(arg) {
                        window.dispatch_keystroke(ks, cx);
                    }
                }
                "type" => {
                    for ch in arg.chars() {
                        let key: &'static str = if ch == ' ' { "space" } else { Box::leak(ch.to_string().into_boxed_str()) };
                        if let Ok(ks) = Keystroke::parse(key) {
                            window.dispatch_keystroke(ks, cx);
                        }
                    }
                }
                #[cfg(feature = "snapshots")]
                "shot" => {
                    window.refresh();
                    window.draw(cx).clear(cx);
                    match window.render_to_image().map(|img| img.save(arg)) {
                        Ok(Ok(())) => log::info!("WA_DEBUG_UI: saved {arg}"),
                        Ok(Err(e)) => log::warn!("WA_DEBUG_UI: saving {arg}: {e}"),
                        Err(e) => log::warn!("WA_DEBUG_UI: rendering {arg}: {e}"),
                    }
                }
                _ => log::warn!("WA_DEBUG_UI: unknown step {step:?}"),
            });
            cx.background_executor().timer(Duration::from_millis(250)).await;
        }
    })
    .detach();
}
