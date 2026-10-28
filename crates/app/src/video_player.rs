//! In-app video playback: AVKit's player view (the same controls as QuickTime, with PiP
//! and full screen) embedded in the window, over the dimmed overlay the window draws.
//! GPUI has no video element, so the player is a native subview of GPUI's view, placed
//! each frame where the overlay lays out its video.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use block2::RcBlock;
use gpui_kit::{Bounds, Pixels, Window};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly as _, Message as _};
use objc2_app_kit::{NSEvent, NSEventMask, NSView};
use objc2_av_foundation::AVPlayer;
use objc2_av_kit::AVPlayerView;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Which window's player: `None` the main window, `Some(jid)` that chat's mini window.
pub type Key = Option<String>;

const ESCAPE_KEY_CODE: u16 = 53;

struct Embedded {
    path: PathBuf,
    /// GPUI's view, which the player sits in.
    parent: Retained<NSView>,
    view: Retained<AVPlayerView>,
    player: Retained<AVPlayer>,
}

thread_local! {
    static EMBEDDED: RefCell<HashMap<Key, Embedded>> = RefCell::new(HashMap::new());
    static ESCAPE_MONITOR: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

pub(crate) fn gpui_view(window: &Window) -> Option<Retained<NSView>> {
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return None };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
    // SAFETY: GPUI's live content view, on the main thread.
    Some(unsafe { handle.ns_view.cast::<NSView>().as_ref() }.retain())
}

/// Show the player for `path` over `frame` (window points, from the top left) in
/// `window`, starting playback the first time. False while some other native view (a
/// glass pane re-stacked this frame) is above it: call again on the next frame.
pub fn place(key: &Key, path: &Path, frame: Bounds<Pixels>, window: &Window) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return true };
    let Some(parent) = gpui_view(window) else { return true };
    install_escape_monitor();
    EMBEDDED.with(|embedded| {
        let mut embedded = embedded.borrow_mut();
        if embedded.get(key).is_some_and(|e| e.path != path || e.parent != parent)
            && let Some(old) = embedded.remove(key)
        {
            stop(old);
        }
        if !embedded.contains_key(key) {
            let Some(created) = create(path, parent.clone(), mtm) else { return true };
            created.parent.addSubview(&created.view);
            // SAFETY: a valid player, on the main thread.
            unsafe { created.player.play() };
            embedded.insert(key.clone(), created);
        }
        let Some(e) = embedded.get(key) else { return true };
        let (x, y) = (f64::from(f32::from(frame.origin.x)), f64::from(f32::from(frame.origin.y)));
        let (w, h) = (f64::from(f32::from(frame.size.width)), f64::from(f32::from(frame.size.height)));
        let y = if e.parent.isFlipped() { y } else { e.parent.bounds().size.height - y - h };
        let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        if e.view.frame() != rect {
            e.view.setFrame(rect);
        }
        let view: &NSView = &e.view;
        let on_top = e.parent.subviews().lastObject().is_some_and(|top| &*top == view);
        if !on_top {
            // Re-adding moves it above the rest.
            e.parent.addSubview(view);
        }
        on_top
    })
}

fn create(path: &Path, parent: Retained<NSView>, mtm: MainThreadMarker) -> Option<Embedded> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
    // SAFETY: creating and configuring AVKit objects on the main thread.
    unsafe {
        let player = AVPlayer::playerWithURL(&url, mtm);
        let view = AVPlayerView::initWithFrame(AVPlayerView::alloc(mtm), NSRect::ZERO);
        view.setPlayer(Some(&player));
        Some(Embedded { path: path.to_path_buf(), parent, view, player })
    }
}

fn stop(e: Embedded) {
    // SAFETY: valid objects, on the main thread.
    unsafe {
        e.player.pause();
        e.player.replaceCurrentItemWithPlayerItem(None);
    }
    e.view.removeFromSuperview();
}

/// Remove `key`'s player, stopping playback.
pub fn close(key: &Key) {
    if let Some(e) = EMBEDDED.with(|embedded| embedded.borrow_mut().remove(key)) {
        stop(e);
    }
}

/// Space while the overlay (not the player) has key focus.
pub fn toggle(key: &Key) {
    EMBEDDED.with(|embedded| {
        if let Some(e) = embedded.borrow().get(key) {
            // SAFETY: a valid player, on the main thread.
            unsafe {
                if e.player.rate() > 0. {
                    e.player.pause();
                } else {
                    e.player.play();
                }
            }
        }
    });
}

/// After a click on its controls the player has key focus, so Esc would go to it. Hand
/// focus back to GPUI's view first: it then gets the Esc and closes the player.
fn install_escape_monitor() {
    ESCAPE_MONITOR.with(|monitor| {
        if monitor.borrow().is_some() {
            return;
        }
        let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a valid event for the handler's duration.
            let ev = unsafe { event.as_ref() };
            if ev.keyCode() == ESCAPE_KEY_CODE {
                hand_focus_back(ev);
            }
            event.as_ptr()
        });
        // SAFETY: the block is retained by AppKit; called on the main thread.
        let token = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block) };
        *monitor.borrow_mut() = token;
    });
}

fn hand_focus_back(ev: &NSEvent) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(window) = ev.window(mtm) else { return };
    let Some(responder) = window.firstResponder() else { return };
    let Ok(focused) = responder.downcast::<NSView>() else { return };
    EMBEDDED.with(|embedded| {
        for e in embedded.borrow().values() {
            // Only while it's in our view (not in its own full-screen window, where Esc
            // leaves full screen).
            // SAFETY: reading a live view's superview on the main thread.
            let in_place = unsafe { e.view.superview() }.is_some_and(|s| s == e.parent);
            if in_place && focused.isDescendantOf(&e.view) {
                window.makeFirstResponder(Some(&e.parent));
            }
        }
    });
}
