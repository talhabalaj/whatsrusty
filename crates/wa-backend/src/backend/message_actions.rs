//! Per-message actions: delete (for everyone / for me) and forward.

use whatsapp_rust::RevokeType;
use whatsapp_rust::types::events::DeleteMessageForMeUpdate;

use super::*;

/// Forwarded photos / videos / documents whose upload is older than this are uploaded
/// again (the server may have dropped the original copy).
const REUPLOAD_AFTER_SECS: i64 = 7 * 86_400;

/// What a forward sends: a text, or a stored media message (with its caption).
#[derive(Debug)]
pub(super) enum Forward {
    Text(String),
    Media {
        media: StoredMedia,
        caption: Option<String>,
        /// The original file on disk, usable for a re-upload (photos, videos, documents).
        file: Option<PathBuf>,
    },
}

/// A forward waiting in the send queue.
pub(super) struct ForwardJob {
    /// The message being forwarded (to download its media if a re-upload needs it).
    pub source: RawMessage,
    pub content: Forward,
}

/// Why `raw` can't be deleted for everyone at `now`, if it can't.
fn revoke_refusal(raw: &RawMessage, now: Timestamp) -> Option<&'static str> {
    if !raw.from_me {
        Some("it isn't yours")
    } else if raw.revoked {
        Some("it was deleted already")
    } else if !matches!(raw.status, DeliveryStatus::Sent | DeliveryStatus::Delivered | DeliveryStatus::Read) {
        Some("it wasn't sent")
    } else if now - raw.timestamp >= REVOKE_WINDOW_SECS {
        Some("it's too old")
    } else {
        None
    }
}

/// `Command::DeleteMessage`.
pub(super) fn delete_message(
    shared: &Arc<Shared>,
    session: Option<&Arc<Session>>,
    chat_jid: &str,
    id: &str,
    for_everyone: bool,
) {
    let Some(Some(raw)) = shared.store(|s| s.raw_message(chat_jid, id)) else {
        log::debug!("not deleting {id}: not stored");
        return;
    };
    if !for_everyone {
        shared.store(|s| s.delete_for_me(&raw.chat_jid, &raw.id, now_secs()));
        shared.emit(Event::MessagesRemoved { chat_jid: raw.chat_jid.clone(), message_ids: vec![raw.id.clone()] });
        shared.mark_chats_dirty();
        // Tell my other devices (best effort: it's gone here either way).
        if let Some(session) = session {
            let task_session = session.clone();
            session.track(tokio::spawn(async move {
                if let Err(e) = task_session.sync_delete_for_me(&raw).await {
                    log::warn!("syncing delete for me of {}: {e:#}", raw.id);
                }
            }));
        }
        return;
    }
    if let Some(why) = revoke_refusal(&raw, now_secs()) {
        shared.emit(Event::Error(format!("Couldn't delete the message for everyone: {why}.")));
        return;
    }
    let Some(session) = session else {
        shared.emit(Event::Error("Couldn't delete the message for everyone: not connected.".to_owned()));
        return;
    };
    // Sent first, applied after: a revoke wipes the content for good, so there's no
    // undoing an optimistic one.
    let task_session = session.clone();
    session.track(tokio::spawn(async move {
        let shared = task_session.shared.clone();
        match task_session.send_revoke(&raw).await {
            Ok(()) => {
                log::info!("deleted {} for everyone", raw.id);
                if let Some(Some(msg)) = shared.store(|s| s.revoke(&raw.chat_jid, &raw.id, DELETED_LABEL)) {
                    shared.emit(Event::MessageUpserted(msg));
                }
                shared.mark_chats_dirty();
            }
            Err(e) => {
                log::warn!("deleting {} for everyone: {e:#}", raw.id);
                shared.emit(Event::Error(format!("Couldn't delete the message for everyone: {e:#}")));
            }
        }
    }));
}

/// A message was deleted for me on another of my devices.
pub(super) fn deleted_elsewhere(shared: &Arc<Shared>, d: &DeleteMessageForMeUpdate) {
    let chat = d.chat_jid.to_non_ad_string();
    let result = shared.store(|s| Ok((s.canonical(&chat)?, s.delete_for_me(&chat, &d.message_id, now_secs())?)));
    if let Some((chat_jid, true)) = result {
        shared.emit(Event::MessagesRemoved { chat_jid, message_ids: vec![d.message_id.clone()] });
        shared.mark_chats_dirty();
    }
}

/// A history-sync stub of a message deleted for everyone: stored as the "deleted"
/// placeholder in its place (or marks the stored copy deleted).
pub(super) fn store_revoked(s: &Store, stub: &NewMessage) -> Result<()> {
    if stub.timestamp > 0 {
        s.insert_message(stub, false)?;
    }
    s.revoke(&stub.chat_jid, &stub.id, DELETED_LABEL)?;
    Ok(())
}

