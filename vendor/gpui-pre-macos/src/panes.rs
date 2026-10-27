//! whatsapp-gpui patch: native hosts for the scene's panes (`Window::paint_pane`).
//!
//! Each pane gets a layer-hosting `GPUIPaneView` whose layer is a `CAMetalLayer` the
//! renderer draws the pane's scene into. Glass panes wrap that view in an
//! `NSGlassEffectView` (macOS 26+) as its `contentView`, so the glass samples the GPUI
//! view's main layer beneath it while the pane's own content stays sharp on top. All
//! views are subviews of the GPUI view, stacked in paint order, and none of them takes
//! part in hit testing: events still reach the GPUI view, which hit-tests its own frame.

use crate::renderer::{MetalRenderer, PaneLayer, PaneTarget};
use gpui::{DevicePixels, PaneDescriptor, PaneMaterial, ScenePane, Size, size};
use objc2::{
    class, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel},
    sel,
};
use objc2_app_kit::NSAutoresizingMaskOptions;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use std::sync::OnceLock;

struct Classes {
    pane_view: &'static AnyClass,
    glass_view: Option<&'static AnyClass>,
    glass_container: Option<&'static AnyClass>,
}

// SAFETY: Objective-C classes are immutable once registered.
unsafe impl Send for Classes {}
unsafe impl Sync for Classes {}

fn classes() -> &'static Classes {
    static CLASSES: OnceLock<Classes> = OnceLock::new();
    CLASSES.get_or_init(|| {
        let pane_view = {
            let mut decl = ClassBuilder::new(c"GPUIPaneView", class!(NSView)).unwrap();
            // SAFETY: the signatures match the selectors.
            unsafe {
                decl.add_method(
                    sel!(hitTest:),
                    hit_test_none as extern "C" fn(_, Sel, NSPoint) -> *mut AnyObject,
                );
            }
            unsafe { decl.add_method(sel!(isOpaque), no as extern "C" fn(_, Sel) -> Bool) };
            decl.register()
        };
        let glass_view = AnyClass::get(c"NSGlassEffectView").map(|glass| {
            let mut decl = ClassBuilder::new(c"GPUIGlassPaneView", glass).unwrap();
            // SAFETY: the signatures match the selectors.
            unsafe {
                decl.add_method(
                    sel!(hitTest:),
                    hit_test_none as extern "C" fn(_, Sel, NSPoint) -> *mut AnyObject,
                );
            }
            decl.register()
        });
        let glass_container = AnyClass::get(c"NSGlassEffectContainerView").map(|container| {
            let mut decl = ClassBuilder::new(c"GPUIGlassContainerView", container).unwrap();
            // SAFETY: the signatures match the selectors.
            unsafe {
                decl.add_method(
                    sel!(hitTest:),
                    hit_test_none as extern "C" fn(_, Sel, NSPoint) -> *mut AnyObject,
                );
            }
            decl.register()
        });
        Classes {
            pane_view,
            glass_view,
            glass_container,
        }
    })
}

extern "C" fn hit_test_none(_: &AnyObject, _: Sel, _: NSPoint) -> *mut AnyObject {
    std::ptr::null_mut()
}

extern "C" fn no(_: &AnyObject, _: Sel) -> Bool {
    Bool::NO
}

/// Whether glass panes get real glass (macOS 26+ has `NSGlassEffectView`).
pub(crate) fn glass_available() -> bool {
    classes().glass_view.is_some()
}

struct NativePane {
    id: u64,
    /// The view added to the GPUI view: the glass view, or the host for plain panes.
    outer: Retained<AnyObject>,
    glass: Option<Retained<AnyObject>>,
    _host: Retained<AnyObject>,
    layer: PaneLayer,
    applied: Option<(PaneDescriptor, f32, f64)>,
}

