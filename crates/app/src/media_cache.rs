//! A bounded image cache for the transcript's photos and stickers. Without one, GPUI keeps
//! every image it loads from a file in a global cache, decoded, for the app's lifetime, so
//! memory grows with every chat scrolled through. This keeps the most recently drawn
//! images up to a budget and frees the rest (memory and GPU textures); scrolled back into
//! view, an image just loads again.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::*;

/// Decoded bytes kept for images not on screen.
const BUDGET: usize = 96 << 20;
/// Images drawn this recently may be on screen: never evicted.
const IN_USE: Duration = Duration::from_secs(3);
/// Check the budget every this many loads (they come once per image per frame).
const CHECK_EVERY: u64 = 64;

pub struct MediaImageCache {
    entries: HashMap<u64, Entry>,
    loads: u64,
}

struct Entry {
    item: ImageCacheItem,
    last_used: Instant,
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
    entry.item.get().and_then(Result::ok)
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

impl ImageCache for MediaImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let now = Instant::now();
        let entry = self
            .entries
            .entry(hash(resource))
            .or_insert_with(|| Entry { item: ImageCacheItem::new(resource, cx), last_used: now });
        entry.last_used = now;
        let result = entry.item.use_image(window);
        self.loads += 1;
        if self.loads.is_multiple_of(CHECK_EVERY) {
            self.evict(window, cx);
        }
        result
    }
}