/// What forwarding `raw` sends, or why it can't be forwarded.
fn forward_plan(raw: &RawMessage) -> Result<Forward, &'static str> {
    if raw.revoked {
        return Err("it was deleted");
    }
    let reuploadable = |kind: MediaKind| {
        matches!(kind, MediaKind::Image | MediaKind::Video | MediaKind::Ptv | MediaKind::Document)
            .then(|| raw.media_path.clone())
            .flatten()
    };
    match (&raw.body, &raw.media) {
        (MessageBody::Text(t), _) => Ok(Forward::Text(t.clone())),
        (MessageBody::ViewOnce(_), _) => Err("view once messages can't be forwarded"),
        (
            MessageBody::Image(_)
            | MessageBody::Video(_)
            | MessageBody::Document(_)
            | MessageBody::Voice(_)
            | MessageBody::Sticker(_),
            Some(media),
        ) => {
            if media.media_key().is_none() && reuploadable(media.kind()).is_none() {
                return Err("its media isn't available");
            }
            let caption = match &raw.body {
                MessageBody::Image(i) => i.caption.clone(),
                MessageBody::Video(v) => v.caption.clone(),
                MessageBody::Document(d) => d.caption.clone(),
                _ => None,
            };
            Ok(Forward::Media { media: media.clone(), caption, file: reuploadable(media.kind()) })
        }
        _ => Err("this kind of message can't be forwarded yet"),
    }
}

/// `Command::ForwardMessage`: store the copy as `Pending` in the target chat (marked
/// forwarded, with its own copy of the file), then queue it.
pub(super) async fn forward_message(
    shared: &Arc<Shared>,
    session: Option<&Arc<Session>>,
    chat_jid: &str,
    id: &str,
    to_chat_jid: String,
    client_id: String,
) {
    let Some(Some(raw)) = shared.store(|s| s.raw_message(chat_jid, id)) else {
        shared.emit(Event::Error("Couldn't forward the message: it's no longer stored.".to_owned()));
        return;
    };
    let content = match forward_plan(&raw) {
        Ok(content) => content,
        Err(why) => {
            shared.emit(Event::Error(format!("Couldn't forward the message: {why}.")));
            return;
        }
    };
    let new_id = message_id_from(client_id);
    let canonical = shared.store(|s| s.canonical(&to_chat_jid)).unwrap_or_else(|| to_chat_jid.clone());
    let media = match &content {
        Forward::Text(_) => None,
        Forward::Media { media, .. } => {
            let copy = match raw.media_path.clone() {
                Some(src) => {
                    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_owned();
                    let dest = media_file(&shared.config.data_dir, &canonical, &new_id, &ext);
                    let generation = session.map_or_else(|| shared.generation(), |s| s.generation);
                    let target = dest.clone();
                    let copied = shared
                        .blocking(generation, move |_| write_atomic(&target, &std::fs::read(&src)?))
                        .await;
                    copied.map_err(|e| log::info!("not copying the forwarded file: {e:#}")).ok().map(|_| dest)
                }
                None => None,
            };
            Some((media.clone(), copy))
        }
    };
    let Some(canonical) = insert_pending(shared, &to_chat_jid, &new_id, raw.body.clone(), media, None) else {
        return;
    };
    shared.store(|s| s.set_forwarded(&canonical, &new_id));
    shared.emit_message(&canonical, &new_id);
    log::info!("forwarding {} to {canonical} as {new_id}", raw.id);
    let forward = Box::new(ForwardJob { source: raw, content });
    queue_send(shared, session, SendJob::Forward { chat_jid: to_chat_jid, canonical, id: new_id, forward });
}

/// The `contextInfo` of a forwarded message.
fn forwarded_context() -> wa::ContextInfo {
    wa::ContextInfo { is_forwarded: Some(true), forwarding_score: Some(1), ..Default::default() }
}

/// Put `ci` on the (single) content of a message built by [`forward_content`].
fn set_context(m: &mut wa::Message, ci: wa::ContextInfo) {
    macro_rules! set {
        ($($field:ident),*) => {
            $(if let Some(x) = m.$field.as_option_mut() {
                x.context_info = MessageField::some(ci);
                return;
            })*
        };
    }
    set!(extended_text_message, image_message, video_message, ptv_message, audio_message, document_message, sticker_message);
}

/// A photo / video / document whose server copy may be gone (or was never made).
fn needs_reupload(media: &StoredMedia, now: Timestamp) -> bool {
    let (path, key_ts) = match media {
        StoredMedia::Image(m) => (&m.direct_path, m.media_key_timestamp),
        StoredMedia::Video(m) | StoredMedia::Ptv(m) => (&m.direct_path, m.media_key_timestamp),
        StoredMedia::Document(m) => (&m.direct_path, m.media_key_timestamp),
        StoredMedia::Audio(_) | StoredMedia::Sticker(_) => return false,
    };
    path.as_deref().is_none_or(str::is_empty)
        || media.media_key().is_none()
        || key_ts.is_none_or(|t| now - t > REUPLOAD_AFTER_SECS)
}

