//! Offline to WhatsApp while the user is away: the Mac asleep, its screens off, locked,
//! or no keyboard, mouse or trackpad input for [`IDLE_AFTER`]. A linked device that's
//! online keeps the phone's notifications quiet, and a Mac that went to sleep with the
//! app in front would otherwise stay "online" (its background wakes reconnect and say so
//! again).

use std::cell::Cell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use gpui_kit::{App, Entity};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceScreensDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSOperationQueue, NSString};
use wa_backend::Command;

use crate::state::Store;

/// How long going to sleep waits for WhatsApp to hear we're offline.
const SLEEP_WAIT: Duration = Duration::from_secs(2);
/// No input for this long counts as away (`WA_IDLE_SECS` overrides it, for testing).
const IDLE_AFTER: Duration = Duration::from_secs(2 * 60);
/// How often the time since the last input is checked.
const IDLE_POLL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
enum Reason {
    Asleep,
    ScreensOff,
    Locked,
    SwitchedOut,
    Idle,
}

impl Reason {
    fn label(self) -> &'static str {
        match self {
            Reason::Asleep => "asleep",
            Reason::ScreensOff => "screens off",
            Reason::Locked => "locked",
            Reason::SwitchedOut => "switched out",
            Reason::Idle => "idle",
        }
    }
}

/// Why we're away, one bit per [`Reason`] (main thread only).
#[derive(Clone)]
struct Away {
    reasons: Rc<Cell<u8>>,
    commands: flume::Sender<Command>,
    store: flume::Sender<bool>,
}

impl Away {
    fn has(&self, reason: Reason) -> bool {
        self.reasons.get() & (1 << reason as u8) != 0
    }

    fn set(&self, reason: Reason, on: bool) {
        let bit = 1 << reason as u8;
        let was_away = self.reasons.get() != 0;
        self.reasons.set(if on { self.reasons.get() | bit } else { self.reasons.get() & !bit });
        let away = self.reasons.get() != 0;
        log::info!("power: {} {}, away={away}", if on { "now" } else { "no longer" }, reason.label());
        if away && !was_away {
            // Straight to the backend: the Mac may be asleep before the UI's next turn.
            let (done, heard) = flume::bounded(1);
            let _ = self.commands.send(Command::SetActive { active: false, done: Some(done) });
            if reason == Reason::Asleep && heard.recv_timeout(SLEEP_WAIT).is_err() {
                log::warn!("power: going to sleep before WhatsApp heard we're offline");
            }
        }
        let _ = self.store.send(away);
    }
}

pub fn track(store: &Entity<Store>, cx: &mut App) {
    let (tx, rx) = flume::unbounded::<bool>();
    let away = Away { reasons: Rc::default(), commands: store.read(cx).commands(), store: tx };

    let observe = |center: &NSNotificationCenter, name: &NSString, reason: Reason, on: bool| {
        let away = away.clone();
        let block = RcBlock::new(move |_: NonNull<NSNotification>| away.set(reason, on));
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: a main-queue block observing a notification by name, kept for the app's life.
        let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, Some(&queue), &block) };
        std::mem::forget(token);
    };
    let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: AppKit's notification name constants.
    unsafe {
        observe(&workspace, NSWorkspaceWillSleepNotification, Reason::Asleep, true);
        observe(&workspace, NSWorkspaceDidWakeNotification, Reason::Asleep, false);
        observe(&workspace, NSWorkspaceScreensDidSleepNotification, Reason::ScreensOff, true);
        observe(&workspace, NSWorkspaceScreensDidWakeNotification, Reason::ScreensOff, false);
        observe(&workspace, NSWorkspaceSessionDidResignActiveNotification, Reason::SwitchedOut, true);
        observe(&workspace, NSWorkspaceSessionDidBecomeActiveNotification, Reason::SwitchedOut, false);
    }
    let distributed = NSDistributedNotificationCenter::defaultCenter();
    observe(&distributed, &NSString::from_str("com.apple.screenIsLocked"), Reason::Locked, true);
    observe(&distributed, &NSString::from_str("com.apple.screenIsUnlocked"), Reason::Locked, false);

    let idle_after = std::env::var("WA_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(IDLE_AFTER, Duration::from_secs);
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(IDLE_POLL).await;
            let idle = idle_for() >= idle_after;
            if idle != away.has(Reason::Idle) {
                away.set(Reason::Idle, idle);
            }
        }
    })
    .detach();

    let store = store.downgrade();
    cx.spawn(async move |cx| {
        while let Ok(away) = rx.recv_async().await {
            if store.update(cx, |s, _| s.set_away(away)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Time since the last keyboard, mouse or trackpad input, in this login session.
fn idle_for() -> Duration {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    const COMBINED_SESSION_STATE: i32 = 0;
    const ANY_INPUT_EVENT: u32 = !0;
    // SAFETY: a plain query with no pointers.
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT) };
    Duration::try_from_secs_f64(secs).unwrap_or_default()
}