impl NativePane {
    fn new(id: u64, material: PaneMaterial, renderer: &MetalRenderer) -> Self {
        let classes = classes();
        let layer = renderer.new_pane_layer();
        unsafe {
            let host: Retained<AnyObject> = msg_send![classes.pane_view, new];
            // Layer-hosting: set the layer before turning on wantsLayer.
            let _: () = msg_send![&*host, setLayer: layer.as_ptr()];
            let _: () = msg_send![&*host, setWantsLayer: true];
            let _: () = msg_send![
                &*host,
                setAutoresizingMask: NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable
            ];
            let glass = match (material, classes.glass_view) {
                (PaneMaterial::Glass { .. }, Some(class)) => {
                    let glass: Retained<AnyObject> = msg_send![class, new];
                    let _: () = msg_send![&*glass, setContentView: &*host];
                    Some(glass)
                }
                _ => None,
            };
            Self {
                id,
                outer: glass.clone().unwrap_or_else(|| host.clone()),
                glass,
                _host: host,
                layer,
                applied: None,
            }
        }
    }

    fn is_glass(&self) -> bool {
        self.glass.is_some()
    }

    /// Applies geometry and material when they changed. Returns whether anything moved.
    fn apply(&mut self, descriptor: &PaneDescriptor, scale: f32, parent_height: f64) -> bool {
        let state = (*descriptor, scale, parent_height);
        if self.applied == Some(state) {
            return false;
        }
        let b = descriptor.bounds;
        let s = scale as f64;
        let frame = NSRect::new(
            NSPoint::new(
                b.origin.x.0 as f64 / s,
                // The GPUI view isn't flipped: AppKit's y axis points up.
                parent_height - (b.origin.y.0 + b.size.height.0) as f64 / s,
            ),
            NSSize::new(b.size.width.0 as f64 / s, b.size.height.0 as f64 / s),
        );
        unsafe {
            let _: () = msg_send![&*self.outer, setFrame: frame];
            if let Some(glass) = &self.glass {
                let _: () =
                    msg_send![&**glass, setCornerRadius: descriptor.corner_radius.0 as f64 / s];
                if let PaneMaterial::Glass { clear, tint } = descriptor.material {
                    // NSGlassEffectViewStyle: 0 regular, 1 clear.
                    let _: () = msg_send![&**glass, setStyle: if clear { 1isize } else { 0isize }];
                    let color: Option<Retained<AnyObject>> = tint.map(|tint| {
                        let rgba = gpui::Rgba::from(tint);
                        msg_send![
                            class!(NSColor),
                            colorWithSRGBRed: rgba.r as f64,
                            green: rgba.g as f64,
                            blue: rgba.b as f64,
                            alpha: rgba.a as f64
                        ]
                    });
                    let _: () = msg_send![&**glass, setTintColor: color.as_deref()];
                }
            }
        }
        let device_size: Size<DevicePixels> = size(
            DevicePixels(b.size.width.0 as i32),
            DevicePixels(b.size.height.0 as i32),
        );
        self.layer.set_size(device_size, scale);
        self.applied = Some(state);
        true
    }
}

/// `GPUI_GLASS_CONTAINER=1` puts glass panes in one `NSGlassEffectContainerView`
/// covering the GPUI view (plain panes stay above it). Off by default: measured on macOS
/// 27 it cost ~15% more WindowServer GPU time than separate glass views, not less (see
/// docs/liquid-glass.md).
fn use_container() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        classes().glass_container.is_some()
            && std::env::var("GPUI_GLASS_CONTAINER").is_ok_and(|v| v == "1")
    })
}

#[derive(Default)]
pub(crate) struct Panes {
    panes: Vec<NativePane>,
    container: Option<Retained<AnyObject>>,
}

