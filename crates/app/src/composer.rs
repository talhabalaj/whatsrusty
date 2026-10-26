//! The message box, laid out like Messages:
//! [+]  ( reply preview                       )
//!      ( attachments                         )
//!      ( Message text…                   (↑) )

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Enter, IndentInline, InputEvent, MoveUp, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::chrome::{round_button, surface};
use crate::icons::{Glyph, glyph};
use crate::recorder::{MicAccess, Recorder};
use crate::state::{Draft, FocusComposer, Store, StoreEvent, body_text};
use crate::theme::Theme;

/// Height of the + button and of the single-line message field.
const COMPOSER_HEIGHT: f32 = 34.;
/// The send / sticker button in the field's end cap, and its gap to the field's edge.
const SEND_SIZE: f32 = 28.;
const SEND_INSET: f32 = 3.;
const THUMB: f32 = 64.;
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "heic", "heif", "webp", "gif", "tiff", "tif", "bmp"];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "m4v"];
/// Level-meter bars shown while recording.
const LEVEL_BARS: usize = 56;

pub struct Composer {
    store: Entity<Store>,
    input: Entity<TextareaState>,
    typing_sent: bool,
    /// The chat the input currently belongs to; drafts are kept per chat (in the store)
    /// so text typed in one conversation can never be sent to another.
    chat: Option<String>,
    /// Files waiting to be sent with the next message (pasted, dropped or chosen with +).
    attachments: Vec<PathBuf>,
    /// `@Name`s inserted from the mention list: (name, JID).
    mentions: Vec<(String, String)>,
    /// `:shortcode` suggestions for the word being typed, and the highlighted one.
    suggestions: Vec<crate::emoji_complete::Suggestion>,
    suggestion_ix: usize,
    /// Group members matching the `@query` being typed, and the highlighted one.
    mention_list: Vec<wa_backend::Participant>,
    mention_ix: usize,
    sticker_panel: bool,
    /// The message being edited (↑ or the tray's Edit); Enter saves the edit.
    editing: Option<wa_backend::ChatMessage>,
    /// A voice note being recorded.
    recording: Option<Recorder>,
    /// Why recording isn't possible (microphone access off, no microphone).
    mic_notice: Option<String>,
    /// Keyboard focus while recording (the text field is gone), so Esc / Enter work;
    /// `move_focus` hands it over (to the recording row, or back to the field) on the
    /// next render, where a window is at hand.
    recording_focus: FocusHandle,
    move_focus: bool,
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
                // Group members may have just arrived for an open `@` list.
                this.update_mentions(false, cx);
                cx.notify();
            }),
            // A chat was just picked in New Message.
            cx.subscribe_in(&store, window, |this, _, _: &FocusComposer, window, cx| {
                this.input.update(cx, |s, cx| s.focus(window, cx));
            }),
            cx.subscribe_in(&store, window, |this, _, ev: &StoreEvent, window, cx| {
                if matches!(ev, StoreEvent::AccountReset) {
                    this.attachments.clear();
                    this.mentions.clear();
                    this.recording = None;
                    this.chat = None;
                    this.typing_sent = false;
                    this.input.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }),
            cx.subscribe_in(&input, window, |this, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift: false, .. } if !this.mention_list.is_empty() => {
                    this.accept_mention(this.mention_ix, window, cx)
                }
                InputEvent::PressEnter { shift: false, .. } if !this.suggestions.is_empty() => {
                    this.accept_suggestion(this.suggestion_ix, window, cx)
                }
                InputEvent::PressEnter { shift: false, .. } => this.submit(state.clone(), window, cx),
                InputEvent::Change if std::mem::take(&mut this.replacing) => this.save_draft(cx),
                InputEvent::Change => {
                    this.apply_text_replacement(window, cx);
                    this.update_suggestions(window, cx);
                    this.update_mentions(true, cx);
                    let has_text = !state.read(cx).value().trim().is_empty();
                    if has_text != this.typing_sent {
                        this.typing_sent = has_text;
                        this.store.read(cx).set_typing(has_text);
                    }
                    this.save_draft(cx);
                    cx.notify();
                }
                _ => {}
            }),
        ];
        let chat = store.read(cx).selected.clone();
        // Reopened window: pick up where the last one left off.
        let draft = chat.as_ref().and_then(|c| store.read(cx).drafts.get(c).cloned()).unwrap_or_default();
        // Test hooks: pre-filled `:query` suggestions / open sticker panel.
        let debug_text = std::env::var("WA_DEBUG_COMPOSER_TEXT").ok();
        let suggestions = debug_text
            .as_deref()
            .and_then(crate::emoji_complete::trailing_query)
            .map(|q| crate::emoji_complete::suggest(q, 8))
            .unwrap_or_default();
        let text = debug_text.unwrap_or(draft.text);
        if !text.is_empty() {
            let len = text.len();
            input.update(cx, |st, cx| {
                st.set_value(text, window, cx);
                st.set_selected_range(len..len, cx);
            });
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
        // Test hook: WA_DEBUG_ATTACH="a.png|b.pdf" attaches those (once a chat is open).
        if let Ok(list) = std::env::var("WA_DEBUG_ATTACH") {
            let paths: Vec<PathBuf> = list.split('|').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(2000)).await;
                let _ = this.update(cx, |this, cx| this.add_attachments(paths, cx));
            })
            .detach();
        }
        // Test hook: WA_DEBUG_RECORD=<ms> starts recording, and sends after that long (0: never).
        if let Ok(ms) = std::env::var("WA_DEBUG_RECORD") {
            let ms: u64 = ms.parse().unwrap_or(0);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(2500)).await;
                let _ = this.update(cx, |this, cx| this.start_recording(cx));
                if ms > 0 {
                    cx.background_executor().timer(Duration::from_millis(ms)).await;
                    let _ = this.update(cx, |this, cx| this.send_recording(cx));
                }
            })
            .detach();
        }
        Self {
            store,
            input,
            typing_sent: false,
            chat,
            attachments: draft.attachments,
            mentions: draft.mentions,
            suggestions,
            suggestion_ix: 0,
            mention_list: Vec::new(),
            mention_ix: 0,
            sticker_panel,
            editing: None,
            recording: None,
            mic_notice: None,
            recording_focus: cx.focus_handle(),
            move_focus: false,
            replacing: false,
            _subs: subs,
        }
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

    /// Keep the chat's draft in the store (survives closing the window). Not while editing
    /// a sent message: that text isn't a draft.
    fn save_draft(&self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let Some(chat) = self.chat.clone() else { return };
        let text = self.input.read(cx).value().to_string();
        let mentions = self.mentions.iter().filter(|(name, _)| text.contains(&format!("@{name}"))).cloned().collect();
        let draft = Draft { text, attachments: self.attachments.clone(), mentions };
        self.store.update(cx, |s, _| s.save_draft(&chat, draft));
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
        } else {
            self.save_draft(cx);
        }
        if let Some(old) = self.chat.take()
            && self.typing_sent
        {
            self.store.read(cx).set_typing_in(&old, false);
        }
        self.typing_sent = false;
        self.recording = None;
        self.mention_list.clear();
        self.suggestions.clear();
        let restored = selected.as_ref().and_then(|c| self.store.read(cx).drafts.get(c).cloned()).unwrap_or_default();
        self.attachments = restored.attachments;
        self.mentions = restored.mentions;
        self.chat = selected;
        self.set_text(restored.text, window, cx);
    }

    pub fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = this.read(cx).input.clone();
        input.update(cx, |s, cx| s.focus(window, cx));
    }

    fn submit(&mut self, state: Entity<TextareaState>, window: &mut Window, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            self.send_recording(cx);
            return;
        }
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
        let (text, mentioned) = crate::mentions::to_wire(text.trim(), &std::mem::take(&mut self.mentions));
        self.mention_list.clear();
        self.save_draft(cx);
        // Like WhatsApp: the text becomes the first photo / video's caption (a caption
        // can't carry mentions, so then it goes as its own message).
        let captioned = !text.is_empty() && mentioned.is_empty() && attachments.first().is_some_and(|p| is_image(p) || is_video(p));
        self.store.update(cx, |s, cx| {
            s.set_typing(false);
            for (i, path) in attachments.into_iter().enumerate() {
                let caption = (i == 0 && captioned).then(|| text.clone());
                if is_image(&path) { s.send_image(path, caption, cx) } else { s.send_file(path, caption, cx) }
            }
            if !captioned && !text.is_empty() {
                s.send_text(text, mentioned, cx);
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
        // Whatever was being written stays the chat's draft for after the edit.
        self.save_draft(cx);
        self.editing = Some(message);
        self.attachments.clear();
        self.mentions.clear();
        self.suggestions.clear();
        self.mention_list.clear();
        self.recording = None;
        self.store.update(cx, |s, cx| s.set_replying_to(None, cx));
        self.set_text(text, window, cx);
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            let draft = self.chat.as_ref().and_then(|c| self.store.read(cx).drafts.get(c).cloned()).unwrap_or_default();
            self.attachments = draft.attachments;
            self.mentions = draft.mentions;
            self.set_text(draft.text, window, cx);
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

    /// Refresh the `@` list (groups only). `load`: ask for the members if they aren't
    /// known yet (typing, not mere repaints).
    fn update_mentions(&mut self, load: bool, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.clone().filter(|c| c.ends_with("@g.us")) else { return };
        if self.editing.is_some() {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        let next = match crate::mentions::trailing_query(&text) {
            Some(q) => {
                let members = if load {
                    self.store.update(cx, |s, cx| s.mentionable(&chat, cx))
                } else {
                    let store = self.store.read(cx);
                    store.profiles.get(&chat).map(|p| p.participants.iter().filter(|p| !p.is_me).cloned().collect()).unwrap_or_default()
                };
                crate::mentions::suggest(&members, q, 6)
            }
            None => Vec::new(),
        };
        if next.iter().map(|p| &p.jid).ne(self.mention_list.iter().map(|p| &p.jid)) {
            self.mention_ix = 0;
            self.mention_list = next;
            cx.notify();
        }
    }

    fn accept_mention(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.mention_list.get(ix).cloned() else { return };
        let text = self.input.read(cx).value().to_string();
        if !self.mentions.iter().any(|(n, j)| *n == p.name && *j == p.jid) {
            self.mentions.push((p.name.clone(), p.jid.clone()));
        }
        self.mention_list.clear();
        self.set_text(crate::mentions::apply(&text, &p.name), window, cx);
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
        if !self.mention_list.is_empty() {
            let n = self.mention_list.len();
            self.mention_ix = (self.mention_ix + n - 1) % n;
            cx.notify();
            cx.stop_propagation();
            return;
        }
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

    /// Enter picks the highlighted suggestion (or sends a recording) before the text
    /// field sees it.
    fn on_enter(&mut self, ev: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if ev.shift || ev.secondary {
            return cx.propagate();
        }
        if self.recording.is_some() {
            self.send_recording(cx);
        } else if !self.mention_list.is_empty() {
            self.accept_mention(self.mention_ix, window, cx);
        } else if !self.suggestions.is_empty() {
            self.accept_suggestion(self.suggestion_ix, window, cx);
        } else {
            return cx.propagate();
        }
        cx.stop_propagation();
    }

    /// Arrow keys / Tab / Esc steer the suggestion lists while one is open; Esc / Enter
    /// cancel / send a recording.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        if self.recording.is_some() {
            match key {
                "escape" => self.cancel_recording(cx),
                "enter" => self.send_recording(cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        if !self.mention_list.is_empty() {
            let n = self.mention_list.len();
            match key {
                "down" => self.mention_ix = (self.mention_ix + 1) % n,
                "up" => self.mention_ix = (self.mention_ix + n - 1) % n,
                "tab" => self.accept_mention(self.mention_ix, window, cx),
                "escape" => self.mention_list.clear(),
                _ => return,
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.suggestions.is_empty() && key == "escape" && self.editing.is_some() {
            self.cancel_edit(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.suggestions.is_empty() {
            if key == "escape" && self.sticker_panel {
                self.sticker_panel = false;
                cx.notify();
                cx.stop_propagation();
            }
            return;
        }
        let n = self.suggestions.len();
        match key {
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

    /// Mic button: record once macOS allows the microphone (asking the first time).
    fn start_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            return;
        }
        match crate::recorder::mic_access() {
            MicAccess::Granted => {
                self.mic_notice = None;
                self.sticker_panel = false;
                self.recording = Some(Recorder::start());
                self.move_focus = true;
                self.tick_recording(cx);
            }
            MicAccess::Undetermined => {
                let answer = crate::recorder::request_access();
                cx.spawn(async move |this, cx| {
                    let granted = answer.recv_async().await.unwrap_or(false);
                    let _ = this.update(cx, |this, cx| {
                        if granted {
                            this.start_recording(cx);
                        } else {
                            this.mic_notice = Some(MIC_OFF.into());
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            MicAccess::Denied => self.mic_notice = Some(MIC_OFF.into()),
        }
        cx.notify();
    }

    /// Repaint the timer and meter while recording; give up on errors, stop at the limit.
    fn tick_recording(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(60)).await;
                let go = this.update(cx, |this, cx| {
                    let Some(rec) = &this.recording else { return false };
                    if let Some(e) = rec.error() {
                        this.recording = None;
                        this.mic_notice = Some(format!("Couldn't record: {e}"));
                    } else if rec.elapsed() >= crate::recorder::MAX_LENGTH {
                        this.send_recording(cx);
                    }
                    cx.notify();
                    this.recording.is_some()
                });
                if !go.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            self.move_focus = true;
            cx.notify();
        }
    }

    fn send_recording(&mut self, cx: &mut Context<Self>) {
        let Some(rec) = self.recording.take() else { return };
        let (samples, rate) = rec.finish();
        self.move_focus = true;
        cx.notify();
        // A click on the mic and straight back: nothing worth sending.
        if rate == 0 || samples.len() < rate as usize / 2 {
            return;
        }
        self.store.update(cx, |s, cx| s.send_voice(samples, rate, cx));
    }

    fn render_suggestions(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        popup(theme).children(self.suggestions.iter().enumerate().map(|(i, s)| {
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

    fn render_mention_list(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        popup(theme).children(self.mention_list.iter().enumerate().map(|(i, p)| {
            let selected = i == self.mention_ix;
            let phone = p.jid.strip_suffix("@s.whatsapp.net").map(|u| format!("+{u}"));
            div()
                .id(("mention-suggestion", i))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(6.))
                .py(px(4.))
                .rounded(px(6.))
                .when(selected, |d| d.bg(theme.accent))
                .when(!selected, |d| d.hover(|d| d.bg(theme.search_bg)))
                .child(crate::avatar::avatar(&p.name, store.avatar_cached(&p.jid), 24., theme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if selected { white() } else { theme.text })
                        .child(p.name.clone()),
                )
                .when_some(phone.filter(|ph| *ph != p.name), |d, phone| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(if selected { white().opacity(0.8) } else { theme.text_secondary })
                            .child(phone),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| this.accept_mention(i, window, cx)))
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

    /// Thumbnails for photos, a file card for anything else; each with a remove badge.
    fn render_attachments(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_wrap().gap(px(8.)).px(px(10.)).pt(px(10.)).pb(px(2.)).children(self.attachments.iter().enumerate().map(
            |(i, path)| {
                let remove = path.clone();
                let tile = if is_image(path) {
                    div().size(px(THUMB)).child(img(path.clone()).size_full().rounded(px(10.)).object_fit(ObjectFit::Cover))
                } else {
                    file_card(path, theme)
                };
                tile.relative().child(
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
                            this.attachments.retain(|p| p != &remove);
                            this.save_draft(cx);
                            cx.notify();
                        })),
                )
            },
        ))
    }

    /// Instead of the text while recording: cancel, a red dot, the time and a live meter.
    fn render_recording(&self, rec: &Recorder, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let secs = rec.elapsed().as_secs();
        let levels = rec.levels(LEVEL_BARS);
        div()
            .flex_1()
            .min_w_0()
            .h(px(COMPOSER_HEIGHT - 2.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("cancel-recording")
                    .flex_none()
                    .size(px(22.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.search_bg))
                    .child(glyph(Glyph::Close, 11., theme.text_secondary))
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_recording(cx))),
            )
            .child(
                div().flex_none().size(px(8.)).rounded_full().bg(theme.failed).with_animation(
                    "recording-dot",
                    Animation::new(Duration::from_millis(1200)).repeat().with_easing(ease_in_out),
                    |d, t| d.opacity(0.35 + 0.65 * (1. - (t * 2. - 1.).abs())),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(34.))
                    .text_size(px(13.))
                    .text_color(theme.text)
                    .child(format!("{}:{:02}", secs / 60, secs % 60)),
            )
            .child(
                div().flex_1().min_w_0().h(px(22.)).overflow_hidden().flex().items_center().justify_end().gap(px(2.)).children(
                    levels.into_iter().map(|l| div().flex_none().w(px(3.)).h(px(3. + 17. * l)).rounded_full().bg(theme.accent)),
                ),
            )
    }

    fn add_attachments(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for p in paths {
            if !self.attachments.contains(&p) {
                self.attachments.push(p);
            }
        }
        self.save_draft(cx);
        cx.notify();
    }

    /// Files dropped on the conversation (folders are skipped).
    pub fn attach_files(this: &Entity<Self>, paths: Vec<PathBuf>, cx: &mut App) {
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file()).collect();
        if !files.is_empty() {
            this.update(cx, |c, cx| {
                if c.editing.is_none() {
                    c.add_attachments(files, cx);
                }
            });
        }
    }

    /// Paste handler: images on the clipboard (screenshots, copied images) and files
    /// copied in Finder become attachments. Anything else pastes as text.
    fn paste(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let mut files = Vec::new();
        let mut images = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => images.push((image.format(), image.bytes().to_vec())),
                ClipboardEntry::ExternalPaths(paths) => files.extend(paths.paths().iter().filter(|p| p.is_file()).cloned()),
                ClipboardEntry::String(_) => {}
            }
        }
        if (files.is_empty() && images.is_empty()) || self.editing.is_some() {
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

    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let paths = paths.into_iter().filter(|p| p.is_file()).collect();
            let _ = this.update(cx, |this, cx| this.add_attachments(paths, cx));
        })
        .detach();
    }
}

const MIC_OFF: &str = "Microphone access is off.";

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

fn is_image(path: &Path) -> bool {
    has_ext(path, IMAGE_EXTS)
}

fn is_video(path: &Path) -> bool {
    has_ext(path, VIDEO_EXTS)
}

/// The suggestion popover above the field (emoji, @mentions).
fn popup(theme: &Theme) -> Div {
    surface(div(), theme)
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
}

/// An attached file that isn't a photo: kind tile, name, size.
fn file_card(path: &Path, theme: &Theme) -> Div {
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_uppercase).unwrap_or_else(|| "FILE".into());
    let size = std::fs::metadata(path).map(|m| crate::kinds::human_size(m.len())).unwrap_or_default();
    let tile = div().flex_none().w(px(36.)).h(px(44.)).rounded(px(6.)).bg(theme.surface_bg).flex().items_center().justify_center();
    let tile = if is_video(path) {
        tile.child(glyph(Glyph::Film, 20., theme.accent))
    } else {
        tile.text_size(px(9.)).font_weight(FontWeight::BOLD).text_color(theme.text).child(ext.chars().take(4).collect::<String>())
    };
    div()
        .h(px(THUMB))
        .w(px(200.))
        .flex()
        .items_center()
        .gap(px(10.))
        .pl(px(10.))
        .pr(px(12.))
        .rounded(px(10.))
        .bg(theme.search_bg)
        .child(tile)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).text_color(theme.text).truncate().child(name))
                .child(div().text_size(px(11.)).text_color(theme.text_secondary).child(if size.is_empty() { ext } else { format!("{ext} · {size}") })),
        )
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

/// A bar across the top of the field (editing, replying, notices): icon, title, detail,
/// close button.
fn field_bar(id: &'static str, icon: Glyph, title: String, detail: Div, theme: &Theme) -> (Div, Stateful<Div>) {
    let close = div()
        .id(id)
        .size(px(22.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .hover(|s| s.bg(theme.search_bg))
        .child(glyph(Glyph::Close, 11., theme.text_secondary));
    let bar = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .pl(px(12.))
        .pr(px(6.))
        .pt(px(7.))
        .pb(px(5.))
        .border_b_1()
        .border_color(theme.separator)
        .child(glyph(icon, 13., theme.accent))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.accent).child(title))
                .child(detail.text_size(px(12.)).text_color(theme.text_secondary).truncate()),
        );
    (bar, close)
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_window(window);
        let has_text = !self.input.read(cx).value().trim().is_empty() || !self.attachments.is_empty();
        let recording = self.recording.is_some();
        if std::mem::take(&mut self.move_focus) {
            if recording {
                window.focus(&self.recording_focus, cx);
            } else {
                self.input.update(cx, |s, cx| s.focus(window, cx));
            }
        }
        let replying = self.store.read(cx).replying_to.clone();
        let weak = cx.entity().downgrade();

        let suggestions_open = !self.suggestions.is_empty();
        let mentions_open = !self.mention_list.is_empty();
        let sticker_panel = self.sticker_panel;
        // The end-cap slot: send while there's something to send (or a recording), else
        // the mic with stickers beside it, like Messages.
        let send_ready = has_text || recording;
        let slot = |id: &'static str, right: f32| {
            div()
                .id(id)
                .absolute()
                .right(px(right))
                .bottom(px(SEND_INSET - 1.))
                .size(px(SEND_SIZE))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
        };
        let text_row = match &self.recording {
            Some(rec) => div()
                .flex_1()
                .min_w_0()
                .track_focus(&self.recording_focus)
                .child(self.render_recording(rec, &theme, cx))
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .child(
                    Textarea::new(&self.input)
                        .on_paste(move |item, _, cx| weak.update(cx, |this, cx| this.paste(item, cx)).unwrap_or(false))
                        .appearance(false)
                        .xsmall()
                        .px_0()
                        .py_0()
                        .text_size(px(14.))
                        .line_height(px(20.)),
                )
                .into_any_element(),
        };

        div()
            .relative()
            .w_full()
            .bg(theme.window_bg)
            .border_t_1()
            .border_color(theme.separator)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| this.on_key(ev, window, cx)))
            // ↑ is bound to the input's MoveUp action, which runs before key-down listeners.
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| this.on_move_up(window, cx)))
            // Enter too: picking a suggestion must not insert a newline first.
            .capture_action(cx.listener(|this, ev: &Enter, window, cx| this.on_enter(ev, window, cx)))
            // Tab (bound to indenting) picks one as well.
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if !this.mention_list.is_empty() {
                    this.accept_mention(this.mention_ix, window, cx);
                } else if !this.suggestions.is_empty() {
                    this.accept_suggestion(this.suggestion_ix, window, cx);
                } else {
                    return cx.propagate();
                }
                cx.stop_propagation();
            }))
            .when(mentions_open, |d| d.child(self.render_mention_list(&theme, cx)))
            .when(suggestions_open && !mentions_open, |d| d.child(self.render_suggestions(&theme, cx)))
            .when(sticker_panel, |d| d.child(self.render_sticker_panel(&theme, cx)))
            .flex()
            .items_end()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .child(
                round_button("attach", COMPOSER_HEIGHT, &theme)
                    .child(glyph(Glyph::Plus, 18., theme.text_secondary))
                    .on_click(cx.listener(|this, _, _, cx| this.pick_files(cx))),
            )
            .child(
                surface(div().flex_1().min_w_0().flex().flex_col().rounded(px(17.)), &theme)
                    .when_some(self.mic_notice.clone(), |d, notice| {
                        let detail = div()
                            .id("mic-settings")
                            .cursor_pointer()
                            .child(format!("{notice} Click to open Privacy & Security settings."))
                            .on_click(|_, _, _| crate::recorder::open_privacy_settings());
                        let (bar, close) = field_bar("dismiss-mic", Glyph::Mic, "Can't record".into(), div().child(detail), &theme);
                        d.child(bar.child(close.on_click(cx.listener(|this, _, _, cx| {
                            this.mic_notice = None;
                            cx.notify();
                        }))))
                    })
                    .when_some(self.editing.clone(), |d, m| {
                        let detail = div().child(Store::editable_text(&m));
                        let (bar, close) = field_bar("cancel-edit", Glyph::Pencil, "Edit message".into(), detail, &theme);
                        d.child(bar.child(close.on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx)))))
                    })
                    .when_some(replying, |d, m| {
                        let who = if m.from_me {
                            "yourself".to_string()
                        } else {
                            m.sender_name.clone().unwrap_or_else(|| "message".into())
                        };
                        let detail = div().child(crate::mentions::display(&body_text(&m.body), &m.mentions).0);
                        let (bar, close) = field_bar("cancel-reply", Glyph::Reply, format!("Replying to {who}"), detail, &theme);
                        d.child(bar.child(close.on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.set_replying_to(None, cx));
                        }))))
                    })
                    .when(!self.attachments.is_empty() && !recording, |d| d.child(self.render_attachments(&theme, cx)))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .items_end()
                            // Border (2px) + this = COMPOSER_HEIGHT on one line, matching the + button.
                            .min_h(px(COMPOSER_HEIGHT - 2.))
                            .pl(px(if recording { 6. } else { 14. }))
                            // Room for the round button that sits in the field's end cap.
                            .pr(px(SEND_SIZE + 2. * SEND_INSET))
                            // The text area draws its line ~2.5pt below its box's center, so the
                            // padding is asymmetric to optically center one line in the 34pt field
                            // (measured from screenshots). Extra lines grow the field downward.
                            .when(!recording, |d| d.pt(px(1.5)).pb(px(6.5)))
                            .text_size(px(14.))
                            .child(text_row)
                            // One slot at the field's right edge: the mic (stickers next to it)
                            // while empty, send once there's something to send (no layout shift,
                            // icon stays at the edge).
                            .when(!send_ready, |d| {
                                d.child(
                                    slot("stickers", SEND_INSET - 1. + SEND_SIZE)
                                        .hover(|d| d.bg(theme.search_bg))
                                        .child(glyph(Glyph::Sticker, 19., if sticker_panel { theme.accent } else { theme.text_secondary }))
                                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sticker_panel(cx))),
                                )
                                .child(
                                    slot("record", SEND_INSET - 1.)
                                        .hover(|d| d.bg(theme.search_bg))
                                        .child(glyph(Glyph::Mic, 18., theme.text_secondary))
                                        .on_click(cx.listener(|this, _, _, cx| this.start_recording(cx))),
                                )
                            })
                            .when(send_ready, |d| {
                                d.child(
                                    // Concentric with the pill's rounded end: a 28pt circle inset 3pt
                                    // on every side of a 34pt field (1pt of that is the border).
                                    slot("send", SEND_INSET - 1.)
                                        .bg(theme.accent)
                                        .child(glyph(Glyph::ArrowUp, 18., white()))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let state = this.input.clone();
                                            this.submit(state, window, cx);
                                        }))
                                        // Pop in when there's something to send, like Messages.
                                        // Fade only: animating margin/size would shift the field's layout.
                                        .with_animation(
                                            "send-in",
                                            Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                                            |d, t| d.opacity(t),
                                        ),
                                )
                            }),
                    ),
            )
    }
}
