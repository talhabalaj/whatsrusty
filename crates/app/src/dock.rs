//! Dock tile badge: unread messages outside muted chats, like Messages. Kept up to date
//! while the window is closed (the app keeps running).

use gpui_kit::{App, Entity};
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;
use wa_backend::ChatSummary;

use crate::state::Store;

pub fn track(store: &Entity<Store>, cx: &mut App) {
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
    chats.iter().filter(|c| !c.muted).map(|c| c.unread_count).sum()
}

fn set_badge(count: u32) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    tile.setBadgeLabel(label.as_deref());
    log::debug!("Dock badge now {:?}", tile.badgeLabel().map(|l| l.to_string()));
}
