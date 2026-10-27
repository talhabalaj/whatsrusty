//! Find in conversation (⌘F): a slim bar under the header with "n of m" and chevrons.

use std::ops::Range;

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::icons::{Glyph, glyph};
use crate::theme::Theme;

pub const FIND_BAR_HEIGHT: f32 = 40.;

pub enum FindEvent {
    Query(String),
    /// Enter / ⌃: the next older match.
    Older,
    /// Shift-Enter / ⌄: the next newer match.
    Newer,
    Close,
}

pub struct FindBar {
    input: Entity<InputState>,
    /// (current, total), counted from the newest match; `None` while searching.
    status: Option<(Option<usize>, usize)>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FindEvent> for FindBar {}

impl FindBar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in Conversation"));
        let subs = vec![cx.subscribe_in(&input, window, |_, state, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => cx.emit(FindEvent::Query(state.read(cx).value().to_string())),
            InputEvent::PressEnter { shift: false, .. } => cx.emit(FindEvent::Older),
            InputEvent::PressEnter { shift: true, .. } => cx.emit(FindEvent::Newer),
            _ => {}
        })];
        Self { input, status: None, _subs: subs }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.focus(window, cx));
    }

    pub fn set_query(&self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.set_value(query.to_string(), window, cx));
        cx.emit(FindEvent::Query(query.to_string()));
    }

    pub fn set_status(&mut self, status: Option<(Option<usize>, usize)>, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }
}

impl Render for FindBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let empty = self.input.read(cx).value().trim().is_empty();
        let label = match self.status {
            _ if empty => String::new(),
            None => "Searching…".to_string(),
            Some((_, 0)) => "No results".to_string(),
            Some((Some(n), total)) => format!("{} of {total}", n + 1),
            Some((None, total)) => format!("{total} found"),
        };
        let can_step = matches!(self.status, Some((_, n)) if n > 0);
        let chevron = |id: &'static str, icon: Glyph, ev: fn() -> FindEvent| {
            div()
                .id(id)
                .size(px(24.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .justify_center()
                .when(can_step, |d| d.cursor_pointer().hover(|s| s.bg(theme.search_bg)))
                .child(glyph(icon, 12., if can_step { theme.text } else { theme.text_secondary.opacity(0.5) }))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ev())))
        };
        div()
            .h(px(FIND_BAR_HEIGHT))
            .w_full()
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .relative()
            .child(crate::chrome::frosted(theme.bar_tint))
            .border_b_1()
            .border_color(theme.separator)
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.emit(FindEvent::Close);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .pl(px(8.))
                    .rounded(px(7.))
                    .bg(theme.search_bg)
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(13.))
                    .child(glyph(Glyph::Magnifier, 12., theme.text_secondary))
                    .child(div().flex_1().min_w_0().child(Input::new(&self.input).appearance(false).cleanable(true))),
            )
            .child(
                div()
                    .min_w(px(64.))
                    .flex()
                    .justify_end()
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .child(label),
            )
            .child(chevron("find-older", Glyph::ChevronUp, || FindEvent::Older))
            .child(chevron("find-newer", Glyph::ChevronDown, || FindEvent::Newer))
            .child(
                div()
                    .id("find-done")
                    .px(px(6.))
                    .h(px(24.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(theme.accent)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.search_bg))
                    .child("Done")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(FindEvent::Close))),
            )
    }
}

/// Byte ranges of `text` matching `needle` case-insensitively. Matching walks `text`'s
/// own chars (lowercasing each), so ranges always fall on its char boundaries.
pub fn match_ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = needle.trim().chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    // Index of the char just past a match starting at `start`.
    let match_at = |start: usize| {
        let mut k = 0;
        for (j, &(_, c)) in chars.iter().enumerate().skip(start) {
            for lc in c.to_lowercase() {
                if needle.get(k) != Some(&lc) {
                    return None;
                }
                k += 1;
            }
            if k == needle.len() {
                return Some(j + 1);
            }
        }
        None
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match match_at(i) {
            Some(end) => {
                let end_byte = chars.get(end).map_or(text.len(), |(b, _)| *b);
                out.push(chars[i].0..end_byte);
                i = end;
            }
            None => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::match_ranges;

    #[test]
    fn ranges_are_case_insensitive_and_on_char_boundaries() {
        assert_eq!(match_ranges("Dinner at 8, dinner!", "DINNER"), [0..6, 13..19]);
        let text = "İstanbul and ümit";
        for r in match_ranges(text, "ümit").into_iter().chain(match_ranges(text, "i̇stan")) {
            assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        }
        assert_eq!(&text[match_ranges(text, "ÜMIT")[0].clone()], "ümit");
        assert!(match_ranges("abc", "  ").is_empty());
        assert_eq!(match_ranges("aaa", "aa"), vec![0..2]);
    }
}