fn upload_type(media: &StoredMedia) -> MediaType {
    match media.kind() {
        MediaKind::Image => MediaType::Image,
        MediaKind::Video | MediaKind::Ptv => MediaType::Video,
        MediaKind::Audio => MediaType::Audio,
        MediaKind::Document => MediaType::Document,
        MediaKind::Sticker => MediaType::Sticker,
    }
}

fn apply_upload(media: &mut StoredMedia, up: whatsapp_rust::upload::UploadResponse) {
    macro_rules! apply {
        ($m:expr) => {{
            $m.url = Some(up.url);
            $m.direct_path = Some(up.direct_path);
            $m.media_key = Some(up.media_key.to_vec());
            $m.file_enc_sha256 = Some(up.file_enc_sha256.to_vec());
            $m.file_sha256 = Some(up.file_sha256.to_vec());
            $m.file_length = Some(up.file_length);
            $m.media_key_timestamp = Some(up.media_key_timestamp);
        }};
    }
    match media {
        StoredMedia::Image(m) => {
            m.static_url = None;
            apply!(m)
        }
        StoredMedia::Video(m) | StoredMedia::Ptv(m) => {
            m.static_url = None;
            apply!(m)
        }
        StoredMedia::Audio(m) => apply!(m),
        StoredMedia::Document(m) => apply!(m),
        StoredMedia::Sticker(m) => apply!(m),
    }
}

