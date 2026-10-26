//! Conversation list: search field + Messages-style rows.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;

use crate::avatar::avatar;
use crate::format::sidebar_time;
use crate::state::Store;
use crate::theme::Theme;

const ROW_HEIGHT: f32 = 68.;

pub struct Sidebar {
    store: Entity<Store>,
    search: Entity<InputState>,
    query: String,
    scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl Sidebar {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&search, window, |this, state, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.query = state.read(cx).value().to_lowercase();
                    cx.notify();
                }
            }),
        ];
        Self { store, search, query: String::new(), scroll: UniformListScrollHandle::new(), _subs: subs }
    }

    fn visible_jids(&self, cx: &App) -> Vec<String> {
        self.store
            .read(cx)
            .chats
            .iter()
            .filter(|c| !c.archived)
            .filter(|c| self.query.is_empty() || c.name.to_lowercase().contains(&self.query))
            .map(|c| c.jid.clone())
            .collect()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let jids = self.visible_jids(cx);
        let store = self.store.clone();
        let count = jids.len();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.sidebar_border)
            // Room for the traffic lights, which sit in our transparent titlebar.
            .child(div().h(px(52.)).flex_none())
            .child(
                div().px(px(10.)).pb(px(8.)).child(
                    div()
                        .rounded(px(8.))
                        .bg(theme.search_bg)
                        .text_size(px(13.))
                        .child(Input::new(&self.search).appearance(false).cleanable(true)),
                ),
            )
            .child(
                uniform_list("chat-list", count, move |range, window, cx| {
                    let theme = Theme::for_window(window);
                    // Ask for photos of rows as they scroll into view.
                    let missing: Vec<String> = range
                        .clone()
                        .filter_map(|ix| jids.get(ix))
                        .filter(|j| store.read(cx).avatar_cached(j).is_none())
                        .cloned()
                        .collect();
                    if !missing.is_empty() {
                        let store = store.clone();
                        cx.defer(move |cx| store.update(cx, |s, _| missing.iter().for_each(|j| s.request_avatar(j))));
                    }
                    let state = store.read(cx);
                    range
                        .filter_map(|ix| {
                            let jid = jids.get(ix)?;
                            let chat = state.chats.iter().find(|c| &c.jid == jid)?;
                            let selected = state.selected.as_deref() == Some(jid.as_str());
                            let typing = state.typing.contains(jid);
                            let photo = state.avatar_cached(jid);
                            Some(chat_row(chat, photo, selected, typing, &theme, store.clone()))
                        })
                        .collect()
                })
                .track_scroll(&self.scroll)
                .flex_1()
                .px(px(10.)),
            )
    }
}

fn chat_row(
    chat: &wa_backend::ChatSummary,
    photo: Option<std::sync::Arc<Image>>,
    selected: bool,
    typing: bool,
    theme: &Theme,
    store: Entity<Store>,
) -> Stateful<Div> {
    let (title, secondary) = if selected {
        (theme.row_selected_text, theme.row_selected_text.opacity(0.8))
    } else {
        (theme.text, theme.text_secondary)
    };
    let preview = if typing {
        "typing…".to_string()
    } else {
        chat.last_message
            .as_ref()
            .map(|m| match (&m.sender_name, chat.is_group, m.from_me) {
                (_, true, true) => format!("You: {}", m.text),
                (Some(s), true, false) => format!("{s}: {}", m.text),
                _ => m.text.clone(),
            })
            .unwrap_or_default()
    };
    let time = chat.last_message.as_ref().map(|m| sidebar_time(m.timestamp)).unwrap_or_default();
    let unread = chat.unread_count > 0 && !selected;
    let jid = chat.jid.clone();

    div()
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
            store.update(cx, |s, cx| s.select(jid, cx));
        })
        // Unread indicator lives in the left gutter, like Messages.
        .child(
            div()
                .w(px(14.))
                .flex_none()
                .flex()
                .justify_center()
                .when(unread, |d| d.child(div().size(px(9.)).rounded_full().bg(theme.unread_dot))),
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
                        .items_baseline()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(title)
                                .child(chat.name.clone()),
                        )
                        .child(div().flex_none().text_size(px(12.)).text_color(secondary).child(time)),
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
        )
}
