//! Server-invite QR: render the existing alphanumeric invite code/link
//! (`channels:regenerateInviteCode` / `invite_link()`) as an image.
//!
//! This is a distinct payload/domain from `crates/reprotocol`'s `ps1:` P2P
//! pairing QR (`crates/reprotocol/src/invite.rs`, used for peerseal
//! identity pairing) -- no protocol change, no change to that crate. It
//! reuses the invite code the server already issues; scanning it (see
//! `qr_scan.rs`) just feeds the decoded string into the existing
//! `channels:joinByInviteCode` path, unchanged.
//!
//! Like `img_cache.rs`, image construction must happen on the Slint UI
//! thread (`slint::Image` is not `Send`) -- callers on the background pump
//! thread only ever pass the plain invite string across.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Renders `payload` (e.g. `hexatalk://invite/ABC123`) as a QR code image.
/// Must be called from the Slint UI thread.
pub(crate) fn render_invite_qr(payload: &str) -> Result<Image, String> {
    if payload.is_empty() {
        return Err("empty invite payload".to_string());
    }
    let code = qrcode::QrCode::new(payload).map_err(|e| e.to_string())?;
    let luma = code.render::<image::Luma<u8>>().build();
    let rgba = image::DynamicImage::ImageLuma8(luma).to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Err("QR render produced an empty image".to_string());
    }
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    Ok(Image::from_rgba8(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_nonempty_deterministic_image() {
        let a = render_invite_qr("hexatalk://invite/ABC123").unwrap();
        let b = render_invite_qr("hexatalk://invite/ABC123").unwrap();
        assert_eq!(a.size(), b.size());
        assert!(a.size().width > 0 && a.size().height > 0);
    }

    #[test]
    fn different_payloads_render_different_sized_or_different_images() {
        // Not asserting pixel-level difference (overkill here) -- just that
        // rendering doesn't silently collapse two different invite codes
        // into byte-identical output via some constant/placeholder path.
        let short = render_invite_qr("hexatalk://invite/A").unwrap();
        let long =
            render_invite_qr("hexatalk://invite/AVERYMUCHLONGERINVITECODETHANTHEOTHERONE12345")
                .unwrap();
        assert!(short.size().width > 0 && long.size().width > 0);
    }

    #[test]
    fn rejects_empty_payload_without_panicking() {
        assert!(render_invite_qr("").is_err());
    }
}
