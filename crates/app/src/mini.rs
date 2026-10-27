//! Pinned mini chat windows: one conversation in a small window that floats above other
//! windows on the Space it was opened on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::conversation::{ChatBinding, Conversation, FindInChat, MINI_HEADER_HEIGHT};
use crate::state::{Store, StoreEvent};
use crate::theme::Theme;

const DEFAULT_SIZE: Size<Pixels> = Size { width: px(320.), height: px(440.) };
const MIN_SIZE: Size<Pixels> = Size { width: px(280.), height: px(320.) };
const MAX_SIZE: Size<Pixels> = Size { width: px(560.), height: px(1000.) };

pub struct MiniChat {
    store: Entity<Store>,
    jid: String,
    conversation: Entity<Conversation>,
    title: String,
    lightbox_focus: crate::LightboxFocus,
    _save_frame: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

/// Show `jid` in its own mini window, or bring its existing one forward.
pub fn pop_out(store: &Entity<Store>, jid: String, cx: &mut App) {
    let store = store.clone();
    // Usually called from a click: that window is leased until the handler returns.
    cx.defer(move |cx| {
        if let Some(window) = store.read(cx).popped.get(&jid).map(|p| p.window)
            && window.update(cx, |_, window, _| window.activate_window()).is_ok()
        {
            return;
        }
        open(store, jid, cx);
    });
}

/// "Open in Main Window": select the chat there and bring the main window forward
/// (reopening it if it was closed), closing the chat's mini window.
pub fn open_in_main(store: &Entity<Store>, jid: String, cx: &mut App) {
    let store = store.clone();
    cx.defer(move |cx| {
        let mini = store.read(cx).popped.get(&jid).map(|p| p.window);
        store.update(cx, |s, cx| s.select(jid, cx));
        crate::open_main_window(store, cx);
        if let Some(window) = mini {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
}

fn open(store: Entity<Store>, jid: String, cx: &mut App) {
    let name = store.read(cx).chats.iter().find(|c| c.jid == jid).map_or_else(|| jid.clone(), |c| c.name.clone());
    // The frame it had last time, if it's still on a screen; else the top right of the
    // main screen, cascading.
    let saved = load_frames().remove(&jid).filter(|b| cx.displays().iter().any(|d| d.bounds().intersects(b)));
    let bounds = saved.unwrap_or_else(|| {
        let n = store.read(cx).popped.len() as f32;
        let origin = cx.primary_display().map_or(point(px(80.), px(80.)), |d| {
            let b = d.bounds();
            point(b.origin.x + b.size.width - DEFAULT_SIZE.width - px(28. + 24. * n), b.origin.y + px(64. + 24. * n))
        });
        Bounds::new(origin, DEFAULT_SIZE)
    });
    let bounds = Bounds::new(bounds.origin, bounds.size.max(&MIN_SIZE).min(&MAX_SIZE));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(name.into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(13.), px((MINI_HEADER_HEIGHT - 14.) / 2.))),
        }),
        window_min_size: Some(MIN_SIZE),
        ..Default::default()
    };
    let (s, j) = (store.clone(), jid.clone());
    match gpui_kit::open_window(options, cx, move |window, cx| cx.new(|cx| MiniChat::new(s, j, window, cx))) {
        Ok((handle, _)) => {
            log::info!("popped out {jid}");
            store.update(cx, |s, _| s.set_popped(&jid, Some(handle)));
            let _ = handle.update(cx, |_, window, _| configure_native(window));
        }
        Err(e) => log::error!("opening a mini window failed: {e:#}"),
    }
}

/// Floating level (only on this Space: no "join all Spaces") and a size limit.
fn configure_native(window: &Window) {
    use objc2_app_kit::{NSFloatingWindowLevel, NSView};
    use objc2_foundation::NSSize;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
    // SAFETY: GPUI's live content view, on the main thread.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(ns_window) = view.window() else { return };
    ns_window.setLevel(NSFloatingWindowLevel);
    ns_window.setContentMaxSize(NSSize::new(f64::from(MAX_SIZE.width), f64::from(MAX_SIZE.height)));
}

/// `mini_windows.json` in the app data folder: each chat's last mini window frame.
fn frames_path() -> PathBuf {
    // Mock runs keep theirs apart from the real app's.
    let dir = if std::env::var_os("WA_MOCK").is_some() {
        std::env::temp_dir().join("WhatsRustyMock")
    } else {
        crate::identity::data_dir()
    };
    dir.join("mini_windows.json")
}

