//! HD video-call media plane over an E2E [`crate::node::Session`].
//!
//! ## Design
//!
//! - Signaling + media share the same Noise-encrypted channel (no cleartext on relay).
//! - Full coded video frames may be **up to 25 MiB** (auto-fragmented under Noise 64 KiB).
//! - Codecs are negotiated by name; default production-friendly path is **JPEG @ 720p/1080p**
//!   for universal decode, plus wire formats for **H264 / VP8 / Opus** when the app supplies
//!   encoded bitstreams (ffmpeg, hardware encoder, browser, etc.).
//!
//! Capture/encode/decode stay in the application (or optional helpers here for test patterns).

use crate::error::{Error, Result};
use crate::protocol::AppMessage;
use std::time::{Duration, Instant};

/// Video codec identifiers on the wire (`VcVideo.codec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VideoCodec {
    /// Motion JPEG / single JPEG images (widely decodable).
    Jpeg = 1,
    /// H.264 Annex-B access unit.
    H264 = 2,
    /// VP8 frame.
    Vp8 = 3,
    /// VP9 frame.
    Vp9 = 4,
    /// AV1 frame.
    Av1 = 5,
    /// Raw RGB24 (testing only — huge).
    RawRgb24 = 10,
    /// Unknown / app-specific.
    Other = 255,
}

impl VideoCodec {
    /// Parse from wire u8.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Jpeg,
            2 => Self::H264,
            3 => Self::Vp8,
            4 => Self::Vp9,
            5 => Self::Av1,
            10 => Self::RawRgb24,
            _ => Self::Other,
        }
    }

    /// Codec name for offer/answer.
    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::H264 => "h264",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
            Self::RawRgb24 => "raw_rgb24",
            Self::Other => "other",
        }
    }

    /// Parse name.
    pub fn from_name(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "jpeg" | "mjpeg" | "image/jpeg" => Self::Jpeg,
            "h264" | "avc" | "video/h264" => Self::H264,
            "vp8" => Self::Vp8,
            "vp9" => Self::Vp9,
            "av1" => Self::Av1,
            "raw_rgb24" | "rgb24" => Self::RawRgb24,
            _ => Self::Other,
        }
    }
}

/// Audio codec identifiers on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    /// Opus packets.
    Opus = 1,
    /// PCM signed 16-bit little-endian.
    PcmS16Le = 2,
    /// Other.
    Other = 255,
}

impl AudioCodec {
    /// From wire.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Opus,
            2 => Self::PcmS16Le,
            _ => Self::Other,
        }
    }

    /// Name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::PcmS16Le => "pcm_s16le",
            Self::Other => "other",
        }
    }

    /// From name.
    pub fn from_name(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "opus" | "audio/opus" => Self::Opus,
            "pcm_s16le" | "pcm" | "s16le" => Self::PcmS16Le,
            _ => Self::Other,
        }
    }
}

/// HD profile presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdProfile {
    /// 1280×720
    P720,
    /// 1920×1080
    P1080,
    /// 2560×1440
    P1440,
    /// Custom size stored in config.
    Custom,
}

impl HdProfile {
    /// Width × height.
    pub fn size(self) -> (u32, u32) {
        match self {
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
            Self::Custom => (0, 0),
        }
    }

    /// Profile label.
    pub fn label(self, fps: u32) -> String {
        match self {
            Self::P720 => format!("720p{fps}"),
            Self::P1080 => format!("1080p{fps}"),
            Self::P1440 => format!("1440p{fps}"),
            Self::Custom => format!("custom{fps}"),
        }
    }
}

/// Local VC send configuration.
#[derive(Debug, Clone)]
pub struct VcConfig {
    /// Resolution profile.
    pub profile: HdProfile,
    /// Explicit width (used when profile is Custom, or overrides).
    pub width: u32,
    /// Explicit height.
    pub height: u32,
    /// Target frames per second.
    pub fps: u32,
    /// Video codec.
    pub video_codec: VideoCodec,
    /// Audio codec.
    pub audio_codec: AudioCodec,
    /// Audio sample rate.
    pub sample_rate: u32,
    /// Audio channels.
    pub channels: u8,
    /// Target video bitrate hint (bps), 0 = unlimited / encoder default.
    pub video_bitrate_bps: u32,
}

impl Default for VcConfig {
    fn default() -> Self {
        Self::hd_1080p30()
    }
}

impl VcConfig {
    /// 720p30 JPEG default (good over relay).
    pub fn hd_720p30() -> Self {
        let (w, h) = HdProfile::P720.size();
        Self {
            profile: HdProfile::P720,
            width: w,
            height: h,
            fps: 30,
            video_codec: VideoCodec::Jpeg,
            audio_codec: AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 1,
            video_bitrate_bps: 2_500_000,
        }
    }

