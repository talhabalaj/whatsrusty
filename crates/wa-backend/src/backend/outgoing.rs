//! Sending files (videos, documents) and recorded voice notes.
//!
//! The file is copied into the media dir and shown as `Pending` right away; the slow
//! part (converting a video, its thumbnail, the upload) runs in the send queue, so
//! things go out in the order they were sent.

use super::message_actions::{apply_upload, set_context, upload_type};
use super::*;

/// Largest file sent (it's read into memory for the upload).
const MAX_FILE_BYTES: u64 = 200 << 20;
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mov"];
pub(super) const FILE_GONE: &str = "The file is no longer available";

/// Media waiting in the send queue: our copy of the file and its message (no link yet).
#[derive(Debug, PartialEq)]
pub(super) struct UploadJob {
    pub file: PathBuf,
    pub media: StoredMedia,
    pub reply_to: Option<String>,
}

/// `Command::SendFile`: copy the file into the media dir, show it as `Pending` and queue
/// its upload. Videos are sent as videos, everything else as a document.
pub(super) async fn send_file(
    shared: &Arc<Shared>,
    session: Option<&Arc<Session>>,
    chat_jid: String,
    path: PathBuf,
    caption: Option<String>,
    client_id: String,
    reply_to: Option<String>,
) {
    let id = message_id_from(client_id);
    let canonical = shared.store(|s| s.canonical(&chat_jid)).unwrap_or_else(|| chat_jid.clone());
    let generation = session.map_or_else(|| shared.generation(), |s| s.generation);
    let caption = caption.map(|c| c.trim().to_owned()).filter(|c| !c.is_empty());
    let name = path.file_name().map_or_else(|| "File".to_owned(), |n| n.to_string_lossy().into_owned());
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let video = VIDEO_EXTS.contains(&ext.as_str());
    let mime = mime_for_ext(&ext);
    let copied = {
        let (data_dir, canonical, id, name) = (shared.config.data_dir.clone(), canonical.clone(), id.clone(), name.clone());
        shared
            .blocking(generation, move |_| {
                let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
                if !meta.is_file() {
                    bail!("it isn't a file");
                }
                if meta.len() > MAX_FILE_BYTES {
                    bail!("files over {} MB can't be sent", MAX_FILE_BYTES >> 20);
                }
                let dest = if video {
                    let dest = media_file(&data_dir, &canonical, &id, &ext);
                    create_private_dir_all(dest.parent().context("media dir")?)?;
                    dest
                } else {
                    reserve_unique(&documents_dir(&data_dir, &canonical), &safe_file_name(&name, Some(mime)))?
                };
                // A clone on APFS: instant, and the copy is ours to keep or delete.
                std::fs::copy(&path, &dest).with_context(|| format!("copying {}", path.display()))?;
                crate::media::restrict_file(&dest);
                Ok((dest, meta.len()))
            })
            .await
    };
    let (file, len) = match copied {
        Ok(c) => c,
        Err(e) => {
            log::warn!("sending {name}: {e:#}");
            shared.emit(Event::Error(format!("Couldn't send “{name}”: {e:#}")));
            return;
        }
    };
    let media = if video {
        StoredMedia::Video(Box::new(wa::message::VideoMessage {
            mimetype: Some("video/mp4".to_owned()),
            caption,
            file_length: Some(len),
            ..Default::default()
        }))
    } else {
        StoredMedia::Document(Box::new(wa::message::DocumentMessage {
            mimetype: Some(mime.to_owned()),
            file_name: Some(name.clone()),
            title: Some(name),
            file_length: Some(len),
            caption,
            ..Default::default()
        }))
    };
    let body = media.body(Some(file.clone()), MediaState::Idle);
    let pending = Some((media.clone(), Some(file.clone())));
    if insert_pending(shared, &chat_jid, &id, body, pending, reply_to.as_deref()).is_none() {
        return;
    }
    let upload = Box::new(UploadJob { file, media, reply_to });
    queue_send(shared, session, SendJob::Upload { chat_jid, canonical, id, upload });
}

