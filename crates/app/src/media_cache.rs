//! A bounded image cache for the transcript's photos and stickers. Without one, GPUI keeps
//! every image it loads from a file in a global cache, decoded at full size, for the app's
//! lifetime, so memory grows with every chat scrolled through. This one decodes photos at
//! the size they're shown, keeps the most recently drawn images up to a budget and frees
//! the rest (memory and GPU textures); scrolled back into view, an image loads again.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::*;

/// Decoded bytes kept for images not on screen.
const BUDGET: usize = 64 << 20;
/// Images drawn this recently may be on screen: never evicted.
const IN_USE: Duration = Duration::from_secs(1);
/// Check the budget every this many loads (they come once per image per frame).
const CHECK_EVERY: u64 = 32;
/// Photos are decoded no larger than this (a bubble is at most 340 pt tall, at 2×).
const MAX_PX: u32 = 720;

type Loaded = Result<Arc<RenderImage>, ImageCacheError>;

pub struct MediaImageCache {
    entries: HashMap<u64, Entry>,
    loads: u64,
}

struct Entry {
    load: Load,
    last_used: Instant,
}

enum Load {
    /// Animated stickers and anything else: GPUI's own loader.
    Gpui(ImageCacheItem),
    /// A photo decoded at display size in the background (the task fills the slot).
    Scaled(Rc<RefCell<Option<Loaded>>>, #[allow(dead_code)] Task<()>),
}

impl Load {
    fn get(&self) -> Option<Loaded> {
        match self {
            Load::Gpui(item) => item.get(),
            Load::Scaled(slot, _) => slot.borrow().clone(),
        }
    }
}

fn decoded_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count())
        .map(|i| {
            let size = image.size(i);
            size.width.0.max(0) as usize * size.height.0.max(0) as usize * 4
        })
        .sum()
}

fn loaded(entry: &Entry) -> Option<Arc<RenderImage>> {
    entry.load.get().and_then(Result::ok)
}

/// Still photos, which are decoded scaled; others (animated WebP, GIF) keep every frame.
fn scalable(resource: &Resource) -> Option<Arc<Path>> {
    let Resource::Path(path) = resource else { return None };
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png").then(|| path.clone())
}

/// Decode `path` no larger than [`MAX_PX`], upright, in GPUI's pixel order (BGRA).
fn decode_scaled(path: &Path) -> anyhow::Result<RenderImage> {
    use image::ImageDecoder as _;
    let mut decoder = image::ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    if image.width() > MAX_PX || image.height() > MAX_PX {
        image = image.thumbnail(MAX_PX, MAX_PX);
    }
    image.apply_orientation(orientation);
    let mut data = image.into_rgba8();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(RenderImage::new(vec![image::Frame::new(data)]))
}

impl MediaImageCache {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let cache = cx.new(|_| MediaImageCache { entries: HashMap::new(), loads: 0 });
        cx.observe_release(&cache, |cache, cx| {
            for (_, entry) in cache.entries.drain() {
                if let Some(image) = loaded(&entry) {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        cache
    }

    /// Over budget: free the least recently drawn images that aren't on screen.
    fn evict(&mut self, window: &mut Window, cx: &mut App) {
        let mut total: usize = self.entries.values().filter_map(loaded).map(|i| decoded_bytes(&i)).sum();
        if total <= BUDGET {
            return;
        }
        let now = Instant::now();
        let mut idle: Vec<(u64, Instant)> = self
            .entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.last_used) > IN_USE)
            .map(|(key, e)| (*key, e.last_used))
            .collect();
        idle.sort_by_key(|(_, used)| *used);
        for (key, _) in idle {
            if total <= BUDGET {
                break;
            }
            if let Some(image) = self.entries.remove(&key).as_ref().and_then(loaded) {
                total = total.saturating_sub(decoded_bytes(&image));
                cx.drop_image(image, Some(window));
            }
        }
    }
}

/// Start decoding a photo at display size; the view drawing it redraws once it's ready.
fn load_scaled(path: Arc<Path>, window: &Window, cx: &mut App) -> Load {
    let slot: Rc<RefCell<Option<Loaded>>> = Rc::default();
    let view = window.current_view();
    let filled = slot.clone();
    let task = cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { decode_scaled(&path) }).await;
        *filled.borrow_mut() = Some(result.map(Arc::new).map_err(|e| ImageCacheError::Other(Arc::new(e))));
        cx.update(|cx| cx.notify(view));
    });
    Load::Scaled(slot, task)
}

impl ImageCache for MediaImageCache {
    fn load(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) -> Option<Loaded> {
        let now = Instant::now();
        let entry = self.entries.entry(hash(resource)).or_insert_with(|| {
            let load = match scalable(resource) {
                Some(path) => load_scaled(path, window, cx),
                None => Load::Gpui(ImageCacheItem::new(resource, cx)),
            };
            Entry { load, last_used: now }
        });
        entry.last_used = now;
        let result = match &entry.load {
            Load::Gpui(item) => item.use_image(window),
            Load::Scaled(..) => entry.load.get(),
        };
        self.loads += 1;
        if self.loads.is_multiple_of(CHECK_EVERY) {
            self.evict(window, cx);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's prelude has its own `test` attribute.
    use super::{MAX_PX, decode_scaled, scalable};
    use gpui_kit::Resource;

    #[test]
    fn photos_decode_at_display_size() {
        let dir = std::env::temp_dir().join(format!("wa-media-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.jpg");
        // A wide photo, red: scaled to fit MAX_PX, red ends up in the B slot (BGRA).
        image::RgbImage::from_pixel(1600, 1200, image::Rgb([255, 0, 0])).save(&path).unwrap();
        let image = decode_scaled(&path).unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (MAX_PX as i32, 540));
        let pixel = &image.as_bytes(0).unwrap()[..4];
        assert!(pixel[2] > 200 && pixel[0] < 50, "BGRA order: {pixel:?}");
        assert_eq!(scalable(&Resource::Path(path.clone().into())).as_deref(), Some(path.as_path()));
        assert!(scalable(&Resource::Path(dir.join("sticker.webp").into())).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
