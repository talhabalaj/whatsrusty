//! The message box, laid out like Messages:
//! [+]  ( reply preview                       )
//!      ( Message text…                   (↑) )

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{InputEvent, MoveUp, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::{Path, PathBuf};

use crate::chrome::{round_button, surface};
use crate::icons::{Glyph, glyph};
use crate::state::{FocusComposer, Store, StoreEvent, body_text};
use crate::theme::Theme;

/// Height of the + button and of the single-line message field.
const COMPOSER_HEIGHT: f32 = 34.;
/// The send / sticker button in the field's end cap, and its gap to the field's edge.
const SEND_SIZE: f32 = 28.;
const SEND_INSET: f32 = 3.;
const THUMB: f32 = 64.;
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "heic", "heif", "webp", "gif", "tiff", "tif", "bmp"];

/// What a chat's composer holds while you switch away.
#[derive(Default)]
struct Draft {
    text: String,
    attachments: Vec<PathBuf>,
}

pub struct Composer {
    store: Entity<Store>,
    input: Entity<TextareaState>,
    typing_sent: bool,
    /// The chat the input currently belongs to; drafts are kept per chat so text typed in
    /// one conversation can never be sent to another.
    chat: Option<String>,
    drafts: std::collections::HashMap<String, Draft>,
    /// Photos waiting to be sent with the next message (pasted, or chosen with +).
    attachments: Vec<PathBuf>,
    /// `:shortcode` suggestions for the word being typed, and the highlighted one.
    suggestions: Vec<crate::emoji_complete::Suggestion>,
    suggestion_ix: usize,
    sticker_panel: bool,
    /// The message being edited (↑ or the tray's Edit); Enter saves the edit.
    editing: Option<wa_backend::ChatMessage>,
    /// Skip the Change event caused by our own text replacement.
    replacing: bool,
    _subs: Vec<Subscription>,
}

