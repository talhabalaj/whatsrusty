mod audio;
mod avatar;
mod composer;
mod conversation;
mod dock;
mod download;
mod emoji_complete;
mod find_bar;
mod forward_picker;
mod format;
mod gallery;
mod glass;
mod chrome;
mod icons;
mod identity;
mod kinds;
mod links;
mod login_item;
mod media_actions;
mod media_cache;
mod mentions;
mod message_info;
mod mini;
mod new_chat;
mod mock;
mod native_menu;
mod pairing;
mod power;
mod profile;
mod recorder;
mod sidebar;
mod state;
mod text_replace;
mod theme;
mod updater;
mod video_player;

use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use wa_backend::ConnectionState;

use crate::conversation::{ChatBinding, Conversation};
use crate::sidebar::Sidebar;
use crate::state::Store;
use crate::theme::Theme;

actions!(whatsapp_gpui, [Quit, Logout, CloseWindow]);

pub const SIDEBAR_WIDTH: f32 = 320.;

struct Root {
    store: Entity<Store>,
    sidebar: Entity<Sidebar>,
    conversation: Entity<Conversation>,
    gallery: Entity<gallery::Gallery>,
    lightbox_focus: LightboxFocus,
    _subs: Vec<Subscription>,
}

impl Root {
    fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(store.clone(), window, cx));
        let conversation = cx.new(|cx| Conversation::new(store.clone(), ChatBinding::Selected, window, cx));
        let gallery = cx.new(|cx| gallery::Gallery::new(store.clone(), cx));
        // Menus, popovers and text fields from gpui-component follow light / dark mode too.
        gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, window, cx| {
                gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
            }),
            // Coming back to the window counts as reading the open conversation.
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                this.store.update(cx, |s, cx| s.window_activation_changed(active, cx));
                if active {
                    this.store.read(cx).mark_selected_read();
                    // Launch at Login may have been changed in System Settings meanwhile.
                    refresh_menus(cx);
                }
            }),
        ];
        Self { store, sidebar, conversation, gallery, lightbox_focus: LightboxFocus(cx.focus_handle(), false), _subs: subs }
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        // Text caret in the system accent blue, like native macOS text fields.
        let component_theme = gpui_kit::component::Theme::global_mut(cx);
        if component_theme.colors.caret != theme.accent {
            component_theme.colors.caret = theme.accent;
        }
        let store = self.store.read(cx);
        let body = match &store.connection {
            ConnectionState::AwaitingQr { code, .. } => {
                pairing::pairing_screen(Some(code), "The code refreshes automatically.", &theme).into_any_element()
            }
            ConnectionState::LoggedOut => {
                pairing::pairing_screen(None, "This device was unlinked. Preparing a new code…", &theme)
                    .into_any_element()
            }
            ConnectionState::Connecting if store.chats.is_empty() => {
                pairing::pairing_screen(None, "Connecting…", &theme).into_any_element()
            }
            _ => div()
                .size_full()
                .flex()
                .child(div().w(px(SIDEBAR_WIDTH)).flex_none().h_full().child(self.sidebar.clone()))
                // The gallery covers the conversation while it's open.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .relative()
                        .child(self.conversation.clone())
                        // Above the conversation's glass header and composer.
                        .when(store.gallery.is_some(), |d| {
                            let gallery = div().absolute().inset_0().child(self.gallery.clone());
                            d.child(conversation::above_glass(glass::enabled(window), "gallery", gallery))
                        }),
                )
                .into_any_element(),
        };
        let banner = match (&store.connection, store.sync_progress) {
            (ConnectionState::Disconnected { reason }, _) => Some(format!("Reconnecting… ({reason})")),
            (_, Some(p)) => Some(format!("Syncing messages… {p}%")),
            _ => None,
        };

        // A photo opened from a mini window shows there instead.
        let lightbox = store.lightbox.clone().filter(|_| store.lightbox_in.is_none());
        let video = store.video.clone().filter(|_| store.video_in.is_none());
        let gallery_open = store.gallery.is_some();
        let was_open = self.lightbox_focus.1;
        follow_lightbox(&mut self.lightbox_focus, lightbox.is_some() || video.is_some(), &self.conversation, window, cx);
        // Back to the gallery, if the photo was opened from it (so Esc closes it next).
        if was_open && !self.lightbox_focus.1 && gallery_open {
            let focus = self.gallery.read(cx).focus_handle().clone();
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
        div()
            .size_full()
            .relative()
            .font_family(".SystemUIFont")
            .text_color(theme.text)
            .on_action(cx.listener(|this, _: &Logout, _, cx| this.store.read(cx).logout()))
            // ⌘W through this window closes this one (the app-level handler is the fallback).
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &conversation::FindInChat, window, cx| {
                this.conversation.update(cx, |c, cx| c.open_find(window, cx))
            }))
            .on_action(cx.listener(|this, _: &sidebar::NewMessage, window, cx| {
                this.sidebar.update(cx, |s, cx| s.new_message(window, cx))
            }))
            .on_action(cx.listener(|this, _: &sidebar::SearchChats, window, cx| {
                this.sidebar.update(cx, |s, cx| s.search_chats(window, cx))
            }))
            .on_action(cx.listener(|this, _: &sidebar::NextChat, _, cx| this.sidebar.update(cx, |s, cx| s.step_chat(1, cx))))
            .on_action(cx.listener(|this, _: &sidebar::PreviousChat, _, cx| this.sidebar.update(cx, |s, cx| s.step_chat(-1, cx))))
            .child(body)
            .when_some(banner, |d, text| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(92.))
                        .left(px(SIDEBAR_WIDTH + 16.))
                        .px(px(10.))
                        .py(px(4.))
                        .rounded_full()
                        .bg(theme.header_bg)
                        .border_1()
                        .border_color(theme.separator)
                        .text_size(px(11.))
                        .text_color(theme.text_secondary)
                        .child(text),
                )
            })
            .when_some(lightbox, |d, path| {
                d.child(conversation::above_glass(glass::enabled(window), "lightbox", lightbox_overlay(path, self.store.clone(), &self.lightbox_focus.0, glass::enabled(window), cx)))
            })
            .when_some(video, |d, video| {
                let overlay = video_overlay(video, None, self.store.clone(), &self.lightbox_focus.0);
                d.child(conversation::above_glass(glass::enabled(window), "video", overlay))
            })
    }
}