/// `Command::SendVoice`: encode the recording as a WhatsApp voice note (Ogg/Opus, kept for
/// the upload) plus a WAV our player can play, show it as `Pending`, queue the upload.
pub(super) async fn send_voice(
    shared: &Arc<Shared>,
    session: Option<&Arc<Session>>,
    chat_jid: String,
    samples: Vec<i16>,
    sample_rate: u32,
    client_id: String,
    reply_to: Option<String>,
) {
    let id = message_id_from(client_id);
    let canonical = shared.store(|s| s.canonical(&chat_jid)).unwrap_or_else(|| chat_jid.clone());
    let generation = session.map_or_else(|| shared.generation(), |s| s.generation);
    let ogg = media_file(&shared.config.data_dir, &canonical, &id, "ogg");
    let wav = ogg.with_extension("wav");
    let encoded = {
        let (ogg, wav) = (ogg.clone(), wav.clone());
        shared
            .blocking(generation, move |_| {
                let note = crate::audio::encode_voice_note(&samples, sample_rate)?;
                write_atomic(&ogg, &note.ogg)?;
                // Played from what is sent, decoded back.
                let pcm = crate::audio::decode_ogg_opus(&note.ogg)?;
                write_atomic(&wav, &crate::audio::wav_bytes(&pcm))?;
                Ok((note.seconds, note.waveform))
            })
            .await
    };
    let (seconds, waveform) = match encoded {
        Ok(e) => e,
        Err(e) => {
            log::warn!("encoding voice note: {e:#}");
            shared.emit(Event::Error(format!("Couldn't send the voice message: {e:#}")));
            return;
        }
    };
    let media = StoredMedia::Audio(Box::new(wa::message::AudioMessage {
        mimetype: Some("audio/ogg; codecs=opus".to_owned()),
        seconds: Some(seconds),
        ptt: Some(true),
        waveform: Some(waveform),
        ..Default::default()
    }));
    let body = media.body(Some(wav.clone()), MediaState::Idle);
    if insert_pending(shared, &chat_jid, &id, body, Some((media.clone(), Some(wav))), reply_to.as_deref()).is_none() {
        return;
    }
    let upload = Box::new(UploadJob { file: ogg, media, reply_to });
    queue_send(shared, session, SendJob::Upload { chat_jid, canonical, id, upload });
}

/// How to resend a failed file / video / voice note: from our copy of it.
pub(super) fn retry_upload(raw: &RawMessage) -> Result<Option<UploadJob>, RetryRefusal> {
    let media = match (&raw.body, &raw.media) {
        (MessageBody::Video(v), Some(StoredMedia::Video(m))) => {
            StoredMedia::Video(Box::new(wa::message::VideoMessage { caption: v.caption.clone(), ..(**m).clone() }))
        }
        (MessageBody::Document(d), Some(StoredMedia::Document(m))) => {
            StoredMedia::Document(Box::new(wa::message::DocumentMessage { caption: d.caption.clone(), ..(**m).clone() }))
        }
        (MessageBody::Voice(_), Some(m @ StoredMedia::Audio(_))) => m.clone(),
        (MessageBody::Video(_) | MessageBody::Document(_) | MessageBody::Voice(_), _) => {
            return Err(RetryRefusal::Refuse(NOT_RESENDABLE));
        }
        _ => return Ok(None),
    };
    let path = raw.media_path.clone().ok_or(RetryRefusal::Refuse(FILE_GONE))?;
    // Voice notes play from a WAV; the Ogg/Opus next to it is what's sent.
    let file = if media.kind() == MediaKind::Audio { path.with_extension("ogg") } else { path };
    if !file.is_file() {
        return Err(RetryRefusal::Refuse(FILE_GONE));
    }
    Ok(Some(UploadJob { file, media, reply_to: raw.quoted_id.clone() }))
}

