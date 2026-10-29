//! How media that isn't on disk yet looks: a down arrow to fetch it, then a ring that
//! fills as it downloads.

use std::sync::OnceLock;
use std::time::Instant;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wa_backend::MediaState;

use crate::icons::{Glyph, glyph};

/// Where a message's media is.
#[derive(Clone, Copy, PartialEq)]
pub enum Fetch {
    /// On disk.
    Ready,
    /// Not downloaded (yet): the arrow fetches it.
    Missing,
    /// Downloading: how far (0..1), when the size is known.
    Downloading(Option<f32>),
    /// The download failed: the arrow tries again.
    Failed,
}

impl Fetch {
    pub fn of(has_path: bool, state: MediaState, progress: Option<f32>) -> Self {
        match (has_path, state) {
            (true, _) => Fetch::Ready,
            (false, MediaState::Idle) => Fetch::Missing,
            (false, MediaState::Downloading) => Fetch::Downloading(progress),
            (false, MediaState::Failed) => Fetch::Failed,
        }
    }

    pub fn ready(self) -> bool {
        self == Fetch::Ready
    }
}

/// A ring filling its parent (make the parent `relative`): `fraction` of it drawn
/// clockwise from the top, or a spinning arc while the size is unknown.
pub fn ring(fraction: Option<f32>, width: f32, color: Hsla) -> Canvas<()> {
    canvas(|_, _, _| {}, move |bounds, _, window, _| {
        let (start, sweep) = match fraction {
            Some(f) => (-std::f32::consts::FRAC_PI_2, std::f32::consts::TAU * f.clamp(0.02, 1.)),
            None => {
                // Spinning: a quarter arc going round once a second.
                window.request_animation_frame();
                static EPOCH: OnceLock<Instant> = OnceLock::new();
                let t = EPOCH.get_or_init(Instant::now).elapsed().as_secs_f32();
                (t.fract() * std::f32::consts::TAU, std::f32::consts::FRAC_PI_2)
            }
        };
        let center = bounds.center();
        let (cx, cy) = (f32::from(center.x), f32::from(center.y));
        let r = f32::from(bounds.size.width.min(bounds.size.height)) / 2. - width / 2.;
        let steps = ((sweep / std::f32::consts::TAU) * 64.).ceil().max(2.) as usize;
        let mut path = PathBuilder::stroke(px(width));
        for i in 0..=steps {
            let a = start + sweep * i as f32 / steps as f32;
            let p = point(px(cx + r * a.cos()), px(cy + r * a.sin()));
            if i == 0 { path.move_to(p) } else { path.line_to(p) }
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    })
    .absolute()
    .inset_0()
}

/// The round control over a photo, video or file that isn't downloaded: a down arrow,
/// circled by the progress ring while it downloads. `None` once it's on disk.
pub fn badge(fetch: Fetch, size: f32) -> Option<Div> {
    let progress = match fetch {
        Fetch::Ready => return None,
        Fetch::Downloading(p) => Some(p),
        Fetch::Missing | Fetch::Failed => None,
    };
    Some(
        div()
            .relative()
            .flex_none()
            .size(px(size))
            .rounded_full()
            .bg(black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(glyph(Glyph::ArrowDown, size * 0.42, white().opacity(if progress.is_some() { 0.7 } else { 1. })))
            .when_some(progress, |d, p| d.child(ring(p, 2.5, white()).inset(px(3.)))),
    )
}

/// Over a photo or video that isn't downloaded: dimmed, with the [`badge`] in the
/// middle and "Couldn't download" under it after a failure.
pub fn not_downloaded(fetch: Fetch, size: f32) -> Div {
    div()
        .absolute()
        .inset_0()
        .bg(black().opacity(0.28))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .children(badge(fetch, size))
        .when(fetch == Fetch::Failed, |d| {
            d.child(
                div()
                    .px(px(8.))
                    .py(px(3.))
                    .rounded_full()
                    .bg(black().opacity(0.5))
                    .text_color(white())
                    .text_size(px(11.))
                    .child("Couldn't download"),
            )
        })
}
