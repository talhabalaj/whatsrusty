//! Conversation list: search field, pinned chats, Messages-style rows, archived chats,
//! New Message picker.

use std::time::{Duration, Instant};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
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
/// With Liquid Glass: the New Message circle and the search capsule.
const COMPOSE_GLASS_SIZE: f32 = 32.;
const SEARCH_GLASS_HEIGHT: f32 = 32.;
/// The traffic-light row (52) and the search field with its bottom margin (32 + 8): with
/// glass they float over the list, which starts below them.
const SIDEBAR_TOP: f32 = 52. + SEARCH_GLASS_HEIGHT + 8.;

/// A row of the sidebar list.
#[derive(Clone, PartialEq)]
enum Row {
    /// "‹ Archived", back to the main list.
    ArchivedBack,
    Pinned,
    /// The "Archived" entry above the chats.
    ArchivedEntry,
    Chat(String),
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
                if matches!(ev, InputEvent::Change) {
                    this.query = state.read(cx).value().to_lowercase();
                    cx.notify();
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
        }
        self.sync_rows(cx);
        if let Some(ix) = self.rows.iter().position(|r| *r == Row::Chat(jid.clone())) {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    /// The list's rows for the current view and query.
    fn current_rows(&self, cx: &App) -> Vec<Row> {
        let chats = &self.store.read(cx).chats;
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
        rows.extend(self.visible_jids(cx).into_iter().map(Row::Chat));
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
            Row::ArchivedBack => self.archived_header(theme, cx).into_any_element(),
            Row::Pinned => self.pinned_grid(&chats, theme, cx).unwrap_or_else(|| div().into_any_element()),
            Row::ArchivedEntry => self.archived_row(&chats, theme, cx).unwrap_or_else(|| div().into_any_element()),
            Row::Chat(jid) => {
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
                div()
                    .px(px(10.))
                    .child(chat_row(chat, photo, selected, typing, pin_count, theme, self.store.clone()))
                    .into_any_element()
            }
        }
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

    fn archived_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("archived-back")
            .mx(px(10.))
            .px(px(6.))
            .mb(px(4.))
            .h(px(32.))
            .flex()
            .items_center()
            .gap(px(4.))
            .rounded(px(6.))
            .hover(|s| s.bg(theme.search_bg))
            .on_click(cx.listener(|this, _, _, cx| {
                this.archived_view = false;
                cx.notify();
            }))
            .child(glyph(Glyph::ChevronLeft, 15., theme.accent))
            .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child("Archived"))
    }

    /// The "Archived" entry above the chats, when anything is archived.
    fn archived_row(&self, chats: &[ChatSummary], theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let archived: Vec<_> = chats.iter().filter(|c| c.archived).collect();
        if archived.is_empty() {
            return None;
        }
        let unread = archived.iter().filter(|c| c.unread_count > 0).count();
        let row = div()
            .id("archived-row")
            .mx(px(10.))
            .h(px(40.))
            .flex()
            .items_center()
            .gap(px(8.))
            .pr(px(10.))
            .rounded(px(8.))
            .hover(|s| s.bg(theme.search_bg))
            .on_click(cx.listener(|this, _, _, cx| {
                this.archived_view = true;
                cx.notify();
            }))
            .child(div().w(px(14.)).flex_none())
            .child(div().w(px(44.)).flex_none().flex().justify_center().child(glyph(Glyph::Archive, 19., theme.text_secondary)))
            .child(div().flex_1().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).child("Archived"))
            .when(unread > 0, |d| d.child(div().text_size(px(12.)).text_color(theme.accent).child(unread.to_string())));
        Some(row.into_any_element())
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
            pinned_tile(chat, state.avatar_cached(&chat.jid), selected, pin_count, theme, self.store.clone())
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
            .when(glass_on, |d| d.absolute().top_0().left_0().right_0())
            .child(self.header(glass_on, &theme, cx))
            .child(self.search_field(glass_on, &theme));
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
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

/// Right-click menu of a chat (row or pinned tile).
fn with_chat_menu<E>(element: E, chat: &ChatSummary, pin_count: usize, store: Entity<Store>) -> impl IntoElement
where
    E: InteractiveElement + ParentElement + Styled + IntoElement + 'static,
{
    let chat = chat.clone();
    element.context_menu(move |menu, window, cx| chat_menu(menu, &chat, pin_count, &store, window, cx))
}

fn chat_menu(
    menu: PopupMenu,
    chat: &ChatSummary,
    pin_count: usize,
    store: &Entity<Store>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let jid = chat.jid.clone();
    let mut menu = menu.min_w(px(180.));
    let j = jid.clone();
    menu = menu.item(menu_item(store, "Pop Out", move |s, cx| crate::mini::pop_out(s, j.clone(), cx))).separator();
    if !chat.archived {
        let pinned = chat.pinned;
        let j = jid.clone();
        menu = menu.item(
            menu_item(store, if pinned { "Unpin" } else { "Pin" }, move |s, cx| s.read(cx).set_pinned(&j, !pinned))
                .disabled(!pinned && pin_count >= MAX_PINNED),
        );
    }
    let j = jid.clone();
    menu = menu.item(match chat.unread_count > 0 {
        true => menu_item(store, "Mark as Read", move |s, cx| s.read(cx).mark_read(&j)),
        false => menu_item(store, "Mark as Unread", move |s, cx| s.update(cx, |s, cx| s.mark_unread(&j, cx))),
    });
    menu = menu.separator();
    if chat.muted {
        let j = jid.clone();
        menu = menu.item(menu_item(store, "Unmute", move |s, cx| s.read(cx).set_muted(&j, 0)));
    } else {
        let (j, store) = (jid.clone(), store.clone());
        menu = menu.submenu("Mute", window, cx, move |m, _, _| {
            let option = |label: &str, secs: Option<i64>| {
                let (j, store) = (j.clone(), store.clone());
                PopupMenuItem::new(label.to_string()).on_click(move |_, _, cx| {
                    let until = secs.map_or(i64::MAX, |s| chrono::Utc::now().timestamp() + s);
                    store.read(cx).set_muted(&j, until);
                })
            };
            m.min_w(px(140.))
                .item(option("For 8 Hours", Some(8 * 3600)))
                .item(option("For 1 Week", Some(7 * 86_400)))
                .item(option("Always", None))
        });
    }
    let archived = chat.archived;
    menu.item(menu_item(store, if archived { "Unarchive" } else { "Archive" }, move |s, cx| {
        s.read(cx).set_archived(&jid, !archived)
    }))
}

fn menu_item(store: &Entity<Store>, label: &str, f: impl Fn(&Entity<Store>, &mut App) + 'static) -> PopupMenuItem {
    let store = store.clone();
    PopupMenuItem::new(label.to_string()).on_click(move |_, _, cx| f(&store, cx))
}

/// Unread marker: blue dot, like Messages (gray when the chat is muted).
fn unread_dot(muted: bool, theme: &Theme) -> Div {
    div().size(px(9.)).rounded_full().bg(if muted { theme.text_secondary } else { theme.unread_dot })
}

fn pinned_tile(
    chat: &ChatSummary,
    photo: Option<std::sync::Arc<Image>>,
    selected: bool,
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
