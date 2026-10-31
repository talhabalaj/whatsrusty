//! What can be done with a message's photo, video or document once it's on this Mac:
//! copy, save, show in Finder, open. Shared by the transcript's menu, the gallery and
//! the photo viewer.

use std::path::{Path, PathBuf};

use gpui_kit::*;
use wa_backend::{ChatMessage, MessageBody};

use crate::native_menu::Item;
use crate::state::{MessageAction, Store};

/// The message's file on disk, once downloaded.
pub fn file_of(m: &ChatMessage) -> Option<&PathBuf> {
    match &m.body {
        MessageBody::Image(i) => i.path.as_ref(),
        MessageBody::Video(v) => v.path.as_ref(),
        MessageBody::Document(d) => d.path.as_ref(),
        MessageBody::Voice(v) => v.path.as_ref(),
        _ => None,
    }
}

/// A name for saving it: a document's own, else like WhatsApp's
/// ("WhatsApp Image 2026-10-31 at 23.16.01.jpg").
fn suggested_name(m: &ChatMessage, path: &Path) -> String {
    if let MessageBody::Document(d) = &m.body
        && !d.file_name.trim().is_empty()
    {
        return d.file_name.clone();
    }
    let kind = match &m.body {
        MessageBody::Video(_) => "Video",
        MessageBody::Voice(_) => "Audio",
        _ => "Image",
    };
    let when = chrono::DateTime::from_timestamp(m.timestamp, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d at %H.%M.%S").to_string())
        .unwrap_or_default();
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    format!("WhatsApp {kind} {when}{ext}")
}

fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    match bytes {
        [0xFF, 0xD8, ..] => Some(ImageFormat::Jpeg),
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [b'G', b'I', b'F', ..] => Some(ImageFormat::Gif),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(ImageFormat::Webp),
        _ => None,
    }
}

/// Put the photo on the clipboard, to paste into other apps.
pub fn copy_image(path: &Path, cx: &mut App) {
    match std::fs::read(path) {
        Ok(bytes) => match image_format(&bytes) {
            Some(format) => cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(format, bytes))),
            None => log::warn!("copying {}: not a photo format the clipboard takes", path.display()),
        },
        Err(e) => log::warn!("copying {}: {e}", path.display()),
    }
}

/// "Save As…": the system's save panel, opening in Downloads.
pub fn save_as(m: &ChatMessage, path: &Path, cx: &mut App) {
    save_to(path, &suggested_name(m, path), cx);
}

fn save_to(path: &Path, name: &str, cx: &mut App) {
    let dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir);
    let answer = cx.prompt_for_new_path(&dir, Some(name));
    let from = path.to_path_buf();
    cx.spawn(async move |_| {
        if let Ok(Ok(Some(to))) = answer.await
            && let Err(e) = std::fs::copy(&from, &to)
        {
            log::warn!("saving {} to {}: {e}", from.display(), to.display());
        }
    })
    .detach();
}

/// The file actions for `m`'s media, if it's downloaded: Copy (photos), Save As…, Show in
/// Finder, Open.
pub fn file_items(m: &ChatMessage) -> Vec<Item> {
    let Some(path) = file_of(m) else { return Vec::new() };
    path_items(path, &suggested_name(m, path), matches!(m.body, MessageBody::Image(_)))
}

/// The same for any file (a profile photo): `name` to save it as.
pub fn path_items(path: &Path, name: &str, image: bool) -> Vec<Item> {
    let mut items = Vec::new();
    if image {
        let p = path.to_path_buf();
        items.push(Item::action("Copy Image", move |_, cx| copy_image(&p, cx)));
    }
    let (p, name) = (path.to_path_buf(), name.to_string());
    items.push(Item::action("Save As…", move |_, cx| save_to(&p, &name, cx)));
    let p = path.to_path_buf();
    items.push(Item::action("Show in Finder", move |_, cx| cx.reveal_path(&p)));
    let p = path.to_path_buf();
    items.push(Item::action("Open in Default App", move |_, cx| cx.open_with_system(&p)));
    items
}

/// The full menu for media shown outside its conversation (the gallery, the viewer): the
/// file actions, then Forward… and Show in Chat. `mini`: the popped-out chat's window it's
/// shown in (`None`: the main window).
pub fn media_menu(m: &ChatMessage, store: &Entity<Store>, mini: Option<String>) -> Vec<Item> {
    let mut items = file_items(m);
    if file_of(m).is_none() {
        let (store, chat, id) = (store.clone(), m.chat_jid.clone(), m.id.clone());
        items.push(Item::action("Download", move |_, cx| store.update(cx, |s, _| s.request_media(&chat, &id))));
    }
    items.push(Item::Separator);
    if Store::can_forward(m) {
        let (store, message, mini) = (store.clone(), m.clone(), mini.clone());
        items.push(Item::action("Forward…", move |_, cx| {
            store.update(cx, |s, cx| s.message_action(MessageAction::Forward(message), mini, cx))
        }));
    }
    let (store, message) = (store.clone(), m.clone());
    items.push(Item::action("Show in Chat", move |_, cx| {
        store.update(cx, |s, cx| s.message_action(MessageAction::Reveal(message), mini, cx))
    }));
    items
}