    /// 1080p30 JPEG / H264-ready default.
    pub fn hd_1080p30() -> Self {
        let (w, h) = HdProfile::P1080.size();
        Self {
            profile: HdProfile::P1080,
            width: w,
            height: h,
            fps: 30,
            video_codec: VideoCodec::Jpeg,
            audio_codec: AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 1,
            video_bitrate_bps: 4_000_000,
        }
    }

    /// 1080p30 with H.264 name (app must supply H264 NAL units).
    pub fn hd_1080p30_h264() -> Self {
        let mut c = Self::hd_1080p30();
        c.video_codec = VideoCodec::H264;
        c.video_bitrate_bps = 3_500_000;
        c
    }

    /// Wire profile string.
    pub fn profile_label(&self) -> String {
        self.profile.label(self.fps)
    }
}

/// One outbound / inbound video frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Presentation time (ms since arbitrary epoch, usually call start).
    pub pts_ms: u64,
    /// Sequence number.
    pub seq: u32,
    /// Width.
    pub width: u16,
    /// Height.
    pub height: u16,
    /// Codec.
    pub codec: VideoCodec,
    /// Keyframe / IDR.
    pub keyframe: bool,
    /// Encoded payload.
    pub data: Vec<u8>,
}

/// One audio frame / packet.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// PTS ms.
    pub pts_ms: u64,
    /// Sequence.
    pub seq: u32,
    /// Codec.
    pub codec: AudioCodec,
    /// Sample rate.
    pub sample_rate: u32,
    /// Channels.
    pub channels: u8,
    /// Encoded or PCM payload.
    pub data: Vec<u8>,
}

/// VC control kinds (`VcControl.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VcControlKind {
    /// Picture loss indication — please send a keyframe.
    RequestKeyframe = 1,
    /// Hang up / end media.
    Bye = 2,
    /// Suggest max bitrate (value = bps).
    MaxBitrate = 3,
    /// Receiver ready.
    ReceiverReady = 4,
}

impl VcControlKind {
    /// From wire.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::RequestKeyframe),
            2 => Some(Self::Bye),
            3 => Some(Self::MaxBitrate),
            4 => Some(Self::ReceiverReady),
            _ => None,
        }
    }
}

/// Events demuxed by [`VcCall::handle_app`] / session loop.
#[derive(Debug, Clone)]
pub enum VcEvent {
    /// Remote offered a call.
    Offer(VcConfig),
    /// Remote accepted.
    Answer(VcConfig),
    /// Remote video frame.
    Video(VideoFrame),
    /// Remote audio frame.
    Audio(AudioFrame),
    /// Control.
    Control {
        /// Kind.
        kind: VcControlKind,
        /// Value.
        value: u32,
    },
    /// Non-VC app message (chat etc.) — bubble up.
    Other(AppMessage),
}

/// Stateless helpers to build/parse VC app messages + call timing.
#[derive(Debug)]
pub struct VcCall {
    /// Local config.
    pub local: VcConfig,
    /// Negotiated / remote config after answer.
    pub remote: Option<VcConfig>,
    /// Call start for PTS.
    pub started: Instant,
    video_seq: u32,
    audio_seq: u32,
    active: bool,
}

impl VcCall {
    /// New call with local send config.
    pub fn new(local: VcConfig) -> Self {
        Self {
            local,
            remote: None,
            started: Instant::now(),
            video_seq: 0,
            audio_seq: 0,
            active: false,
        }
    }

    /// Milliseconds since call start.
    pub fn pts_now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Frame pacing interval.
    pub fn frame_interval(&self) -> Duration {
        let fps = self.local.fps.max(1);
        Duration::from_micros(1_000_000 / u64::from(fps))
    }

    /// Whether media is active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Build offer message.
    pub fn make_offer(&self) -> AppMessage {
        AppMessage::VcOffer {
            profile: self.local.profile_label(),
            video_codec: self.local.video_codec.name().into(),
            audio_codec: self.local.audio_codec.name().into(),
            width: self.local.width,
            height: self.local.height,
            fps: self.local.fps,
        }
    }

