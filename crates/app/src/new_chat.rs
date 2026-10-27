//! "New Message" picker: find a contact or group by name or number, or type a phone
//! number to check it's on WhatsApp; picking opens the chat with the composer focused.

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::avatar::avatar;
use crate::state::Store;
use crate::theme::Theme;

/// Rows shown at most (the rest is one more keystroke away).
const MAX_RESULTS: usize = 60;
const ROW_HEIGHT: f32 = 44.;

#[derive(Clone, Debug, PartialEq)]
enum Candidate {
    Chat { jid: String, name: String, detail: Option<String> },
    /// A typed number not among my contacts: check it with WhatsApp.
    Phone { digits: String },
}

pub struct NewChat {
    store: Entity<Store>,
    input: Entity<InputState>,
    query: String,
    highlighted: usize,
    /// The number being checked (`phone_lookups` key).
    checking: Option<String>,
    /// Why the last checked number can't be messaged.
    note: Option<String>,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for NewChat {}

impl NewChat {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Name or phone number"));
        let subs = vec![
            cx.observe(&store, |this, _, cx| this.on_store_changed(cx)),
            cx.subscribe_in(&input, window, |this, state, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    this.query = state.read(cx).value().trim().to_string();
                    this.highlighted = 0;
                    this.note = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.pick(this.highlighted, cx),
                _ => {}
            }),
        ];
        store.read(cx).request_contacts();
        Self {
            store,
            input,
            query: String::new(),
            highlighted: 0,
            checking: None,
            note: None,
            scroll: ScrollHandle::new(),
            _subs: subs,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |s, cx| s.focus(window, cx));
    }

    pub fn set_query(&mut self, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.set_value(q.to_string(), window, cx));
        self.query = q.trim().to_string();
        cx.notify();
    }

    fn candidates(&self, cx: &App) -> Vec<Candidate> {
        let store = self.store.read(cx);
        let q = self.query.to_lowercase();
        let digits: String = self.query.chars().filter(char::is_ascii_digit).collect();
        let phone_like = !digits.is_empty() && self.query.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c));
        let digits_of = |p: &str| p.chars().filter(char::is_ascii_digit).collect::<String>();
        let matches = |name: &str, phone: Option<&str>| {
            let name = name.to_lowercase();
            // From the start of a word, like Messages ("sa" finds Sana, not Message).
            q.is_empty()
                || name.starts_with(&q)
                || name.match_indices(&q).any(|(i, _)| !name[..i].ends_with(char::is_alphanumeric))
                || (digits.len() >= 3 && phone.is_some_and(|p| digits_of(p).contains(&digits)))
        };
        // Someone known only by their number (no saved or WhatsApp name) shows up only once
        // a typed number matches it, not among the names.
        let number_only = |name: &str| !name.chars().any(char::is_alphabetic);
        let mut out: Vec<Candidate> = store
            .contacts
            .iter()
            .filter(|c| !number_only(&c.name) || (digits.len() >= 3 && phone_like))
            .filter(|c| matches(&c.name, c.phone.as_deref()))
            .map(|c| Candidate::Chat {
                jid: c.jid.clone(),
                name: c.name.clone(),
                detail: c.phone.clone().filter(|p| *p != c.name),
            })
            .collect();
        // Groups I'm in, after people.
        out.extend(store.chats.iter().filter(|c| c.is_group && matches(&c.name, None)).map(|c| Candidate::Chat {
            jid: c.jid.clone(),
            name: c.name.clone(),
            detail: Some("Group".into()),
        }));
        let known = store.contacts.iter().any(|c| c.phone.as_deref().is_some_and(|p| digits_of(p) == digits));
        if phone_like && digits.len() >= 8 && !known {
            out.insert(0, Candidate::Phone { digits });
        }
        out.truncate(MAX_RESULTS);
        out
    }

    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(candidate) = self.candidates(cx).into_iter().nth(ix) else { return };
        match candidate {
            Candidate::Chat { jid, .. } => self.open(jid, cx),
            Candidate::Phone { .. } => {
                let phone = self.query.clone();
                self.checking = Some(phone.clone());
                self.note = None;
                self.store.update(cx, |s, cx| s.lookup_phone(&phone, cx));
                cx.notify();
            }
        }
    }

    fn open(&mut self, jid: String, cx: &mut Context<Self>) {
        // Close first, so focus moves on to the composer, not back here.
        cx.emit(DismissEvent);
        let store = self.store.clone();
        cx.defer(move |cx| store.update(cx, |s, cx| s.open_chat(jid, cx)));
    }

    fn on_store_changed(&mut self, cx: &mut Context<Self>) {
        let Some(phone) = self.checking.clone() else { return cx.notify() };
        let Some(answer) = self.store.read(cx).phone_lookups.get(&phone).cloned() else { return };
        self.checking = None;
        match answer {
            Ok(Some(jid)) => self.open(jid, cx),
            Ok(None) => {
                let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
                self.note = Some(format!("+{digits} isn't on WhatsApp."));
            }
            Err(e) => self.note = Some(e),
        }
        cx.notify();
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.candidates(cx).len();
        if n == 0 {
            return;
        }
        self.highlighted = (self.highlighted as isize + delta).rem_euclid(n as isize) as usize;
        self.scroll.scroll_to_item(self.highlighted);
        cx.notify();
    }
}

