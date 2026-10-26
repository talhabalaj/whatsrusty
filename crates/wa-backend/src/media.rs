//! Media files on disk: where they live, owner-only permissions, atomic writes, and
//! converting media (photos to send, stickers to show).

use std::fs::{File, OpenOptions};
use std::io::{Cursor, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail};
use image::{DynamicImage, ImageDecoder, ImageReader};

/// Longest side of the inline JPEG thumbnail sent with a photo.
const THUMB_SIDE: u32 = 72;
/// Sent photos are capped to this longest side (WA's "HD" is ~2560).
const MAX_SIDE: u32 = 2560;

/// Keep a JID or message id usable as a single file name component.
pub fn file_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '-' | '_') { c } else { '_' })
        .collect()
}

/// `<data_dir>/media/<chat>/<id>.<ext>`
pub fn media_file(data_dir: &Path, chat_jid: &str, id: &str, ext: &str) -> PathBuf {
    data_dir.join("media").join(file_component(chat_jid)).join(format!("{}.{ext}", file_component(id)))
}

/// `<data_dir>/avatars/<jid>.jpg`
pub fn avatar_file(data_dir: &Path, jid: &str) -> PathBuf {
    data_dir.join("avatars").join(format!("{}.jpg", file_component(jid)))
}

/// Mode of everything the backend writes: messages and media are private.
const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;

/// `create_dir_all`, owner-only. Only the directories it creates get the mode; an
/// existing `path` is tightened too (never its parents, which aren't ours).
pub fn create_private_dir_all(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(DIR_MODE))
        .with_context(|| format!("restricting {}", path.display()))
}

/// Create `path` empty and owner-only if it doesn't exist (e.g. before SQLite opens it).
pub fn create_private_file(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match OpenOptions::new().write(true).create_new(true).mode(FILE_MODE).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("creating {}", path.display()))),
    }
}

/// Open `path` for writing (truncated), owner-only even if it already existed.
pub fn private_file(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        create_private_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))?;
    Ok(file)
}

/// Make an existing file owner-only (missing files are fine).
pub fn restrict_file(path: &Path) {
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => log::warn!("restricting {}: {e}", path.display()),
        _ => {}
    }
}

/// Make everything under `dir` (and `dir`) owner-only: files written by older builds.
/// Symlinks are left alone and not followed.
pub fn restrict_tree(dir: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(dir) else { return };
    if meta.file_type().is_symlink() {
        return;
    }
    let mode = if meta.is_dir() { DIR_MODE } else { FILE_MODE };
    if meta.permissions().mode() & 0o777 != mode
        && let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
    {
        log::warn!("restricting {}: {e}", dir.display());
    }
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            restrict_tree(&entry.path());
        }
    }
}

/// A temporary file name next to `dest` for writing it: short (a long `dest` name can't
/// push it past the file name limit) and unique per call, so two writers (e.g. a retry
/// while an abandoned download still writes) never share a file.
pub fn temp_path(dest: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    dest.with_file_name(format!(".tmp-{}-{}.part", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// A file deleted when this guard drops (error, cancelled task), unless [`kept`](Self::keep).
pub struct TempFile(Option<PathBuf>);

impl TempFile {
    pub fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    pub fn path(&self) -> &Path {
        self.0.as_deref().expect("kept")
    }

    pub fn keep(mut self) -> PathBuf {
        self.0.take().expect("kept")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take()
            && let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("removing {}: {e}", path.display());
        }
    }
}

/// Write via a temporary file + rename, so a reader (or a crash) never sees half a file.
/// Owner-only.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = TempFile::new(temp_path(path));
    let mut file = private_file(tmp.path())?;
    file.write_all(data).with_context(|| format!("writing {}", tmp.path().display()))?;
    drop(file);
    std::fs::rename(tmp.path(), path).with_context(|| format!("renaming to {}", path.display()))?;
    tmp.keep();
    Ok(())
}

/// `<data_dir>/media/<chat>/documents/`: received documents, under their own names.
pub fn documents_dir(data_dir: &Path, chat_jid: &str) -> PathBuf {
    data_dir.join("media").join(file_component(chat_jid)).join("documents")
}

/// Longest file name macOS (APFS/HFS+) accepts, in bytes.
const MAX_NAME_BYTES: usize = 255;

/// A sender-chosen document name made safe as one file name: no path separators,
/// `..`, leading dots, control or `:` characters; at most 255 bytes (cut on a char
/// boundary, keeping the extension); an extension from the mimetype if it has none.
pub fn safe_file_name(name: &str, mime: Option<&str>) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .collect();
    let mut name = cleaned.trim().trim_start_matches('.').trim().to_owned();
    if name.is_empty() || name.chars().all(|c| c == '.' || c == '_') {
        name = "Document".to_owned();
    }
    if Path::new(&name).extension().is_none()
        && let Some(ext) = doc_ext_for_mime(mime)
    {
        name = format!("{name}.{ext}");
    }
    truncate_name(&name, MAX_NAME_BYTES)
}