    /// Build answer from remote offer (intersection / accept local preference).
    pub fn make_answer_for(&mut self, offer: &VcConfig) -> AppMessage {
        let width = self.local.width.min(offer.width);
        let height = self.local.height.min(offer.height);
        let fps = self.local.fps.min(offer.fps).max(1);
        // Prefer remote codec if we understand it, else local.
        let video_codec = if offer.video_codec != VideoCodec::Other {
            offer.video_codec
        } else {
            self.local.video_codec
        };
        let audio_codec = if offer.audio_codec != AudioCodec::Other {
            offer.audio_codec
        } else {
            self.local.audio_codec
        };
        let remote = VcConfig {
            profile: HdProfile::Custom,
            width,
            height,
            fps,
            video_codec,
            audio_codec,
            sample_rate: offer.sample_rate.max(self.local.sample_rate),
            channels: offer.channels.max(1),
            video_bitrate_bps: self
                .local
                .video_bitrate_bps
                .min(offer.video_bitrate_bps.max(1)),
        };
        self.remote = Some(remote.clone());
        self.active = true;
        AppMessage::VcAnswer {
            profile: format!("{width}x{height}p{fps}"),
            video_codec: video_codec.name().into(),
            audio_codec: audio_codec.name().into(),
            width,
            height,
            fps,
        }
    }

    /// Apply remote answer.
    pub fn apply_answer(&mut self, cfg: VcConfig) {
        self.remote = Some(cfg);
        self.active = true;
    }

    /// Encode video frame to app message (increments seq).
    pub fn pack_video(&mut self, mut frame: VideoFrame) -> AppMessage {
        if frame.seq == 0 {
            frame.seq = self.video_seq;
            self.video_seq = self.video_seq.wrapping_add(1);
        } else {
            self.video_seq = frame.seq.wrapping_add(1);
        }
        if frame.pts_ms == 0 {
            frame.pts_ms = self.pts_now();
        }
        AppMessage::VcVideo {
            pts_ms: frame.pts_ms,
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            codec: frame.codec as u8,
            flags: if frame.keyframe { 1 } else { 0 },
            data: frame.data,
        }
    }

    /// Encode audio frame.
    pub fn pack_audio(&mut self, mut frame: AudioFrame) -> AppMessage {
        if frame.seq == 0 {
            frame.seq = self.audio_seq;
            self.audio_seq = self.audio_seq.wrapping_add(1);
        }
        if frame.pts_ms == 0 {
            frame.pts_ms = self.pts_now();
        }
        AppMessage::VcAudio {
            pts_ms: frame.pts_ms,
            seq: frame.seq,
            codec: frame.codec as u8,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            data: frame.data,
        }
    }

    /// Keyframe request control.
    pub fn request_keyframe_msg() -> AppMessage {
        AppMessage::VcControl {
            kind: VcControlKind::RequestKeyframe as u8,
            value: 0,
        }
    }

    /// Bye control.
    pub fn bye_msg() -> AppMessage {
        AppMessage::VcControl {
            kind: VcControlKind::Bye as u8,
            value: 0,
        }
    }

    /// Demux an app message into a VC event.
    pub fn handle_app(&mut self, msg: AppMessage) -> Result<VcEvent> {
        Ok(match msg {
            AppMessage::VcOffer {
                profile: _,
                video_codec,
                audio_codec,
                width,
                height,
                fps,
            } => {
                let cfg = VcConfig {
                    profile: HdProfile::Custom,
                    width,
                    height,
                    fps,
                    video_codec: VideoCodec::from_name(&video_codec),
                    audio_codec: AudioCodec::from_name(&audio_codec),
                    sample_rate: 48_000,
                    channels: 1,
                    video_bitrate_bps: 4_000_000,
                };
                VcEvent::Offer(cfg)
            }
            AppMessage::VcAnswer {
                video_codec,
                audio_codec,
                width,
                height,
                fps,
                ..
            } => {
                let cfg = VcConfig {
                    profile: HdProfile::Custom,
                    width,
                    height,
                    fps,
                    video_codec: VideoCodec::from_name(&video_codec),
                    audio_codec: AudioCodec::from_name(&audio_codec),
                    sample_rate: 48_000,
                    channels: 1,
                    video_bitrate_bps: 4_000_000,
                };
                self.apply_answer(cfg.clone());
                VcEvent::Answer(cfg)
            }
            AppMessage::VcVideo {
                pts_ms,
                seq,
                width,
                height,
                codec,
                flags,
                data,
            } => VcEvent::Video(VideoFrame {
                pts_ms,
                seq,
                width,
                height,
                codec: VideoCodec::from_u8(codec),
                keyframe: flags & 1 != 0,
                data,
            }),
            AppMessage::VcAudio {
                pts_ms,
                seq,
                codec,
                sample_rate,
                channels,
                data,
            } => VcEvent::Audio(AudioFrame {
                pts_ms,
                seq,
                codec: AudioCodec::from_u8(codec),
                sample_rate,
                channels,
                data,
            }),
            AppMessage::VcControl { kind, value } => {
                let kind = VcControlKind::from_u8(kind)
                    .ok_or_else(|| Error::Protocol(format!("unknown vc control {kind}")))?;
                if kind == VcControlKind::Bye {
                    self.active = false;
                }
                VcEvent::Control { kind, value }
            }
            other => VcEvent::Other(other),
        })
    }
}

