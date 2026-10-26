mod audio;
mod avatar;
mod composer;
mod conversation;
mod emoji_complete;
mod format;
mod chrome;
mod icons;
mod kinds;
mod mock;
mod pairing;
mod profile;
mod sidebar;
mod state;
mod text_replace;
mod theme;
mod video_player;

use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use wa_backend::ConnectionState;

use crate::conversation::Conversation;
use crate::sidebar::Sidebar;
use crate::state::Store;
use crate::theme::Theme;

actions!(whatsapp_gpui, [Quit, Logout, CloseWindow]);

const SIDEBAR_WIDTH: f32 = 320.;

struct Root {
    store: Entity<Store>,
    sidebar: Entity<Sidebar>,
    conversation: Entity<Conversation>,
    _subs: Vec<Subscription>,
}

impl Root {
    fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(store.clone(), window, cx));
        let conversation = cx.new(|cx| Conversation::new(store.clone(), window, cx));
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            // Coming back to the window counts as reading the open conversation.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.store.read(cx).mark_selected_read();
                }
            }),
        ];
        Self { store, sidebar, conversation, _subs: subs }
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
                .child(div().flex_1().min_w_0().h_full().child(self.conversation.clone()))
                .into_any_element(),
        };
        let banner = match (&store.connection, store.sync_progress) {
            (ConnectionState::Disconnected { reason }, _) => Some(format!("Reconnecting… ({reason})")),
            (_, Some(p)) => Some(format!("Syncing messages… {p}%")),
            _ => None,
        };

        let lightbox = store.lightbox.clone();
        div()
            .size_full()
            .relative()
            .font_family(".SystemUIFont")
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && this.store.read(cx).lightbox.is_some() {
                    this.store.update(cx, |s, cx| s.close_lightbox(cx));
                    cx.stop_propagation();
                }
            }))
            .text_color(theme.text)
            .on_action(cx.listener(|this, _: &Logout, _, cx| this.store.read(cx).logout()))
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
            .when_some(lightbox, |d, path| d.child(lightbox_overlay(path, self.store.clone())))
    }
}

/// Full-window photo viewer: dimmed backdrop, image fitted to the window, click or
/// Esc to close.
fn lightbox_overlay(path: std::path::PathBuf, store: Entity<Store>) -> impl IntoElement {
    let close = move |_: &ClickEvent, _: &mut Window, cx: &mut App| store.update(cx, |s, cx| s.close_lightbox(cx));
    div()
        .id("lightbox")
        .absolute()
        .inset_0()
        .bg(black().opacity(0.88))
        .flex()
        .items_center()
        .justify_center()
        .p(px(36.))
        .on_click(close.clone())
        .child(img(path).size_full().object_fit(ObjectFit::Contain))
        .child(
            div()
                .id("lightbox-close")
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
        .with_animation(
            "lightbox-in",
            Animation::new(std::time::Duration::from_millis(160)).with_easing(ease_in_out),
            |d, t| d.opacity(t),
        )
}

fn data_dir() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("WhatsApp GPUI")
}

fn live_backend() -> wa_backend::BackendHandle {
    let config = wa_backend::BackendConfig { data_dir: data_dir(), device_name: "WhatsApp GPUI".into() };
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

/// Log to stderr and to `~/Library/Logs/WhatsApp GPUI/whatsapp-gpui.log` (the app launched
/// from Finder has no terminal). The file is restarted when it grows past 5 MB.
fn init_logging() {
    use std::io::Write;
    let dir = dirs::home_dir().unwrap_or_else(std::env::temp_dir).join("Library/Logs/WhatsApp GPUI");
    let path = dir.join("whatsapp-gpui.log");
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

struct MainStore(Entity<Store>);
impl gpui_kit::Global for MainStore {}

/// Bring the main window forward, or open a new one if it was closed (⌘W / red button).
fn open_main_window(store: Entity<Store>, cx: &mut App) {
    if let Some(window) = cx.windows().into_iter().next() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
        cx.activate(true);
        return;
    }
    let bounds = Bounds::centered(None, size(px(1080.), px(720.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("WhatsApp".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(18.), px(18.))),
        }),
        window_min_size: Some(size(px(720.), px(480.))),
        ..Default::default()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Root::new(store, window, cx))) {
        log::error!("opening the main window failed: {e:#}");
    }
    cx.activate(true);
}

fn main() {
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
        audio::AudioPlayer::init(cx);
        text_replace::TextReplacements::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None), KeyBinding::new("cmd-w", CloseWindow, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &CloseWindow, cx| {
            // `active_window` can be None right after activation; there's only one GPUI window.
            // Deferred: when dispatched through the window, that window is leased right now.
            if let Some(window) = cx.active_window().or_else(|| cx.windows().into_iter().next()) {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                });
            }
        });
        cx.set_menus(vec![Menu {
            name: "WhatsApp GPUI".into(),
            items: vec![
                MenuItem::action("Log Out", Logout),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
                MenuItem::action("Quit", Quit),
            ],
            disabled: false,
        }]);

        let store = Store::create(backend.clone(), cx);
        // Clicking a notification opens that conversation.
        let notified_store = store.clone();
        cx.on_system_notification_response(move |response, cx| {
            let jid = response.tag.to_string();
            notified_store.update(cx, |s, cx| s.select(jid, cx));
            open_main_window(notified_store.clone(), cx);
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
        if let Ok(jid) = std::env::var("WA_DEBUG_PROFILE") {
            let store = debug_store.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(1200)).await;
                store.update(cx, |s, cx| s.toggle_profile(jid, cx));
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
        if let Some(path) = std::env::var_os("WA_DEBUG_PLAY") {
            audio::AudioPlayer::toggle(cx, "debug-play", std::path::Path::new(&path));
        }
        if let Some(path) = std::env::var_os("WA_DEBUG_VIDEO") {
            video_player::play(std::path::Path::new(&path), "Test video", None, None);
        }
        cx.activate(true);
    });
}
