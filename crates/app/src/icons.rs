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

const COMPOSE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M11.5 4.5 H7 A3 3 0 0 0 4 7.5 V17 A3 3 0 0 0 7 20 H16.5 A3 3 0 0 0 19.5 17 V12.5" fill="none" stroke="black" stroke-width="1.8" stroke-linecap="round"/><path d="M18.2 3.6 L20.4 5.8 L12.6 13.6 L9.6 14.4 L10.4 11.4 Z" fill="none" stroke="black" stroke-width="1.8" stroke-linejoin="round"/></svg>"#;
const BELL_SLASH: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M7 16.5 V11 A5 5 0 0 1 14.5 6.6 M17 10.5 V16.5 L18.8 18 H6.5 M10 20.2 A2.2 2.2 0 0 0 14 20.2 M12 4.2 V3.4" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.5 4.5 L19.5 19.5" fill="none" stroke="black" stroke-width="2" stroke-linecap="round"/></svg>"#;
const ARCHIVE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="3.5" y="4.5" width="17" height="4.6" rx="1.3" fill="none" stroke="black" stroke-width="1.8"/><path d="M5.2 9.1 V17.5 A2 2 0 0 0 7.2 19.5 H16.8 A2 2 0 0 0 18.8 17.5 V9.1 M10 13 H14" fill="none" stroke="black" stroke-width="1.8" stroke-linecap="round"/></svg>"#;
const CHEVRON_LEFT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M15 4.5 L7.5 12 L15 19.5" fill="none" stroke="black" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

const FORWARD: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M14 5 L20 11 L14 17 M19.5 11 H10.5 C6.4 11 4 13.4 4 17.5 V19" fill="none" stroke="black" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;
const CHEVRON_UP: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4.5 15 L12 7.5 L19.5 15" fill="none" stroke="black" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;
const CHEVRON_DOWN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4.5 9 L12 16.5 L19.5 9" fill="none" stroke="black" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;
const ARROW_DOWN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 19.5 L5.5 13 M12 19.5 L18.5 13 M12 19.2 V4.5" fill="none" stroke="black" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;
const MAGNIFIER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="10.5" cy="10.5" r="6" fill="none" stroke="black" stroke-width="2.2"/><path d="M15 15 L20 20" fill="none" stroke="black" stroke-width="2.4" stroke-linecap="round"/></svg>"#;

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
    Compose,
    BellSlash,
    Archive,
    ChevronLeft,
    Forward,
    ChevronUp,
    ChevronDown,
    ArrowDown,
    Magnifier,
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
        Glyph::Compose => COMPOSE,
        Glyph::BellSlash => BELL_SLASH,
        Glyph::Archive => ARCHIVE,
        Glyph::ChevronLeft => CHEVRON_LEFT,
        Glyph::Forward => FORWARD,
        Glyph::ChevronUp => CHEVRON_UP,
        Glyph::ChevronDown => CHEVRON_DOWN,
        Glyph::ArrowDown => ARROW_DOWN,
        Glyph::Magnifier => MAGNIFIER,
    };
    Icon::default().data(svg.as_bytes()).size(px(size)).text_color(color)
}