/// The photo viewer's key focus (it takes it while open, so ←/→/Esc reach it rather than
/// the message field), and whether it was open last frame.
struct LightboxFocus(FocusHandle, bool);

/// Move key focus into the viewer when it opens, and back to the message field when it closes.
fn follow_lightbox<T: 'static>(
    focus: &mut LightboxFocus,
    open: bool,
    conversation: &Entity<Conversation>,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    if open == focus.1 {
        return;
    }
    focus.1 = open;
    let (handle, composer) = (focus.0.clone(), conversation.read(cx).composer().clone());
    window.defer(cx, move |window, cx| {
        if open {
            handle.focus(window, cx);
        } else {
            composer::Composer::focus(&composer, window, cx);
        }
    });
}

/// Full-window photo viewer: dimmed backdrop, image fitted to the window, click or
/// Esc to close, ←/→ for the chat's other photos. A bar along the top says whose photo
/// it is and when, with Forward, Save, Show in Chat and the rest (right-click too).
fn lightbox_overlay(path: std::path::PathBuf, store: Entity<Store>, focus: &FocusHandle, glass_on: bool, cx: &App) -> impl IntoElement {
    let step_button = |delta: isize| -> AnyElement {
        let store = store.clone();
        let (id, icon) = if delta < 0 { ("lightbox-prev", icons::Glyph::ChevronLeft) } else { ("lightbox-next", icons::Glyph::ChevronRight) };
        // Only the circle takes clicks (a full-height strip would sit under ✕).
        let button = div()
            .id(id)
            .absolute()
            .top_0()
            .bottom_0()
            .my_auto()
            .map(|d| if delta < 0 { d.left(px(12.)) } else { d.right(px(12.)) })
            .child(icons::glyph(icon, 16., white()))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                store.update(cx, |s, cx| s.step_lightbox(delta, cx));
            });
        if glass_on {
            glass::circle_button(id, 40., white().opacity(0.12), button).into_any_element()
        } else {
            button
                .size(px(36.))
                .rounded_full()
                .bg(white().opacity(0.15))
                .hover(|s| s.bg(white().opacity(0.25)))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .into_any_element()
        }
    };
    let s = store.read(cx);
    let (prev, next) = (s.lightbox_can_step(-1), s.lightbox_can_step(1));
    let (prev, next) = (prev.then(|| step_button(-1)), next.then(|| step_button(1)));
    let message = s.lightbox_message().cloned();
    let mini = s.lightbox_in.clone();
    let title = s.lightbox_title.clone();
    let position = message.as_ref().and(s.lightbox_position()).filter(|(_, n)| *n > 1);
    // What the menus offer: everything for a chat's photo, the file actions for a profile photo.
    let menu_items = {
        let (store, message, mini, path, title) = (store.clone(), message.clone(), mini.clone(), path.clone(), title.clone());
        move || match &message {
            Some(m) => media_actions::media_menu(m, &store, mini.clone()),
            None => media_actions::path_items(&path, &format!("{}.jpg", title.as_deref().unwrap_or("Photo")), true),
        }
    };
    let keys = store.clone();
    let close_store = store.clone();
    // A click is offered to everything under the pointer: closing stops it here.
    let close = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
        cx.stop_propagation();
        close_store.update(cx, |s, cx| s.close_lightbox(cx))
    };
    let bar_button = |id: &'static str, icon: icons::Glyph, tip: &'static str| {
        div()
            .id(id)
            .flex_none()
            .size(px(32.))
            .rounded_full()
            .hover(|s| s.bg(white().opacity(0.18)))
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
            .child(icons::glyph(icon, 15., white()))
    };
    let chat_name = |jid: &str| s.chats.iter().find(|c| c.jid == jid).map(|c| c.name.clone()).unwrap_or_default();
    let (who, when) = match (&message, &title) {
        (Some(m), _) => (
            match &m.sender_name {
                _ if m.from_me => "You".to_string(),
                Some(name) => name.clone(),
                // In a chat with one person, that person.
                None => chat_name(&m.chat_jid),
            },
            Some(format::transcript_time(m.timestamp)),
        ),
        (None, Some(name)) => (name.clone(), Some("Profile photo".to_string())),
        (None, None) => (String::new(), None),
    };
    let mut actions = div().flex().items_center().gap(px(4.));
    if let Some(m) = &message {
        if state::Store::can_forward(m) {
            let (store, m, mini) = (store.clone(), m.clone(), mini.clone());
            actions = actions.child(bar_button("lightbox-forward", icons::Glyph::Forward, "Forward…").on_click(move |_, _, cx| {
                cx.stop_propagation();
                store.update(cx, |s, cx| s.message_action(state::MessageAction::Forward(m.clone()), mini.clone(), cx))
            }));
        }
        let (save_m, save_path) = (m.clone(), path.clone());
        actions = actions.child(bar_button("lightbox-save", icons::Glyph::Download, "Save As…").on_click(move |_, _, cx| {
            cx.stop_propagation();
            media_actions::save_as(&save_m, &save_path, cx)
        }));
        let (store, m, mini) = (store.clone(), m.clone(), mini.clone());
        actions = actions.child(bar_button("lightbox-reveal", icons::Glyph::Reply, "Show in Chat").on_click(move |_, _, cx| {
            cx.stop_propagation();
            store.update(cx, |s, cx| s.message_action(state::MessageAction::Reveal(m.clone()), mini.clone(), cx))
        }));
    }
    let more_items = menu_items.clone();
    actions = actions
        .child(bar_button("lightbox-more", icons::Glyph::More, "More").on_click(move |ev, window, cx| {
            cx.stop_propagation();
            native_menu::show(more_items(), ev.position(), window, cx)
        }))
        .child(bar_button("lightbox-close", icons::Glyph::Close, "Close").on_click(close.clone()));
    let info = div()
        .min_w_0()
        .max_w(px(360.))
        .flex()
        .flex_col()
        .items_center()
        .child(div().text_size(px(13.)).line_height(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(white()).truncate().child(who))
        .children(when.map(|w| div().text_size(px(11.5)).line_height(px(14.)).text_color(white().opacity(0.7)).truncate().child(w)));
    let position = position.map(|(i, n)| {
        div().flex_none().px(px(6.)).text_size(px(12.)).text_color(white().opacity(0.7)).child(format!("{} of {n}", i + 1))
    });
    let actions = div().flex().items_center().gap(px(4.)).children(position).child(actions);
    let bar = div()
        .id("lightbox-bar")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(56.))
        // Clear of the traffic lights.
        .pl(px(84.))
        .pr(px(16.))
        .flex()
        .items_center()
        .gap(px(12.))
        // Clicks on the bar don't close the viewer.
        .on_click(|_, _, cx| cx.stop_propagation());
    // With glass: whose photo, and the buttons, in floating capsules; else a dark strip.
    // Whose photo, centered over the window; the buttons at the end.
    let centered = |info: AnyElement| div().absolute().inset_0().flex().items_center().justify_center().child(info);
    let bar = if glass_on {
        bar.child(centered(glass::glass("lightbox-info", 20., info.h(px(40.)).px(px(18.)).justify_center()).into_any_element()))
            .child(div().flex_1())
            .child(glass::glass("lightbox-actions", 20., actions.h(px(40.)).px(px(4.))))
    } else {
        bar.bg(black().opacity(0.35)).child(centered(info.into_any_element())).child(div().flex_1()).child(actions)
    };
    div()
        // Its full-size photos are freed when it closes (not kept in GPUI's global cache).
        .image_cache(retain_all("lightbox-images"))
        .id("lightbox")
        // Clicks (on ✕ too) mustn't reach what's under the viewer.
        .occlude()
        .track_focus(focus)
        .on_key_down(move |ev: &KeyDownEvent, _, cx| {
            match ev.keystroke.key.as_str() {
                "left" => keys.update(cx, |s, cx| s.step_lightbox(-1, cx)),
                "right" => keys.update(cx, |s, cx| s.step_lightbox(1, cx)),
                "escape" => keys.update(cx, |s, cx| s.close_lightbox(cx)),
                _ => return,
            }
            cx.stop_propagation();
        })
        .absolute()
        .inset_0()
        .bg(black().opacity(0.9))
        .on_click(close)
        .child(
            div()
                .id("lightbox-photo")
                .absolute()
                .top(px(64.))
                .bottom(px(24.))
                .left(px(60.))
                .right(px(60.))
                // A column the photo fills: its height is bounded, so Contain fits it.
                .flex()
                .flex_col()
                .child(img(path).flex_1().min_h_0().w_full().object_fit(ObjectFit::Contain))
                .on_mouse_down(MouseButton::Right, move |ev, window, cx| {
                    cx.stop_propagation();
                    native_menu::show(menu_items(), ev.position, window, cx)
                }),
        )
        .children(prev)
        .children(next)
        .child(bar)
        .with_animation(
            "lightbox-in",
            Animation::new(std::time::Duration::from_millis(160)).with_easing(ease_in_out),
            |d, t| d.opacity(t),
        )
}


