//! Liquid Glass (macOS 26+) for the floating header and composer: an element that paints
//! its child into a pane of its own, which GPUI hosts inside an `NSGlassEffectView` over
//! the transcript (see docs/liquid-glass.md). Below macOS 26, or with `WA_GLASS=0`, the
//! callers keep their solid surfaces and nothing here is used.

use gpui_kit::*;

/// Whether this window draws real glass. `WA_GLASS=0` turns it off (for comparisons).
pub fn enabled(window: &Window) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    !*OFF.get_or_init(|| std::env::var("WA_GLASS").is_ok_and(|v| v == "0")) && window.supports_glass_panes()
}

/// Paints `child` into its own surface with glass behind it, shaped as a rounded rect
/// with `radius` (use half the height for a capsule). Positioned and sized by the child's
/// own style; events, focus and hit testing are the child's as usual.
pub fn glass(id: &'static str, radius: f32, child: impl IntoElement) -> Pane {
    glass_material(id, radius, false, child)
}

/// A round glass button, like macOS 26 toolbar buttons: `button` (with its id, icon,
/// handlers and position) gets the circle's size, shape and hover, then glass behind it.
pub fn circle_button(id: &'static str, size: f32, hover: Hsla, button: Stateful<Div>) -> Pane {
    let button = button
        .size(px(size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |s| s.bg(hover));
    glass(id, size / 2., button)
}

/// Like [`glass`], with the glass AppKit puts behind window sidebars: edge-to-edge, a
/// subtle shade over the window background, no shadow.
pub fn sidebar_glass(id: &'static str, child: impl IntoElement) -> Pane {
    glass_material(id, 0., true, child)
}

fn glass_material(id: &'static str, radius: f32, sidebar: bool, child: impl IntoElement) -> Pane {
    // `WA_GLASS=clear`: the clearer glass variant (for comparisons).
    static CLEAR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let clear = *CLEAR.get_or_init(|| std::env::var("WA_GLASS").is_ok_and(|v| v == "clear"));
    // `WA_GLASS_PLAIN=<id>,…`: those panes get no glass (to measure each view's cost).
    static PLAIN: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let plain = PLAIN.get_or_init(|| std::env::var("WA_GLASS_PLAIN").map(|v| v.split(',').map(String::from).collect()).unwrap_or_default());
    let material = if plain.iter().any(|p| p == id) { PaneMaterial::Plain } else { PaneMaterial::Glass { clear, tint: None, sidebar } };
    Pane { id: pane_id(id), radius: px(radius), material, child: child.into_any_element() }
}

/// Paints `child` into a plain surface stacked above the glass, for things that must
/// cover it (modal backdrops, the photo viewer). Only used while glass is on.
pub fn above_glass(id: &'static str, child: impl IntoElement) -> Pane {
    Pane { id: pane_id(id), radius: px(0.), material: PaneMaterial::Plain, child: child.into_any_element() }
}

fn pane_id(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

pub struct Pane {
    id: u64,
    radius: Pixels,
    material: PaneMaterial,
    child: AnyElement,
}

impl IntoElement for Pane {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Pane {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        window.paint_pane(self.id, bounds, self.radius, self.material, |window| child.paint(window, cx));
    }
}
