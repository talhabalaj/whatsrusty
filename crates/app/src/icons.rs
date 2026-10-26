//! Small SF-Symbols-like glyphs as inline SVG (rendered as masks tinted by text color).

use gpui_kit::component::Icon;
use gpui_kit::{Hsla, Styled as _, px};

const ARROW_UP: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 4.5 L5.5 11 M12 4.5 L18.5 11 M12 4.8 V19.5" fill="none" stroke="black" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;
const PLUS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 5 V19 M5 12 H19" fill="none" stroke="black" stroke-width="2.4" stroke-linecap="round"/></svg>"#;
const PLAY: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M7 4.6 C7 3.8 7.9 3.3 8.6 3.7 L19.6 10.6 C20.3 11 20.3 12 19.6 12.4 L8.6 19.3 C7.9 19.7 7 19.2 7 18.4 Z" fill="black"/></svg>"#;
const PAUSE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="6" y="4" width="4.2" height="16" rx="1.4" fill="black"/><rect x="13.8" y="4" width="4.2" height="16" rx="1.4" fill="black"/></svg>"#;
const REPLY: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M10 5 L4 11 L10 17 M4.5 11 H13.5 C17.6 11 20 13.4 20 17.5 V19" fill="none" stroke="black" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;
const SMILE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="8.6" fill="none" stroke="black" stroke-width="2"/><circle cx="9.2" cy="10" r="1.3" fill="black"/><circle cx="14.8" cy="10" r="1.3" fill="black"/><path d="M8.3 14 C9.4 16 14.6 16 15.7 14" fill="none" stroke="black" stroke-width="2" stroke-linecap="round"/></svg>"#;
const STICKER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4.5 8.5 A4 4 0 0 1 8.5 4.5 H15.5 A4 4 0 0 1 19.5 8.5 V13 L13 19.5 H8.5 A4 4 0 0 1 4.5 15.5 Z" fill="none" stroke="black" stroke-width="2" stroke-linejoin="round"/><path d="M19.3 13.2 H15.5 A2.4 2.4 0 0 0 13.1 15.6 V19.3" fill="none" stroke="black" stroke-width="2" stroke-linejoin="round"/><circle cx="9.6" cy="10.3" r="1.1" fill="black"/><circle cx="14.4" cy="10.3" r="1.1" fill="black"/></svg>"#;
const PENCIL: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M15.5 4.8 L19.2 8.5 L8.6 19.1 L4.4 19.6 L4.9 15.4 Z M13.6 6.7 L17.3 10.4" fill="none" stroke="black" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/></svg>"#;
const CLOSE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M6.5 6.5 L17.5 17.5 M17.5 6.5 L6.5 17.5" fill="none" stroke="black" stroke-width="2.4" stroke-linecap="round"/></svg>"#;

#[derive(Clone, Copy)]
pub enum Glyph {
    ArrowUp,
    Plus,
    Play,
    Pause,
    Reply,
    Close,
    Smile,
    Sticker,
    Pencil,
}

pub fn glyph(g: Glyph, size: f32, color: Hsla) -> Icon {
    let svg = match g {
        Glyph::ArrowUp => ARROW_UP,
        Glyph::Plus => PLUS,
        Glyph::Play => PLAY,
        Glyph::Pause => PAUSE,
        Glyph::Reply => REPLY,
        Glyph::Close => CLOSE,
        Glyph::Smile => SMILE,
        Glyph::Sticker => STICKER,
        Glyph::Pencil => PENCIL,
    };
    Icon::default().data(svg.as_bytes()).size(px(size)).text_color(color)
}