/// Build a **minimal valid JPEG** of solid color (very small) — useful for tests.
///
/// For real HD frames, use a proper encoder (turbojpeg, ffmpeg, hardware).
/// This helper returns a tiny JPEG; scale metadata still carries HD width/height
/// for pipeline testing. Use [`generate_hd_test_pattern_rgb`] + external encoder for bulk.
pub fn minimal_jpeg_bytes() -> Vec<u8> {
    // 1x1 pixel JPEG (red-ish) — standard minimal file
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06,
        0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B,
        0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20,
        0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31,
        0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF,
        0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00,
        0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05,
        0x04, 0x04, 0x00, 0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08,
        0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A,
        0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75,
        0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9,
        0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6,
        0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
        0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7,
        0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x7F, 0xFF,
        0xD9,
    ]
}

/// Generate RGB24 test pattern (gradient + frame number bar) for HD sizes.
///
/// Size = width * height * 3. For 1080p this is ~6.2 MiB raw (fits in 25 MiB logical).
pub fn generate_hd_test_pattern_rgb(width: u32, height: u32, frame_idx: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = vec![0u8; w * h * 3];
    let phase = (frame_idx % 256) as u8;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            buf[i] = ((x * 255 / w.max(1)) as u8).wrapping_add(phase);
            buf[i + 1] = ((y * 255 / h.max(1)) as u8).wrapping_add(phase / 2);
            buf[i + 2] = phase.wrapping_add((x + y) as u8);
        }
        // moving bar
        if y == (frame_idx as usize % h) {
            for x in 0..w {
                let i = (y * w + x) * 3;
                buf[i] = 255;
                buf[i + 1] = 255;
                buf[i + 2] = 255;
            }
        }
    }
    buf
}

/// Wrap RGB (or any payload) as a [`VideoFrame`] with HD metadata.
pub fn video_frame_from_payload(
    width: u32,
    height: u32,
    codec: VideoCodec,
    keyframe: bool,
    pts_ms: u64,
    seq: u32,
    data: Vec<u8>,
) -> VideoFrame {
    VideoFrame {
        pts_ms,
        seq,
        width: width.min(u16::MAX as u32) as u16,
        height: height.min(u16::MAX as u32) as u16,
        codec,
        keyframe,
        data,
    }
}

/// Simple drop-old jitter buffer for video (by seq).
#[derive(Debug, Default)]
pub struct VideoJitterBuffer {
    last_seq: Option<u32>,
    /// Dropped frames count.
    pub dropped: u64,
    /// Received frames count.
    pub received: u64,
}

impl VideoJitterBuffer {
    /// Push frame; returns Some if it should be displayed (not late/dup).
    pub fn push(&mut self, frame: VideoFrame) -> Option<VideoFrame> {
        self.received += 1;
        match self.last_seq {
            None => {
                self.last_seq = Some(frame.seq);
                Some(frame)
            }
            Some(last) => {
                // allow wrap
                let late = frame.seq.wrapping_sub(last) > u32::MAX / 2 && frame.seq < last;
                if late || frame.seq == last {
                    self.dropped += 1;
                    None
                } else {
                    self.last_seq = Some(frame.seq);
                    Some(frame)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_answer_roundtrip_via_handle() {
        let mut a = VcCall::new(VcConfig::hd_1080p30());
        let offer_msg = a.make_offer();
        let mut b = VcCall::new(VcConfig::hd_720p30());
        let ev = b.handle_app(offer_msg).unwrap();
        match ev {
            VcEvent::Offer(cfg) => {
                let ans = b.make_answer_for(&cfg);
                let ev2 = a.handle_app(ans).unwrap();
                assert!(matches!(ev2, VcEvent::Answer(_)));
                assert!(a.is_active());
            }
            _ => panic!("expected offer"),
        }
    }

    #[test]
    fn pack_video_large_payload() {
        let mut c = VcCall::new(VcConfig::hd_1080p30());
        let rgb = generate_hd_test_pattern_rgb(320, 180, 0); // smaller for unit test speed
        let frame = video_frame_from_payload(320, 180, VideoCodec::RawRgb24, true, 0, 0, rgb.clone());
        let msg = c.pack_video(frame);
        let encoded = msg.encode().unwrap();
        let decoded = AppMessage::decode(&encoded).unwrap();
        match decoded {
            AppMessage::VcVideo { data, width, height, .. } => {
                assert_eq!(data, rgb);
                assert_eq!(width, 320);
                assert_eq!(height, 180);
            }
            _ => panic!("not video"),
        }
    }
}
