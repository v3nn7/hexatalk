//! Screen/window capture for screen sharing during a call.
//!
//! There's no real video codec in this build (the no-C-toolchain constraint
//! rules out VP8/H264 encoders, same as it ruled out Opus for audio — see
//! adpcm.rs). Instead,
//! frames are captured, downscaled and JPEG-encoded with the pure-Rust
//! `image` crate (used via xcap's own re-export, so it's guaranteed to be
//! the same crate instance xcap's `RgbaImage` return values come from), and
//! handed to `call.rs`, which chunks them over the call's WebRTC data
//! channel. It's a deliberately low-bandwidth, low-framerate compromise --
//! good enough to show a shared screen or window, not a substitute for a
//! real video track.
//!
//! System audio is best-effort: we look for a loopback-style capture device
//! (Stereo Mix / What U Hear / VB-Cable / WASAPI loopback names) via cpal
//! and stream mono PCM chunks over the share data channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use webrtc::data_channel::RTCDataChannel;
use xcap::image::codecs::jpeg::JpegEncoder;
use xcap::image::{DynamicImage, ExtendedColorType, imageops::FilterType};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ShareTarget {
    Monitor(String),
    Window(String),
}

impl ShareTarget {
    pub(crate) fn label(&self) -> &str {
        match self {
            ShareTarget::Monitor(name) => name,
            ShareTarget::Window(title) => title,
        }
    }
}