impl Session {
    /// Upload a queued file (preparing a video first) and send its message.
    pub(super) async fn send_upload(&self, chat_jid: &str, id: String, job: UploadJob) -> Result<()> {
        let UploadJob { mut file, mut media, reply_to } = job;
        let canonical = self.shared.store(|s| s.canonical(chat_jid)).unwrap_or_else(|| chat_jid.to_owned());
        if let StoredMedia::Video(proto) = &mut media {
            let dest = media_file(&self.shared.config.data_dir, &canonical, &id, "mp4");
            let src = file.clone();
            let video = self.shared.blocking(self.generation, move |_| prepare_video(&src, &dest)).await?;
            proto.seconds = Some(video.seconds);
            proto.width = video.width;
            proto.height = video.height;
            proto.jpeg_thumbnail = video.thumbnail;
            proto.file_length = Some(video.len);
            if video.file != file {
                let _ = std::fs::remove_file(&file);
                file = video.file;
            }
            // Show the thumbnail and duration while it uploads.
            self.shared.store(|s| {
                s.set_media(chat_jid, &id, &media)?;
                s.set_media_path(chat_jid, &id, Some(&file))
            });
            self.shared.emit_message(chat_jid, &id);
        }
        let read = file.clone();
        let data = self
            .shared
            .blocking(self.generation, move |_| std::fs::read(&read).with_context(|| format!("reading {}", read.display())))
            .await?;
        // Big files take longer than the usual upload timeout (assume ≥ 50 KB/s).
        let timeout = UPLOAD_TIMEOUT.max(Duration::from_secs(data.len() as u64 / 50_000));
        let upload = self.client.upload(data, upload_type(&media), UploadOptions::new());
        let up = tokio::time::timeout(timeout, upload).await.context("upload timed out")??;
        let sidecar = up.streaming_sidecar.clone();
        apply_upload(&mut media, up);
        match &mut media {
            StoredMedia::Audio(m) => m.streaming_sidecar = sidecar,
            StoredMedia::Video(m) => m.streaming_sidecar = sidecar,
            _ => {}
        }
        // Keep what's needed to re-download / quote / forward it later.
        self.shared.store(|s| s.set_media(chat_jid, &id, &media));
        let caption = match &media {
            StoredMedia::Video(m) => m.caption.clone(),
            StoredMedia::Document(m) => m.caption.clone(),
            _ => None,
        };
        let mut message = media.to_message(caption.as_deref());
        if let Some(ctx) = reply_to.as_deref().and_then(|r| reply_context(&self.shared, chat_jid, r)) {
            set_context(&mut message, ctx);
        }
        self.send(chat_jid, message, id).await
    }
}

/// A video ready to upload: an H.264 MP4 and what its message says about it.
struct PreparedVideo {
    file: PathBuf,
    len: u64,
    seconds: u32,
    width: Option<u32>,
    height: Option<u32>,
    thumbnail: Option<Vec<u8>>,
}

/// Make `src` something every phone plays (H.264 in MP4; anything else is converted to
/// `dest` with macOS `avconvert`), and read its duration, size and a thumbnail.
fn prepare_video(src: &Path, dest: &Path) -> Result<PreparedVideo> {
    let data = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    let info = mp4_info(&data);
    let is_mp4 = src.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "m4v"));
    let (file, info, len) = if is_mp4 && info.as_ref().is_some_and(|i| i.codec.as_deref() == Some("avc1")) {
        (src.to_path_buf(), info, data.len() as u64)
    } else {
        drop(data);
        log::info!("converting {} ({:?}) to H.264 MP4", src.display(), info.as_ref().and_then(|i| i.codec.clone()));
        let tmp = crate::media::TempFile::new(crate::media::temp_path(dest).with_extension("mp4"));
        let out = std::process::Command::new("/usr/bin/avconvert")
            .args(["--preset", "Preset1280x720", "--replace", "--source"])
            .arg(src)
            .arg("--output")
            .arg(tmp.path())
            .output()
            .context("running avconvert")?;
        if !out.status.success() {
            bail!("couldn't convert the video ({})", String::from_utf8_lossy(&out.stderr).trim());
        }
        std::fs::rename(tmp.path(), dest)?;
        tmp.keep();
        crate::media::restrict_file(dest);
        let data = std::fs::read(dest)?;
        (dest.to_path_buf(), mp4_info(&data), data.len() as u64)
    };
    let info = info.unwrap_or_default();
    Ok(PreparedVideo {
        thumbnail: video_thumbnail(&file),
        file,
        len,
        seconds: info.seconds.round() as u32,
        width: info.width,
        height: info.height,
    })
}

