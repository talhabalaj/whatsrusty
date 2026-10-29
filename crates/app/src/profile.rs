//! Info panel for a contact or group, opened from the conversation header (or a group
//! sender's name/avatar): photo, name, number, About; groups add the description and
//! participants.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::{ChatSummary, MessageBody, Profile};

use crate::avatar::avatar;
use crate::download::Fetch;
use crate::icons::{Glyph, glyph};
use crate::state::{MediaTile, Store};
use crate::theme::Theme;

pub const PANEL_WIDTH: f32 = 300.;

/// Phone number from a phone-number JID ("923001234567@s.whatsapp.net" → "+923001234567").
fn phone_from_jid(jid: &str) -> Option<String> {
    let (user, server) = jid.split_once('@')?;
    (server == "s.whatsapp.net" && user.chars().all(|c| c.is_ascii_digit())).then(|| format!("+{user}"))
}

fn section(title: &str, body: impl IntoElement, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(12.))
        .rounded(px(10.))
        .bg(theme.search_bg)
        .child(div().text_size(px(11.)).text_color(theme.text_secondary).child(title.to_string()))
        .child(body)
}

/// Media grid columns, and the gap between tiles.
const MEDIA_COLUMNS: usize = 3;
const MEDIA_GAP: f32 = 4.;