impl Composer {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx).auto_grow(1, 8).placeholder("WhatsApp").submit_on_enter(true)
        });
        let subs = vec![
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.sync_chat(window, cx);
                cx.notify();
            }),
            // A chat was just picked in New Message.
            cx.subscribe_in(&store, window, |this, _, _: &FocusComposer, window, cx| {
                this.input.update(cx, |s, cx| s.focus(window, cx));
            }),
            cx.subscribe_in(&store, window, |this, _, ev: &StoreEvent, window, cx| {
                if matches!(ev, StoreEvent::AccountReset) {
                    this.drafts.clear();
                    this.attachments.clear();
                    this.chat = None;
                    this.typing_sent = false;
                    this.input.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }),
            cx.subscribe_in(&input, window, |this, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift: false, .. } if !this.suggestions.is_empty() => {
                    this.accept_suggestion(this.suggestion_ix, window, cx)
                }
                InputEvent::PressEnter { shift: false, .. } => this.submit(state.clone(), window, cx),
                InputEvent::Change if std::mem::take(&mut this.replacing) => {}
                InputEvent::Change => {
                    this.apply_text_replacement(window, cx);
                    this.update_suggestions(window, cx);
                    let has_text = !state.read(cx).value().trim().is_empty();
                    if has_text != this.typing_sent {
                        this.typing_sent = has_text;
                        this.store.read(cx).set_typing(has_text);
                    }
                    cx.notify();
                }
                _ => {}
            }),
        ];
        let chat = store.read(cx).selected.clone();
        // Test hooks: pre-filled `:query` suggestions / open sticker panel.
        let debug_text = std::env::var("WA_DEBUG_COMPOSER_TEXT").ok();
        let suggestions = debug_text
            .as_deref()
            .and_then(crate::emoji_complete::trailing_query)
            .map(|q| crate::emoji_complete::suggest(q, 8))
            .unwrap_or_default();
        if let Some(t) = debug_text {
            input.update(cx, |st, cx| st.set_value(t, window, cx));
        }
        // Test hook: type ":jo" through real keystrokes (the IME/input path).
        if std::env::var_os("WA_DEBUG_TYPE").is_some() {
            let input = input.clone();
            cx.spawn_in(window, async move |_, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(2500)).await;
                let _ = cx.update(|window, cx| input.update(cx, |s, cx| s.focus(window, cx)));
                // WA_DEBUG_TYPE="h i enter up" types those keys (one per tick, like real
                // typing); "1" types ":jo".
                let spec = std::env::var("WA_DEBUG_TYPE").unwrap_or_default();
                let keys: Vec<String> =
                    if spec == "1" { vec![":".into(), "j".into(), "o".into()] } else { spec.split(' ').map(String::from).collect() };
                for k in keys {
                    let k: &'static str = Box::leak(k.into_boxed_str());
                    let _ = cx.update(|window, cx| match Keystroke::parse(k) {
                        Ok(ks) => {
                            let handled = window.dispatch_keystroke(ks, cx);
                            log::info!("debug key {k:?} handled={handled}");
                        }
                        Err(e) => log::info!("debug key {k:?} invalid: {e}"),
                    });
                    cx.background_executor().timer(std::time::Duration::from_millis(120)).await;
                }
                let _ = cx.update(|_, cx| log::info!("debug composer value now {:?}", input.read(cx).value()));
            })
            .detach();
        }
        let sticker_panel = std::env::var_os("WA_DEBUG_STICKERS").is_some();
        if sticker_panel {
            store.read(cx).request_stickers();
        }
        Self { store, input, typing_sent: false, chat, drafts: Default::default(), attachments: Vec::new(), suggestions, suggestion_ix: 0, sticker_panel, editing: None, replacing: false, _subs: subs }
    }

    /// Replace the field's text and put the cursor at the end (set_value leaves it at the
    /// start, which is wrong for edits, replacements and restored drafts).
    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let len = text.len();
        self.input.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.set_selected_range(len..len, cx);
        });
    }

    /// Selection changed: stash the draft (and stop "typing…") for the old chat, restore
    /// the new chat's draft.
    fn sync_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected.clone();
        if selected == self.chat {
            return;
        }
        if self.editing.take().is_some() {
            self.input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        let text = self.input.read(cx).value().to_string();
        if let Some(old) = self.chat.take() {
            if self.typing_sent {
                self.store.read(cx).set_typing_in(&old, false);
            }
            let attachments = std::mem::take(&mut self.attachments);
            if text.trim().is_empty() && attachments.is_empty() {
                self.drafts.remove(&old);
            } else {
                self.drafts.insert(old, Draft { text, attachments });
            }
        }
        self.typing_sent = false;
        let restored = selected.as_ref().and_then(|c| self.drafts.remove(c)).unwrap_or_default();
        self.attachments = restored.attachments;
        self.set_text(restored.text, window, cx);
        self.chat = selected;
    }

    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = this.read(cx).input.clone();
        input.update(cx, |s, cx| s.focus(window, cx));
    }

    fn submit(&mut self, state: Entity<TextareaState>, window: &mut Window, cx: &mut Context<Self>) {
        let text = state.read(cx).value().to_string();
        if text.trim().is_empty() && self.attachments.is_empty() {
            return;
        }
        // Belt and braces: never send text that belongs to a different chat.
        if self.store.read(cx).selected != self.chat {
            self.sync_chat(window, cx);
            return;
        }
        if let Some(message) = self.editing.take() {
            state.update(cx, |s, cx| s.set_value("", window, cx));
            self.store.update(cx, |s, cx| s.edit_message(&message, text, cx));
            cx.notify();
            return;
        }
        state.update(cx, |s, cx| s.set_value("", window, cx));
        self.typing_sent = false;
        let attachments = std::mem::take(&mut self.attachments);
        self.store.update(cx, |s, cx| {
            s.set_typing(false);
            let text = text.trim().to_string();
            match attachments.as_slice() {
                // One photo + text: the text becomes its caption, like WhatsApp.
                [one] if !text.is_empty() => s.send_image(one.clone(), Some(text), cx),
                _ => {
                    for path in attachments {
                        s.send_image(path, None, cx);
                    }
                    if !text.is_empty() {
                        s.send_text(text, cx);
                    }
                }
            }
        });
        cx.notify();
    }

    /// macOS Text Replacements: expand a shortcut once a space/punctuation follows it.
    fn apply_text_replacement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if let Some(expanded) = crate::text_replace::TextReplacements::expand(cx, &text) {
            self.replacing = true;
            self.set_text(expanded, window, cx);
        }
    }

    /// Start editing `message` from outside (Edit in the tapback tray).
    pub fn start_edit(this: &Entity<Self>, message: wa_backend::ChatMessage, window: &mut Window, cx: &mut App) {
        this.update(cx, |c, cx| c.begin_edit(message, window, cx));
    }

    fn begin_edit(&mut self, message: wa_backend::ChatMessage, window: &mut Window, cx: &mut Context<Self>) {
        let text = Store::editable_text(&message);
        self.editing = Some(message);
        self.attachments.clear();
        self.suggestions.clear();
        self.store.update(cx, |s, cx| s.set_replying_to(None, cx));
        self.set_text(text, window, cx);
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.input.update(cx, |s, cx| s.set_value("", window, cx));
            cx.notify();
        }
    }

    /// Refresh `:shortcode` suggestions from the text; a completed `:name:` is replaced
    /// by its emoji straight away.
    fn update_suggestions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if let Some(expanded) = crate::emoji_complete::expand_complete(&text) {
            self.suggestions.clear();
            self.set_text(expanded, window, cx);
            return;
        }
        let next = crate::emoji_complete::trailing_query(&text)
            .map(|q| crate::emoji_complete::suggest(q, 8))
            .unwrap_or_default();
        if next.iter().map(|s| s.shortcode).ne(self.suggestions.iter().map(|s| s.shortcode)) {
            self.suggestion_ix = 0;
        }
        self.suggestions = next;
        cx.notify();
    }

    fn accept_suggestion(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.suggestions.get(ix).cloned() else { return };
        let text = self.input.read(cx).value().to_string();
        let replaced = crate::emoji_complete::apply(&text, s.emoji);
        self.suggestions.clear();
        self.set_text(replaced, window, cx);
        cx.notify();
    }

    /// ↑: move through suggestions, or edit my last message when the field is empty;
    /// otherwise let the text field move the cursor.
    fn on_move_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.suggestions.is_empty() {
            let n = self.suggestions.len();
            self.suggestion_ix = (self.suggestion_ix + n - 1) % n;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let empty = self.input.read(cx).value().is_empty() && self.attachments.is_empty();
        if empty
            && self.editing.is_none()
            && let Some(m) = self.store.read(cx).last_editable()
        {
            self.begin_edit(m, window, cx);
            cx.stop_propagation();
            return;
        }
        cx.propagate();
    }

    /// Arrow keys / Tab / Esc steer the suggestion list while it's open.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.suggestions.is_empty() && ev.keystroke.key == "escape" && self.editing.is_some() {
            self.cancel_edit(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.suggestions.is_empty() {
            if ev.keystroke.key == "escape" && self.sticker_panel {
                self.sticker_panel = false;
                cx.notify();
                cx.stop_propagation();
            }
            return;
        }
        let n = self.suggestions.len();
        match ev.keystroke.key.as_str() {
            "down" => self.suggestion_ix = (self.suggestion_ix + 1) % n,
            "up" => self.suggestion_ix = (self.suggestion_ix + n - 1) % n,
            "tab" => self.accept_suggestion(self.suggestion_ix, window, cx),
            "escape" => self.suggestions.clear(),
            _ => return,
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn toggle_sticker_panel(&mut self, cx: &mut Context<Self>) {
        self.sticker_panel = !self.sticker_panel;
        if self.sticker_panel {
            self.store.read(cx).request_stickers();
        }
        cx.notify();
    }

    fn render_suggestions(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        crate::chrome::surface(div(), theme)
            .absolute()
            .bottom_full()
            .left(px(56.))
            .mb(px(4.))
            .w(px(280.))
            .p(px(4.))
            .rounded(px(10.))
            .shadow_md()
            .flex()
            .flex_col()
            .children(self.suggestions.iter().enumerate().map(|(i, s)| {
                let selected = i == self.suggestion_ix;
                div()
                    .id(("emoji-suggestion", i))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .when(selected, |d| d.bg(theme.accent))
                    .hover(|d| d.bg(theme.search_bg))
                    .child(div().text_size(px(18.)).child(s.emoji))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(if selected { white() } else { theme.text })
                            .child(format!(":{}:", s.shortcode)),
                    )
                    .when_some(s.keyword, |d, kw| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(11.5))
                                .text_color(if selected { white().opacity(0.8) } else { theme.text_secondary })
                                .child(kw.to_string()),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.accept_suggestion(i, window, cx)))
            }))
    }

    fn render_sticker_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let stickers = self.store.read(cx).stickers.clone();
        let section = |title: &str, items: Vec<wa_backend::StickerRef>, cx: &mut Context<Self>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_size(px(11.)).text_color(theme.text_secondary).child(title.to_string()))
                .child(div().flex().flex_wrap().gap(px(6.)).children(items.into_iter().map(|st| {
                    let id = st.id.clone();
                    div()
                        .id(SharedString::from(format!("sticker-pick-{}", st.id)))
                        .size(px(68.))
                        .rounded(px(10.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|d| d.bg(theme.search_bg))
                        .cursor_pointer()
                        .map(|d| match &st.preview {
                            Some(p) => d.child(
                                img(p.clone())
                                    .id(SharedString::from(format!("sticker-preview-{}", st.id)))
                                    .size(px(60.))
                                    .object_fit(ObjectFit::Contain),
                            ),
                            None => d.child(div().size(px(52.)).rounded(px(10.)).bg(theme.search_bg)),
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = id.clone();
                            this.store.update(cx, |s, cx| s.send_sticker(id, cx));
                            this.sticker_panel = false;
                            cx.notify();
                        }))
                })))
        };
        let favorites: Vec<_> = stickers.iter().filter(|s| s.source == wa_backend::StickerSource::Favorite).cloned().collect();
        let recents: Vec<_> = stickers.iter().filter(|s| s.source == wa_backend::StickerSource::Recent).cloned().collect();
        crate::chrome::surface(div(), theme)
            .id("sticker-panel")
            .absolute()
            .bottom_full()
            .right(px(14.))
            .mb(px(4.))
            .w(px(340.))
            .max_h(px(360.))
            .overflow_y_scroll()
            .p(px(12.))
            .rounded(px(12.))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(12.))
            .when(stickers.is_empty(), |d| {
                d.child(
                    div()
                        .py(px(24.))
                        .text_center()
                        .text_size(px(12.5))
                        .text_color(theme.text_secondary)
                        .child("No stickers yet. Star stickers on your phone, or send some, and they'll show up here."),
                )
            })
            .when(!favorites.is_empty(), |d| d.child(section("Favorites", favorites, cx)))
            .when(!recents.is_empty(), |d| d.child(section("Recent", recents, cx)))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.sticker_panel = false;
                cx.notify();
            }))
    }

    fn add_attachments(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for p in paths {
            if !self.attachments.contains(&p) {
                self.attachments.push(p);
            }
        }
        cx.notify();
    }

    /// Paste handler: images on the clipboard (screenshots, copied images) and image
    /// files copied in Finder become attachments. Anything else pastes as text.
    fn paste(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let mut files = Vec::new();
        let mut images = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => images.push((image.format(), image.bytes().to_vec())),
                ClipboardEntry::ExternalPaths(paths) => files.extend(paths.paths().iter().filter(|p| is_image(p)).cloned()),
                ClipboardEntry::String(_) => {}
            }
        }
        if files.is_empty() && images.is_empty() {
            return false;
        }
        self.add_attachments(files, cx);
        if !images.is_empty() {
            // Writing (and converting TIFF/BMP) happens off the UI thread.
            cx.spawn(async move |this, cx| {
                let paths = cx.background_executor().spawn(async move { save_pasted(images) }).await;
                let _ = this.update(cx, |this, cx| this.add_attachments(paths, cx));
            })
            .detach();
        }
        true
    }

    fn pick_photo(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let paths = paths.into_iter().filter(|p| is_image(p)).collect();
            let _ = this.update(cx, |this, cx| this.add_attachments(paths, cx));
        })
        .detach();
    }
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Write pasted image data to private temp files the backend can send. Formats the
/// backend can't read (TIFF, BMP: what some apps put on the clipboard) go through `sips`.
fn save_pasted(images: Vec<(ImageFormat, Vec<u8>)>) -> Vec<PathBuf> {
    let dir = std::env::temp_dir().join("whatsapp-gpui-paste");
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    images
        .into_iter()
        .filter_map(|(format, bytes)| {
            let id = wa_backend::new_message_id();
            let (ext, convert) = match format {
                ImageFormat::Png => ("png", false),
                ImageFormat::Jpeg => ("jpg", false),
                ImageFormat::Webp => ("webp", false),
                ImageFormat::Gif => ("gif", false),
                ImageFormat::Tiff => ("tiff", true),
                ImageFormat::Bmp => ("bmp", true),
                _ => return None,
            };
            let path = dir.join(format!("{id}.{ext}"));
            std::fs::write(&path, bytes).ok()?;
            if !convert {
                return Some(path);
            }
            let png = dir.join(format!("{id}.png"));
            let ok = std::process::Command::new("/usr/bin/sips")
                .args(["-s", "format", "png"])
                .arg(&path)
                .arg("--out")
                .arg(&png)
                .output()
                .is_ok_and(|o| o.status.success());
            let _ = std::fs::remove_file(&path);
            ok.then_some(png)
        })
        .collect()
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let has_text = !self.input.read(cx).value().trim().is_empty() || !self.attachments.is_empty();
        let replying = self.store.read(cx).replying_to.clone();
        let attachments = self.attachments.clone();
        let weak = cx.entity().downgrade();

        let suggestions_open = !self.suggestions.is_empty();
        let sticker_panel = self.sticker_panel;
        div()
            .relative()
            .w_full()
            .bg(theme.window_bg)
            .border_t_1()
            .border_color(theme.separator)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| this.on_key(ev, window, cx)))
            // ↑ is bound to the input's MoveUp action, which runs before key-down listeners.
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| this.on_move_up(window, cx)))
            .when(suggestions_open, |d| d.child(self.render_suggestions(&theme, cx)))
            .when(sticker_panel, |d| d.child(self.render_sticker_panel(&theme, cx)))
            .flex()
            .items_end()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .child(
                round_button("attach", COMPOSER_HEIGHT, &theme)
                    .child(glyph(Glyph::Plus, 18., theme.text_secondary))
                    .on_click(cx.listener(|this, _, _, cx| this.pick_photo(cx))),
            )
            .child(
                surface(div().flex_1().min_w_0().flex().flex_col().rounded(px(17.)), &theme)
                    .when_some(self.editing.clone(), |d, m| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .pl(px(12.))
                                .pr(px(6.))
                                .pt(px(7.))
                                .pb(px(5.))
                                .border_b_1()
                                .border_color(theme.separator)
                                .child(glyph(Glyph::Pencil, 13., theme.accent))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(theme.accent)
                                                .child("Edit message"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(theme.text_secondary)
                                                .truncate()
                                                .child(Store::editable_text(&m)),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("cancel-edit")
                                        .size(px(22.))
                                        .rounded_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .hover(|s| s.bg(theme.search_bg))
                                        .child(glyph(Glyph::Close, 11., theme.text_secondary))
                                        .on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx))),
                                ),
                        )
                    })
                    .when_some(replying, |d, m| {
                        let who = if m.from_me {
                            "yourself".to_string()
                        } else {
                            m.sender_name.clone().unwrap_or_else(|| "message".into())
                        };
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .pl(px(12.))
                                .pr(px(6.))
                                .pt(px(7.))
                                .pb(px(5.))
                                .border_b_1()
                                .border_color(theme.separator)
                                .child(glyph(Glyph::Reply, 13., theme.accent))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(theme.accent)
                                                .child(format!("Replying to {who}")),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(theme.text_secondary)
                                                .truncate()
                                                .child(body_text(&m.body).to_string()),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("cancel-reply")
                                        .size(px(22.))
                                        .rounded_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .hover(|s| s.bg(theme.search_bg))
                                        .child(glyph(Glyph::Close, 11., theme.text_secondary))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.store.update(cx, |s, cx| s.set_replying_to(None, cx));
                                        })),
                                ),
                        )
                    })
                    .when(!attachments.is_empty(), |d| {
                        d.child(
                            div().flex().flex_wrap().gap(px(8.)).px(px(10.)).pt(px(10.)).pb(px(2.)).children(
                                attachments.iter().enumerate().map(|(i, path)| {
                                    let path_for_remove = path.clone();
                                    div()
                                        .relative()
                                        .size(px(THUMB))
                                        .child(
                                            img(path.clone())
                                                .size_full()
                                                .rounded(px(10.))
                                                .object_fit(ObjectFit::Cover),
                                        )
                                        .child(
                                            div()
                                                .id(("remove-attachment", i))
                                                .absolute()
                                                .top(px(-6.))
                                                .right(px(-6.))
                                                .size(px(20.))
                                                .rounded_full()
                                                .bg(theme.text_secondary)
                                                .border_2()
                                                .border_color(theme.surface_bg)
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(glyph(Glyph::Close, 9., white()))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.attachments.retain(|p| p != &path_for_remove);
                                                    cx.notify();
                                                })),
                                        )
                                }),
                            ),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .flex()
                            .items_end()
                            // Border (2px) + this = COMPOSER_HEIGHT on one line, matching the + button.
                            .min_h(px(COMPOSER_HEIGHT - 2.))
                            .pl(px(14.))
                            // Room for the round button that sits in the field's end cap.
                            .pr(px(SEND_SIZE + 2. * SEND_INSET))
                            // The text area draws its line ~2.5pt below its box's center, so the
                            // padding is asymmetric to optically center one line in the 34pt field
                            // (measured from screenshots). Extra lines grow the field downward.
                            .pt(px(1.5))
                            .pb(px(6.5))
                            .text_size(px(14.))
                            .child(div().flex_1().min_w_0().child(Textarea::new(&self.input)
                                .on_paste(move |item, _, cx| {
                                    weak.update(cx, |this, cx| this.paste(item, cx)).unwrap_or(false)
                                })
                                .appearance(false)
                                .xsmall().px_0().py_0().text_size(px(14.)).line_height(px(20.))))
                            // One slot at the field's right edge: stickers while empty, send once
                            // there's something to send (no layout shift, icon stays at the edge).
                            .when(!has_text, |d| d.child(
                                div()
                                    .id("stickers")
                                    .absolute()
                                    .right(px(SEND_INSET - 1.))
                                    .bottom(px(SEND_INSET - 1.))
                                    .size(px(SEND_SIZE))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme.search_bg))
                                    .child(glyph(Glyph::Sticker, 19., if sticker_panel { theme.accent } else { theme.text_secondary }))
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sticker_panel(cx))),
                            ))
                            .when(has_text, |d| d.child(
                                div()
                                    .id("send")
                                    // Concentric with the pill's rounded end: a 28pt circle inset 3pt
                                    // on every side of a 34pt field (1pt of that is the border).
                                    .absolute()
                                    .right(px(SEND_INSET - 1.))
                                    .bottom(px(SEND_INSET - 1.))
                                    .size(px(SEND_SIZE))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(theme.accent)
                                    .child(glyph(Glyph::ArrowUp, 18., white()))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let state = this.input.clone();
                                        this.submit(state, window, cx);
                                    }))
                                    // Pop in when there's something to send, like Messages.
                                    // Fade only: animating margin/size would shift the field's layout.
                                    .with_animation(
                                        if has_text { "send-in" } else { "send-out" },
                                        Animation::new(std::time::Duration::from_millis(160)).with_easing(ease_out_quint()),
                                        move |d, t| d.opacity(if has_text { t } else { 1. }),
                                    ),)
                            ),
                    ),
            )
    }
}
