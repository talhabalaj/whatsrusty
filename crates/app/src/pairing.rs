//! Linking screen: QR code + instructions, shown until the phone scans it.

use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use qrcode::{Color, QrCode};

use crate::theme::Theme;

pub fn qr_element(code: &str, side: f32) -> impl IntoElement {
    let modules: Option<(usize, Vec<bool>)> = QrCode::new(code.as_bytes()).ok().map(|qr| {
        let width = qr.width();
        (width, qr.to_colors().into_iter().map(|c| c == Color::Dark).collect())
    });
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let Some((width, dark)) = &modules else { return };
            // Quiet zone of 2 modules on each side.
            let cells = *width as f32 + 4.;
            let cell = bounds.size.width.as_f32().min(bounds.size.height.as_f32()) / cells;
            window.paint_quad(fill(bounds, white()));
            for (i, is_dark) in dark.iter().enumerate() {
                if !*is_dark {
                    continue;
                }
                let (x, y) = ((i % width) as f32 + 2., (i / width) as f32 + 2.);
                let origin = point(bounds.origin.x + px(x * cell), bounds.origin.y + px(y * cell));
                // Slight overlap avoids hairline gaps between adjacent modules.
                window.paint_quad(fill(Bounds::new(origin, size(px(cell + 0.3), px(cell + 0.3))), black()));
            }
        },
    )
    .size(px(side))
}

pub fn pairing_screen(code: Option<&str>, status: &str, theme: &Theme) -> Div {
    let steps = [
        "Open WhatsApp on your phone",
        "Tap Settings → Linked Devices",
        "Tap Link a Device",
        "Point your phone at this screen",
    ];
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.window_bg)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(48.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .max_w(px(320.))
                        .child(
                            div()
                                .text_size(px(26.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.text)
                                .child("Link with your phone"),
                        )
                        .children(steps.iter().enumerate().map(|(i, s)| {
                            div()
                                .flex()
                                .gap(px(10.))
                                .text_size(px(14.))
                                .text_color(theme.text)
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(20.))
                                        .rounded_full()
                                        .bg(theme.accent)
                                        .text_color(white())
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::BOLD)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child((i + 1).to_string()),
                                )
                                .child(*s)
                        }))
                        .child(div().pt(px(6.)).text_size(px(12.)).text_color(theme.text_secondary).child(status.to_string())),
                )
                .child(
                    div()
                        .size(px(264.))
                        .rounded(px(16.))
                        .overflow_hidden()
                        .bg(white())
                        .border_1()
                        .border_color(theme.separator)
                        .flex()
                        .items_center()
                        .justify_center()
                        .map(|d| match code {
                            Some(code) => d.child(qr_element(code, 248.)),
                            None => d.child(div().text_color(black().opacity(0.5)).text_size(px(13.)).child("Generating code…")),
                        }),
                ),
        )
}