/// The chat's photos and videos, newest first: a grid of square tiles opening in the
/// photo viewer / video player (or downloading, the ones not on this Mac yet).
fn media_grid(tiles: &[MediaTile], store: &Entity<Store>, theme: &Theme, cx: &App) -> Div {
    // The panel's padding, then the section's.
    let side = (PANEL_WIDTH - 32. - 24. - MEDIA_GAP * (MEDIA_COLUMNS - 1) as f32) / MEDIA_COLUMNS as f32;
    let s = store.read(cx);
    let tile = |t: &MediaTile| {
        let m = &t.message;
        let (path, state, video) = match &m.body {
            MessageBody::Image(i) => (i.path.clone(), i.state, None),
            MessageBody::Video(v) => (v.path.clone(), v.state, Some(v)),
            _ => return div().into_any_element(),
        };
        let fetch = Fetch::of(path.is_some(), state, s.download_progress(&m.chat_jid, &m.id));
        let picture = match (&path, &t.thumb, video) {
            (Some(p), _, None) => Some(img(p.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element()),
            (_, Some(thumb), _) => Some(img(thumb.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element()),
            _ => None,
        };
        let (store, chat, id) = (store.clone(), m.chat_jid.clone(), m.id.clone());
        let (width, height) = video.map_or((None, None), |v| (v.width, v.height));
        let is_video = video.is_some();
        div()
            .id(SharedString::from(format!("media-{}", m.id)))
            .relative()
            .flex_none()
            .size(px(side))
            .rounded(px(6.))
            .overflow_hidden()
            .bg(theme.separator)
            .cursor_pointer()
            .children(picture)
            .when_some(video.filter(|_| fetch.ready()), |d, v| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(4.))
                        .left(px(5.))
                        .flex()
                        .items_center()
                        .gap(px(3.))
                        .text_size(px(10.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(white())
                        .child(glyph(Glyph::Play, 9., white()))
                        .when(!v.gif, |d| d.child(format!("{}:{:02}", v.duration_secs / 60, v.duration_secs % 60))),
                )
            })
            .when_some(crate::download::badge(fetch, 28.), |d, badge| {
                d.child(div().absolute().inset_0().bg(black().opacity(0.28)).flex().items_center().justify_center().child(badge))
            })
            .on_click(move |_, _, cx| match path.clone() {
                Some(path) if is_video => store.update(cx, |s, cx| {
                    s.open_video_in(crate::state::OpenVideo { path, width, height }, None, cx)
                }),
                Some(path) => store.update(cx, |s, cx| s.open_lightbox_in(path, Some(id.clone()), None, cx)),
                None if !matches!(fetch, Fetch::Downloading(_)) => store.update(cx, |s, _| s.request_media(&chat, &id)),
                None => {}
            })
            .into_any_element()
    };
    div().flex().flex_col().gap(px(MEDIA_GAP)).pt(px(4.)).children(
        tiles.chunks(MEDIA_COLUMNS).map(|row| div().flex().gap(px(MEDIA_GAP)).children(row.iter().map(tile))),
    )
}

pub fn panel(jid: &str, store: &Entity<Store>, glass_on: bool, theme: &Theme, cx: &App) -> impl IntoElement {
    let s = store.read(cx);
    let profile: Option<&Profile> = s.profiles.get(jid);
    let chat: Option<&ChatSummary> = s.chats.iter().find(|c| c.jid == jid);
    let name = profile
        .map(|p| p.name.clone())
        .or_else(|| chat.map(|c| c.name.clone()))
        .unwrap_or_else(|| jid.split('@').next().unwrap_or(jid).to_string());
    let is_group = profile.map_or(jid.ends_with("@g.us"), |p| p.is_group);
    let photo = s.avatar_cached(&format!("{jid}#full")).or_else(|| s.avatar_cached(jid));
    let phone = profile.and_then(|p| p.phone.clone()).or_else(|| phone_from_jid(jid));
    let loading = s.profile_loading(jid);
    let close_store = store.clone();

    let subtitle = if is_group {
        profile.map(|p| format!("Group · {} participants", p.participants.len())).unwrap_or_else(|| "Group".into())
    } else {
        phone.clone().unwrap_or_default()
    };

    div()
        .w(px(PANEL_WIDTH))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(theme.window_bg)
        .border_l_1()
        .border_color(theme.separator)
        .child(
            div().h(px(52.)).flex_none().flex().items_center().justify_end().px(px(12.)).child({
                let close = div()
                    .id("close-profile")
                    .on_click(move |_, _, cx| close_store.update(cx, |s, cx| s.close_profile(cx)));
                if glass_on {
                    // A glass circle, like the header's buttons.
                    let close = close.child(glyph(Glyph::Close, 12., theme.text));
                    crate::glass::circle_button("close-profile", 28., theme.search_bg, close).into_any_element()
                } else {
                    close
                        .size(px(26.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|st| st.bg(theme.search_bg))
                        .child(glyph(Glyph::Close, 12., theme.text_secondary))
                        .into_any_element()
                }
            }),
        )
        .child(
            div()
                .id("profile-scroll")
                .flex_1()
                .overflow_y_scroll()
                .px(px(16.))
                .pb(px(20.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.))
                        .pb(px(4.))
                        .child(avatar(&name, photo, 96., theme))
                        .child(
                            div()
                                .pt(px(6.))
                                .text_size(px(19.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .text_center()
                                .child(name.clone()),
                        )
                        .child(div().text_size(px(12.5)).text_color(theme.text_secondary).child(subtitle)),
                )
                .when(!is_group, |d| {
                    let about = profile.and_then(|p| p.about.clone());
                    d.child(section(
                        "About",
                        div().text_size(px(13.)).text_color(theme.text).child(match (&about, loading) {
                            (Some(a), _) => a.clone(),
                            (None, true) => "Loading…".into(),
                            (None, false) => "Not available".into(),
                        }),
                        theme,
                    ))
                    .when_some(phone.clone(), |d, ph| {
                        d.child(section("Phone", div().text_size(px(13.)).text_color(theme.text).child(ph), theme))
                    })
                })
                .when(is_group, |d| {
                    let description = profile.and_then(|p| p.description.clone());
                    d.when_some(description, |d, desc| {
                        d.child(section("Description", div().text_size(px(13.)).text_color(theme.text).child(desc), theme))
                    })
                })
                .when_some(s.chat_media(jid).filter(|t| !t.is_empty()), |d, tiles| {
                    d.child(section("Photos & Videos", media_grid(tiles, store, theme, cx), theme))
                })
                .when(is_group, |d| {
                    d
                    .child(section(
                        &profile.map_or("Participants".to_string(), |p| format!("{} participants", p.participants.len())),
                        div().flex().flex_col().map(|list| match profile {
                            None => list.child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme.text_secondary)
                                    .child(if loading { "Loading…" } else { "Not available" }),
                            ),
                            Some(p) => list.children(p.participants.iter().map(|m| {
                                let shown = if m.is_me { "You".to_string() } else { m.name.clone() };
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .py(px(5.))
                                    .child(avatar(&m.name, s.avatar_cached(&m.jid), 30., theme))
                                    .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).text_color(theme.text).child(shown))
                                    .when(m.is_admin, |d| {
                                        d.child(
                                            div()
                                                .px(px(6.))
                                                .py(px(1.))
                                                .rounded(px(4.))
                                                .bg(theme.separator)
                                                .text_size(px(10.5))
                                                .text_color(theme.text_secondary)
                                                .child("Admin"),
                                        )
                                    })
                            })),
                        }),
                        theme,
                    ))
                }),
        )
}
