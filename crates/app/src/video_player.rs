//! In-app video playback with AVKit's native player (the same controls as QuickTime),
//! in a window owned by this app. GPUI has no video element, and AVPlayerView gives us
//! hardware decoding, scrubbing, PiP and full screen for free.

use std::cell::RefCell;
use std::path::Path;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask, NSWindowWillCloseNotification};
use objc2_av_foundation::AVPlayer;
use objc2_av_kit::AVPlayerView;
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};

struct Open {
    window: Retained<NSWindow>,
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

thread_local! {
    /// Player windows stay alive while shown; entries are dropped when they close.
    static OPEN: RefCell<Vec<Open>> = const { RefCell::new(Vec::new()) };
}

/// Fit the video's size into a comfortable window, keeping its aspect ratio.
fn window_size(width: Option<u32>, height: Option<u32>) -> (f64, f64) {
    let (w, h) = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f64, h as f64),
        _ => (16., 9.),
    };
    let scale = (960. / w).min(640. / h);
    ((w * scale).max(320.), (h * scale).max(240.))
}

fn prune_closed() {
    OPEN.with(|open| {
        open.borrow_mut().retain(|o| {
            let visible = o.window.isVisible() || o.window.isMiniaturized();
            if !visible {
                // SAFETY: removing our own observer token on the main thread.
                unsafe { NSNotificationCenter::defaultCenter().removeObserver(AsRef::<objc2::runtime::AnyObject>::as_ref(&*o.observer)) };
            }
            visible
        })
    });
}

/// Open `path` in a native player window and start playing. Must run on the main thread
/// (GPUI event handlers do).
pub fn play(path: &Path, title: &str, width: Option<u32>, height: Option<u32>) {
    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("video_player::play called off the main thread");
        return;
    };
    let Some(path_str) = path.to_str() else { return };
    prune_closed();
    let (w, h) = window_size(width, height);
    let rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(w, h));

    // SAFETY: all calls happen on the main thread with valid, retained objects.
    unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
        let player = AVPlayer::playerWithURL(&url, mtm);

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        window.setReleasedWhenClosed(false);
        window.setTitle(&NSString::from_str(title));
        window.setTitlebarAppearsTransparent(true);
        window.setContentAspectRatio(NSSize::new(w, h));

        let view = AVPlayerView::initWithFrame(AVPlayerView::alloc(mtm), rect);
        view.setPlayer(Some(&player));
        window.setContentView(Some(&view));
        window.center();
        window.makeKeyAndOrderFront(None);
        player.play();

        // Stop audio when the window closes. The window itself is released on the next
        // `play` (dropping it from inside its own will-close notification isn't safe).
        let on_close = RcBlock::new(move |_: NonNull<NSNotification>| {
            player.pause();
            player.replaceCurrentItemWithPlayerItem(None);
        });
        let observer = NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSWindowWillCloseNotification),
            Some(&window),
            None,
            &on_close,
        );
        OPEN.with(|open| open.borrow_mut().push(Open { window, observer }));
    }
}