/// A small JPEG of the first frame (Quick Look renders it).
fn video_thumbnail(file: &Path) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("wa-thumb-{}", new_message_id()));
    std::fs::create_dir_all(&dir).ok()?;
    let ok = std::process::Command::new("/usr/bin/qlmanage")
        .args(["-t", "-s", "480", "-o"])
        .arg(&dir)
        .arg(file)
        .output()
        .is_ok_and(|o| o.status.success());
    let png = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "png"));
    let thumb = ok.then_some(()).and(png).and_then(|p| std::fs::read(p).ok()).and_then(|b| crate::media::prepare_image(&b).ok());
    let _ = std::fs::remove_dir_all(&dir);
    thumb.map(|t| t.thumbnail)
}

/// What an MP4 / QuickTime file's header says (`mvhd` + the video track).
#[derive(Debug, Default, PartialEq)]
pub(super) struct Mp4Info {
    pub seconds: f64,
    /// As displayed (a 90° rotation matrix swaps them).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Sample entry of the video track: `avc1`, `hvc1`, …
    pub codec: Option<String>,
}

pub(super) fn mp4_info(data: &[u8]) -> Option<Mp4Info> {
    let moov = boxes(data).find(|(t, _)| t == b"moov")?.1;
    let mut info = Mp4Info::default();
    for (kind, body) in boxes(moov) {
        match &kind {
            b"mvhd" => {
                let (timescale, duration) = match *body.first()? {
                    1 => (be32(body, 20)?, be64(body, 24)?),
                    _ => (be32(body, 12)?, u64::from(be32(body, 16)?)),
                };
                if timescale > 0 {
                    info.seconds = duration as f64 / f64::from(timescale);
                }
            }
            b"trak" if info.codec.is_none() => {
                let mdia = boxes(body).find(|(t, _)| t == b"mdia")?.1;
                let hdlr = boxes(mdia).find(|(t, _)| t == b"hdlr")?.1;
                if hdlr.get(8..12) != Some(b"vide") {
                    continue;
                }
                if let Some((_, tkhd)) = boxes(body).find(|(t, _)| t == b"tkhd") {
                    let at = if tkhd.first() == Some(&1) { 4 + 32 } else { 4 + 20 };
                    let matrix = at + 16;
                    let (w, h) = (be32(tkhd, matrix + 36)? >> 16, be32(tkhd, matrix + 40)? >> 16);
                    // [a b u; c d v; x y w]: a == 0 means a quarter turn.
                    let turned = be32(tkhd, matrix)? == 0 && be32(tkhd, matrix + 4)? != 0;
                    let (w, h) = if turned { (h, w) } else { (w, h) };
                    (info.width, info.height) = (Some(w).filter(|v| *v > 0), Some(h).filter(|v| *v > 0));
                }
                let stsd = ["minf", "stbl", "stsd"].iter().try_fold(mdia, |b, name| {
                    boxes(b).find(|(t, _)| t == name.as_bytes()).map(|(_, body)| body)
                })?;
                info.codec = stsd.get(12..16).map(|c| String::from_utf8_lossy(c).into_owned());
            }
            _ => {}
        }
    }
    Some(info)
}

