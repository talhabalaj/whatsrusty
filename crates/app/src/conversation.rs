//! Transcript + floating header/composer, styled after Messages.app.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::{ChatMessage, DeliveryStatus, ImageMedia, MediaState, MessageBody, QuotedMessage, VoiceMedia};

use crate::audio::AudioPlayer;
use crate::avatar::avatar;
use crate::composer::Composer;
use crate::format::{short_time, transcript_time};
use crate::chrome::surface;
use crate::icons::{Glyph, glyph};
use crate::state::{Change, Store, StoreEvent};
use crate::theme::Theme;

/// Messages within this window from the same sender share a bubble group.
const GROUP_GAP_SECS: i64 = 5 * 60;
/// A centered timestamp is shown when the conversation resumes after this gap.
const SEPARATOR_GAP_SECS: i64 = 60 * 60;
use crate::kinds::BUBBLE_RADIUS;
const HEADER_HEIGHT: f32 = 52.;
/// Space reserved under the transcript for the floating composer.
const COMPOSER_CLEARANCE: f32 = 78.;
const IMAGE_MAX_W: f32 = 260.;
const IMAGE_MAX_H: f32 = 340.;

#[derive(Clone)]
enum Row {
    Separator(i64),
    Message { ix: usize, first: bool, last: bool, show_name: bool, status: Option<(String, bool)> },
    Typing,
}

pub struct Conversation {
    store: Entity<Store>,
    composer: Entity<Composer>,
    list: ListState,
    rows: Vec<Row>,
    messages: Vec<ChatMessage>,
    /// Decoded inline thumbnails, keyed by message id.
    thumbs: HashMap<String, Arc<Image>>,
    chat_jid: Option<String>,
    is_group: bool,
    /// Message whose tapback tray is open.
    picker_for: Option<String>,
    _subs: Vec<Subscription>,
}

