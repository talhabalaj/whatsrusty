//! "Launch at Login" via `SMAppService.mainAppService` (macOS 13+). Only offered when running
//! from an app bundle; the system tracks the registration per bundle.

use gpui_kit::*;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::{NSBundle, NSError};

actions!(whatsrusty_login_item, [ToggleLaunchAtLogin]);

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

/// `SMAppServiceStatus`
const STATUS_ENABLED: isize = 1;
const STATUS_REQUIRES_APPROVAL: isize = 2;

pub fn init(cx: &mut App) {
    cx.on_action(|_: &ToggleLaunchAtLogin, cx| {
        toggle();
        crate::refresh_menus(cx);
    });
}

pub fn is_available() -> bool {
    let main = NSBundle::mainBundle();
    main.bundleIdentifier().is_some() && main.bundlePath().to_string().ends_with(".app") && service().is_some()
}

/// Registered and allowed (not waiting for approval in System Settings).
pub fn is_enabled() -> bool {
    status() == Some(STATUS_ENABLED)
}

fn service() -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"SMAppService")?;
    unsafe { msg_send![class, mainAppService] }
}

fn status() -> Option<isize> {
    let service = service()?;
    Some(unsafe { msg_send![&*service, status] })
}

fn toggle() {
    let Some(service) = service() else { return };
    let status: isize = unsafe { msg_send![&*service, status] };
    let registered = matches!(status, STATUS_ENABLED | STATUS_REQUIRES_APPROVAL);
    let result: Result<(), Retained<NSError>> = if registered {
        unsafe { msg_send![&*service, unregisterAndReturnError: _] }
    } else {
        unsafe { msg_send![&*service, registerAndReturnError: _] }
    };
    if let Err(e) = result {
        log::warn!("launch at login: {}", e.localizedDescription());
    }
    let now: isize = unsafe { msg_send![&*service, status] };
    log::info!("launch at login status {status} -> {now}");
    // The user turned the login item off in System Settings before; send them there.
    if !registered && now == STATUS_REQUIRES_APPROVAL
        && let Some(class) = AnyClass::get(c"SMAppService")
    {
        let _: () = unsafe { msg_send![class, openSystemSettingsLoginItems] };
    }
}
