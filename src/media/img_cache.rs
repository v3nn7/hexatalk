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
use std::collections::HashMap;
use std::sync::Arc;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

thread_local! {
    static DECODED: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
}

/// Decode raw image bytes into a Slint `Image`, or `None` if undecodable.
pub(crate) fn decode(bytes: &[u8]) -> Option<Image> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let raw = rgba.into_raw();
    let pixels: Vec<Rgba8Pixel> = raw
        .chunks_exact(4)
        .map(|c| Rgba8Pixel {
            r: c[0],
            g: c[1],
            b: c[2],
            a: c[3],
        })
        .collect();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_slice().copy_from_slice(&pixels);
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
    DECODED.with(|d| d.borrow_mut().insert(url.to_string(), img.clone()));
    Some(img)
}
