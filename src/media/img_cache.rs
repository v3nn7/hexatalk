//! UI-thread image decoding.
//!
//! `slint::Image` is **not** `Send`, so it cannot travel from the background
//! pump thread (where `App` lives and where avatar/attachment bytes are
//! fetched over the network) into the Slint event-loop thread the way the
//! rest of the snapshot data does. Instead the snapshot carries the raw bytes
//! (`Arc<[u8]>`, which *is* `Send`) and we decode them here -- on the Slint
//! UI thread -- into `slint::Image` values right before they're assigned to a
//! widget property. Decoded images are cached per-URL so we don't re-decode on
//! every UI resync.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Cap on decoded-cache entries. Without a bound the cache grew for the
/// whole session (every avatar + attachment ever shown, at 4 bytes/pixel
/// decoded), which turned long sessions into hundreds of MB of resident
/// decoded bitmaps. Eviction is simple FIFO: entries are looked up far more
/// often right after they first appear than later, and a re-decode on the
/// rare miss is cheap compared to the memory pressure.
const MAX_CACHED_IMAGES: usize = 256;

/// Decoded-image pixel cap (~200 MB RGBA worst case). Guards against a
/// corrupt or hostile file claiming absurd dimensions -- the `image` crate
/// allocates the full RGBA buffer on decode.
const MAX_PIXELS: u64 = 50_000_000;

thread_local! {
    static DECODED: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
    /// Insertion order of `DECODED` keys for FIFO eviction.
    static ORDER: RefCell<VecDeque<String>> = RefCell::new(VecDeque::new());
}

/// Decode raw image bytes into a Slint `Image`, or `None` if undecodable.
pub(crate) fn decode(bytes: &[u8]) -> Option<Image> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
        return None;
    }
    // `clone_from_slice` uploads straight from the RGBA bytes -- the old
    // path built a per-pixel `Vec<Rgba8Pixel>` copy first and then copied
    // *that* into the buffer.
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    Some(Image::from_rgba8(buf))
}

/// Look up (lazily decoding + caching) the image for `url` given the byte
/// cache carried in the snapshot. Returns `None` when there are no bytes yet
/// for that URL, so the caller can fall back to the colored-initial
/// placeholder.
pub(crate) fn image_for(byte_cache: &HashMap<String, Arc<[u8]>>, url: &str) -> Option<Image> {
    if url.is_empty() {
        return None;
    }
    if let Some(img) = DECODED.with(|d| d.borrow().get(url).cloned()) {
        return Some(img);
    }
    let bytes = byte_cache.get(url)?;
    let img = decode(bytes)?;
    DECODED.with(|d| {
        let mut map = d.borrow_mut();
        if map.contains_key(url) {
            return;
        }
        map.insert(url.to_string(), img.clone());
        ORDER.with(|o| {
            let mut order = o.borrow_mut();
            order.push_back(url.to_string());
            while order.len() > MAX_CACHED_IMAGES {
                if let Some(evicted) = order.pop_front() {
                    map.remove(&evicted);
                }
            }
        });
    });
    Some(img)
}