/// Cut `name` to `max` bytes, keeping a (short) extension.
fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).filter(|e| e.len() < 16).unwrap_or("");
    let stem_max = if ext.is_empty() { max } else { max - ext.len() - 1 };
    let mut cut = stem_max.min(name.len());
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = &name[..cut];
    if ext.is_empty() { stem.to_owned() } else { format!("{stem}.{ext}") }
}

/// The longest prefix of `s` of at most `max` bytes, cut on a char boundary.
fn cut_bytes(s: &str, max: usize) -> &str {
    let mut cut = max.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// `dir/name`, or `dir/name (2).ext`, `(3)`, ... if taken: reserved by creating it empty
/// and owner-only, so concurrent downloads can't pick the same name. Blocking.
pub fn reserve_unique(dir: &Path, name: &str) -> Result<PathBuf> {
    create_private_dir_all(dir)?;
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 1..10_000u32 {
        let candidate = if n == 1 {
            truncate_name(name, MAX_NAME_BYTES)
        } else {
            // Cut the stem, not the " (n)" that makes the name unique.
            let suffix = format!(" ({n})");
            let ext_len = ext.map_or(0, |e| e.len() + 1);
            let stem = cut_bytes(stem, MAX_NAME_BYTES.saturating_sub(suffix.len() + ext_len).max(1));
            match ext {
                Some(ext) => format!("{stem}{suffix}.{ext}"),
                None => format!("{stem}{suffix}"),
            }
        };
        let candidate = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).mode(FILE_MODE).open(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow::Error::new(e).context(format!("creating {}", candidate.display()))),
        }
    }
    bail!("no free file name for {name} in {}", dir.display())
}

fn doc_ext_for_mime(mime: Option<&str>) -> Option<&'static str> {
    let mime = mime?.split(';').next()?.trim();
    Some(match mime {
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "application/vnd.android.package-archive" => "apk",
        other => match ext_for_mime(Some(other), "") {
            "" => return None,
            ext => ext,
        },
    })
}

/// File extension for a downloaded media mimetype.
pub fn ext_for_mime(mime: Option<&str>, default: &'static str) -> &'static str {
    let mime = mime.unwrap_or("").split(';').next().unwrap_or("").trim();
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/amr" => "amr",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/3gpp" => "3gp",
        "video/webm" => "webm",
        _ => default,
    }
}

/// A photo ready to upload: JPEG bytes, oriented dimensions, inline thumbnail.
pub struct PreparedImage {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub thumbnail: Vec<u8>,
}

/// Decode with the EXIF orientation applied.
fn decode_oriented(data: &[u8]) -> Result<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(data)).with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&img.to_rgb8())?;
    Ok(out)
}

/// Prepare raw image bytes (JPEG/PNG/WebP/GIF) for sending. Always re-encoded from
/// the oriented pixels, like WhatsApp's own clients: that drops EXIF (GPS, device) and
/// bakes in the rotation so receivers don't have to honour an orientation tag.
pub fn prepare_image(data: &[u8]) -> Result<PreparedImage> {
    let img = decode_oriented(data).context("unsupported or corrupt image")?;
    if img.width() == 0 || img.height() == 0 {
        bail!("empty image");
    }
    let thumbnail = encode_jpeg(&img.thumbnail(THUMB_SIDE, THUMB_SIDE), 60)?;
    let img = if img.width().max(img.height()) > MAX_SIDE {
        img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    Ok(PreparedImage { jpeg: encode_jpeg(&img, 87)?, width: img.width(), height: img.height(), thumbnail })
}

/// A sticker ready to show: PNG bytes of its (first) frame.
pub struct StickerPng {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
}

/// Convert a downloaded sticker (WebP, possibly animated; sometimes PNG) to PNG, using
/// the first frame of animated ones.
pub fn sticker_to_png(data: &[u8]) -> Result<StickerPng> {
    let format = image::guess_format(data).context("unknown sticker format")?;
    let (img, animated) = if format == image::ImageFormat::WebP {
        let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(data))?;
        let animated = decoder.has_animation();
        (DynamicImage::from_decoder(decoder)?, animated)
    } else {
        (image::load_from_memory_with_format(data, format)?, false)
    };
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(StickerPng { png, width: img.width(), height: img.height(), animated })
}