impl Conversation {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| Composer::new(store.clone(), window, cx));
        let subs = vec![
            cx.observe(&store, |this, _, cx| this.sync_header(cx)),
            cx.subscribe(&store, |this, _, ev: &StoreEvent, cx| match ev {
                StoreEvent::TranscriptChanged { chat_jid, change } => {
                    if this.store.read(cx).selected.as_deref() == Some(chat_jid.as_str()) {
                        this.rebuild(*change, cx);
                    }
                }
                StoreEvent::AccountReset => {
                    this.rows.clear();
                    this.messages.clear();
                    this.thumbs.clear();
                    this.chat_jid = None;
                    this.list.reset(0);
                    cx.notify();
                }
            }),
        ];
        let list = ListState::new(0, ListAlignment::Bottom, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        let mut this = Self {
            store,
            composer,
            list,
            rows: Vec::new(),
            messages: Vec::new(),
            thumbs: HashMap::new(),
            chat_jid: None,
            is_group: false,
            // Test hook: open the tapback tray for a message id.
            picker_for: std::env::var("WA_DEBUG_PICKER").ok(),
            _subs: subs,
        };
        // A reopened window starts on whatever chat the store already has selected.
        this.rebuild(Change::Appended, cx);
        this
    }

    fn sync_header(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        if selected != self.chat_jid {
            self.chat_jid = selected;
            self.rebuild(Change::Appended, cx);
        }
        cx.notify();
    }

    fn rebuild(&mut self, change: Change, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let switched = store.selected != self.chat_jid;
        self.chat_jid = store.selected.clone();
        self.is_group = store.selected_chat().is_some_and(|c| c.is_group);
        // Where the previously-first message sat, to splice older rows in above it.
        let old_first = self.messages.first().map(|m| m.id.clone());
        let old_first_row = self.rows.iter().position(|r| matches!(r, Row::Message { .. }));
        let old_keys = row_keys(&self.rows, &self.messages);
        let old_messages: HashMap<String, ChatMessage> =
            if switched { HashMap::new() } else { self.messages.iter().map(|m| (m.id.clone(), m.clone())).collect() };
        self.messages = self.chat_jid.as_ref().and_then(|j| store.messages.get(j)).cloned().unwrap_or_default();
        let typing = self.chat_jid.as_ref().is_some_and(|j| store.typing.contains(j));
        self.rows = build_rows(&self.messages, typing);
        if switched {
            self.thumbs.clear();
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
                        if let Row::Message { ix, .. } = row
                            && (row_ix < prefix || row_ix >= new - suffix)
                            && old_messages.get(&self.messages[*ix].id).is_some_and(|m| m != &self.messages[*ix])
                        {
                            self.list.remeasure_items(row_ix..row_ix + 1);
                        }
                    }
                }
                if change == Change::Appended && following {
                    self.list.scroll_to_end();
                }
            }
        }
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

    fn reveal_message(&mut self, id: &str) {
        if let Some(row) =
            self.rows.iter().position(|r| matches!(r, Row::Message { ix, .. } if self.messages[*ix].id == id))
        {
            self.list.scroll_to_reveal_item(row);
        }
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
            Some(Row::Message { ix, first, last, show_name, status }) => {
                self.render_message(&self.messages[*ix], *first, *last, *show_name, status.as_ref(), theme, window, cx)
            }
            Some(Row::Typing) => div()
                .pt(px(8.))
                .px(px(16.))
                .when(self.is_group, |d| d.pl(px(52.)))
                .child(typing_bubble(theme))
                .into_any_element(),
            None => div().into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_message(
        &self,
        m: &ChatMessage,
        first: bool,
        last: bool,
        show_name: bool,
        status: Option<&(String, bool)>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let from_me = m.from_me;
        let show_sender = self.is_group && !from_me && show_name;
        let gutter = self.is_group && !from_me;
        let sender = m.sender_name.clone().unwrap_or_else(|| m.sender_jid.split('@').next().unwrap_or("").to_string());
        let sender_photo = if gutter && last { self.photo_for(&m.sender_jid, cx) } else { None };

        if let MessageBody::System { text } = &m.body {
            return crate::kinds::system_line(text, theme).into_any_element();
        }
        let thumb = self.thumbs.get(&m.id).cloned();
        let content: AnyElement = match &m.body {
            MessageBody::Image(media) => self.image_content(m, media, from_me, last, theme, cx),
            MessageBody::Video(v) => crate::kinds::video(&self.store, m, v, thumb, theme, cx).into_any_element(),
            MessageBody::Document(d) => {
                bubble(div().child(crate::kinds::document(&self.store, m, d, from_me, theme)), from_me, last, theme)
                    .into_any_element()
            }
            MessageBody::Sticker(s) => crate::kinds::sticker(&self.store, m, s, theme, cx).into_any_element(),
            MessageBody::Location(l) => crate::kinds::location(m, l, thumb, theme, from_me).into_any_element(),
            MessageBody::Contacts(c) => bubble(crate::kinds::contacts(c, from_me, theme), from_me, last, theme).into_any_element(),
            MessageBody::Poll(p) => bubble(crate::kinds::poll(p, from_me, theme), from_me, last, theme).into_any_element(),
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
            MessageBody::Text(t) => bubble(text_content(t, false), from_me, last, theme).into_any_element(),
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
                this.store.update(cx, |s, cx| s.set_replying_to(Some(target), cx));
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
        let header = (show_sender || quoted_who.is_some()).then(|| {
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
        });

        let picker_open = self.picker_for.as_deref() == Some(m.id.as_str());
        let bubble_area = div()
            .relative()
            .when(!m.reactions.is_empty(), |d| d.pt(px(14.)))
            .child(content)
            .when(!m.reactions.is_empty(), |d| d.child(reaction_pill(&m.reactions, from_me, theme)))
            .when(picker_open, |d| d.child(self.reaction_picker(m, from_me, theme, cx)));

        let right_click_id = m.id.clone();
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
                    .child(div().pb(px(4.)).flex().gap(px(2.)).child(react_button).child(reply_button)),
            )
            .when(m.edited, |d| {
                d.child(
                    div()
                        .pt(px(2.))
                        .px(px(if gutter { 50. } else { 6. }))
                        .text_size(px(11.))
                        .text_color(theme.text_secondary)
                        .child("Edited"),
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
                        .child(label.clone())
                        .when(*failed, |d| {
                            d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                let target = retry_target.clone();
                                this.store.update(cx, |s, cx| s.retry_send(&target, cx));
                            }))
                        }),
                )
            })
            // Right-click opens the tapback tray (which also offers Reply), like Messages.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.picker_for = Some(right_click_id.clone());
                    cx.notify();
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
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.reveal_message(&id);
                        cx.notify();
                    })),
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
                        this.picker_for = None;
                        cx.notify();
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
                        this.store.update(cx, |s, cx| s.set_replying_to(Some(target), cx));
                        this.picker_for = None;
                        Composer::focus(&this.composer, window, cx);
                        cx.notify();
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
                            this.picker_for = None;
                            Composer::start_edit(&this.composer, target.clone(), window, cx);
                            cx.notify();
                        })),
                )
            })
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.picker_for = None;
                cx.notify();
            }));
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

    fn image_content(
        &self,
        m: &ChatMessage,
        media: &ImageMedia,
        from_me: bool,
        last: bool,
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
        let (path, store) = (media.path.clone(), self.store.clone());
        let overlay = match (media.path.is_some(), media.state) {
            (true, _) => None,
            (false, MediaState::Failed) => Some("Couldn't download"),
            (false, _) => Some("Downloading…"),
        };
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
            .when_some(overlay, |d, label| {
                d.child(
                    div().absolute().inset_0().flex().items_end().justify_center().pb(px(10.)).child(
                        div()
                            .px(px(8.))
                            .py(px(3.))
                            .rounded_full()
                            .bg(black().opacity(0.45))
                            .text_color(white())
                            .text_size(px(11.))
                            .child(label),
                    ),
                )
            })
            .on_click(move |_, _, cx| {
                if let Some(path) = path.clone() {
                    store.update(cx, |s, cx| s.open_lightbox(path, cx));
                }
            });
        match media.caption.as_deref().filter(|c| !c.trim().is_empty()) {
            None => frame.into_any_element(),
            Some(caption) => div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .when(from_me, |d| d.items_end())
                .child(frame)
                .child(bubble(text_content(caption, false), from_me, last, theme))
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

    let bars = waveform_bars(&media.waveform, 30);
    let ready = media.path.is_some();
    let (path, id, chat) = (media.path.clone(), m.id.clone(), m.chat_jid.clone());
    let button = div()
        .id(SharedString::from(format!("play-{}", m.id)))
        .flex_none()
        .size(px(28.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(if from_me { white().opacity(0.22) } else { theme.accent })
        .child(glyph(if playing { Glyph::Pause } else { Glyph::Play }, 13., white()))
        .on_click(cx.listener(move |this, _, _, cx| match &path {
            Some(path) => AudioPlayer::toggle(cx, &id, path),
            None => this.store.update(cx, |s, _| s.request_media(&chat, &id)),
        }));

    let content = div()
        .flex()
        .items_center()
        .gap(px(9.))
        .py(px(2.))
        .child(button)
        .child(
            div().flex().items_center().gap(px(2.)).h(px(26.)).children(bars.iter().enumerate().map(|(i, v)| {
                let done = (i as f32 + 0.5) / bars.len() as f32 <= progress;
                div()
                    .w(px(2.5))
                    .h(px(4. + 20. * v))
                    .rounded_full()
                    .bg(if done { played } else { unplayed })
            })),
        )
        .child(
            div()
                .min_w(px(30.))
                .text_size(px(12.))
                .text_color(fg.opacity(0.8))
                .child(match (ready, media.state) {
                    (false, MediaState::Downloading) => "…".to_string(),
                    (false, MediaState::Failed) => "!".to_string(),
                    _ => format!("{}:{:02}", shown_secs / 60, shown_secs % 60),
                }),
        );
    if !ready && media.state == MediaState::Idle {
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

/// Identity of each row for diffing: rows with equal keys are the same list item.
#[derive(PartialEq)]
enum RowKey {
    Separator(i64),
    Message { id: String, first: bool, last: bool, show_name: bool, status: bool },
    Typing,
}

fn row_keys(rows: &[Row], messages: &[ChatMessage]) -> Vec<RowKey> {
    rows.iter()
        .map(|r| match r {
            Row::Separator(ts) => RowKey::Separator(*ts),
            Row::Message { ix, first, last, show_name, status } => RowKey::Message {
                id: messages[*ix].id.clone(),
                first: *first,
                last: *last,
                show_name: *show_name,
                status: status.is_some(),
            },
            Row::Typing => RowKey::Typing,
        })
        .collect()
}

fn build_rows(messages: &[ChatMessage], typing: bool) -> Vec<Row> {
    let mut rows = Vec::with_capacity(messages.len() + 8);
    let last_mine = messages.iter().rposition(|m| m.from_me);
    for (ix, m) in messages.iter().enumerate() {
        let prev = ix.checked_sub(1).map(|p| &messages[p]);
        let next = messages.get(ix + 1);
        let separator = prev.is_none_or(|p| m.timestamp - p.timestamp > SEPARATOR_GAP_SECS);
        if separator {
            rows.push(Row::Separator(m.timestamp));
        }
        let same = |a: &ChatMessage, b: &ChatMessage| {
            a.from_me == b.from_me && a.sender_jid == b.sender_jid && (b.timestamp - a.timestamp).abs() <= GROUP_GAP_SECS
        };
        let new_sender = separator || prev.is_none_or(|p| !same(p, m));
        let first = new_sender || m.quoted.is_some();
        let next_separates = next.is_some_and(|n| n.timestamp - m.timestamp > SEPARATOR_GAP_SECS);
        let last = next.is_none_or(|n| next_separates || !same(m, n) || n.quoted.is_some());
        let status = (Some(ix) == last_mine || (m.from_me && m.status == DeliveryStatus::Failed)).then(|| status_label(m));
        rows.push(Row::Message { ix, first, last: last || status.is_some(), show_name: new_sender, status });
    }
    if typing {
        rows.push(Row::Typing);
    }
    rows
}

fn status_label(m: &ChatMessage) -> (String, bool) {
    match m.status {
        DeliveryStatus::Pending => ("Sending…".into(), false),
        DeliveryStatus::Sent => ("Sent".into(), false),
        DeliveryStatus::Delivered => ("Delivered".into(), false),
        DeliveryStatus::Read => (format!("Read {}", short_time(m.timestamp)), false),
        DeliveryStatus::Failed => ("Not Delivered · Try Again".into(), true),
    }
}

impl Render for Conversation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let store = self.store.read(cx);
        let Some(chat) = store.selected_chat().cloned() else {
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
        let profile_open = store.profile_open.clone();
        let history = store.selected_history();
        let loading_older = history.page_pending || history.fetching_remote;
        let replying = store.replying_to.is_some();
        let header_photo = store.avatar_cached(&chat.jid);
        if header_photo.is_none() {
            let (s, jid) = (self.store.clone(), chat.jid.clone());
            cx.defer(move |cx| s.update(cx, |s, _| s.request_avatar(&jid)));
        }
        let view = cx.entity();
        let store_handle = self.store.clone();

        let pane = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .bg(theme.window_bg)
            // Transcript fills the pane and scrolls under the floating header and composer.
            .child(
                list(self.list.clone(), move |ix, window, cx| {
                    // The first rows are laid out once they come within the list's
                    // overdraw of the viewport: prefetch the previous page.
                    if ix < 3 {
                        let store = store_handle.clone();
                        cx.defer(move |cx| store.update(cx, |s, cx| s.load_older(cx)));
                    }
                    let theme = Theme::for_window(window);
                    view.update(cx, |this, cx| this.render_row(ix, &theme, window, cx))
                })
                .size_full()
                .pt(px(HEADER_HEIGHT))
                .pb(px(COMPOSER_CLEARANCE + if replying { 44. } else { 0. })),
            )
            .when(loading_older, |d| {
                d.child(
                    div().absolute().top(px(HEADER_HEIGHT + 10.)).left_0().right_0().flex().justify_center().child(
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
            // Header: compact centered avatar + name, like Messages. Click for info.
            .child(
                div()
                    .id("chat-header")
                    .cursor_pointer()
                    .on_click({
                        let (store, jid) = (self.store.clone(), chat.jid.clone());
                        move |_, _, cx| store.update(cx, |s, cx| s.toggle_profile(jid.clone(), cx))
                    })
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(HEADER_HEIGHT))
                    .bg(theme.header_bg)
                    .border_b_1()
                    .border_color(theme.separator)
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.))
                    .child(avatar(&chat.name, header_photo, 28., &theme))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(chat.name.clone()),
                            )
                            .when(typing, |d| {
                                d.child(div().text_size(px(11.)).text_color(theme.text_secondary).child("typing…"))
                            }),
                    ),
            )
            .child(div().absolute().bottom_0().left_0().right_0().child(self.composer.clone()));
        div()
            .size_full()
            .flex()
            .child(pane)
            .when_some(profile_open, |d, jid| d.child(crate::profile::panel(&jid, &self.store, &theme, cx)))
            .into_any_element()
    }
}
