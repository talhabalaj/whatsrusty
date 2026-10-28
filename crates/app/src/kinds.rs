//! Bubbles for the less common message kinds: video, documents, stickers, locations,
//! contact cards, polls, business messages and system lines.

use std::sync::Arc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::{
    ChatMessage, ContactCard, DocumentMedia, LocationInfo, MediaState, PollInfo, RichText, StickerMedia, VideoMedia,
};

use crate::avatar::avatar;
use crate::icons::{Glyph, glyph};
use crate::state::Store;
use crate::theme::Theme;

pub const BUBBLE_RADIUS: f32 = 18.;
const CARD_W: f32 = 260.;

/// Ask the backend for a message's media once it's on screen.
fn ensure_download(store: &Entity<Store>, m: &ChatMessage, has_path: bool, state: MediaState, cx: &mut App) {
    if !has_path && state == MediaState::Idle {
        let (store, chat, id) = (store.clone(), m.chat_jid.clone(), m.id.clone());
        cx.defer(move |cx| store.update(cx, |s, _| s.request_media(&chat, &id)));
    }
}

/// Open the file if it's downloaded, otherwise (re)request it.
fn open_or_fetch(store: Entity<Store>, m: &ChatMessage, path: Option<std::path::PathBuf>) -> impl Fn(&ClickEvent, &mut Window, &mut App) + use<> {
    let (chat, id) = (m.chat_jid.clone(), m.id.clone());
    move |_, _, cx| match &path {
        Some(p) => cx.open_with_system(p),
        None => store.update(cx, |s, _| s.request_media(&chat, &id)),
    }
}

fn pill(text: impl Into<SharedString>) -> Div {
    div().px(px(7.)).py(px(2.)).rounded_full().bg(black().opacity(0.5)).text_color(white()).text_size(px(11.)).child(text.into())
}

fn duration(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn video(
    store: &Entity<Store>,
    m: &ChatMessage,
    v: &VideoMedia,
    thumb: Option<Arc<Image>>,
    mini: Option<String>,
    theme: &Theme,
    cx: &mut App,
) -> Stateful<Div> {
    // Fetch GIFs eagerly (they're small); full videos only on click.
    if v.gif {
        ensure_download(store, m, v.path.is_some(), v.state, cx);
    }
    let (w, h) = if v.round {
        (200., 200.)
    } else {
        crate::conversation::image_box(v.width, v.height)
    };
    let radius = if v.round { 100. } else { BUBBLE_RADIUS };
    let status = match (v.path.is_some(), v.state) {
        (false, MediaState::Downloading) => Some("Downloading…".to_string()),
        (false, MediaState::Failed) => Some("Couldn't download".to_string()),
        _ => None,
    };
    let badge = if v.gif { "GIF".to_string() } else { duration(v.duration_secs) };
    div()
        .id(SharedString::from(format!("video-{}", m.id)))
        .relative()
        .w(px(w))
        .h(px(h))
        .rounded(px(radius))
        .overflow_hidden()
        .bg(theme.bubble_in)
        .cursor_pointer()
        .when_some(thumb, |d, t| d.child(img(t).size_full().rounded(px(radius)).object_fit(ObjectFit::Cover)))
        .child(
            div().absolute().inset_0().flex().items_center().justify_center().child(
                div()
                    .size(px(46.))
                    .rounded_full()
                    .bg(black().opacity(0.45))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph(Glyph::Play, 20., white())),
            ),
        )
        .child(div().absolute().bottom(px(8.)).right(px(if v.round { 70. } else { 8. })).child(pill(status.unwrap_or(badge))))
        .on_click({
            let (store, chat, id, path) = (store.clone(), m.chat_jid.clone(), m.id.clone(), v.path.clone());
            let (width, height) = (v.width, v.height);
            move |_, _, cx| match &path {
                Some(p) => store.update(cx, |s, cx| {
                    let video = crate::state::OpenVideo { path: p.clone(), width, height };
                    s.open_video_in(video, mini.clone(), cx)
                }),
                None => store.update(cx, |s, _| s.request_media(&chat, &id)),
            }
        })
}

