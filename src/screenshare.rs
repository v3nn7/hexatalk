//! Screen/window capture for screen sharing during a call.
//!
//! There's no real video codec in this build -- see g711.rs's comment on
//! why (same C-toolchain constraint rules out VP8/H264 encoders). Instead,
//! frames are captured, downscaled and JPEG-encoded with the pure-Rust
//! `image` crate (used via xcap's own re-export, so it's guaranteed to be
//! the same crate instance xcap's `RgbaImage` return values come from), and
//! handed to `call.rs`, which chunks them over the call's WebRTC data
//! channel. It's a deliberately low-bandwidth, low-framerate compromise --
//! good enough to show a shared screen or window, not a substitute for a
//! real video track.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use xcap::image::codecs::jpeg::JpegEncoder;
use xcap::image::{imageops::FilterType, DynamicImage, ExtendedColorType};

#[derive(Debug, Clone, PartialEq)]
pub enum ShareTarget {
    Monitor(String),
    Window(String),
}

impl ShareTarget {
    pub fn label(&self) -> &str {
        match self {
            ShareTarget::Monitor(name) => name,
            ShareTarget::Window(title) => title,
        }
    }
}

/// Enumerates monitors and visible, titled windows as candidate share
/// targets. Best-effort: any entry whose properties can't be read is
/// skipped rather than failing the whole list.
pub fn list_share_targets() -> Vec<ShareTarget> {
    let mut targets = Vec::new();
    if let Ok(monitors) = xcap::Monitor::all() {
        for monitor in monitors {
            if let Ok(name) = monitor.name() {
                targets.push(ShareTarget::Monitor(name));
            }
        }
    }
    if let Ok(windows) = xcap::Window::all() {
        for window in windows {
            let title = window.title().unwrap_or_default();
            let minimized = window.is_minimized().unwrap_or(true);
            if !minimized && !title.trim().is_empty() {
                targets.push(ShareTarget::Window(title));
            }
        }
    }
    targets
}

const MAX_WIDTH: u32 = 1280;
const JPEG_QUALITY: u8 = 55;
const TARGET_FPS: u64 = 8;

/// xcap doesn't capture the mouse cursor (it's composited by the OS
/// separately from whatever backs the screenshot), so it's drawn in here
/// after the fact: grab the cursor's absolute screen position and paint a
/// small marker at its location relative to the captured region's origin.
/// A synthetic dot rather than the real cursor bitmap -- extracting and
/// compositing the actual cursor icon needs HICON mask/XOR-AND handling
/// that's easy to get subtly wrong and impossible for us to verify visually
/// in this environment, so a clearly-visible marker is the safer trade.
#[cfg(windows)]
fn cursor_screen_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut point = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut point) };
    (ok != 0).then_some((point.x, point.y))
}

#[cfg(not(windows))]
fn cursor_screen_pos() -> Option<(i32, i32)> {
    None
}

fn draw_cursor_marker(img: &mut xcap::image::RgbaImage, x: i32, y: i32) {
    const RADIUS: i32 = 7;
    const RING_INNER: i32 = RADIUS - 2;
    for dy in -RADIUS..=RADIUS {
        for dx in -RADIUS..=RADIUS {
            let dist2 = dx * dx + dy * dy;
            if dist2 > RADIUS * RADIUS {
                continue;
            }
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 || px as u32 >= img.width() || py as u32 >= img.height() {
                continue;
            }
            let color = if dist2 >= RING_INNER * RING_INNER {
                [20, 20, 20, 255]
            } else {
                [255, 221, 0, 255]
            };
            img.put_pixel(px as u32, py as u32, xcap::image::Rgba(color));
        }
    }
}

fn capture_once(target: &ShareTarget) -> Option<Vec<u8>> {
    let (mut rgba, origin_x, origin_y) = match target {
        ShareTarget::Monitor(name) => {
            let monitor = xcap::Monitor::all()
                .ok()?
                .into_iter()
                .find(|m| m.name().map(|n| &n == name).unwrap_or(false))?;
            let image = monitor.capture_image().ok()?;
            (image, monitor.x().unwrap_or(0), monitor.y().unwrap_or(0))
        }
        ShareTarget::Window(title) => {
            let window = xcap::Window::all()
                .ok()?
                .into_iter()
                .find(|w| w.title().map(|t| &t == title).unwrap_or(false))?;
            let image = window.capture_image().ok()?;
            (image, window.x().unwrap_or(0), window.y().unwrap_or(0))
        }
    };

    if let Some((cursor_x, cursor_y)) = cursor_screen_pos() {
        draw_cursor_marker(&mut rgba, cursor_x - origin_x, cursor_y - origin_y);
    }

    let dyn_img: DynamicImage = rgba.into();
    let (width, height) = (dyn_img.width(), dyn_img.height());
    let resized = if width > MAX_WIDTH {
        let new_height = ((height as f32) * (MAX_WIDTH as f32 / width as f32))
            .round()
            .max(1.0) as u32;
        dyn_img.resize(MAX_WIDTH, new_height, FilterType::Triangle)
    } else {
        dyn_img
    };

    let rgb = resized.to_rgb8();
    let mut buf = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    encoder
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
        .ok()?;
    Some(buf)
}

/// After this many consecutive failed capture attempts (target window
/// closed, monitor unplugged, ...) the thread gives up rather than
/// spinning forever -- about 4 seconds at `TARGET_FPS`.
const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = TARGET_FPS as u32 * 4;

/// Runs on a dedicated OS thread until `stop` is set -- screen capture
/// calls are blocking and don't belong inside an async task. Encoded JPEG
/// frames are handed off through `frame_tx`; the async side (`call.rs`)
/// chunks and sends them over the data channel. Exits early (dropping
/// `frame_tx`) if the capture target becomes unavailable, which the
/// receiving side in `call.rs` distinguishes from a deliberate stop so it
/// can tell the user sharing failed instead of just going quiet.
pub fn spawn_capture_thread(
    target: ShareTarget,
    frame_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let interval = Duration::from_millis(1000 / TARGET_FPS);
        let mut consecutive_failures = 0u32;
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            match capture_once(&target) {
                Some(jpeg) => {
                    consecutive_failures = 0;
                    if frame_tx.send(jpeg).is_err() {
                        break;
                    }
                }
                None => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_CAPTURE_FAILURES {
                        break;
                    }
                }
            }
            let elapsed = started.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }
    });
}
