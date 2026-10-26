//! Sparkle 2 auto-update. Sparkle.framework is embedded by the Xcode project and loaded at
//! runtime, so plain `cargo run` builds neither link nor start it.

use std::cell::OnceCell;

use gpui_kit::*;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_foundation::{NSBundle, NSString};

use crate::identity;

actions!(whatsrusty_updater, [CheckForUpdates]);

thread_local! {
    /// The SPUStandardUpdaterController, alive for the whole app run.
    static CONTROLLER: OnceCell<Option<Retained<AnyObject>>> = const { OnceCell::new() };
}

/// Starts Sparkle (scheduled checks per Info.plist) when running from the Release bundle.
pub fn init(cx: &mut App) {
    CONTROLLER.with(|c| {
        c.get_or_init(start_controller);
    });
    cx.on_action(|_: &CheckForUpdates, _| check_for_updates());
}

pub fn is_available() -> bool {
    CONTROLLER.with(|c| c.get().is_some_and(Option::is_some))
}

fn check_for_updates() {
    CONTROLLER.with(|c| {
        if let Some(Some(controller)) = c.get() {
            let _: () = unsafe { msg_send![&**controller, checkForUpdates: std::ptr::null::<AnyObject>()] };
        }
    });
}

fn start_controller() -> Option<Retained<AnyObject>> {
    if cfg!(debug_assertions) {
        return None;
    }
    let main = NSBundle::mainBundle();
    if main.bundleIdentifier()?.to_string() != identity::RELEASE_BUNDLE_ID {
        return None;
    }
    let path = std::path::Path::new(&main.privateFrameworksPath()?.to_string()).join("Sparkle.framework");
    let Some(bundle) = NSBundle::bundleWithPath(&NSString::from_str(&path.to_string_lossy())) else {
        log::warn!("updates disabled: {} is missing", path.display());
        return None;
    };
    if !unsafe { bundle.load() } {
        log::warn!("updates disabled: Sparkle.framework failed to load");
        return None;
    }
    let class = AnyClass::get(c"SPUStandardUpdaterController")?;
    let nil = std::ptr::null_mut::<AnyObject>();
    let controller: Option<Retained<AnyObject>> = unsafe {
        msg_send![
            msg_send![class, alloc],
            initWithStartingUpdater: Bool::YES,
            updaterDelegate: nil,
            userDriverDelegate: nil
        ]
    };
    log::info!("Sparkle updater {}", if controller.is_some() { "started" } else { "failed to start" });
    controller
}
