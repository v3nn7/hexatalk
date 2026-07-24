//! Camera-based scanner for server-invite QR codes (see `qr.rs` for the
//! matching generator). Runs on a dedicated OS thread -- camera I/O is
//! blocking and doesn't belong inside an async task, mirroring
//! `screenshare::spawn_capture_thread`.
//!
//! Camera access is the most platform-fragile part of this feature
//! (permissions, missing device, differing native backends per OS). Every
//! failure path here degrades to a `QrScanEvent::Error` rather than a
//! panic or a silent retry loop, so the UI can always fall back to the
//! pre-existing manual "enter code" field
//! (`channels:joinByInviteCode`) instead of getting stuck on a dead
//! camera.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};

/// ~5 fps: fast enough to feel live, slow enough that decode (a few ms on a
/// small frame) never backs up behind capture.
const SCAN_INTERVAL: Duration = Duration::from_millis(200);
/// JPEG quality for the live preview frame -- readability of the QR pattern
/// matters more than photo quality, and lower quality keeps frames small
/// over the channel (same convention as the screenshare preview).
const PREVIEW_JPEG_QUALITY: u8 = 70;
/// ~6s of consecutive read failures before giving up and reporting Error
/// instead of spinning forever on a camera that stopped responding.
const MAX_CONSECUTIVE_FAILURES: u32 = 30;

pub(crate) enum QrScanEvent {
    /// One camera frame, JPEG-encoded (same convention as
    /// `screenshare::spawn_capture_thread`'s frames) for a live preview
    /// widget. Decode with `img_cache::decode` on the UI thread.
    Preview(Vec<u8>),
    /// A QR code was found and decoded in a frame. The scan thread exits
    /// after sending this -- the caller re-spawns it to scan again.
    Decoded(String),
    /// Camera unavailable, permission denied, or repeated read failures.
    /// Terminal -- the thread exits; the UI must offer manual code entry.
    Error(String),
}

/// Opens the system default camera and scans for a server-invite QR code
/// until a code is decoded, `stop` is set, or the camera fails
/// persistently.
pub(crate) fn spawn_qr_scan_thread(
    tx: tokio::sync::mpsc::UnboundedSender<QrScanEvent>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let format =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
        let mut camera = match Camera::new(CameraIndex::Index(0), format) {
            Ok(c) => c,
            Err(err) => {
                let _ = tx.send(QrScanEvent::Error(format!("no camera available: {err}")));
                return;
            }
        };
        if let Err(err) = camera.open_stream() {
            let _ = tx.send(QrScanEvent::Error(format!(
                "could not start camera stream: {err}"
            )));
            return;
        }

        let mut consecutive_failures = 0u32;
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            match camera.frame().and_then(|buf| buf.decode_image::<RgbFormat>()) {
                Ok(rgb) => {
                    consecutive_failures = 0;
                    let gray = image::DynamicImage::ImageRgb8(rgb.clone()).into_luma8();
                    let mut prepared = rqrr::PreparedImage::prepare(gray);
                    let decoded = prepared
                        .detect_grids()
                        .into_iter()
                        .find_map(|grid| grid.decode().ok().map(|(_meta, content)| content));
                    if let Some(content) = decoded {
                        let _ = tx.send(QrScanEvent::Decoded(content));
                        break;
                    }
                    let mut jpeg = Vec::new();
                    let encoded = JpegEncoder::new_with_quality(&mut jpeg, PREVIEW_JPEG_QUALITY)
                        .encode_image(&rgb)
                        .is_ok();
                    if encoded && tx.send(QrScanEvent::Preview(jpeg)).is_err() {
                        break; // receiver gone -- UI closed the scan screen
                    }
                }
                Err(err) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        let _ = tx.send(QrScanEvent::Error(format!(
                            "camera read failed repeatedly: {err}"
                        )));
                        break;
                    }
                }
            }
            let elapsed = started.elapsed();
            if elapsed < SCAN_INTERVAL {
                std::thread::sleep(SCAN_INTERVAL - elapsed);
            }
        }

        let _ = camera.stop_stream();
    });
}
