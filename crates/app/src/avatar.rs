use std::sync::Arc;

use gpui_kit::{
    AnyElement, FontWeight, Image, IntoElement as _, ObjectFit, ParentElement as _, Styled as _, StyledImage as _, div,
    img, linear_color_stop, linear_gradient, px, white,
};

use crate::format::initials;
use crate::theme::Theme;

/// Messages-style avatar: the profile photo when we have one, otherwise a soft gray
/// gradient monogram with white initials.
pub fn avatar(name: &str, photo: Option<Arc<Image>>, size: f32, theme: &Theme) -> AnyElement {
    let circle = div().flex_none().size(px(size)).rounded_full().overflow_hidden();
    if let Some(image) = photo {
        return circle
            .bg(theme.avatar_bottom)
            .child(img(image).size_full().rounded_full().object_fit(ObjectFit::Cover))
            .into_any_element();
    }
    circle
        .flex()
        .items_center()
        .justify_center()
        .bg(linear_gradient(
            180.,
            linear_color_stop(theme.avatar_top, 0.),
            linear_color_stop(theme.avatar_bottom, 1.),
        ))
        .text_color(white())
        .text_size(px(size * 0.4))
        .font_weight(FontWeight::MEDIUM)
        .child(initials(name))
        .into_any_element()
}
