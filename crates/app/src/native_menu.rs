//! Context menus as real macOS menus (NSMenu): the system's look, keyboard navigation,
//! type-to-select and submenus, instead of menus drawn by GPUI.

use std::cell::Cell;
use std::time::Duration;

use gpui_kit::{App, Pixels, Point, Window};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSMenu, NSMenuItem};
use objc2_foundation::{NSPoint, NSString};

type Run = Box<dyn FnOnce(&mut Window, &mut App)>;

pub enum Item {
    Action { title: String, enabled: bool, run: Run },
    Submenu { title: String, items: Vec<Item> },
    Separator,
}

impl Item {
    pub fn action(title: impl Into<String>, run: impl FnOnce(&mut Window, &mut App) + 'static) -> Self {
        Item::Action { title: title.into(), enabled: true, run: Box::new(run) }
    }

    pub fn submenu(title: impl Into<String>, items: Vec<Item>) -> Self {
        Item::Submenu { title: title.into(), items }
    }

    /// Shown grayed out.
    pub fn enabled(mut self, on: bool) -> Self {
        if let Item::Action { enabled, .. } = &mut self {
            *enabled = on;
        }
        self
    }
}

thread_local! {
    /// The tag of the item picked in the menu being shown.
    static CHOSEN: Cell<Option<isize>> = const { Cell::new(None) };
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `MenuTarget` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WhatsRustyMenuTarget"]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(pick:))]
        fn pick(&self, sender: &NSMenuItem) {
            CHOSEN.set(Some(sender.tag()));
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: plain NSObject initialization.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/// Show `items` at `position` (window coordinates) in `window`; the chosen item runs once
/// the menu closes. Returns at once: the menu opens after the current event.
pub fn show(items: Vec<Item>, position: Point<Pixels>, window: &Window, cx: &mut App) {
    let Some(view) = crate::video_player::gpui_view(window) else { return };
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        // Outside the click's handling (the menu runs its own event loop), and after a
        // frame, so whatever the click opened (the tapback tray) is drawn under it.
        cx.background_executor().timer(Duration::from_millis(16)).await;
        let Some(mtm) = MainThreadMarker::new() else { return };
        let target = MenuTarget::new(mtm);
        let mut actions = Vec::new();
        let menu = build(items, &mut actions, &target, mtm);
        let (x, y) = (f64::from(f32::from(position.x)), f64::from(f32::from(position.y)));
        let y = if view.isFlipped() { y } else { view.bounds().size.height - y };
        CHOSEN.set(None);
        menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(x, y), Some(&view));
        let chosen = CHOSEN.take().and_then(|tag| usize::try_from(tag).ok());
        if let Some(run) = chosen.and_then(|i| actions.get_mut(i)).and_then(Option::take) {
            let _ = handle.update(cx, |_, window, cx| run(window, cx));
        }
    })
    .detach();
}

fn build(items: Vec<Item>, actions: &mut Vec<Option<Run>>, target: &MenuTarget, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for item in items {
        let entry = match item {
            Item::Separator => NSMenuItem::separatorItem(mtm),
            Item::Action { title, enabled, run } => {
                let entry = new_item(&title, mtm);
                // SAFETY: `pick:` is defined on the target, which outlives the menu's loop.
                unsafe {
                    entry.setAction(Some(sel!(pick:)));
                    entry.setTarget(Some(target));
                }
                entry.setTag(actions.len() as isize);
                entry.setEnabled(enabled);
                actions.push(Some(run));
                entry
            }
            Item::Submenu { title, items } => {
                let entry = new_item(&title, mtm);
                let submenu = build(items, actions, target, mtm);
                submenu.setTitle(&NSString::from_str(&title));
                entry.setSubmenu(Some(&submenu));
                entry
            }
        };
        menu.addItem(&entry);
    }
    menu
}

fn new_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    // SAFETY: initializing a menu item on the main thread.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), None, &NSString::new())
    }
}