/// The in-app video player: AVKit's player (see `video_player`) fitted into the window
/// over a dimmed backdrop. Esc, a click outside the video or ✕ closes it; Space plays and
/// pauses. `key`: which window's player (`None`: the main window).
fn video_overlay(video: state::OpenVideo, key: Option<String>, store: Entity<Store>, focus: &FocusHandle) -> impl IntoElement {
    let (keys, close_store) = (store.clone(), store.clone());
    // A click is offered to everything under the pointer: closing stops it here.
    let close = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
        cx.stop_propagation();
        close_store.update(cx, |s, cx| s.close_video(cx))
    };
    let toggle_key = key.clone();
    let (w, h) = match (video.width, video.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f32, h as f32),
        _ => (16., 9.),
    };
    div()
        .id("video")
        // Clicks (on ✕ too) mustn't reach what's under the player.
        .occlude()
        .absolute()
        .inset_0()
        .bg(black().opacity(0.88))
        .track_focus(focus)
        .on_key_down(move |ev: &KeyDownEvent, _, cx| {
            match ev.keystroke.key.as_str() {
                "escape" => keys.update(cx, |s, cx| s.close_video(cx)),
                "space" => video_player::toggle(&toggle_key),
                _ => return,
            }
            cx.stop_propagation();
        })
        .on_click(close.clone())
        .px(px(36.))
        .pt(px(56.))
        .pb(px(36.))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    // The video's aspect, as large as fits, centered.
                    let scale = (f32::from(bounds.size.width) / w).min(f32::from(bounds.size.height) / h);
                    let size = size(px(w * scale), px(h * scale));
                    let origin = bounds.origin + point((bounds.size.width - size.width) / 2., (bounds.size.height - size.height) / 2.);
                    if !video_player::place(&key, &video.path, Bounds::new(origin, size), window) {
                        window.request_animation_frame();
                    }
                },
            )
            .size_full(),
        )
        .child(
            div()
                .id("video-close")
                .absolute()
                .top(px(14.))
                .right(px(16.))
                .size(px(30.))
                .rounded_full()
                .bg(white().opacity(0.15))
                .hover(|s| s.bg(white().opacity(0.25)))
                .flex()
                .items_center()
                .justify_center()
                .child(icons::glyph(icons::Glyph::Close, 14., white()))
                .on_click(close),
        )
}