pub fn document(
    store: &Entity<Store>,
    m: &ChatMessage,
    d: &DocumentMedia,
    from_me: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let ext = std::path::Path::new(&d.file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_uppercase())
        .unwrap_or_else(|| "FILE".into());
    let mut meta = vec![ext.clone()];
    if let Some(size) = d.size_bytes {
        meta.push(human_size(size));
    }
    if let Some(pages) = d.page_count.filter(|p| *p > 0) {
        meta.push(format!("{pages} page{}", if pages == 1 { "" } else { "s" }));
    }
    let meta = match (d.path.is_some(), d.state) {
        (false, MediaState::Downloading) => "Downloading…".to_string(),
        (false, MediaState::Failed) => "Couldn't download. Click to retry".to_string(),
        _ => meta.join(" · "),
    };
    let (fg, sub, tile) = if from_me {
        (theme.bubble_out_text, theme.bubble_out_text.opacity(0.75), white().opacity(0.2))
    } else {
        (theme.bubble_in_text, theme.text_secondary, theme.search_bg)
    };
    div()
        .id(SharedString::from(format!("doc-{}", m.id)))
        .w(px(CARD_W + 20.))
        .flex()
        .items_center()
        .gap(px(10.))
        .cursor_pointer()
        .child(
            div()
                .flex_none()
                .w(px(36.))
                .h(px(44.))
                .rounded(px(6.))
                .bg(tile)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.))
                .font_weight(FontWeight::BOLD)
                .text_color(fg)
                .child(ext.chars().take(4).collect::<String>()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).text_color(fg).truncate().child(d.file_name.clone()))
                .child(div().text_size(px(11.5)).text_color(sub).child(meta)),
        )
        .on_click(open_or_fetch(store.clone(), m, d.path.clone()))
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b / (1 << 10)),
        b => format!("{b} B"),
    }
}

/// Stickers float without a bubble, like Messages.
pub fn sticker(store: &Entity<Store>, m: &ChatMessage, s: &StickerMedia, theme: &Theme, cx: &mut App) -> Stateful<Div> {
    ensure_download(store, m, s.path.is_some(), s.state, cx);
    let size = 140.;
    let failed = s.path.is_none() && s.state == MediaState::Failed;
    div()
        .id(SharedString::from(format!("sticker-{}", m.id)))
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        // Expired stickers can sometimes be re-uploaded by the sender's phone: click retries.
        .when(failed, |d| d.cursor_pointer().on_click(open_or_fetch(store.clone(), m, None)))
        .map(|d| match &s.path {
        // An element id is what lets GPUI keep the frame index, so animated WebP plays.
        Some(p) => d.child(img(p.clone()).id(SharedString::from(format!("sticker-img-{}", m.id))).size_full().object_fit(ObjectFit::Contain)),
        None => d.child(
            div()
                .size(px(size * 0.8))
                .rounded(px(24.))
                .bg(theme.search_bg)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .text_color(theme.text_secondary)
                .text_center()
                .child(if failed { "Sticker unavailable\nClick to retry" } else { "…" }),
        ),
    })
}

