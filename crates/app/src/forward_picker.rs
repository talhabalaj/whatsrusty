//! "Forward to…": a searchable chat picker floating over the conversation.

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::ChatMessage;

use crate::avatar::avatar;
use crate::chrome::surface;
use crate::state::Store;
use crate::theme::Theme;

pub enum ForwardEvent {
    /// Forward to this chat (JID, name).
    Picked(String, String),
    Dismissed,
}

pub struct ForwardPicker {
    store: Entity<Store>,
    pub message: ChatMessage,
    input: Entity<InputState>,
    query: String,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ForwardEvent> for ForwardPicker {}

impl ForwardPicker {
    pub fn new(store: Entity<Store>, message: ChatMessage, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        input.update(cx, |s, cx| s.focus(window, cx));
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&input, window, |this, state, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    this.query = state.read(cx).value().trim().to_lowercase();
                    cx.notify();
                }
                // Enter picks the first match.
                InputEvent::PressEnter { .. } => {
                    if let Some((jid, name)) = this.matches(cx).into_iter().next() {
                        cx.emit(ForwardEvent::Picked(jid, name));
                    }
                }
                _ => {}
            }),
        ];
        Self { store, message, input, query: String::new(), _subs: subs }
    }

    fn matches(&self, cx: &App) -> Vec<(String, String)> {
        self.store
            .read(cx)
            .chats
            .iter()
            .filter(|c| self.query.is_empty() || c.name.to_lowercase().contains(&self.query))
            .map(|c| (c.jid.clone(), c.name.clone()))
            .collect()
    }
}

impl Render for ForwardPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let matches = self.matches(cx);
        let photos: Vec<_> = matches.iter().map(|(jid, _)| self.store.read(cx).avatar_cached(jid)).collect();
        let missing: Vec<String> =
            matches.iter().zip(&photos).filter(|(_, p)| p.is_none()).map(|((jid, _), _)| jid.clone()).collect();
        if !missing.is_empty() {
            let store = self.store.clone();
            cx.defer(move |cx| store.update(cx, |s, _| missing.iter().for_each(|j| s.request_avatar(j))));
        }
        let preview = crate::state::body_text(&self.message.body).replace('\n', " ");
        let rows: Vec<AnyElement> = matches
            .into_iter()
            .zip(photos)
            .map(|((jid, name), photo)| {
                div()
                    .id(SharedString::from(format!("fwd-{jid}")))
                    .h(px(44.))
                    .px(px(10.))
                    .rounded(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.search_bg))
                    .child(avatar(&name, photo, 30., &theme))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).text_color(theme.text).child(name.clone()))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ForwardEvent::Picked(jid.clone(), name.clone()))))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        surface(div(), &theme)
            .w(px(340.))
            .max_h(px(460.))
            .rounded(px(12.))
            .shadow_lg()
            .flex()
            .flex_col()
            .overflow_hidden()
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.emit(ForwardEvent::Dismissed);
                cx.stop_propagation();
            }))
            // Clicks inside don't reach the backdrop (which dismisses).
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(14.))
                    .pt(px(12.))
                    .pb(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Forward to…"))
                    .child(div().text_size(px(11.5)).text_color(theme.text_secondary).truncate().child(preview)),
            )
            .child(
                div().px(px(10.)).pb(px(8.)).child(
                    div()
                        .rounded(px(8.))
                        .bg(theme.search_bg)
                        .text_size(px(13.))
                        .child(Input::new(&self.input).appearance(false).cleanable(true)),
                ),
            )
            .child(div().h(px(1.)).bg(theme.separator))
            .child(
                div()
                    .id("forward-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(6.))
                    .children(rows)
                    .when(empty, |d| {
                        d.child(
                            div()
                                .py(px(18.))
                                .flex()
                                .justify_center()
                                .text_size(px(12.))
                                .text_color(theme.text_secondary)
                                .child("No chats"),
                        )
                    }),
            )
    }
}
