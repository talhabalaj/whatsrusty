//! Dock tile badge: unread messages outside muted and archived chats, like WhatsApp. Kept up to date
//! while the window is closed (the app keeps running).

use block2::RcBlock;
use gpui_kit::{App, Entity};
use objc2::MainThreadMarker;
use objc2::runtime::Bool;
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNNotificationSetting, UNNotificationSettings, UNUserNotificationCenter,
};
use wa_backend::ChatSummary;

use crate::state::Store;

pub fn track(store: &Entity<Store>, cx: &mut App) {
    request_badge_permission();
    let mut shown = None;
    cx.observe(store, move |store, cx| {
        let total = unread_total(&store.read(cx).chats);
        // Store notifies often (typing, statuses): only touch AppKit on a change.
        if shown != Some(total) {
            shown = Some(total);
            set_badge(total);
        }
    })
    .detach();
}

pub fn unread_total(chats: &[ChatSummary]) -> u32 {
    chats.iter().filter(|c| !c.muted && !c.archived).map(|c| c.unread_count).sum()
}

fn set_badge(count: u32) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    tile.setBadgeLabel(label.as_deref());
    log::info!("Dock badge now {:?}", tile.badgeLabel().map(|l| l.to_string()));
}

/// macOS only shows an app's Dock badge with notification permission that includes badges,
/// so ask at launch (with alerts and sounds, as the first banner would).
fn request_badge_permission() {
    // Outside an app bundle (`cargo run`) the notification center aborts the process.
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return;
    }
    let done = RcBlock::new(|granted: Bool, error: *mut NSError| {
        // SAFETY: when non-null, `error` is a valid NSError for the callback's duration.
        match unsafe { error.as_ref() } {
            Some(e) => log::warn!("notification permission request failed: {}", e.localizedDescription()),
            None => log::info!("notification permission (badge) granted: {}", granted.as_bool()),
        }
        log_badge_setting();
    });
    let options = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge;
    UNUserNotificationCenter::currentNotificationCenter().requestAuthorizationWithOptions_completionHandler(options, &done);
}

/// Whether macOS will show the badge: System Settings → Notifications → WhatsRusty →
/// "Badge application icon" can be off even with permission granted.
fn log_badge_setting() {
    let done = RcBlock::new(|settings: std::ptr::NonNull<UNNotificationSettings>| {
        // SAFETY: a valid settings object for the callback's duration.
        let settings = unsafe { settings.as_ref() };
        let status = settings.authorizationStatus();
        let badge = settings.badgeSetting();
        let status = match status {
            UNAuthorizationStatus::Authorized => "authorized",
            UNAuthorizationStatus::Denied => "denied",
            UNAuthorizationStatus::NotDetermined => "not determined",
            UNAuthorizationStatus::Provisional => "provisional",
            _ => "other",
        };
        let badge = match badge {
            UNNotificationSetting::Enabled => "on",
            UNNotificationSetting::Disabled => "off",
            UNNotificationSetting::NotSupported => "not supported",
            _ => "other",
        };
        log::info!("notifications {status}, Dock badge setting {badge}");
    });
    UNUserNotificationCenter::currentNotificationCenter().getNotificationSettingsWithCompletionHandler(&done);
}