fn live_backend() -> wa_backend::BackendHandle {
    let config = wa_backend::BackendConfig { data_dir: identity::data_dir(), device_name: identity::APP_NAME.into() };
    wa_backend::spawn(config).unwrap_or_else(|e| {
        eprintln!("failed to start WhatsApp backend: {e:#}");
        std::process::exit(1);
    })
}

/// `whatsapp-gpui --logout`: unlink this device from the phone and wipe local data,
/// without opening a window. Quit the app first (both would share one session).
fn headless_logout() -> i32 {
    use std::time::{Duration, Instant};
    use wa_backend::{Command, Event};

    let backend = live_backend();
    let deadline = Instant::now() + Duration::from_secs(45);
    let wait = |until: Instant| backend.events.recv_timeout(until.saturating_duration_since(Instant::now()));
    // Wait until we're online so the server-side unlink actually reaches WhatsApp.
    loop {
        match wait(deadline) {
            Ok(Event::Connection(ConnectionState::Connected { .. })) => break,
            Ok(Event::Connection(ConnectionState::AwaitingQr { .. } | ConnectionState::LoggedOut)) => {
                eprintln!("not linked: nothing to log out");
                let _ = backend.commands.send(Command::Shutdown);
                return 0;
            }
            Ok(_) => {}
            Err(_) => {
                eprintln!("timed out connecting; wiping local data anyway (unlink it from the phone too)");
                break;
            }
        }
    }
    let _ = backend.commands.send(Command::Logout);
    let deadline = Instant::now() + Duration::from_secs(30);
    let code = loop {
        match wait(deadline) {
            Ok(Event::Connection(ConnectionState::LoggedOut | ConnectionState::AwaitingQr { .. })) => {
                eprintln!("logged out: device unlinked and local data removed");
                break 0;
            }
            Ok(_) => {}
            Err(_) => {
                eprintln!("timed out waiting for logout to finish");
                break 1;
            }
        }
    };
    let _ = backend.commands.send(Command::Shutdown);
    std::thread::sleep(Duration::from_secs(1));
    code
}