/// The boxes (atoms) directly inside `data`: type and body.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let size = be32(data, at)? as usize;
        let kind: [u8; 4] = data.get(at + 4..at + 8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, data.len() - at),
            1 => (16, usize::try_from(be64(data, at + 8)?).ok()?),
            n => (8, n),
        };
        let body = data.get(at + header..at.checked_add(size)?)?;
        at += size.max(header);
        Some((kind, body))
    })
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// MIME type WhatsApp expects for a document, from its extension.
pub(super) fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pages" => "application/vnd.apple.pages",
        "numbers" => "application/vnd.apple.numbers",
        "key" => "application/vnd.apple.keynote",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "rtf" => "application/rtf",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "tar" => "application/x-tar",
        "apk" => "application/vnd.android.package-archive",
        "dmg" => "application/x-apple-diskimage",
        "vcf" => "text/vcard",
        "ics" => "text/calendar",
        "epub" => "application/epub+zip",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A `moov` with `mvhd` (version `v`), an audio track and a video track.
    fn movie(v: u8, timescale: u32, duration: u64, w: u32, h: u32, quarter_turn: bool, codec: &[u8; 4]) -> Vec<u8> {
        let mut mvhd = vec![v, 0, 0, 0];
        if v == 1 {
            mvhd.extend_from_slice(&[0; 16]);
            mvhd.extend_from_slice(&timescale.to_be_bytes());
            mvhd.extend_from_slice(&duration.to_be_bytes());
        } else {
            mvhd.extend_from_slice(&[0; 8]);
            mvhd.extend_from_slice(&timescale.to_be_bytes());
            mvhd.extend_from_slice(&(duration as u32).to_be_bytes());
        }
        mvhd.extend_from_slice(&[0; 80]);
        let track = |handler: &[u8; 4], codec: &[u8; 4]| {
            let mut tkhd = vec![0u8; 4 + 20 + 16];
            let matrix: [u32; 9] = if quarter_turn {
                [0, 0x10000, 0, 0xFFFF_0000, 0, 0, 0, 0, 0x4000_0000]
            } else {
                [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000]
            };
            matrix.iter().for_each(|m| tkhd.extend_from_slice(&m.to_be_bytes()));
            tkhd.extend_from_slice(&(w << 16).to_be_bytes());
            tkhd.extend_from_slice(&(h << 16).to_be_bytes());
            let mut hdlr = vec![0u8; 8];
            hdlr.extend_from_slice(handler);
            hdlr.extend_from_slice(&[0; 12]);
            let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
            stsd.extend(bx(codec, &[0; 8]));
            let stbl = bx(b"stbl", &bx(b"stsd", &stsd));
            let mdia = [bx(b"hdlr", &hdlr), bx(b"minf", &stbl)].concat();
            bx(b"trak", &[bx(b"tkhd", &tkhd), bx(b"mdia", &mdia)].concat())
        };
        let moov = [bx(b"mvhd", &mvhd), track(b"soun", b"mp4a"), track(b"vide", codec)].concat();
        [bx(b"ftyp", b"isom\0\0\0\0"), bx(b"mdat", &[1, 2, 3]), bx(b"moov", &moov)].concat()
    }

    #[test]
    fn reads_mp4_duration_size_and_codec() {
        let info = mp4_info(&movie(0, 600, 4500, 1280, 720, false, b"avc1")).unwrap();
        assert_eq!(info, Mp4Info { seconds: 7.5, width: Some(1280), height: Some(720), codec: Some("avc1".into()) });
        // iPhone portrait clips: landscape track size plus a quarter-turn matrix.
        let info = mp4_info(&movie(1, 1000, 61_000, 1920, 1080, true, b"hvc1")).unwrap();
        assert_eq!(info, Mp4Info { seconds: 61.0, width: Some(1080), height: Some(1920), codec: Some("hvc1".into()) });
        assert_eq!(mp4_info(b"not a movie"), None);
    }

    fn shared(name: &str) -> (Arc<Shared>, PathBuf, flume::Receiver<Event>) {
        let data_dir = std::env::temp_dir().join(format!("wa-{name}-{}", new_message_id()));
        create_private_dir_all(&data_dir).unwrap();
        let store = Store::open(&data_dir.join("messages.db")).unwrap();
        let (events, rx) = flume::unbounded();
        let config = BackendConfig { data_dir: data_dir.clone(), device_name: "test".into() };
        (Arc::new(Shared::new(store, events, config)), data_dir, rx)
    }

    /// Offline: a voice note and a document are stored (playable / openable copies in the
    /// media dir), fail, and can be resent from those copies.
    #[tokio::test(flavor = "multi_thread")]
    async fn files_and_voice_notes_offline() {
        let (shared, data_dir, rx) = shared("outgoing");
        let alice = "15550000001@s.whatsapp.net";
        let samples: Vec<i16> = (0..32_000).map(|i| ((i as f32 * 0.05).sin() * 8000.) as i16).collect();
        let voice = Command::SendVoice { chat_jid: alice.into(), samples, sample_rate: 16_000, client_id: "VOICE0001".into(), reply_to: None };
        handle_command(&shared, None, voice).await;
        let m = shared.store(|s| s.get_message(alice, "VOICE0001")).flatten().unwrap();
        let MessageBody::Voice(v) = &m.body else { panic!("{:?}", m.body) };
        assert!(v.ptt);
        assert_eq!((v.duration_secs, v.waveform.len()), (2, 64));
        let wav = v.path.clone().unwrap();
        assert!(std::fs::read(&wav).unwrap().starts_with(b"RIFF"));
        assert!(crate::audio::is_ogg(&std::fs::read(wav.with_extension("ogg")).unwrap()));
        assert_eq!(m.status, DeliveryStatus::Failed);

        let src = data_dir.join("Quarterly report.pdf");
        std::fs::write(&src, b"%PDF-1.4 test").unwrap();
        let file = Command::SendFile {
            chat_jid: alice.into(),
            path: src.clone(),
            caption: Some(" numbers ".into()),
            client_id: "FILE00001".into(),
            reply_to: None,
        };
        handle_command(&shared, None, file).await;
        let m = shared.store(|s| s.get_message(alice, "FILE00001")).flatten().unwrap();
        let MessageBody::Document(d) = &m.body else { panic!("{:?}", m.body) };
        assert_eq!(d.file_name, "Quarterly report.pdf");
        assert_eq!(d.mimetype.as_deref(), Some("application/pdf"));
        assert_eq!((d.size_bytes, d.caption.as_deref()), (Some(13), Some("numbers")));
        let copy = d.path.clone().unwrap();
        assert!(copy.starts_with(documents_dir(&data_dir, alice)) && copy != src);
        assert_eq!(m.status, DeliveryStatus::Failed);
        rx.try_iter().for_each(drop);

        for id in ["VOICE0001", "FILE00001"] {
            handle_command(&shared, None, Command::RetrySend { chat_jid: alice.into(), message_id: id.into() }).await;
            let events: Vec<Event> = rx.try_iter().collect();
            assert!(matches!(events.first(), Some(Event::MessageUpserted(m)) if m.id == id && m.status == DeliveryStatus::Pending), "{events:?}");
        }
        // Our copy is gone: can't be resent.
        std::fs::remove_file(&copy).unwrap();
        handle_command(&shared, None, Command::RetrySend { chat_jid: alice.into(), message_id: "FILE00001".into() }).await;
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(matches!(events.as_slice(), [Event::Error(e)] if e == FILE_GONE), "{events:?}");

        // Too big, or not a file: an error, nothing stored.
        let dir = Command::SendFile { chat_jid: alice.into(), path: data_dir.clone(), caption: None, client_id: "DIR000001".into(), reply_to: None };
        handle_command(&shared, None, dir).await;
        assert!(matches!(rx.try_iter().collect::<Vec<_>>().as_slice(), [Event::Error(_)]));
        assert!(shared.store(|s| s.get_message(alice, "DIR000001")).flatten().is_none());
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    /// Manual: `WA_TEST_VIDEO=clip.mov cargo test -p wa-backend prepares_a_real_video -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn prepares_a_real_video() {
        let src = PathBuf::from(std::env::var("WA_TEST_VIDEO").unwrap());
        let dest = std::env::temp_dir().join(format!("wa-video-{}.mp4", new_message_id()));
        let v = prepare_video(&src, &dest).unwrap();
        println!("{} {} bytes, {} s, {:?}x{:?}, thumbnail {:?} bytes", v.file.display(), v.len, v.seconds, v.width, v.height, v.thumbnail.as_ref().map(Vec::len));
        assert_eq!(mp4_info(&std::fs::read(&v.file).unwrap()).unwrap().codec.as_deref(), Some("avc1"));
        assert!(v.thumbnail.is_some());
    }

    #[test]
    fn document_mime_types() {
        assert_eq!(mime_for_ext("pdf"), "application/pdf");
        assert_eq!(mime_for_ext("docx"), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(mime_for_ext("weird"), "application/octet-stream");
    }
}
