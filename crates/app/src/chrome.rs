//! Surfaces for the header, composer and status pills, like Messages.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::Theme;

/// A solid surface with a hairline border.
pub fn surface<E: Styled>(el: E, theme: &Theme) -> E {
    el.bg(theme.surface_bg).border_1().border_color(theme.surface_border)
}

/// Round button with a hairline border (the "+" next to the composer).
pub fn round_button(id: &'static str, size: f32, theme: &Theme) -> Stateful<Div> {
    let hover = theme.search_bg;
    surface(div().id(id), theme)
        .flex_none()
        .size(px(size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .hover(move |s| s.bg(hover))
        .map(|d| d.cursor_pointer())
}

/// Blur radius of the frosted bars, like Messages' header and composer.
const BAR_BLUR: f32 = 20.;

/// Frosted glass filling its parent: blurs what's drawn under it, tinted. Make it the
/// parent's first child so the parent's own content draws on top.
pub fn frosted(tint: Hsla) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds, _, window, _| {
        window.paint_backdrop_blur(bounds, Corners::default(), px(BAR_BLUR));
        window.paint_quad(fill(bounds, tint));
    })
    .absolute()
    .inset_0()
}