impl Session {
    /// Delete one of my messages for everyone (`protocolMessage` REVOKE).
    async fn send_revoke(&self, raw: &RawMessage) -> Result<()> {
        let chat = parse_jid(&raw.wire_chat).with_context(|| format!("invalid chat JID {}", raw.wire_chat))?;
        let send = self.client.revoke_message(chat, raw.id.clone(), RevokeType::Sender);
        match tokio::time::timeout(SEND_TIMEOUT, send).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => bail!("timed out"),
        }
    }

    /// Sync a delete-for-me to my other devices (app state `deleteMessageForMe`).
    async fn sync_delete_for_me(&self, raw: &RawMessage) -> Result<()> {
        let chat = parse_jid(&raw.wire_chat).with_context(|| format!("invalid chat JID {}", raw.wire_chat))?;
        let participant = (is_group_jid(&raw.chat_jid) && !raw.from_me)
            .then(|| raw.wire_sender.as_deref().map(non_ad).and_then(|w| parse_jid(&w)))
            .flatten();
        let actions = self.client.chat_actions();
        let delete = actions.delete_message_for_me(
            &chat,
            participant.as_ref(),
            &raw.id,
            raw.from_me,
            true,
            Some(raw.timestamp),
        );
        match tokio::time::timeout(SEND_TIMEOUT, delete).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => bail!("timed out"),
        }
    }

    /// The message a forward sends, re-uploading its media first if needed (downloading
    /// the original if it isn't on disk). The new upload is stored with the copy.
    pub(super) async fn forward_content(&self, chat_jid: &str, id: &str, job: ForwardJob) -> Result<wa::Message> {
        let ForwardJob { source, content } = job;
        let mut message = match content {
            Forward::Text(text) => wa::Message {
                extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                    text: Some(text),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Forward::Media { mut media, caption, file } => {
                if needs_reupload(&media, now_secs()) {
                    let file = match file.filter(|f| f.is_file()) {
                        Some(file) => file,
                        None => {
                            let file = self.fetch_media(&source).await.context("downloading it first")?;
                            self.shared.store(|s| s.set_media_path(&source.chat_jid, &source.id, Some(&file)));
                            file
                        }
                    };
                    let read = file.clone();
                    let bytes = self
                        .shared
                        .blocking(self.generation, move |_| {
                            std::fs::read(&read).with_context(|| format!("reading {}", read.display()))
                        })
                        .await?;
                    log::info!("forward {id}: uploading the media again");
                    let upload = self.client.upload(bytes, upload_type(&media), UploadOptions::new());
                    let up = tokio::time::timeout(UPLOAD_TIMEOUT, upload).await.context("upload timed out")??;
                    apply_upload(&mut media, up);
                    self.shared.store(|s| s.set_media(chat_jid, id, &media));
                }
                media.to_message(caption.as_deref())
            }
        };
        set_context(&mut message, forwarded_context());
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(body: MessageBody, media: Option<StoredMedia>) -> RawMessage {
        RawMessage {
            chat_jid: "1@s.whatsapp.net".into(),
            id: "A".into(),
            sender_jid: "me@s.whatsapp.net".into(),
            wire_chat: "1@s.whatsapp.net".into(),
            wire_sender: None,
            from_me: true,
            body,
            media,
            media_path: None,
            timestamp: 1_000,
            status: DeliveryStatus::Read,
            edit_ts: None,
            revoked: false,
            quoted_id: None,
        }
    }

    #[test]
    fn history_revoke_stub_keeps_a_placeholder() {
        let s = Store::open_in_memory().unwrap();
        let chat = "1@s.whatsapp.net";
        let stub = |id: &str, ts: u64| wa::HistorySyncMsg {
            message: MessageField::some(wa::WebMessageInfo {
                key: MessageField::some(wa::MessageKey {
                    remote_jid: Some(chat.into()),
                    from_me: Some(false),
                    id: Some(id.into()),
                    ..Default::default()
                }),
                message_timestamp: Some(ts),
                message_stub_type: Some(wa::web_message_info::StubType::REVOKE),
                ..Default::default()
            }),
            ..Default::default()
        };
        let hs = wa::HistorySync {
            conversations: vec![wa::Conversation {
                id: chat.into(),
                messages: vec![stub("GONE", 1_000)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let own = Own { pn: Some("me@s.whatsapp.net".into()), lid: None };
        apply_history_sync(&s, &hs, &own, 2_000).unwrap();
        let m = s.get_message(chat, "GONE").unwrap().expect("placeholder stored");
        assert!(m.deleted);
        assert_eq!((m.timestamp, m.from_me, m.sender_jid.as_str()), (1_000, false, chat));
        // Applied again (a later chunk): still one deleted placeholder.
        apply_history_sync(&s, &hs, &own, 2_000).unwrap();
        assert!(s.get_message(chat, "GONE").unwrap().unwrap().deleted);
    }

    #[test]
    fn revoke_window_and_eligibility() {
        let mine = raw(MessageBody::Text("hi".into()), None);
        assert_eq!(revoke_refusal(&mine, 1_000 + 60), None);
        assert!(revoke_refusal(&mine, 1_000 + REVOKE_WINDOW_SECS).is_some());
        assert!(revoke_refusal(&RawMessage { from_me: false, ..mine.clone() }, 1_000).is_some());
        assert!(revoke_refusal(&RawMessage { revoked: true, ..mine.clone() }, 1_000).is_some());
        for status in [DeliveryStatus::Pending, DeliveryStatus::Failed] {
            assert!(revoke_refusal(&RawMessage { status, ..mine.clone() }, 1_000).is_some());
        }
    }

    #[test]
    fn forward_plans() {
        assert!(matches!(forward_plan(&raw(MessageBody::Text("hi".into()), None)), Ok(Forward::Text(t)) if t == "hi"));
        let deleted = RawMessage { revoked: true, ..raw(MessageBody::Text("x".into()), None) };
        assert!(forward_plan(&deleted).is_err());
        assert!(forward_plan(&raw(MessageBody::ViewOnce(ViewOnceKind::Photo), None)).is_err());

        let image = wa::message::ImageMessage {
            media_key: Some(vec![1; 32]),
            direct_path: Some("/v/t62/x".into()),
            media_key_timestamp: Some(1_000),
            caption: Some("old".into()),
            ..Default::default()
        };
        let stored = StoredMedia::Image(Box::new(image));
        let body = stored.body(None, MediaState::Idle);
        let plan = forward_plan(&RawMessage { media_path: Some("/tmp/a.jpg".into()), ..raw(body, Some(stored.clone())) });
        let Ok(Forward::Media { media, caption, file }) = plan else { panic!("photo forwards") };
        assert_eq!((caption.as_deref(), file.as_deref()), (Some("old"), Some(Path::new("/tmp/a.jpg"))));
        // Fresh upload: reused; a week later: uploaded again. Voice notes are always reused.
        assert!(!needs_reupload(&media, 1_000 + 3_600));
        assert!(needs_reupload(&media, 1_000 + REUPLOAD_AFTER_SECS + 1));
        let voice = StoredMedia::Audio(Box::new(wa::message::AudioMessage { media_key: Some(vec![1; 32]), ..Default::default() }));
        assert!(!needs_reupload(&voice, i64::MAX / 2));

        // The forwarded mark lands on the media message itself.
        let mut m = media.to_message(caption.as_deref());
        set_context(&mut m, forwarded_context());
        assert!(crate::extract::is_forwarded(&m));
        let mut t = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage { text: Some("x".into()), ..Default::default() }),
            ..Default::default()
        };
        set_context(&mut t, forwarded_context());
        assert!(crate::extract::is_forwarded(&t));

        // Media without download info and no file can't go anywhere.
        let bare = StoredMedia::Image(Box::default());
        assert!(forward_plan(&raw(bare.body(None, MediaState::Idle), Some(bare))).is_err());
    }
}