/// Enumerates monitors and visible, titled windows as candidate share
/// targets. Best-effort: any entry whose properties can't be read is
/// skipped rather than failing the whole list.
pub(crate) fn list_share_targets() -> Vec<ShareTarget> {
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

/// A share target resolved to a live capture handle. Resolving (enumerating
/// every monitor/window via `xcap::*::all()`) per frame was a measurable
/// chunk of the 8 fps frame budget, so the handle is cached by the capture
/// thread and only re-resolved after capture failures (window moved to
/// another virtual desktop, transient OS hiccup, ...).
enum ResolvedTarget {
    Monitor(xcap::Monitor),
    Window(xcap::Window),
}

fn resolve_target(target: &ShareTarget) -> Option<ResolvedTarget> {
    match target {
        ShareTarget::Monitor(name) => xcap::Monitor::all()
            .ok()?
            .into_iter()
            .find(|m| m.name().map(|n| &n == name).unwrap_or(false))
            .map(ResolvedTarget::Monitor),
        ShareTarget::Window(title) => xcap::Window::all()
            .ok()?
            .into_iter()
            .find(|w| w.title().map(|t| &t == title).unwrap_or(false))
            .map(ResolvedTarget::Window),
    }
}

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

fn capture_once(target: &ResolvedTarget) -> Option<Vec<u8>> {
    let (mut rgba, origin_x, origin_y) = match target {
        ResolvedTarget::Monitor(monitor) => {
            let image = monitor.capture_image().ok()?;
            (image, monitor.x().unwrap_or(0), monitor.y().unwrap_or(0))
        }
        ResolvedTarget::Window(window) => {
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
    // A 1280px-wide JPEG at quality 55 lands well under 100 KB; pre-sizing
    // avoids the encode loop's repeated growth reallocations.
    let mut buf = Vec::with_capacity(96 * 1024);
    let mut encoder = JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
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
pub(crate) fn spawn_capture_thread(
    target: ShareTarget,
    frame_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        // Fractional-fps-safe pacing (`from_millis(1000 / TARGET_FPS)`
        // truncates, e.g. 7 fps would pace at 142 ms instead of ~142.86 ms).
        let interval = Duration::from_secs_f64(1.0 / TARGET_FPS as f64);
        // Resolved lazily and re-resolved about once a second while captures
        // keep failing, so a transiently-uncapturable target (minimized
        // window, locked screen) recovers without a full restart.
        let mut resolved = resolve_target(&target);
        let mut consecutive_failures = 0u32;
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let frame = resolved.as_ref().and_then(capture_once);
            match frame {
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
                    if consecutive_failures % (TARGET_FPS as u32) == 0 {
                        resolved = resolve_target(&target);
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

const MSG_KIND_SYS_AUDIO: u8 = 4;

/// Resampling accumulator for loopback capture: downsamples the device rate
/// to the 24 kHz wire rate by integer-step decimation and ships 20 ms
/// (480-sample) mono chunks. Lives entirely inside the cpal callback
/// closure — the old version shared its phase/buffer through `Arc<Mutex>`,
/// which cost a lock/unlock pair per *sample* (~48k locks/sec) for state
/// that only ever had one accessor.
struct SysAudioSink {
    step: f32,
    phase: f32,
    pcm: Vec<i16>,
    msg_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl SysAudioSink {
    fn new(
        sample_rate: u32,
        msg_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
        enabled: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            step: (sample_rate as f32 / 24_000.0).max(1.0),
            phase: 0.0,
            pcm: Vec::with_capacity(480),
            msg_tx,
            enabled,
            stop,
        }
    }

    fn push(&mut self, mono: f32) {
        if self.stop.load(Ordering::Relaxed) || !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        self.phase += 1.0;
        if self.phase < self.step {
            return;
        }
        self.phase -= self.step;
        let s = (mono.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        self.pcm.push(s);
        if self.pcm.len() >= 480 {
            let msg = encode_sys_audio_chunk(&self.pcm);
            self.pcm.clear();
            let _ = self.msg_tx.send(msg);
        }
    }
}

fn looks_like_loopback(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("stereo mix")
        || n.contains("what u hear")
        || n.contains("wave out mix")
        || n.contains("loopback")
        || n.contains("cable output")
        || n.contains("vb-audio")
        || n.contains("voicemeeter")
}

/// Try to open a system-audio (loopback-style) capture device and stream
/// mono i16 chunks over the share data channel while `enabled` is true.
/// Returns `Some(())` if a thread was started (device may still fail open).
pub(crate) fn spawn_system_audio_thread(
    dc: Arc<RTCDataChannel>,
    stop: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
) -> Option<()> {
    let host = cpal::default_host();
    let device = host
        .input_devices()
        .ok()?
        .find(|d| d.name().map(|n| looks_like_loopback(&n)).unwrap_or(false))?;
    let config = device.default_input_config().ok()?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    // Queue of encoded sys-audio messages; a tiny async pump on the runtime
    // drains it onto the data channel (cpal callbacks are not async).
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    {
        let dc = Arc::clone(&dc);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while let Some(bytes) = msg_rx.recv().await {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = dc.send(&bytes).await;
            }
        });
    }

    std::thread::spawn(move || {
        let err_fn = |e| eprintln!("[sys-audio] stream error: {e}");
        let make_sink = || {
            SysAudioSink::new(
                sample_rate,
                msg_tx.clone(),
                Arc::clone(&enabled),
                Arc::clone(&stop),
            )
        };

        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let mut sink = make_sink();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _| {
                        for frame in data.chunks(channels.max(1)) {
                            let mono =
                                frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32;
                            sink.push(mono);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut sink = make_sink();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _| {
                        for frame in data.chunks(channels.max(1)) {
                            let mono = frame.iter().map(|&s| s as f32).sum::<f32>()
                                / frame.len().max(1) as f32
                                / i16::MAX as f32;
                            sink.push(mono);
                        }
                    },
                    err_fn,
                    None,
                )
            }
            _ => {
                eprintln!("[sys-audio] unsupported sample format on loopback device");
                return;
            }
        };

        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[sys-audio] could not open loopback: {e}");
                return;
            }
        };
        if let Err(e) = stream.play() {
            eprintln!("[sys-audio] play failed: {e}");
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(stream);
    });
    Some(())
}

fn encode_sys_audio_chunk(samples: &[i16]) -> Bytes {
    let n = samples.len().min(u16::MAX as usize) as u16;
    let mut msg = Vec::with_capacity(1 + 2 + samples.len() * 2);
    msg.push(MSG_KIND_SYS_AUDIO);
    msg.extend_from_slice(&n.to_le_bytes());
    for &s in samples.iter().take(n as usize) {
        msg.extend_from_slice(&s.to_le_bytes());
    }
    Bytes::from(msg)
}
