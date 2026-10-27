//! iMessage-inspired palette. Values follow macOS Messages (system blue bubbles,
//! neutral gray incoming bubbles, translucent sidebar).

use gpui_kit::{Hsla, Rgba, Window, WindowAppearance, rgb, rgba};

#[derive(Clone, Copy)]
pub struct Theme {
    pub surface_bg: Hsla,
    pub surface_border: Hsla,
    pub waveform_in: Hsla,
    pub window_bg: Hsla,
    pub sidebar_bg: Hsla,
    pub sidebar_border: Hsla,
    pub separator: Hsla,
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub accent: Hsla,
    pub row_selected: Hsla,
    pub row_selected_text: Hsla,
    pub bubble_out: Hsla,
    pub bubble_out_text: Hsla,
    pub bubble_in: Hsla,
    pub bubble_in_text: Hsla,
    pub search_bg: Hsla,
    pub header_bg: Hsla,
    pub avatar_top: Hsla,
    pub avatar_bottom: Hsla,
    pub unread_dot: Hsla,
    pub failed: Hsla,
    /// Over the backdrop blur of the header and composer bars.
    pub bar_tint: Hsla,
}

fn c(v: Rgba) -> Hsla {
    v.into()
}

impl Theme {
    pub fn for_window(window: &Window) -> Self {
        match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            _ => Self::light(),
        }
    }

    pub fn light() -> Self {
        Self {
            surface_bg: c(rgb(0xffffff)),
            surface_border: c(rgba(0x00000026)),
            waveform_in: c(rgba(0x0000004d)),
            window_bg: c(rgb(0xffffff)),
            sidebar_bg: c(rgba(0xeeeef0e6)),
            sidebar_border: c(rgba(0x0000001a)),
            separator: c(rgba(0x00000014)),
            text: c(rgb(0x1d1d1f)),
            text_secondary: c(rgb(0x86868b)),
            accent: c(rgb(0x0a7cff)),
            row_selected: c(rgb(0x0a7cff)),
            row_selected_text: c(rgb(0xffffff)),
            bubble_out: c(rgb(0x0a7cff)),
            bubble_out_text: c(rgb(0xffffff)),
            bubble_in: c(rgb(0xe9e9eb)),
            bubble_in_text: c(rgb(0x000000)),
            search_bg: c(rgba(0x0000000d)),
            header_bg: c(rgb(0xf6f6f6)),
            avatar_top: c(rgb(0xa5abb8)),
            avatar_bottom: c(rgb(0x858994)),
            unread_dot: c(rgb(0x0a7cff)),
            failed: c(rgb(0xff3b30)),
            bar_tint: c(rgba(0xf6f6f6b8)),
        }
    }

    pub fn dark() -> Self {
        Self {
            surface_bg: c(rgb(0x1e1e1e)),
            surface_border: c(rgba(0xffffff26)),
            waveform_in: c(rgba(0xffffff59)),
            window_bg: c(rgb(0x1e1e1e)),
            sidebar_bg: c(rgba(0x2a2a2ce6)),
            sidebar_border: c(rgba(0x00000080)),
            separator: c(rgba(0xffffff14)),
            text: c(rgb(0xf5f5f7)),
            text_secondary: c(rgb(0x98989d)),
            accent: c(rgb(0x0a84ff)),
            row_selected: c(rgb(0x0a84ff)),
            row_selected_text: c(rgb(0xffffff)),
            bubble_out: c(rgb(0x0a84ff)),
            bubble_out_text: c(rgb(0xffffff)),
            bubble_in: c(rgb(0x3a3a3c)),
            bubble_in_text: c(rgb(0xffffff)),
            search_bg: c(rgba(0xffffff14)),
            header_bg: c(rgb(0x262628)),
            avatar_top: c(rgb(0x8e929c)),
            avatar_bottom: c(rgb(0x6c707a)),
            unread_dot: c(rgb(0x0a84ff)),
            failed: c(rgb(0xff453a)),
            bar_tint: c(rgba(0x262628b3)),
        }
    }
}