/// Log to stderr and to `~/Library/Logs/WhatsRusty/WhatsRusty.log` (the app launched
/// from Finder has no terminal). The file is restarted when it grows past 5 MB.
fn init_logging() {
    use std::io::Write;
    let dir = identity::log_dir();
    let path = dir.join(format!("{}.log", identity::APP_NAME));
    let file = std::fs::create_dir_all(&dir).ok().and_then(|_| {
        let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 << 20);
        std::fs::OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(&path).ok()
    });
    let file = std::sync::Mutex::new(file);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(move |buf, record| {
            let line = format!(
                "{} {:<5} [{}] {}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            );
            if let Ok(mut f) = file.lock()
                && let Some(f) = f.as_mut()
            {
                let _ = f.write_all(line.as_bytes());
            }
            buf.write_all(line.as_bytes())
        })
        .init();
}

/// Rebuilt when a checkmark changes (Launch at Login).
fn refresh_menus(cx: &mut App) {
    let mut items = Vec::new();
    if updater::is_available() {
        items.extend([MenuItem::action("Check for Updates…", updater::CheckForUpdates), MenuItem::separator()]);
    }
    if login_item::is_available() {
        items.extend([
            MenuItem::action("Launch at Login", login_item::ToggleLaunchAtLogin).checked(login_item::is_enabled()),
            MenuItem::separator(),
        ]);
    }
    items.extend([
        MenuItem::action("Log Out", Logout),
        MenuItem::separator(),
        MenuItem::action("Close Window", CloseWindow),
        MenuItem::action("Quit", Quit),
    ]);
    cx.set_menus(vec![Menu { name: identity::APP_NAME.into(), items, disabled: false }]);
}

fn force_appearance(mode: &str) {
    use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication};
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let name = unsafe { if mode == "dark" { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua } };
    let appearance = NSAppearance::appearanceNamed(name);
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

struct MainStore(Entity<Store>);
impl gpui_kit::Global for MainStore {}