fn load_frames() -> HashMap<String, Bounds<Pixels>> {
    let Ok(text) = std::fs::read_to_string(frames_path()) else { return HashMap::new() };
    let raw: HashMap<String, [f32; 4]> = serde_json::from_str(&text).unwrap_or_default();
    raw.into_iter().map(|(jid, [x, y, w, h])| (jid, Bounds::new(point(px(x), px(y)), size(px(w), px(h))))).collect()
}

fn save_frame(jid: &str, b: Bounds<Pixels>) {
    let path = frames_path();
    let mut raw: HashMap<String, [f32; 4]> =
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    raw.insert(jid.to_string(), [b.origin.x.into(), b.origin.y.into(), b.size.width.into(), b.size.height.into()]);
    let written = std::fs::create_dir_all(path.parent().unwrap_or(&path))
        .and_then(|_| std::fs::write(&path, serde_json::to_string(&raw).unwrap_or_default()));
    if let Err(e) = written {
        log::warn!("saving {}: {e}", path.display());
    }
}

impl MiniChat {
    fn new(store: Entity<Store>, jid: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let conversation = cx.new(|cx| Conversation::new(store.clone(), ChatBinding::Fixed(jid.clone()), window, cx));
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, window, cx| {
                gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
            }),
            // Coming to the window reads the chat, if its newest messages are on screen.
            cx.observe_window_activation(window, |this, window, cx| {
                let store = this.store.read(cx);
                if window.is_window_active() && store.popped.get(&this.jid).is_some_and(|p| p.at_latest) {
                    store.mark_read_if_unread(&this.jid);
                }
            }),
            // Remember moves and resizes (written once it settles).
            cx.observe_window_bounds(window, |this, window, cx| {
                let (jid, bounds) = (this.jid.clone(), window.bounds());
                this._save_frame = Some(cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                    save_frame(&jid, bounds);
                }));
            }),
            cx.subscribe_in(&store, window, |this, _, ev: &StoreEvent, window, _| match ev {
                StoreEvent::AccountReset => window.remove_window(),
                StoreEvent::ChatMerged { from, to } if *from == this.jid => this.jid = to.clone(),
                _ => {}
            }),
            cx.on_release(|this, cx| {
                let (store, jid) = (this.store.clone(), this.jid.clone());
                log::info!("closed mini window for {jid}");
                cx.defer(move |cx| {
                    store.update(cx, |s, cx| {
                        s.set_popped(&jid, None);
                        if s.lightbox_in.as_deref() == Some(jid.as_str()) {
                            s.close_lightbox(cx);
                        }
                        if s.video_in.as_deref() == Some(jid.as_str()) {
                            s.close_video(cx);
                        }
                    })
                });
            }),
        ];
        // Typing can start right away.
        let composer = conversation.read(cx).composer().clone();
        crate::composer::Composer::focus(&composer, window, cx);
        // Test hook: WA_DEBUG_UI_MINI_<n> scripts the n-th mini window opened (1-based).
        static OPENED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        crate::sidebar::run_debug_ui_script(&format!("WA_DEBUG_UI_MINI_{n}"), window, cx);
        let lightbox_focus = crate::LightboxFocus(cx.focus_handle(), false);
        Self { store, jid, conversation, title: String::new(), lightbox_focus, _save_frame: None, _subs: subs }
    }
}

impl Render for MiniChat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let store = self.store.read(cx);
        if let Some(chat) = store.chats.iter().find(|c| c.jid == self.jid)
            && chat.name != self.title
        {
            self.title = chat.name.clone();
            window.set_window_title(&self.title);
        }
        let lightbox = store.lightbox.clone().filter(|_| store.lightbox_in.as_deref() == Some(self.jid.as_str()));
        let video = store.video.clone().filter(|_| store.video_in.as_deref() == Some(self.jid.as_str()));
        crate::follow_lightbox(&mut self.lightbox_focus, lightbox.is_some() || video.is_some(), &self.conversation, window, cx);
        div()
            .size_full()
            .relative()
            .font_family(".SystemUIFont")
            .text_color(theme.text)
            .on_action(|_: &crate::CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &FindInChat, window, cx| {
                this.conversation.update(cx, |c, cx| c.open_find(window, cx))
            }))
            .child(self.conversation.clone())
            .when_some(lightbox, |d, path| d.child(crate::lightbox_overlay(path, self.store.clone(), &self.lightbox_focus.0, cx)))
            .when_some(video, |d, video| {
                d.child(crate::video_overlay(video, Some(self.jid.clone()), self.store.clone(), &self.lightbox_focus.0))
            })
    }
}