impl Panes {
    pub(crate) fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Creates, updates, restacks and removes native panes to match this frame's scene
    /// panes. When anything changed, returns the still-open Core Animation transaction:
    /// the caller presents the frame before dropping it, so views and pixels move together.
    pub(crate) fn sync(
        &mut self,
        parent: *mut AnyObject,
        scale: f32,
        scene_panes: &[ScenePane],
        renderer: &MetalRenderer,
    ) -> Option<Transaction> {
        let unchanged = self.panes.len() == scene_panes.len()
            && self
                .panes
                .iter()
                .zip(scene_panes)
                .all(|(native, pane)| native.applied.map(|a| a.0) == Some(*pane.descriptor()));
        let parent_height = unsafe {
            let bounds: NSRect = msg_send![parent, bounds];
            bounds.size.height
        };
        if unchanged
            && self
                .panes
                .iter()
                .all(|p| p.applied.is_some_and(|a| a.1 == scale && a.2 == parent_height))
        {
            return None;
        }

        let transaction = Transaction::begin();
        let mut previous = std::mem::take(&mut self.panes);
        let mut restack = false;
        for pane in scene_panes {
            let descriptor = pane.descriptor();
            let wants_glass =
                matches!(descriptor.material, PaneMaterial::Glass { .. }) && glass_available();
            let existing = previous
                .iter()
                .position(|p| p.id == descriptor.id && p.is_glass() == wants_glass);
            let mut native = match existing {
                Some(found) => {
                    restack |= found != 0;
                    previous.remove(found)
                }
                None => {
                    restack = true;
                    NativePane::new(descriptor.id, descriptor.material, renderer)
                }
            };
            native.apply(descriptor, scale, parent_height);
            self.panes.push(native);
        }
        for gone in previous {
            unsafe {
                let _: () = msg_send![&*gone.outer, removeFromSuperview];
            }
        }
        if restack {
            let container = self.container(parent);
            // Re-adding a subview moves it to the top, which leaves them in paint order.
            for pane in &self.panes {
                let superview = match &container {
                    Some(container) if pane.is_glass() => &**container as *const AnyObject as *mut AnyObject,
                    _ => parent,
                };
                unsafe {
                    let _: () = msg_send![superview, addSubview: &*pane.outer];
                }
            }
        }
        Some(transaction)
    }

    /// The shared glass container, created on first use, below every plain pane.
    fn container(&mut self, parent: *mut AnyObject) -> Option<Retained<AnyObject>> {
        if !use_container() {
            return None;
        }
        let container = self.container.get_or_insert_with(|| unsafe {
            let class = classes().glass_container.expect("checked by use_container");
            let container: Retained<AnyObject> = msg_send![class, new];
            let bounds: NSRect = msg_send![parent, bounds];
            let _: () = msg_send![&*container, setFrame: bounds];
            let _: () = msg_send![
                &*container,
                setAutoresizingMask: NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable
            ];
            container
        });
        // (Re-)adding moves it to the top; plain panes are re-added after it, above.
        unsafe {
            let _: () = msg_send![parent, addSubview: &**container];
        }
        Some(container.clone())
    }

    /// The render targets for this frame, in the same order as `scene_panes`.
    pub(crate) fn targets<'a>(&'a self, scene_panes: &'a [ScenePane]) -> Vec<PaneTarget<'a>> {
        self.panes
            .iter()
            .zip(scene_panes)
            .map(|(native, pane)| {
                let b = pane.descriptor().bounds;
                PaneTarget {
                    layer: &native.layer,
                    scene: &pane.scene,
                    size: size(
                        DevicePixels(b.size.width.0 as i32),
                        DevicePixels(b.size.height.0 as i32),
                    ),
                }
            })
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        for pane in self.panes.drain(..) {
            unsafe {
                let _: () = msg_send![&*pane.outer, removeFromSuperview];
            }
        }
        if let Some(container) = self.container.take() {
            unsafe {
                let _: () = msg_send![&*container, removeFromSuperview];
            }
        }
    }
}

/// A Core Animation transaction without implicit animations, so pane views jump to their
/// new frames in the same commit as the frame that moved them.
pub(crate) struct Transaction;

impl Transaction {
    fn begin() -> Self {
        unsafe {
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: true];
        }
        Transaction
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![class!(CATransaction), commit];
        }
    }
}