/// Read and prepare a photo from disk. HEIC/HEIF (which the `image` crate can't read)
/// is converted to JPEG with macOS `sips` first. Blocking.
pub fn prepare_image_file(path: &Path) -> Result<PreparedImage> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    match prepare_image(&data) {
        Ok(p) => Ok(p),
        Err(e) if is_heif(&data) => {
            log::debug!("{e:#}; converting HEIF with sips");
            prepare_image(&heif_to_jpeg(path)?)
        }
        Err(e) => Err(e),
    }
}

/// ISO-BMFF `ftyp` box with a HEIF brand.
fn is_heif(data: &[u8]) -> bool {
    data.len() >= 12
        && &data[4..8] == b"ftyp"
        && matches!(&data[8..12], b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1" | b"avif")
}

fn heif_to_jpeg(path: &Path) -> Result<Vec<u8>> {
    let out = std::env::temp_dir().join(format!("wa-heic-{}.jpg", crate::new_message_id()));
    let status = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "jpeg", "-s", "formatOptions", "90"])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .status()
        .context("running sips")?;
    let data = if status.success() { std::fs::read(&out).context("reading sips output") } else {
        Err(anyhow::anyhow!("sips could not convert {} ({status})", path.display()))
    };
    let _ = std::fs::remove_file(&out);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255])));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn prepares_png_as_jpeg_with_thumbnail() {
        let p = prepare_image(&png(300, 150)).unwrap();
        assert_eq!((p.width, p.height), (300, 150));
        assert!(p.jpeg.starts_with(&[0xFF, 0xD8]));
        let thumb = image::load_from_memory(&p.thumbnail).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (72, 36));
        assert!(prepare_image(b"nope").is_err());

        let big = prepare_image(&png(3000, 1000)).unwrap();
        assert_eq!((big.width, big.height), (2560, 853));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wa-media-{tag}-{}", crate::new_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn webp_lossless(color: [u8; 4], w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(color));
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(&img, w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = fourcc.to_vec();
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn u24(v: u32) -> [u8; 3] {
        let b = v.to_le_bytes();
        [b[0], b[1], b[2]]
    }

    /// A two-frame animated WebP (red, then blue) built from lossless single frames.
    fn animated_webp(w: u32, h: u32) -> Vec<u8> {
        let mut vp8x = vec![0x10 | 0x02, 0, 0, 0]; // alpha + animation
        vp8x.extend(u24(w - 1));
        vp8x.extend(u24(h - 1));
        let mut body = b"WEBP".to_vec();
        body.extend(chunk(b"VP8X", &vp8x));
        body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
        for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
            let still = webp_lossless(color, w, h);
            let mut frame = Vec::new();
            frame.extend(u24(0));
            frame.extend(u24(0));
            frame.extend(u24(w - 1));
            frame.extend(u24(h - 1));
            frame.extend(u24(100));
            frame.push(0);
            frame.extend_from_slice(&still[12..]); // the VP8L chunk
            body.extend(chunk(b"ANMF", &frame));
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn stickers_become_png() {
        let still = sticker_to_png(&webp_lossless([10, 200, 30, 255], 8, 6)).unwrap();
        assert!(!still.animated);
        assert_eq!((still.width, still.height), (8, 6));
        let png = image::load_from_memory_with_format(&still.png, image::ImageFormat::Png).unwrap();
        assert_eq!(png.to_rgba8().get_pixel(0, 0).0, [10, 200, 30, 255]);

        let anim = sticker_to_png(&animated_webp(4, 4)).unwrap();
        assert!(anim.animated);
        let first = image::load_from_memory(&anim.png).unwrap().to_rgba8();
        // Red, not blue (compositing may round 255 to 254).
        let [r, _, b, _] = first.get_pixel(2, 2).0;
        assert!(r > 250 && b < 5, "not the first frame: {:?}", first.get_pixel(2, 2));

        // PNG stickers pass through; garbage fails.
        assert!(!sticker_to_png(&png_bytes(3, 3)).unwrap().animated);
        assert!(sticker_to_png(b"RIFF....WEBPjunk").is_err());
        assert!(sticker_to_png(b"").is_err());
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        png(w, h)
    }

    #[test]
    fn document_names_are_safe() {
        assert_eq!(safe_file_name("report.pdf", None), "report.pdf");
        assert_eq!(safe_file_name("../../etc/passwd", None), "_.._etc_passwd");
        assert_eq!(safe_file_name("..", Some("application/pdf")), "Document.pdf");
        assert_eq!(safe_file_name(".hidden", None), "hidden");
        assert_eq!(safe_file_name("a:b\nc\\d", None), "a_b_c_d");
        assert_eq!(safe_file_name("  ", Some("application/pdf")), "Document.pdf");
        assert_eq!(safe_file_name("Invoice", Some("application/pdf; charset=binary")), "Invoice.pdf");
        assert_eq!(safe_file_name("Invoice", Some("application/x-unknown")), "Invoice");
        let long = format!("{}.pdf", "é".repeat(200));
        let cut = safe_file_name(&long, None);
        assert!(cut.len() <= 255 && cut.ends_with(".pdf"), "{cut}");
    }

    #[test]
    fn unique_names_and_private_permissions() {
        let dir = temp_dir("docs");
        let docs = dir.join("media/chat/documents");
        let a = reserve_unique(&docs, "a.pdf").unwrap();
        let b = reserve_unique(&docs, "a.pdf").unwrap();
        let c = reserve_unique(&docs, "noext").unwrap();
        let d = reserve_unique(&docs, "noext").unwrap();
        assert_eq!(a.file_name().unwrap(), "a.pdf");
        assert_eq!(b.file_name().unwrap(), "a (2).pdf");
        assert_eq!(d.file_name().unwrap(), "noext (2)");
        for f in [&a, &b, &c, &d] {
            assert_eq!(mode(f), 0o600);
        }
        assert_eq!(mode(&docs), 0o700);
        assert_eq!(mode(&dir.join("media")), 0o700);

        // Atomic writes are owner-only, also over an existing world-readable file.
        let f = dir.join("x/y.bin");
        write_atomic(&f, b"1").unwrap();
        assert_eq!(mode(&f), 0o600);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&f, b"2").unwrap();
        assert_eq!((mode(&f), std::fs::read(&f).unwrap()), (0o600, b"2".to_vec()));

        // Fixing up an old tree: files 0600, dirs 0700, symlinks not followed.
        let outside = temp_dir("outside");
        let target = outside.join("not-ours.txt");
        std::fs::write(&target, b"x").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let old = dir.join("old");
        std::fs::create_dir_all(old.join("sub")).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(old.join("sub/f"), b"x").unwrap();
        std::fs::set_permissions(old.join("sub/f"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, old.join("link")).unwrap();
        restrict_tree(&dir);
        assert_eq!((mode(&old), mode(&old.join("sub")), mode(&old.join("sub/f"))), (0o700, 0o700, 0o600));
        assert_eq!(mode(&target), 0o644);

        create_private_file(&dir.join("db")).unwrap();
        assert_eq!(mode(&dir.join("db")), 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn paths_and_atomic_write() {
        let dir = std::env::temp_dir().join(format!("wa-media-test-{}", crate::new_message_id()));
        let p = media_file(&dir, "1203-4@g.us", "ABC/../x", "jpg");
        assert!(p.ends_with("media/1203-4@g.us/ABC_.._x.jpg"));
        write_atomic(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        // Only the file itself: no temporary left behind.
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(ext_for_mime(Some("audio/ogg; codecs=opus"), "bin"), "ogg");
        assert_eq!(ext_for_mime(Some("video/mp4"), "bin"), "mp4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn longest_document_names_download_and_dedupe() {
        // A 255-byte name: its download temp file and its " (n)" copies must still fit
        // (ENAMETOOLONG before).
        let dir = temp_dir("long");
        let name = safe_file_name(&format!("{}.pdf", "a".repeat(400)), None);
        assert_eq!(name.len(), MAX_NAME_BYTES);
        let first = reserve_unique(&dir, &name).unwrap();
        let tmp = temp_path(&first);
        assert!(tmp.file_name().unwrap().len() < 64);
        std::fs::write(&tmp, b"pdf").unwrap();
        std::fs::rename(&tmp, &first).unwrap();
        let mut seen = std::collections::HashSet::from([first.clone()]);
        for n in 2..=12 {
            let next = reserve_unique(&dir, &name).unwrap();
            let file = next.file_name().unwrap().to_str().unwrap();
            assert!(file.len() <= MAX_NAME_BYTES && file.ends_with(&format!(" ({n}).pdf")), "{file}");
            assert!(seen.insert(next));
        }
        // Temp names are unique per call, and the guard removes them.
        assert_ne!(temp_path(&first), temp_path(&first));
        let guard = TempFile::new(temp_path(&first));
        std::fs::write(guard.path(), b"x").unwrap();
        let path = guard.path().to_owned();
        drop(guard);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
