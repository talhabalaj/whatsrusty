//! Info for one of my messages (from its menu): when it was sent, and when it reached
//! and was read by the other person, or by each member of a group.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::{ChatMessage, DeliveryStatus, MessageBody, MessageReceipt, Timestamp};

use crate::avatar::avatar;
use crate::format::transcript_time;
use crate::icons::{Glyph, glyph};
use crate::profile::{PANEL_WIDTH, section};
use crate::state::Store;
use crate::theme::Theme;

/// What the message says, or what it is.
pub fn preview(m: &ChatMessage) -> String {
    if let Some(text) = crate::conversation::copy_text(m) {
        return text;
    }
    match &m.body {
        MessageBody::Image(_) => "Photo",
        MessageBody::Voice(_) => "Voice message",
        MessageBody::Video(v) if v.gif => "GIF",
        MessageBody::Video(_) => "Video",
        MessageBody::Document(d) => return d.file_name.clone(),
        MessageBody::Sticker(_) => "Sticker",
        MessageBody::Location(_) => "Location",
        MessageBody::Contacts(_) => "Contact",
        MessageBody::Poll(p) => return p.question.clone(),
        MessageBody::ViewOnce(_) => "View once",
        _ => "Message",
    }
    .into()
}

fn time_or_dash(ts: Option<Timestamp>) -> String {
    ts.map_or_else(|| "—".into(), transcript_time)
}

/// "Read" / "Delivered" / "Sent" and when, one per line.
fn times(rows: &[(&str, String)], theme: &Theme) -> Div {
    div().flex().flex_col().gap(px(6.)).children(rows.iter().map(|(label, time)| {
        div()
            .flex()
            .justify_between()
            .gap(px(8.))
            .text_size(px(13.))
            .child(div().text_color(theme.text).child(label.to_string()))
            .child(div().text_color(theme.text_secondary).child(time.clone()))
    }))
}

/// One member: picture, name, and when (or nothing, for who it hasn't reached).
fn member(name: &str, jid: &str, when: Option<String>, s: &Store, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .py(px(3.))
        .child(avatar(name, s.avatar_cached(jid), 28., theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(13.)).text_color(theme.text).truncate().child(name.to_string()))
                .children(when.map(|w| div().text_size(px(11.5)).text_color(theme.text_secondary).child(w))),
        )
}

fn members(title: &str, rows: Vec<Div>, theme: &Theme) -> Option<Div> {
    (!rows.is_empty()).then(|| section(title, div().flex().flex_col().pt(px(2.)).children(rows), theme))
}

pub fn panel(chat_jid: &str, id: &str, store: &Entity<Store>, glass_on: bool, theme: &Theme, cx: &App) -> impl IntoElement {
    let s = store.read(cx);
    let message = s.messages.get(chat_jid).and_then(|ms| ms.iter().find(|m| m.id == id));
    let receipts: &[MessageReceipt] = s.receipts.get(&(chat_jid.to_string(), id.to_string())).map_or(&[], Vec::as_slice);
    let is_group = chat_jid.ends_with("@g.us");
    let close_store = store.clone();

    let mut body = div().flex().flex_col().gap(px(12.));
    if let Some(m) = message {
        body = body.child(
            div().flex().justify_end().pl(px(24.)).child(
                div()
                    .max_w_full()
                    .px(px(12.))
                    .py(px(7.))
                    .rounded(px(16.))
                    .bg(theme.bubble_out)
                    .text_color(theme.bubble_out_text)
                    .text_size(px(13.))
                    .line_clamp(6)
                    .child(preview(m)),
            ),
        );
        let sent = match m.status {
            DeliveryStatus::Pending => "Sending…".to_string(),
            DeliveryStatus::Failed => "Not sent".to_string(),
            _ => transcript_time(m.timestamp),
        };
        let voice = matches!(m.body, MessageBody::Voice(_));
        if is_group {
            let read: Vec<Div> = receipts
                .iter()
                .filter_map(|r| {
                    let when = if voice && r.played.is_some() {
                        format!("Played {}", transcript_time(r.played?))
                    } else {
                        transcript_time(r.read?)
                    };
                    Some(member(&r.name, &r.jid, Some(when), s, theme))
                })
                .collect();
            let delivered: Vec<Div> = receipts
                .iter()
                .filter(|r| r.read.is_none())
                .filter_map(|r| Some(member(&r.name, &r.jid, Some(transcript_time(r.delivered?)), s, theme)))
                .collect();
            // The members it hasn't reached yet (once the group's members are known).
            let waiting: Vec<Div> = s
                .profiles
                .get(chat_jid)
                .map(|p| {
                    p.participants
                        .iter()
                        .filter(|p| !p.is_me && !receipts.iter().any(|r| r.jid == p.jid))
                        .map(|p| member(&p.name, &p.jid, None, s, theme))
                        .collect()
                })
                .unwrap_or_default();
            let sent_before = receipts.is_empty() && matches!(m.status, DeliveryStatus::Delivered | DeliveryStatus::Read);
            body = body
                .children(members("Read by", read, theme))
                .children(members("Delivered to", delivered, theme))
                .when(!sent_before && m.status != DeliveryStatus::Failed, |d| {
                    d.children(members("Not delivered yet", waiting, theme))
                })
                .when(sent_before, |d| {
                    d.child(
                        div()
                            .px(px(4.))
                            .text_size(px(12.))
                            .text_color(theme.text_secondary)
                            .child("Who read it isn't known for messages sent before this version."),
                    )
                })
                .child(section("Sent", div().text_size(px(13.)).text_color(theme.text).child(sent), theme));
        } else {
            let r = receipts.first();
            // Receipts from before this version left only the status.
            let reached = |status| m.status != DeliveryStatus::Failed && m.status >= status;
            let mut rows = Vec::new();
            if voice {
                rows.push(("Played", time_or_dash(r.and_then(|r| r.played))));
            }
            let read = r.and_then(|r| r.read);
            rows.push(("Read", if read.is_none() && reached(DeliveryStatus::Read) { "Yes".into() } else { time_or_dash(read) }));
            let delivered = r.and_then(|r| r.delivered);
            rows.push((
                "Delivered",
                if delivered.is_none() && reached(DeliveryStatus::Delivered) { "Yes".into() } else { time_or_dash(delivered) },
            ));
            rows.push(("Sent", sent));
            body = body.child(section("Status", times(&rows, theme), theme));
        }
    } else {
        body = body.child(div().text_size(px(13.)).text_color(theme.text_secondary).child("This message isn't loaded."));
    }

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
            div()
                .h(px(52.))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px(px(16.))
                .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Message Info"))
                .child({
                    let close = div()
                        .id("close-message-info")
                        .on_click(move |_, _, cx| close_store.update(cx, |s, cx| s.close_message_info(cx)));
                    if glass_on {
                        let close = close.child(glyph(Glyph::Close, 12., theme.text));
                        crate::glass::circle_button("close-message-info", 28., theme.search_bg, close).into_any_element()
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
        .child(div().id("message-info-scroll").flex_1().overflow_y_scroll().px(px(16.)).pb(px(20.)).child(body))
}