pub fn location(m: &ChatMessage, l: &LocationInfo, thumb: Option<Arc<Image>>, theme: &Theme, from_me: bool) -> Stateful<Div> {
    let (lat, lon) = (l.latitude, l.longitude);
    let q = l.name.clone().unwrap_or_else(|| "Shared Location".into());
    let url = format!("https://maps.apple.com/?ll={lat},{lon}&q={}", url_escape(&q));
    let (bg, fg, sub) = if from_me {
        (theme.bubble_out, theme.bubble_out_text, theme.bubble_out_text.opacity(0.75))
    } else {
        (theme.bubble_in, theme.bubble_in_text, theme.text_secondary)
    };
    div()
        .id(SharedString::from(format!("loc-{}", m.id)))
        .w(px(CARD_W))
        .rounded(px(BUBBLE_RADIUS))
        .overflow_hidden()
        .bg(bg)
        .cursor_pointer()
        .child(div().w_full().h(px(140.)).bg(theme.search_bg).map(|d| match thumb {
            Some(t) => d.child(img(t).w(px(CARD_W)).h(px(140.)).aspect_ratio(CARD_W / 140.).object_fit(ObjectFit::Cover)),
            None => d.flex().items_center().justify_center().text_size(px(34.)).child("📍"),
        }))
        .child(
            div()
                .px(px(12.))
                .py(px(8.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(13.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(fg)
                        .child(if l.live { format!("Live location · {q}") } else { q }),
                )
                .when_some(l.address.clone(), |d, a| d.child(div().text_size(px(11.5)).text_color(sub).line_clamp(2).child(a))),
        )
        .on_click(move |_, _, cx| cx.open_url(&url))
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn contacts(cards: &[ContactCard], from_me: bool, theme: &Theme) -> Div {
    let (fg, sub) = if from_me {
        (theme.bubble_out_text, theme.bubble_out_text.opacity(0.75))
    } else {
        (theme.bubble_in_text, theme.text_secondary)
    };
    div().w(px(CARD_W - 24.)).flex().flex_col().gap(px(8.)).children(cards.iter().map(|c| {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(avatar(&c.display_name, None, 34., theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(13.5)).font_weight(FontWeight::MEDIUM).text_color(fg).truncate().child(c.display_name.clone()))
                    .when_some(c.phones.first().cloned(), |d, p| d.child(div().text_size(px(11.5)).text_color(sub).child(p))),
            )
    }))
}

pub fn poll(p: &PollInfo, from_me: bool, theme: &Theme) -> Div {
    let (fg, sub, ring) = if from_me {
        (theme.bubble_out_text, theme.bubble_out_text.opacity(0.75), white().opacity(0.7))
    } else {
        (theme.bubble_in_text, theme.text_secondary, theme.text_secondary)
    };
    div()
        .w(px(CARD_W - 24.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(div().text_size(px(11.)).text_color(sub).child(if p.multiple { "POLL · SELECT ONE OR MORE" } else { "POLL · SELECT ONE" }))
        .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(fg).child(p.question.clone()))
        .children(p.options.iter().map(|o| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(2.))
                .child(
                    div()
                        .flex_none()
                        .size(px(15.))
                        .border_1()
                        .border_color(ring)
                        .map(|d| if p.multiple { d.rounded(px(4.)) } else { d.rounded_full() }),
                )
                .child(div().text_size(px(13.5)).text_color(fg).child(o.clone()))
        }))
        .child(div().text_size(px(11.)).text_color(sub).child("Vote on your phone"))
}

/// Business/bot message: optional header, body, footer, and its buttons as rows.
pub fn rich(r: &RichText, from_me: bool, theme: &Theme) -> Div {
    let (fg, sub, accent, line) = if from_me {
        (theme.bubble_out_text, theme.bubble_out_text.opacity(0.75), theme.bubble_out_text, white().opacity(0.25))
    } else {
        (theme.bubble_in_text, theme.text_secondary, theme.accent, theme.separator)
    };
    div()
        .max_w(px(380.))
        .flex()
        .flex_col()
        .gap(px(3.))
        .when_some(r.header.clone(), |d, h| d.child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(fg).child(h)))
        .child(div().text_size(px(14.)).line_height(px(19.)).text_color(fg).child(r.body.clone()))
        .when_some(r.footer.clone(), |d, f| d.child(div().text_size(px(11.5)).text_color(sub).child(f)))
        .when(!r.buttons.is_empty(), |d| {
            d.child(div().mt(px(4.)).flex().flex_col().children(r.buttons.iter().map(|b| {
                div()
                    .border_t_1()
                    .border_color(line)
                    .py(px(6.))
                    .flex()
                    .justify_center()
                    .text_size(px(13.5))
                    .text_color(accent)
                    .child(b.clone())
            })))
        })
}

/// Centered gray line for calls, events, invites.
pub fn system_line(text: &str, theme: &Theme) -> Div {
    div()
        .w_full()
        .py(px(6.))
        .flex()
        .justify_center()
        .text_size(px(11.5))
        .text_color(theme.text_secondary)
        .child(text.to_string())
}