impl Render for NewChat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let candidates = self.candidates(cx);
        let highlighted = self.highlighted.min(candidates.len().saturating_sub(1));
        // Photos of the people listed, as they're shown.
        let missing: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                Candidate::Chat { jid, .. } if self.store.read(cx).avatar_cached(jid).is_none() => Some(jid.clone()),
                _ => None,
            })
            .take(12)
            .collect();
        if !missing.is_empty() {
            let store = self.store.clone();
            cx.defer(move |cx| store.update(cx, |s, _| missing.iter().for_each(|j| s.request_avatar(j))));
        }
        let status = match (&self.checking, &self.note) {
            (Some(_), _) => Some(("Checking…".to_string(), theme.text_secondary)),
            (None, Some(note)) => Some((note.clone(), theme.failed)),
            _ => None,
        };
        let empty = candidates.is_empty() && status.is_none();

        let rows: Vec<_> = candidates
            .into_iter()
            .enumerate()
            .map(|(ix, c)| {
                let active = ix == highlighted;
                let (fg, fg2) = match active {
                    true => (theme.row_selected_text, theme.row_selected_text.opacity(0.8)),
                    false => (theme.text, theme.text_secondary),
                };
                let (leading, title, detail) = match &c {
                    Candidate::Chat { jid, name, detail } => {
                        (avatar(name, self.store.read(cx).avatar_cached(jid), 30., &theme), name.clone(), detail.clone())
                    }
                    Candidate::Phone { digits } => {
                        (avatar("#", None, 30., &theme), format!("+{digits}"), Some("Message this number".to_string()))
                    }
                };
                div()
                    .id(("new-chat-row", ix))
                    .h(px(ROW_HEIGHT))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .rounded(px(7.))
                    .when(active, |d| d.bg(theme.row_selected))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.highlighted != ix {
                            this.highlighted = ix;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(ix, cx)))
                    .child(leading)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_size(px(13.)).text_color(fg).child(title))
                            .when_some(detail, |d, t| d.child(div().truncate().text_size(px(11.5)).text_color(fg2).child(t))),
                    )
            })
            .collect();

        div()
            .id("new-chat")
            .occlude()
            .flex()
            .flex_col()
            .rounded(px(12.))
            .bg(theme.window_bg)
            .border_1()
            .border_color(theme.surface_border)
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_highlight(-1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_highlight(1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.emit(DismissEvent);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .px(px(12.))
                    .pt(px(10.))
                    .pb(px(6.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("New Message"),
            )
            .child(
                div()
                    .mx(px(10.))
                    .mb(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .bg(theme.search_bg)
                    .text_size(px(13.))
                    .child(div().text_color(theme.text_secondary).child("To:"))
                    .child(div().flex_1().child(Input::new(&self.input).appearance(false).cleanable(true))),
            )
            .when_some(status, |d, (text, color)| {
                d.child(div().px(px(14.)).pb(px(6.)).text_size(px(12.)).text_color(color).child(text))
            })
            .child(
                div()
                    .id("new-chat-results")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(6.))
                    .pb(px(6.))
                    .children(rows)
                    .when(empty, |d| {
                        let text = match self.query.is_empty() {
                            true => "No contacts yet.",
                            false => "No matches. Type a number with country code.",
                        };
                        d.child(div().px(px(8.)).py(px(10.)).text_size(px(12.)).text_color(theme.text_secondary).child(text))
                    }),
            )
    }
}