/// Bring the main window forward, or open a new one if it was closed (⌘W / red button).
/// Mini chat windows don't count.
fn open_main_window(store: Entity<Store>, cx: &mut App) {
    if let Some(window) = store.read(cx).main_window
        && window.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        cx.activate(true);
        return;
    }
    // Test hook: WA_DEBUG_MAIN_SIZE="<w>,<h>" opens the window at that size.
    let (w, h) = std::env::var("WA_DEBUG_MAIN_SIZE")
        .ok()
        .and_then(|v| v.split_once(',').and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?))))
        .unwrap_or((1080., 720.));
    let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(identity::APP_NAME.into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(18.), px(18.))),
        }),
        window_min_size: Some(size(px(720.), px(480.))),
        ..Default::default()
    };
    let s = store.clone();
    match gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Root::new(s, window, cx))) {
        Ok((handle, _)) => store.update(cx, |s, _| s.main_window = Some(handle)),
        Err(e) => log::error!("opening the main window failed: {e:#}"),
    }
    cx.activate(true);
}

fn main() {
    // Never in mock mode: a mock run must not move the real data folders.
    if std::env::var_os("WA_MOCK").is_none() {
        identity::migrate_legacy_dirs();
    }
    init_logging();

    if std::env::args().any(|a| a == "--logout") {
        std::process::exit(headless_logout());
    }

    let backend = if std::env::var_os("WA_MOCK").is_some() { mock::spawn() } else { live_backend() };

    let app = gpui_kit::application();
    // Dock icon click / launching the app again while it runs: the window may have been closed
    // (the app keeps running in the background, like Messages), so bring it back.
    app.on_reopen(|cx| {
        if let Some(store) = cx.try_global::<MainStore>().map(|g| g.0.clone()) {
            open_main_window(store, cx);
        }
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        // Test hook: WA_DEBUG_APPEARANCE=dark|light overrides the system appearance.
        if let Ok(mode) = std::env::var("WA_DEBUG_APPEARANCE") {
            force_appearance(&mode);
        }
        audio::AudioPlayer::init(cx);
        // A prepared microphone engine, so the first voice note starts without a pause.
        recorder::warm_up();
        text_replace::TextReplacements::init(cx);
        conversation::init(cx);
        sidebar::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None), KeyBinding::new("cmd-w", CloseWindow, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &CloseWindow, cx| {
            // `active_window` can be None right after activation; then only a lone window is
            // a safe guess (never close the main window for a mini one, or vice versa).
            // Deferred: when dispatched through the window, that window is leased right now.
            let windows = cx.windows();
            if let Some(window) = cx.active_window().or_else(|| (windows.len() == 1).then(|| windows[0])) {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                });
            }
        });
        updater::init(cx);
        login_item::init(cx);
        refresh_menus(cx);

        let store = Store::create(backend.clone(), cx);
        dock::track(&store, cx);
        power::track(&store, cx);
        // A notification: Reply sends what was typed, Mark as Read reads the chat, and
        // clicking it opens the conversation.
        let notified_store = store.clone();
        cx.on_system_notification_response(move |response, cx| {
            let jid = state::notification_chat(&response.tag).to_string();
            match response.action_id.as_deref() {
                Some(state::NOTIFY_REPLY) => {
                    let text = response.text.clone().unwrap_or_default();
                    notified_store.update(cx, |s, cx| s.send_notification_reply(jid, &text, cx));
                }
                Some(state::NOTIFY_READ) => notified_store.read(cx).mark_read(&jid),
                _ => {
                    notified_store.update(cx, |s, cx| s.select(jid, cx));
                    open_main_window(notified_store.clone(), cx);
                }
            }
        });
        cx.set_global(MainStore(store.clone()));
        let quit_store = store.clone();
        cx.on_app_quit(move |cx| {
            // Let the backend flush, fail pending sends and close its databases before exit.
            let store = quit_store.read(cx);
            store.shutdown();
            if !store.wait_for_backend_stop(std::time::Duration::from_secs(3)) {
                log::warn!("backend did not stop within 3 s; quitting anyway");
            }
            async {}
        })
        .detach();

        let debug_store = store.clone();
        open_main_window(store, cx);
        // Test hooks for the viewers (no mouse needed).
        if let Some(path) = std::env::var_os("WA_DEBUG_LIGHTBOX") {
            debug_store.update(cx, |s, cx| s.open_lightbox(path.into(), cx));
        }
        if let Ok(jid) = std::env::var("WA_DEBUG_SELECT") {
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(800)).await;
                store.update(cx, |s, cx| s.select(jid, cx));
            })
            .detach();
        }
        // WA_DEBUG_POPOUT="<jid>,<jid>": pop those chats out into mini windows.
        if let Ok(list) = std::env::var("WA_DEBUG_POPOUT") {
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                for jid in list.split(',').filter(|j| !j.is_empty()) {
                    cx.background_executor().timer(std::time::Duration::from_millis(1000)).await;
                    cx.update(|cx| mini::pop_out(&store, jid.to_string(), cx));
                }
            })
            .detach();
        }
        // "chat jid/message id": open that message's Info.
        if let Ok(target) = std::env::var("WA_DEBUG_MESSAGE_INFO")
            && let Some((chat, id)) = target.split_once('/')
        {
            let (store, chat, id) = (debug_store.clone(), chat.to_string(), id.to_string());
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(1200)).await;
                store.update(cx, |s, cx| s.open_message_info(chat, id, cx));
            })
            .detach();
        }
        if let Ok(jid) = std::env::var("WA_DEBUG_PROFILE") {
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(1200)).await;
                store.update(cx, |s, cx| s.toggle_profile(jid, cx));
            })
            .detach();
        }
        // WA_DEBUG_GALLERY="<jid>" (or "<jid>|docs", "<jid>|links"): open that chat's gallery.
        if let Ok(spec) = std::env::var("WA_DEBUG_GALLERY") {
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                let (jid, tab) = spec.split_once('|').unwrap_or((&spec, "media"));
                let tab = match tab {
                    "docs" => state::GalleryTab::Docs,
                    "links" => state::GalleryTab::Links,
                    _ => state::GalleryTab::Media,
                };
                store.update(cx, |s, cx| s.open_gallery(jid.to_string(), tab, cx));
            })
            .detach();
        }
        // WA_DEBUG_PIN="<chat_jid>|<message_id>,…": pin those messages (7 days).
        // WA_DEBUG_VIEW="<chat_jid>|<message_id>": open that photo in the viewer.
        for (var, delay) in [("WA_DEBUG_PIN", 1500), ("WA_DEBUG_VIEW", 5000)] {
            let Ok(spec) = std::env::var(var) else { continue };
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(delay)).await;
                store.update(cx, |s, cx| {
                    for target in spec.split(',') {
                        let Some((chat, id)) = target.split_once('|') else { continue };
                        let Some(m) = s.messages.get(chat).and_then(|ms| ms.iter().find(|m| m.id == id)).cloned() else {
                            log::warn!("{var}: no message {id} loaded in {chat}");
                            continue;
                        };
                        if var == "WA_DEBUG_PIN" {
                            s.pin_message(&m, Some(604_800));
                        } else if let Some(path) = media_actions::file_of(&m).cloned() {
                            s.open_lightbox_in(path, Some(m.id.clone()), None, cx);
                        }
                    }
                });
            })
            .detach();
        }
        // WA_DEBUG_DOWNLOAD="<chat_jid>|<message_id>": download one message's media.
        if let Ok(spec) = std::env::var("WA_DEBUG_DOWNLOAD")
            && let Some((chat, id)) = spec.split_once('|')
        {
            let (store, chat, id) = (debug_store.clone(), chat.to_string(), id.to_string());
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                store.update(cx, |s, _| s.request_media(&chat, &id));
            })
            .detach();
        }
        // WA_DEBUG_REOPEN="<close ms>,<reopen ms>": close the window (⌘W), then bring it back.
        if let Some((close, reopen)) = std::env::var("WA_DEBUG_REOPEN").ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
        }) {
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(close)).await;
                // The main window itself (a mini window may be the key one).
                cx.update(|cx| {
                    if let Some(window) = store.read(cx).main_window {
                        let _ = window.update(cx, |_, window, _| window.remove_window());
                    }
                });
                cx.background_executor().timer(std::time::Duration::from_millis(reopen)).await;
                cx.update(|cx| open_main_window(store, cx));
            })
            .detach();
        }
        if let Some(path) = std::env::var_os("WA_DEBUG_PLAY") {
            audio::AudioPlayer::toggle(cx, "debug-play", std::path::Path::new(&path));
        }
        if let Some(path) = std::env::var_os("WA_DEBUG_VIDEO") {
            let video = state::OpenVideo { path: path.into(), width: None, height: None };
            debug_store.update(cx, |s, cx| s.open_video_in(video, None, cx));
        }
        cx.activate(true);
    });
}
