//! A chat's gallery, over the conversation: every photo and video in a grid by month,
//! its documents, and the links shared in it. Photos open in the viewer (←/→ steps
//! through all of them), videos in the player; right-click for copy, save, forward and
//! the rest.

use std::ops::Range;

use chrono::{Local, TimeZone as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::{ChatMessage, FileKind, MessageBody};

use crate::download::Fetch;
use crate::format::transcript_time;
use crate::icons::{Glyph, glyph};
use crate::native_menu::Item;
use crate::state::{GalleryTab, MediaTile, MessageAction, Store};
use crate::theme::Theme;

const PAD: f32 = 20.;
const GAP: f32 = 4.;
/// Tiles are about this wide; the grid fits as many as it can.
const TILE: f32 = 150.;
const HEADER_HEIGHT: f32 = 52.;

#[derive(Clone)]
enum Row {
    /// "October 2026".
    Month(String),
    /// Media tiles `range` (indices into the chat's media, newest first).
    Tiles(Range<usize>),
    /// A document or a link message, by index.
    File(usize),
    Note(&'static str),
}

pub struct Gallery {
    store: Entity<Store>,
    list: ListState,
    rows: Vec<Row>,
    /// What `rows` were built from: chat, tab, item count, columns.
    built: Option<(String, GalleryTab, usize, usize)>,
    focus: FocusHandle,
    was_open: bool,
}

impl Gallery {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            list: ListState::new(0, ListAlignment::Top, px(600.)),
            rows: Vec::new(),
            built: None,
            focus: cx.focus_handle(),
            was_open: false,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn columns(width: f32) -> usize {
        (((width - 2. * PAD + GAP) / (TILE + GAP)).floor() as usize).max(3)
    }

    fn month(ts: i64) -> String {
        Local.timestamp_opt(ts, 0).single().map(|t| t.format("%B %Y").to_string()).unwrap_or_default()
    }

    /// Rows for the tab: media in rows of `columns` under month headings; documents and
    /// links one per row, under month headings too.
    fn build(&mut self, chat: &str, tab: GalleryTab, columns: usize, cx: &App) {
        let s = self.store.read(cx);
        let mut rows = Vec::new();
        let count;
        match tab {
            GalleryTab::Media => {
                let tiles = s.chat_media(chat).unwrap_or_default();
                count = tiles.len();
                let mut start = 0;
                while start < tiles.len() {
                    let month = Self::month(tiles[start].message.timestamp);
                    let end = tiles[start..]
                        .iter()
                        .position(|t| Self::month(t.message.timestamp) != month)
                        .map_or(tiles.len(), |n| start + n);
                    rows.push(Row::Month(month));
                    let mut i = start;
                    while i < end {
                        rows.push(Row::Tiles(i..(i + columns).min(end)));
                        i += columns;
                    }
                    start = end;
                }
                if s.chat_media(chat).is_none() {
                    rows.push(Row::Note("Loading…"));
                } else if count == 0 {
                    rows.push(Row::Note("No photos or videos in this chat"));
                }
            }
            GalleryTab::Docs | GalleryTab::Links => {
                let kind = if tab == GalleryTab::Docs { FileKind::Docs } else { FileKind::Links };
                let items = s.chat_files(chat, kind);
                let items = items.unwrap_or_default();
                count = items.len();
                let mut month = String::new();
                for (i, m) in items.iter().enumerate() {
                    let this = Self::month(m.timestamp);
                    if this != month {
                        rows.push(Row::Month(this.clone()));
                        month = this;
                    }
                    rows.push(Row::File(i));
                }
                if s.chat_files(chat, kind).is_none() {
                    rows.push(Row::Note("Loading…"));
                } else if count == 0 {
                    rows.push(Row::Note(if tab == GalleryTab::Docs { "No documents in this chat" } else { "No links in this chat" }));
                }
            }
        }
        let key = (chat.to_string(), tab, count, columns);
        if self.built.as_ref().is_some_and(|b| b.0 != key.0 || b.1 != key.1) {
            self.list.scroll_to(ListOffset::default());
        }
        self.rows = rows;
        self.list.reset(self.rows.len());
        self.built = Some(key);
    }

    fn render_row(&self, ix: usize, chat: &str, tab: GalleryTab, side: f32, theme: &Theme, cx: &App) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else { return div().into_any_element() };
        match row {
            Row::Month(label) => div()
                .px(px(PAD))
                .pt(px(16.))
                .pb(px(8.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .child(label)
                .into_any_element(),
            Row::Note(text) => div()
                .pt(px(60.))
                .flex()
                .justify_center()
                .text_size(px(13.))
                .text_color(theme.text_secondary)
                .child(text)
                .into_any_element(),
            Row::Tiles(range) => {
                let s = self.store.read(cx);
                let tiles = s.chat_media(chat).unwrap_or_default();
                let tiles: Vec<AnyElement> =
                    tiles.get(range).unwrap_or_default().iter().map(|t| media_tile(t, side, &self.store, theme, cx)).collect();
                div().px(px(PAD)).pb(px(GAP)).flex().gap(px(GAP)).children(tiles).into_any_element()
            }
            Row::File(i) => {
                let kind = if tab == GalleryTab::Docs { FileKind::Docs } else { FileKind::Links };
                let Some(m) = self.store.read(cx).chat_files(chat, kind).and_then(|f| f.get(i)).cloned() else {
                    return div().into_any_element();
                };
                match kind {
                    FileKind::Docs => doc_row(&m, &self.store, theme, cx).into_any_element(),
                    FileKind::Links => link_row(&m, &self.store, theme).into_any_element(),
                }
            }
        }
    }
}

/// A photo or video: its picture, a video's length, a download badge until it's here.
fn media_tile(t: &MediaTile, side: f32, store: &Entity<Store>, theme: &Theme, cx: &App) -> AnyElement {
    let m = &t.message;
    let (path, state, video) = match &m.body {
        MessageBody::Image(i) => (i.path.clone(), i.state, None),
        MessageBody::Video(v) => (v.path.clone(), v.state, Some(v.clone())),
        _ => return div().into_any_element(),
    };
    let fetch = Fetch::of(path.is_some(), state, store.read(cx).download_progress(&m.chat_jid, &m.id));
    let picture = match (&path, &t.thumb, &video) {
        (Some(p), _, None) => Some(img(p.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element()),
        (_, Some(thumb), _) => Some(img(thumb.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element()),
        _ => None,
    };
    let (click_store, chat, id) = (store.clone(), m.chat_jid.clone(), m.id.clone());
    let (menu_store, message) = (store.clone(), m.clone());
    let hover = theme.accent.opacity(0.0);
    div()
        .id(SharedString::from(format!("gallery-{}", m.id)))
        .relative()
        .flex_none()
        .size(px(side))
        .rounded(px(4.))
        .overflow_hidden()
        .bg(theme.separator)
        .cursor_pointer()
        .children(picture)
        .child(div().absolute().inset_0().bg(hover).hover(|s| s.bg(white().opacity(0.08))))
        .when_some(video.as_ref().filter(|_| fetch.ready() || t.thumb.is_some()), |d, v| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(5.))
                    .left(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(white())
                    .child(glyph(Glyph::Play, 10., white()))
                    .child(if v.gif { "GIF".to_string() } else { format!("{}:{:02}", v.duration_secs / 60, v.duration_secs % 60) }),
            )
        })
        .when_some(crate::download::badge(fetch, 30.), |d, badge| {
            d.child(div().absolute().inset_0().bg(black().opacity(0.28)).flex().items_center().justify_center().child(badge))
        })
        .on_click(move |_, _, cx| match path.clone() {
            Some(path) => match &video {
                Some(v) => click_store.update(cx, |s, cx| {
                    s.open_video_in(crate::state::OpenVideo { path, width: v.width, height: v.height }, None, cx)
                }),
                None => click_store.update(cx, |s, cx| s.open_lightbox_in(path, Some(id.clone()), None, cx)),
            },
            None if !matches!(fetch, Fetch::Downloading(_)) => click_store.update(cx, |s, _| s.request_media(&chat, &id)),
            None => {}
        })
        .on_mouse_down(MouseButton::Right, move |ev, window, cx| {
            cx.stop_propagation();
            crate::native_menu::show(crate::media_actions::media_menu(&message, &menu_store, None), ev.position, window, cx);
        })
        .into_any_element()
}

fn sender_label(m: &ChatMessage) -> String {
    if m.from_me { "You".into() } else { m.sender_name.clone().unwrap_or_default() }
}

fn row_frame(id: String, theme: &Theme) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .mx(px(PAD - 8.))
        .px(px(8.))
        .py(px(8.))
        .rounded(px(8.))
        .flex()
        .items_center()
        .gap(px(12.))
        .cursor_pointer()
        .hover(|s| s.bg(theme.search_bg))
}

fn icon_box(icon: Glyph, theme: &Theme) -> Div {
    div()
        .flex_none()
        .size(px(40.))
        .rounded(px(8.))
        .bg(theme.search_bg)
        .flex()
        .items_center()
        .justify_center()
        .child(glyph(icon, 18., theme.text_secondary))
}

/// A document: name, size and kind, who sent it when. Click opens it (or downloads it).
fn doc_row(m: &ChatMessage, store: &Entity<Store>, theme: &Theme, cx: &App) -> Stateful<Div> {
    let MessageBody::Document(d) = &m.body else { return div().id("doc-none") };
    let fetch = Fetch::of(d.path.is_some(), d.state, store.read(cx).download_progress(&m.chat_jid, &m.id));
    let ext = std::path::Path::new(&d.file_name).extension().and_then(|e| e.to_str()).map(str::to_uppercase);
    let mut details: Vec<String> = Vec::new();
    if let Some(size) = d.size_bytes {
        details.push(crate::kinds::human_size(size));
    }
    if let Some(pages) = d.page_count.filter(|p| *p > 0) {
        details.push(format!("{pages} page{}", if pages == 1 { "" } else { "s" }));
    }
    details.extend(ext);
    let byline = format!("{} · {}", sender_label(m), transcript_time(m.timestamp));
    let (path, click_store, chat, id) = (d.path.clone(), store.clone(), m.chat_jid.clone(), m.id.clone());
    let (menu_store, message) = (store.clone(), m.clone());
    row_frame(format!("doc-{}", m.id), theme)
        .child(icon_box(Glyph::Document, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(div().text_size(px(13.)).text_color(theme.text).truncate().child(d.file_name.clone()))
                .child(div().text_size(px(11.5)).text_color(theme.text_secondary).truncate().child(details.join(" · ")))
                .child(div().text_size(px(11.5)).text_color(theme.text_secondary).truncate().child(byline)),
        )
        .children(crate::download::badge(fetch, 26.).map(|b| div().flex_none().child(b)))
        .on_click(move |_, _, cx| match &path {
            Some(p) => cx.open_with_system(p),
            None if !matches!(fetch, Fetch::Downloading(_)) => click_store.update(cx, |s, _| s.request_media(&chat, &id)),
            None => {}
        })
        .on_mouse_down(MouseButton::Right, move |ev, window, cx| {
            cx.stop_propagation();
            crate::native_menu::show(crate::media_actions::media_menu(&message, &menu_store, None), ev.position, window, cx);
        })
}

/// A message with links: the first link's host and address, and the message around it.
fn link_row(m: &ChatMessage, store: &Entity<Store>, theme: &Theme) -> Stateful<Div> {
    let text = crate::conversation::copy_text(m).unwrap_or_default();
    let linked = crate::links::linkify(&text, &[]);
    let Some(link) = linked.links.first().cloned() else { return div().id(SharedString::from(format!("link-{}", m.id))) };
    let host = link.url.host_str().unwrap_or(link.url.as_str()).trim_start_matches("www.").to_string();
    let shown = linked.text[link.range.clone()].to_string();
    let others = linked.links.len() - 1;
    let byline = format!("{} · {}", sender_label(m), transcript_time(m.timestamp));
    let snippet = text.replace('\n', " ");
    let (copy_url, menu_store, message) = (link.url.to_string(), store.clone(), m.clone());
    row_frame(format!("link-{}", m.id), theme)
        .child(icon_box(Glyph::Link, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.text).truncate().child(host))
                .child(div().text_size(px(12.)).text_color(theme.accent).truncate().child(if others > 0 {
                    format!("{shown}  (+{others} more)")
                } else {
                    shown
                }))
                .when(snippet.trim() != linked.text[link.range.clone()].trim(), |d| {
                    d.child(div().text_size(px(11.5)).text_color(theme.text_secondary).truncate().child(snippet))
                })
                .child(div().text_size(px(11.5)).text_color(theme.text_secondary).truncate().child(byline)),
        )
        .on_click(move |_, window, cx| {
            if link.mark.is_some() {
                crate::links::confirm(link.clone(), window, cx);
            } else {
                crate::links::open(&link, cx);
            }
        })
        .on_mouse_down(MouseButton::Right, move |ev, window, cx| {
            cx.stop_propagation();
            let url = copy_url.clone();
            let (store, message) = (menu_store.clone(), message.clone());
            let items = vec![
                Item::action("Copy Link", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(url))),
                Item::Separator,
                Item::action("Show in Chat", move |_, cx| {
                    store.update(cx, |s, cx| s.message_action(MessageAction::Reveal(message), None, cx))
                }),
            ];
            crate::native_menu::show(items, ev.position, window, cx);
        })
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let Some((chat, tab)) = self.store.read(cx).gallery.clone() else {
            self.was_open = false;
            return div().into_any_element();
        };
        if !self.was_open {
            self.was_open = true;
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
        let width = f32::from(window.viewport_size().width) - crate::SIDEBAR_WIDTH;
        let columns = Self::columns(width);
        let side = ((width - 2. * PAD - GAP * (columns - 1) as f32) / columns as f32).floor();
        // Rebuild when what's listed changed (new photos, the tab, the window's width).
        let s = self.store.read(cx);
        let count = match tab {
            GalleryTab::Media => s.chat_media(&chat).map_or(0, <[_]>::len),
            GalleryTab::Docs => s.chat_files(&chat, FileKind::Docs).map_or(0, <[_]>::len),
            GalleryTab::Links => s.chat_files(&chat, FileKind::Links).map_or(0, <[_]>::len),
        };
        let loaded = match tab {
            GalleryTab::Media => s.chat_media(&chat).is_some(),
            GalleryTab::Docs => s.chat_files(&chat, FileKind::Docs).is_some(),
            GalleryTab::Links => s.chat_files(&chat, FileKind::Links).is_some(),
        };
        let name = s.chats.iter().find(|c| c.jid == chat).map(|c| c.name.clone()).unwrap_or_default();
        let counts = [
            s.chat_media(&chat).map(<[_]>::len),
            s.chat_files(&chat, FileKind::Docs).map(<[_]>::len),
            s.chat_files(&chat, FileKind::Links).map(<[_]>::len),
        ];
        let key = (chat.clone(), tab, count, columns);
        let stale_note = loaded && self.rows.iter().any(|r| matches!(r, Row::Note("Loading…")));
        if self.built.as_ref() != Some(&key) || stale_note {
            self.build(&chat, tab, columns, cx);
        }

        let glass_on = crate::glass::enabled(window);
        let tab_button = |t: GalleryTab, label: &str, n: Option<usize>| {
            let selected = t == tab;
            let store = self.store.clone();
            div()
                .id(SharedString::from(format!("gallery-tab-{t:?}")))
                .px(px(12.))
                .py(px(4.))
                .text_size(px(12.5))
                .font_weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                .text_color(if selected { theme.text } else { theme.text_secondary })
                // On glass, the selected tab is a tinted capsule; otherwise a raised segment.
                .map(|d| match (glass_on, selected) {
                    (true, true) => d.rounded_full().bg(theme.text.opacity(0.1)),
                    (true, false) => d.rounded_full().hover(|s| s.bg(theme.text.opacity(0.05))),
                    (false, true) => d.rounded(px(6.)).bg(theme.window_bg).shadow_sm(),
                    (false, false) => d.rounded(px(6.)),
                })
                .cursor_pointer()
                .child(match n {
                    Some(n) => format!("{label} {n}"),
                    None => label.to_string(),
                })
                .on_click(move |_, _, cx| store.update(cx, |s, cx| s.set_gallery_tab(t, cx)))
        };
        let tabs = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(tab_button(GalleryTab::Media, "Media", counts[0]))
            .child(tab_button(GalleryTab::Docs, "Docs", counts[1]))
            .child(tab_button(GalleryTab::Links, "Links", counts[2]));
        let title = div()
            .min_w_0()
            .flex()
            .flex_col()
            .child(div().text_size(px(14.)).line_height(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).truncate().child(name))
            .child(div().text_size(px(11.)).line_height(px(13.)).text_color(theme.text_secondary).child("Media, documents and links"));
        let close_store = self.store.clone();
        let close = div()
            .id("gallery-close")
            .on_click(move |_, _, cx| close_store.update(cx, |s, cx| s.close_gallery(cx)));
        let keys_store = self.store.clone();
        let view = cx.entity();
        let row_chat = chat.clone();
        let grid = list(self.list.clone(), move |ix, window, cx| {
            let theme = Theme::for_window(window);
            view.read(cx).render_row(ix, &row_chat, tab, side, &theme, cx)
        });
        let root = div()
            .id("gallery")
            .track_focus(&self.focus)
            .on_key_down(move |ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    keys_store.update(cx, |s, cx| s.close_gallery(cx));
                    cx.stop_propagation();
                }
            })
            // Covers the conversation: nothing under it takes clicks.
            .occlude()
            .absolute()
            .inset_0()
            .bg(theme.window_bg);
        if glass_on {
            // Like the conversation: the grid scrolls under floating glass (the title, the
            // tabs, ✕), fading into the window background at the top.
            let header = div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(HEADER_HEIGHT))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(10.))
                .child(div().flex_1().min_w_0().flex().child(crate::glass::glass(
                    "gallery-title",
                    19.,
                    title.h(px(38.)).px(px(16.)).justify_center(),
                )))
                .child(crate::glass::glass("gallery-tabs", 19., tabs.h(px(38.)).px(px(4.))))
                .child(crate::glass::circle_button(
                    "gallery-close-glass",
                    32.,
                    theme.search_bg,
                    close.child(glyph(Glyph::Close, 12., theme.text)),
                ));
            return root
                .child(grid.size_full().pt(px(HEADER_HEIGHT + 4.)).pb(px(PAD)))
                .child(crate::conversation::edge_fade(true, HEADER_HEIGHT + 12., &theme))
                .child(header)
                .into_any_element();
        }
        let header = div()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px(px(PAD))
            .flex()
            .items_center()
            .gap(px(12.))
            .border_b_1()
            .border_color(theme.separator)
            .child(title.flex_1())
            .child(tabs.p(px(2.)).rounded(px(8.)).bg(theme.search_bg))
            .child(
                close
                    .flex_none()
                    .size(px(28.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.search_bg))
                    .child(glyph(Glyph::Close, 12., theme.text_secondary)),
            );
        root.flex().flex_col().child(header).child(grid.flex_1().pb(px(PAD))).into_any_element()
    }
}
