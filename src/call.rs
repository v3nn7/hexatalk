//! Voice call engine: WebRTC peer connection (Google STUN, trickle ICE),
//! G.711 PCMU audio over a local track, and Convex as the signaling channel
//! (the `calls` table carries the offer/answer SDP; both peers watch the
//! same reactive `calls:myCall` query to learn when the other side answers
//! or hangs up). ICE candidates trickle over `calls:addIceCandidate` /
//! `calls:listPeerIceCandidates` as they're discovered, rather than waiting
//! for gathering to finish before sending the offer/answer at all -- that
//! upfront wait (up to 10s on *each* side, so up to ~20s total) used to be
//! dead air before the other side even saw "incoming call", which read as
//! the whole app lagging. Mic capture and speaker
//! playback each run on a dedicated OS thread because `cpal::Stream` is not
//! `Send` and cannot live inside an async task.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use convex::{ConvexClient, FunctionResult, Value};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::channel::mpsc::Sender as EventSender;
use futures::{SinkExt, StreamExt};
use maplit::btreemap;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_PCMU};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use crate::g711;
use crate::screenshare::{self, ShareTarget};

/// Optional TURN relay, baked in via `build.rs` from `.env.local`/`.env`
/// (`TURN_URL`/`TURN_USERNAME`/`TURN_CREDENTIAL`), overridable at runtime
/// through the same-named environment variables. Absent by default -- the
/// STUN-only ICE servers set up in `spawn_call_engine` handle direct P2P,
/// which works for most network pairs, but two peers both behind symmetric
/// NAT (or a similarly restrictive firewall) can only be bridged by a
/// relay. Setting these three values (e.g. pointing at a self-hosted
/// coturn instance) is the only step needed to add one -- no other code
/// changes required.
pub(crate) fn turn_ice_server() -> Option<RTCIceServer> {
    const BAKED_TURN_URL: &str = env!("TURN_URL");
    const BAKED_TURN_USERNAME: &str = env!("TURN_USERNAME");
    const BAKED_TURN_CREDENTIAL: &str = env!("TURN_CREDENTIAL");

    fn resolve(runtime_key: &str, baked: &str) -> String {
        std::env::var(runtime_key)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| baked.to_string())
    }

    let url = resolve("TURN_URL", BAKED_TURN_URL);
    if url.is_empty() {
        return None;
    }
    let username = resolve("TURN_USERNAME", BAKED_TURN_USERNAME);
    let credential = resolve("TURN_CREDENTIAL", BAKED_TURN_CREDENTIAL);
    if username.is_empty() || credential.is_empty() {
        return None;
    }

    Some(RTCIceServer {
        urls: vec![url],
        username,
        credential,
    })
}

/// Bounds for the adaptive jitter buffer target (see `JitterEstimator`):
/// never so small that ordinary network jitter causes underruns, never so
/// large that a jitter spike adds more latency than a call can tolerate.
const JITTER_TARGET_MIN_MS: f32 = 40.0;
const JITTER_TARGET_MAX_MS: f32 = 500.0;

/// Tracks how irregular packet arrival timing is and derives a target
/// buffer depth from it: a stable connection converges toward
/// `JITTER_TARGET_MIN_MS` of latency, while a bursty one grows the target
/// (up to `JITTER_TARGET_MAX_MS`) to absorb the burstiness instead of
/// glitching. This replaces what used to be a single fixed 500ms cap on
/// the jitter buffer regardless of actual network behavior.
struct JitterEstimator {
    last_arrival: Option<std::time::Instant>,
    last_interval_ms: f32,
    /// RFC 3550-style smoothed mean absolute deviation of inter-packet
    /// arrival intervals.
    jitter_ms: f32,
}

impl JitterEstimator {
    fn new() -> Self {
        Self {
            last_arrival: None,
            // 20ms is this call's nominal frame interval (160 samples at
            // 8kHz); used only as the initial reference before the first
            // real interval is observed.
            last_interval_ms: 20.0,
            jitter_ms: 0.0,
        }
    }

    /// Call once per received packet. Returns the current target buffer
    /// depth in samples (at this call's fixed 8kHz sample rate).
    fn on_packet_arrival(&mut self) -> usize {
        let now = std::time::Instant::now();
        if let Some(last) = self.last_arrival {
            let interval_ms = now.duration_since(last).as_secs_f32() * 1000.0;
            let deviation = (interval_ms - self.last_interval_ms).abs();
            self.jitter_ms += (deviation - self.jitter_ms) / 16.0;
            self.last_interval_ms = interval_ms;
        }
        self.last_arrival = Some(now);

        let target_ms =
            (self.jitter_ms * 4.0 + JITTER_TARGET_MIN_MS).clamp(JITTER_TARGET_MIN_MS, JITTER_TARGET_MAX_MS);
        (target_ms * 8.0) as usize // 8 samples/ms at 8kHz
    }
}

/// Default noise gate threshold (linear amplitude, 0..1).
pub const DEFAULT_NOISE_GATE: f32 = 0.008;

/// Screen-share frames go out over the call's WebRTC data channel in fixed
/// chunks prefixed with a small header, since there's no real video
/// track/RTP payloader doing that job for us (see screenshare.rs). Kept
/// well under common data-channel message-size limits.
const SHARE_CHUNK_SIZE: usize = 12_000;
const MSG_KIND_FRAME: u8 = 0;
const MSG_KIND_STOP: u8 = 1;

/// Sent from the UI into a running call to start/stop screen sharing.
pub enum ShareCommand {
    Start(ShareTarget),
    Stop,
}

#[derive(Debug, Clone)]
pub enum CallEvent {
    /// Emitted once the caller's row has been created on the server.
    Created,
    Connecting,
    Connected,
    Ended,
    Failed(String),
    /// A complete JPEG frame reassembled from the remote peer's screen share.
    ScreenFrame(Vec<u8>),
    /// The remote peer stopped sharing; clear any displayed frame.
    ScreenShareStopped,
    /// Sharing couldn't start (data channel not ready yet) or stopped on
    /// its own (capture target became unavailable) -- as opposed to the
    /// user deliberately clicking "Stop sharing".
    ScreenShareFailed(String),
}

pub struct CallParams {
    pub client: ConvexClient,
    pub session_token: String,
    pub is_caller: bool,
    /// Required for the callee (the call already exists on the server).
    pub call_id: Option<String>,
    /// Required for the caller (used to create the call).
    pub conversation_id: Option<String>,
    pub callee_id: Option<String>,
    /// Required for the callee (the offer received from `calls:myCall`).
    pub offer_sdp: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub muted: Arc<AtomicBool>,
    /// Speaker mute, independent of `muted` (which is the microphone). A
    /// "Mute all" control in the UI sets both together.
    pub output_muted: Arc<AtomicBool>,
    /// Noise gate threshold, stored as `f32::to_bits()` so it can be tuned
    /// live (from Settings) while a call is in progress.
    pub noise_gate: Arc<AtomicU32>,
    /// Start/stop screen-share commands from the UI, sent while this call
    /// is already running.
    pub share_rx: tokio::sync::mpsc::UnboundedReceiver<ShareCommand>,
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

fn find_input_device(name: &Option<String>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Ok(mut devices) = host.input_devices() {
            if let Some(device) = devices.find(|d| d.name().map(|n| &n == name).unwrap_or(false))
            {
                return Some(device);
            }
        }
    }
    host.default_input_device()
}

fn find_output_device(name: &Option<String>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Ok(mut devices) = host.output_devices() {
            if let Some(device) = devices.find(|d| d.name().map(|n| &n == name).unwrap_or(false))
            {
                return Some(device);
            }
        }
    }
    host.default_output_device()
}

/// A one-pole low-pass filter used as an anti-aliasing filter ahead of
/// decimation, and as a gentle smoother on interpolated playback output.
struct OnePoleLowPass {
    alpha: f32,
    state: f32,
}

impl OnePoleLowPass {
    fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        let dt = 1.0 / sample_rate.max(1.0);
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz.max(1.0));
        Self {
            alpha: dt / (rc + dt),
            state: 0.0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.state += self.alpha * (input - self.state);
        self.state
    }
}

/// A one-pole high-pass filter: strips DC offset and low-frequency rumble
/// from the mic signal before it reaches the codec.
struct OnePoleHighPass {
    alpha: f32,
    prev_input: f32,
    prev_output: f32,
}

impl OnePoleHighPass {
    fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        let dt = 1.0 / sample_rate.max(1.0);
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz.max(1.0));
        Self {
            alpha: rc / (rc + dt),
            prev_input: 0.0,
            prev_output: 0.0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.alpha * (self.prev_output + input - self.prev_input);
        self.prev_input = input;
        self.prev_output = output;
        output
    }
}

/// Downward expander / noise gate with a soft knee so it doesn't chop speech
/// onsets into square-ish artifacts that G.711 then turns into harsh crackle.
struct NoiseGate {
    envelope: f32,
    gain: f32,
    attack: f32,
    release: f32,
}

impl NoiseGate {
    fn new(sample_rate: f32) -> Self {
        let attack_ms = 3.0_f32;
        let release_ms = 220.0_f32;
        Self {
            envelope: 0.0,
            gain: 1.0,
            attack: 1.0 - (-1.0 / (sample_rate * attack_ms / 1000.0)).exp(),
            release: 1.0 - (-1.0 / (sample_rate * release_ms / 1000.0)).exp(),
        }
    }

    /// `threshold` is read fresh on every call so it can be tuned live.
    fn process(&mut self, input: f32, threshold: f32) -> f32 {
        let rectified = input.abs();
        let env_coeff = if rectified > self.envelope {
            self.attack
        } else {
            self.release
        };
        self.envelope += env_coeff * (rectified - self.envelope);

        // Soft knee around the threshold: full open above ~1.6x threshold,
        // fully closed below ~0.5x, linear fade between. Hard open/close
        // was producing audible gate chatter that sounded metallic on G.711.
        let target_gain = if threshold <= 0.0 {
            1.0
        } else {
            let ratio = self.envelope / threshold;
            if ratio >= 1.6 {
                1.0
            } else if ratio <= 0.5 {
                0.0
            } else {
                ((ratio - 0.5) / 1.1).clamp(0.0, 1.0)
            }
        };
        let gain_coeff = if target_gain > self.gain {
            self.attack
        } else {
            self.release
        };
        self.gain += gain_coeff * (target_gain - self.gain);

        input * self.gain
    }
}

/// Peak + RMS hybrid AGC. Quiet mics get a modest boost so they survive
/// G.711 quantization; loud Windows mics get pulled down so they don't sit
/// against full scale. Gain reduction is much faster than increase, which
/// stops the classic AGC pump into clipping after a loud syllable.
struct AutoGain {
    peak: f32,
    rms: f32,
    gain: f32,
}

impl AutoGain {
    /// Target speech level into the codec (linear 0..1). Kept moderate —
    /// G.711 already has headroom bias, and pushing hotter only buys grit.
    const TARGET_PEAK: f32 = 0.18;
    const TARGET_RMS: f32 = 0.055;
    const MAX_GAIN: f32 = 1.9;
    const MIN_GAIN: f32 = 0.15;

    fn new() -> Self {
        Self {
            peak: 0.02,
            rms: 0.01,
            gain: 0.75,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let rectified = input.abs();
        // Fast-attack / slow-release peak detector.
        if rectified > self.peak {
            self.peak += 0.12 * (rectified - self.peak);
        } else {
            self.peak += 0.0002 * (rectified - self.peak);
        }
        // Slow RMS for average loudness (less twitchy than peak alone).
        self.rms += 0.0015 * (rectified - self.rms);

        if self.peak > 0.001 {
            let from_peak =
                (Self::TARGET_PEAK / self.peak.max(0.001)).clamp(Self::MIN_GAIN, Self::MAX_GAIN);
            let from_rms =
                (Self::TARGET_RMS / self.rms.max(0.001)).clamp(Self::MIN_GAIN, Self::MAX_GAIN);
            // Peak ceiling wins when it asks for less gain (anti-clip).
            let desired = from_peak.min(from_rms);
            let coeff = if desired < self.gain { 0.018 } else { 0.00035 };
            self.gain += coeff * (desired - self.gain);
        }
        input * self.gain
    }
}

/// Mild high-frequency taming: blends the dry signal with a lower-cutoff
/// low-pass so sibilants / G.711 band-edge hash don't sound like a microwave.
struct Deharsh {
    lp: TwoPoleLowPass,
    /// 0 = dry, 1 = fully low-passed. ~0.4 softens without muddying speech.
    amount: f32,
}

impl Deharsh {
    fn new(sample_rate: f32) -> Self {
        Self {
            lp: TwoPoleLowPass::new(sample_rate, 2200.0),
            amount: 0.38,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let dark = self.lp.process(input);
        input * (1.0 - self.amount) + dark * self.amount
    }
}

/// ~12 dB/oct low-pass (two cascaded one-poles).
struct TwoPoleLowPass {
    a: OnePoleLowPass,
    b: OnePoleLowPass,
}

impl TwoPoleLowPass {
    fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        Self {
            a: OnePoleLowPass::new(sample_rate, cutoff_hz),
            b: OnePoleLowPass::new(sample_rate, cutoff_hz),
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.b.process(self.a.process(input))
    }
}

/// ~24 dB/oct low-pass for anti-alias / reconstruction. Two poles alone left
/// too much energy near Nyquist, which folded into a harsh metallic sheen
/// after decimation to 8 kHz.
struct FourPoleLowPass {
    a: TwoPoleLowPass,
    b: TwoPoleLowPass,
}

impl FourPoleLowPass {
    fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        Self {
            a: TwoPoleLowPass::new(sample_rate, cutoff_hz),
            b: TwoPoleLowPass::new(sample_rate, cutoff_hz),
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.b.process(self.a.process(input))
    }
}

/// Mild rumble cut without the thin/nasal character of an aggressive
/// two-pole high-pass at 120 Hz.
struct TwoPoleHighPass {
    a: OnePoleHighPass,
    b: OnePoleHighPass,
}

impl TwoPoleHighPass {
    fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        Self {
            a: OnePoleHighPass::new(sample_rate, cutoff_hz),
            b: OnePoleHighPass::new(sample_rate, cutoff_hz),
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.b.process(self.a.process(input))
    }
}

/// Soft knee peak limiter with **unity small-signal gain**.
///
/// The previous tanh-drive form (`tanh(1.6x)/tanh(1.6)`) had a small-signal
/// slope of ~1.73× — it boosted everything by ~4.8 dB and then saturated,
/// which is exactly the "talking through a microwave" grit. This version
/// is linear until near full scale and only rounds peaks off.
fn soft_limit(x: f32) -> f32 {
    const THRESHOLD: f32 = 0.82;
    let ax = x.abs();
    if ax <= THRESHOLD {
        return x;
    }
    let headroom = 1.0 - THRESHOLD;
    let over = ax - THRESHOLD;
    // Asymptotically approaches ±0.98 instead of hard-clipping at ±1.
    let limited = THRESHOLD + headroom * (over / (over + headroom));
    x.signum() * limited.min(0.98)
}

/// Cheap xorshift PRNG (no extra dependency) used to generate dither noise.
struct Rng(u32);

impl Rng {
    fn next_unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32) / (u32::MAX as f32)
    }

    /// Triangular dither in roughly [-1, 1], scaled to ~1 LSB of 16-bit
    /// before quantization so G.711's coarse steps hiss rather than grit.
    fn triangular_dither(&mut self) -> f32 {
        self.next_unit() - self.next_unit()
    }
}

/// G.711 telephone band is ~300–3400 Hz. Cut earlier with a steep filter so
/// decimation to 8 kHz does not fold residual highs into the passband.
const ANTI_ALIAS_CUTOFF_HZ: f32 = 2850.0;

/// Playback reconstruction cutoff (applied after upsampling).
const PLAYBACK_SMOOTH_HZ: f32 = 3000.0;

/// DC / rumble cut. Kept low so speech stays natural, not thin.
const HIGH_PASS_CUTOFF_HZ: f32 = 75.0;

/// Fixed pad on mic input before any dynamics — many Windows devices already
/// run hot and leave no headroom for AGC or peaks.
const INPUT_PAD: f32 = 0.65;

/// Overall level into the μ-law encoder after processing (extra safety).
const ENCODE_LEVEL: f32 = 0.82;

/// Playback volume trim so decoded full-scale G.711 is not ear-splitting.
const PLAYBACK_GAIN: f32 = 0.72;

/// Shared mic processing state used by every sample-format callback.
struct CapturePipeline {
    channels: usize,
    input_rate: u32,
    muted: Arc<AtomicBool>,
    noise_gate: Arc<AtomicU32>,
    frame_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Fractional phase accumulator for linear-interpolated decimation.
    phase: f32,
    phase_step: f32,
    prev_shaped: f32,
    frame_buf: Vec<u8>,
    highpass: TwoPoleHighPass,
    lowpass: FourPoleLowPass,
    deharsh: Deharsh,
    gate: NoiseGate,
    agc: AutoGain,
    rng: Rng,
}

impl CapturePipeline {
    fn new(
        channels: usize,
        input_rate: u32,
        muted: Arc<AtomicBool>,
        noise_gate: Arc<AtomicU32>,
        frame_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> Self {
        let rate_hz = input_rate.max(8000);
        let rate = rate_hz as f32;
        Self {
            channels: channels.max(1),
            input_rate: rate_hz,
            muted,
            noise_gate,
            frame_tx,
            phase: 0.0,
            // Advance 8 kHz of "output time" per input sample.
            phase_step: 8000.0 / rate,
            prev_shaped: 0.0,
            frame_buf: Vec::with_capacity(160),
            highpass: TwoPoleHighPass::new(rate, HIGH_PASS_CUTOFF_HZ),
            lowpass: FourPoleLowPass::new(rate, ANTI_ALIAS_CUTOFF_HZ),
            deharsh: Deharsh::new(rate),
            gate: NoiseGate::new(rate),
            agc: AutoGain::new(),
            rng: Rng(0x9E3779B9),
        }
    }

    /// Downmix one interleaved multi-channel frame to mono.
    /// Prefers the loudest channel when the other is near silence (common
    /// for mono mics exposed as stereo), otherwise averages.
    fn to_mono(&self, frame: &[f32]) -> f32 {
        if frame.is_empty() {
            return 0.0;
        }
        if frame.len() == 1 {
            return frame[0] * INPUT_PAD;
        }
        let mut best = frame[0];
        let mut best_abs = best.abs();
        let mut sum = 0.0_f32;
        for &s in frame {
            sum += s;
            let a = s.abs();
            if a > best_abs {
                best_abs = a;
                best = s;
            }
        }
        let avg = sum / frame.len() as f32;
        // If channels disagree a lot (one-sided mic), keep the loud one.
        let mono = if best_abs > avg.abs() * 2.5 {
            best
        } else {
            avg
        };
        mono * INPUT_PAD
    }

    /// Feed one interleaved buffer of already-normalized mono-capable
    /// samples in `[-1, 1]` (one sample per channel per frame).
    fn push_interleaved_f32(&mut self, data: &[f32]) {
        if self.muted.load(Ordering::Relaxed) {
            return;
        }
        let threshold = f32::from_bits(self.noise_gate.load(Ordering::Relaxed));
        for frame in data.chunks(self.channels) {
            let mono = self.to_mono(frame);

            // HPF → gate → AGC → anti-alias LPF → deharsh → soft peak limit
            let shaped = self.highpass.process(mono);
            let shaped = self.gate.process(shaped, threshold);
            let shaped = self.agc.process(shaped);
            let shaped = self.lowpass.process(shaped);
            let shaped = self.deharsh.process(shaped);
            let shaped = soft_limit(shaped * ENCODE_LEVEL);

            // Linear-interpolated decimation to 8 kHz (less harsh than hold).
            self.phase += self.phase_step;
            while self.phase >= 1.0 {
                // Fraction of the current [prev → shaped] step where the
                // 8 kHz tick lands (phase crossed 1.0 during this sample).
                let phase_before = self.phase - self.phase_step;
                let alpha =
                    ((1.0 - phase_before) / self.phase_step).clamp(0.0, 1.0);
                self.phase -= 1.0;
                let lerped = self.prev_shaped + (shaped - self.prev_shaped) * alpha;
                let dither = self.rng.triangular_dither() * (1.5 / i16::MAX as f32);
                let sample_i16 =
                    ((lerped + dither).clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                self.frame_buf.push(g711::linear_to_ulaw(sample_i16));
                if self.frame_buf.len() >= 160 {
                    let chunk = std::mem::replace(&mut self.frame_buf, Vec::with_capacity(160));
                    let _ = self.frame_tx.send(chunk);
                }
            }
            self.prev_shaped = shaped;
        }
    }
}

fn i16_to_f32(s: i16) -> f32 {
    s as f32 / i16::MAX as f32
}

fn u16_to_f32(s: u16) -> f32 {
    // WASAPI unsigned 16-bit is mid-point biased.
    (s as f32 / u16::MAX as f32) * 2.0 - 1.0
}

fn i32_to_f32(s: i32) -> f32 {
    s as f32 / i32::MAX as f32
}

/// Returns `Ok(())` once a capture thread is running against a real device.
/// Unsupported sample formats and missing devices become `Err` with a
/// user-facing message (Windows WASAPI often exposes I16, not only F32).
pub(crate) fn spawn_capture_thread(
    device_name: Option<String>,
    muted: Arc<AtomicBool>,
    noise_gate: Arc<AtomicU32>,
    frame_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    let Some(device) = find_input_device(&device_name) else {
        return Err(
            "No microphone found. Plug one in or pick a different input in Settings."
                .into(),
        );
    };
    let Ok(config) = device.default_input_config() else {
        return Err("Could not read microphone configuration.".into());
    };
    let sample_format = config.sample_format();
    match sample_format {
        cpal::SampleFormat::F32
        | cpal::SampleFormat::I16
        | cpal::SampleFormat::U16
        | cpal::SampleFormat::I32
        | cpal::SampleFormat::F64 => {}
        other => {
            return Err(format!(
                "Microphone sample format {other:?} is not supported."
            ));
        }
    }
    let stream_config: cpal::StreamConfig = config.into();
    let channels = stream_config.channels.max(1) as usize;
    let input_rate = stream_config.sample_rate.0.max(8000);

    std::thread::spawn(move || {
        let err_fn = |_err| {};
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let mut pipe =
                    CapturePipeline::new(channels, input_rate, muted, noise_gate, frame_tx);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        pipe.push_interleaved_f32(data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut pipe =
                    CapturePipeline::new(channels, input_rate, muted, noise_gate, frame_tx);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let converted: Vec<f32> = data.iter().copied().map(i16_to_f32).collect();
                        pipe.push_interleaved_f32(&converted);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut pipe =
                    CapturePipeline::new(channels, input_rate, muted, noise_gate, frame_tx);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        let converted: Vec<f32> = data.iter().copied().map(u16_to_f32).collect();
                        pipe.push_interleaved_f32(&converted);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                let mut pipe =
                    CapturePipeline::new(channels, input_rate, muted, noise_gate, frame_tx);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i32], _: &cpal::InputCallbackInfo| {
                        let converted: Vec<f32> = data.iter().copied().map(i32_to_f32).collect();
                        pipe.push_interleaved_f32(&converted);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::F64 => {
                let mut pipe =
                    CapturePipeline::new(channels, input_rate, muted, noise_gate, frame_tx);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f64], _: &cpal::InputCallbackInfo| {
                        let converted: Vec<f32> = data.iter().map(|&s| s as f32).collect();
                        pipe.push_interleaved_f32(&converted);
                    },
                    err_fn,
                    None,
                )
            }
            _ => return,
        };

        let Ok(stream) = stream else {
            eprintln!("Talkyss call: could not open microphone stream");
            return;
        };
        if stream.play().is_err() {
            eprintln!("Talkyss call: could not start microphone stream");
            return;
        }
        let _ = stop_rx.recv();
        drop(stream);
    });

    Ok(())
}

/// Shared playback state: 8 kHz mono → device rate/channels with cubic
/// interpolation, reconstruction LPF, deharsh, soft peak limit, mute.
struct PlaybackPipeline {
    channels: usize,
    step: f32,
    phase: f32,
    /// Four-point history for Catmull-Rom upsampling (s0..s3).
    s0: f32,
    s1: f32,
    s2: f32,
    s3: f32,
    smoother: FourPoleLowPass,
    deharsh: Deharsh,
    jitter: Arc<Mutex<VecDeque<i16>>>,
    output_muted: Arc<AtomicBool>,
}

impl PlaybackPipeline {
    fn new(
        channels: usize,
        output_rate: u32,
        jitter: Arc<Mutex<VecDeque<i16>>>,
        output_muted: Arc<AtomicBool>,
    ) -> Self {
        let output_rate = output_rate.max(8000);
        let rate = output_rate as f32;
        Self {
            channels: channels.max(1),
            step: 8000.0_f32 / rate,
            phase: 0.0,
            s0: 0.0,
            s1: 0.0,
            s2: 0.0,
            s3: 0.0,
            smoother: FourPoleLowPass::new(rate, PLAYBACK_SMOOTH_HZ),
            deharsh: Deharsh::new(rate),
            jitter,
            output_muted,
        }
    }

    fn pull_sample(&mut self) -> f32 {
        let popped = self.jitter.lock().ok().and_then(|mut buf| buf.pop_front());
        match popped {
            Some(sample) => sample as f32,
            // Underrun: decay toward silence (no held-tone buzz).
            None => self.s3 * 0.90,
        }
    }

    fn next_f32(&mut self) -> f32 {
        self.phase += self.step;
        while self.phase >= 1.0 {
            self.phase -= 1.0;
            self.s0 = self.s1;
            self.s1 = self.s2;
            self.s2 = self.s3;
            self.s3 = self.pull_sample();
        }
        // Catmull-Rom between s1 and s2 — smoother than linear, less
        // metallic image content after upsampling from 8 kHz.
        let t = self.phase;
        let t2 = t * t;
        let t3 = t2 * t;
        let interpolated = 0.5
            * ((2.0 * self.s1)
                + (-self.s0 + self.s2) * t
                + (2.0 * self.s0 - 5.0 * self.s1 + 4.0 * self.s2 - self.s3) * t2
                + (-self.s0 + 3.0 * self.s1 - 3.0 * self.s2 + self.s3) * t3);
        let mut value = self.smoother.process(interpolated / i16::MAX as f32);
        value = self.deharsh.process(value);
        value = soft_limit(value * PLAYBACK_GAIN);
        if self.output_muted.load(Ordering::Relaxed) {
            value = 0.0;
        }
        value
    }

    fn fill_f32(&mut self, data: &mut [f32]) {
        for frame in data.chunks_mut(self.channels) {
            let value = self.next_f32();
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }
    }

    fn fill_i16(&mut self, data: &mut [i16]) {
        for frame in data.chunks_mut(self.channels) {
            let value = (self.next_f32().clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }
    }

    fn fill_u16(&mut self, data: &mut [u16]) {
        for frame in data.chunks_mut(self.channels) {
            let n = self.next_f32().clamp(-1.0, 1.0);
            let value = ((n * 0.5 + 0.5) * u16::MAX as f32) as u16;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }
    }

    fn fill_i32(&mut self, data: &mut [i32]) {
        for frame in data.chunks_mut(self.channels) {
            let value = (self.next_f32().clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }
    }

    fn fill_f64(&mut self, data: &mut [f64]) {
        for frame in data.chunks_mut(self.channels) {
            let value = self.next_f32() as f64;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }
    }
}

/// Plays back decoded audio from a shared jitter buffer.
pub(crate) fn spawn_playback_thread(
    device_name: Option<String>,
    jitter: Arc<Mutex<VecDeque<i16>>>,
    output_muted: Arc<AtomicBool>,
    stop_rx: std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    let Some(device) = find_output_device(&device_name) else {
        return Err(
            "No speaker/headphones found. Check Windows sound settings.".into(),
        );
    };
    let Ok(config) = device.default_output_config() else {
        return Err("Could not read speaker configuration.".into());
    };
    let sample_format = config.sample_format();
    match sample_format {
        cpal::SampleFormat::F32
        | cpal::SampleFormat::I16
        | cpal::SampleFormat::U16
        | cpal::SampleFormat::I32
        | cpal::SampleFormat::F64 => {}
        other => {
            return Err(format!(
                "Speaker sample format {other:?} is not supported."
            ));
        }
    }
    let stream_config: cpal::StreamConfig = config.into();
    let channels = stream_config.channels.max(1) as usize;
    let output_rate = stream_config.sample_rate.0.max(8000);

    std::thread::spawn(move || {
        let err_fn = |_err| {};
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let mut pipe =
                    PlaybackPipeline::new(channels, output_rate, jitter, output_muted);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        pipe.fill_f32(data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut pipe =
                    PlaybackPipeline::new(channels, output_rate, jitter, output_muted);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        pipe.fill_i16(data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut pipe =
                    PlaybackPipeline::new(channels, output_rate, jitter, output_muted);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        pipe.fill_u16(data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                let mut pipe =
                    PlaybackPipeline::new(channels, output_rate, jitter, output_muted);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                        pipe.fill_i32(data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::F64 => {
                let mut pipe =
                    PlaybackPipeline::new(channels, output_rate, jitter, output_muted);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [f64], _: &cpal::OutputCallbackInfo| {
                        pipe.fill_f64(data);
                    },
                    err_fn,
                    None,
                )
            }
            _ => return,
        };

        let Ok(stream) = stream else {
            eprintln!("Talkyss call: could not open speaker stream");
            return;
        };
        if stream.play().is_err() {
            eprintln!("Talkyss call: could not start speaker stream");
            return;
        }
        let _ = stop_rx.recv();
        drop(stream);
    });

    Ok(())
}

async fn fail(output: &mut EventSender<CallEvent>, message: impl Into<String>) {
    let _ = output.send(CallEvent::Failed(message.into())).await;
}

/// Registers the message handler that reassembles incoming screen-share
/// frames. Chunks for a given frame arrive in order (the data channel is
/// ordered by default), so reassembly just needs to notice when the
/// frame id changes and when the last chunk of a frame has arrived --
/// no out-of-order bookkeeping required.
fn attach_data_channel_handlers(dc: Arc<RTCDataChannel>, output: EventSender<CallEvent>) {
    let mut current_frame: Option<(u32, Vec<u8>)> = None;
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let data = msg.data;
        if data.is_empty() {
            return Box::pin(async {});
        }
        match data[0] {
            MSG_KIND_STOP => {
                current_frame = None;
                let mut tx = output.clone();
                Box::pin(async move {
                    let _ = tx.send(CallEvent::ScreenShareStopped).await;
                })
            }
            MSG_KIND_FRAME if data.len() >= 9 => {
                let frame_id = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                let chunk_index = u16::from_le_bytes([data[5], data[6]]);
                let chunk_count = u16::from_le_bytes([data[7], data[8]]);
                let payload = data.slice(9..);

                if current_frame.as_ref().map(|(id, _)| *id) != Some(frame_id) {
                    current_frame = Some((frame_id, Vec::new()));
                }
                if let Some((_, buf)) = current_frame.as_mut() {
                    buf.extend_from_slice(&payload);
                }

                if chunk_index + 1 >= chunk_count {
                    if let Some((_, buf)) = current_frame.take() {
                        let mut tx = output.clone();
                        return Box::pin(async move {
                            let _ = tx.send(CallEvent::ScreenFrame(buf)).await;
                        });
                    }
                }
                Box::pin(async {})
            }
            _ => Box::pin(async {}),
        }
    }));
}

/// Overall cap from starting the call to reaching `Connected`. Guards
/// against any other stage (ICE connectivity checks, DTLS handshake, ...)
/// hanging silently instead of surfacing a failure the user can act on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);

async fn send_share_frame(dc: &RTCDataChannel, frame_id: u32, jpeg: &[u8]) {
    if dc.ready_state() != RTCDataChannelState::Open {
        return;
    }
    let chunk_count = ((jpeg.len() + SHARE_CHUNK_SIZE - 1) / SHARE_CHUNK_SIZE).max(1) as u16;
    for (index, chunk) in jpeg.chunks(SHARE_CHUNK_SIZE).enumerate() {
        let mut msg = Vec::with_capacity(9 + chunk.len());
        msg.push(MSG_KIND_FRAME);
        msg.extend_from_slice(&frame_id.to_le_bytes());
        msg.extend_from_slice(&(index as u16).to_le_bytes());
        msg.extend_from_slice(&chunk_count.to_le_bytes());
        msg.extend_from_slice(chunk);
        if dc.send(&Bytes::from(msg)).await.is_err() {
            return;
        }
    }
}

pub async fn run_call(params: CallParams, mut output: EventSender<CallEvent>) {
    let CallParams {
        mut client,
        session_token,
        is_caller,
        call_id,
        conversation_id,
        callee_id,
        offer_sdp,
        input_device,
        output_device,
        muted,
        output_muted,
        noise_gate,
        mut share_rx,
    } = params;

    let mut media_engine = MediaEngine::default();
    if media_engine.register_default_codecs().is_err() {
        fail(&mut output, "Could not set up audio codecs").await;
        return;
    }
    let mut registry = Registry::new();
    registry = match register_default_interceptors(registry, &mut media_engine) {
        Ok(r) => r,
        Err(_) => {
            fail(&mut output, "Could not set up media pipeline").await;
            return;
        }
    };
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let mut ice_servers = vec![RTCIceServer {
        // Redundant STUN servers: if one is unreachable or rate-limiting
        // us, ICE gathering can still succeed via another rather than
        // silently coming up empty. Each of these was verified live with a
        // real STUN binding request (not just assumed from an old list --
        // most of the "free public STUN" lists circulating online are from
        // ~2013 and mostly dead now; only 101 of 268 entries from one such
        // list actually answered).
        urls: vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
            "stun:stun2.l.google.com:19302".to_string(),
            "stun:stun3.l.google.com:19302".to_string(),
            "stun:stun4.l.google.com:19302".to_string(),
            "stun:stun.nextcloud.com:443".to_string(),
            "stun:stun.sipnet.net:3478".to_string(),
            "stun:stun.voipgate.com:3478".to_string(),
            "stun:stun.antisip.com:3478".to_string(),
            "stun:stun.easybell.de:3478".to_string(),
        ],
        ..Default::default()
    }];
    if let Some(turn_server) = turn_ice_server() {
        ice_servers.push(turn_server);
    }
    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let pc = match api.new_peer_connection(config).await {
        Ok(pc) => Arc::new(pc),
        Err(err) => {
            fail(&mut output, format!("Could not start call: {err}")).await;
            return;
        }
    };

    let local_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_PCMU.to_string(),
            clock_rate: 8000,
            channels: 1,
            ..Default::default()
        },
        "audio".to_string(),
        "talkyss".to_string(),
    ));
    if pc
        .add_track(Arc::clone(&local_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .is_err()
    {
        fail(&mut output, "Could not attach microphone track").await;
        return;
    }

    let jitter: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (playback_stop_tx, playback_stop_rx) = std::sync::mpsc::channel::<()>();
    if let Err(msg) = spawn_playback_thread(
        output_device,
        Arc::clone(&jitter),
        output_muted,
        playback_stop_rx,
    ) {
        fail(&mut output, msg).await;
        let _ = pc.close().await;
        return;
    }

    let jitter_for_track = Arc::clone(&jitter);
    pc.on_track(Box::new(move |track: Arc<TrackRemote>, _receiver, _transceiver| {
        let jitter = Arc::clone(&jitter_for_track);
        Box::pin(async move {
            let mut jitter_estimator = JitterEstimator::new();
            loop {
                match track.read_rtp().await {
                    Ok((packet, _)) => {
                        let samples = g711::decode(&packet.payload);
                        let target = jitter_estimator.on_packet_arrival();
                        if let Ok(mut buf) = jitter.lock() {
                            buf.extend(samples);
                            while buf.len() > target {
                                buf.pop_front();
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    }));

    // Screen share travels over a data channel rather than a video track
    // (see screenshare.rs). The caller creates it explicitly further down;
    // the callee only learns about it once negotiation completes, via this
    // handler. Both roles end up with the same `Arc<RTCDataChannel>`
    // through `data_channel_rx`.
    let (data_channel_tx, data_channel_rx) =
        tokio::sync::watch::channel::<Option<Arc<RTCDataChannel>>>(None);
    let data_channel_tx_for_incoming = data_channel_tx.clone();
    let output_for_incoming = output.clone();
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        attach_data_channel_handlers(Arc::clone(&dc), output_for_incoming.clone());
        let _ = data_channel_tx_for_incoming.send(Some(dc));
        Box::pin(async {})
    }));

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (capture_stop_tx, capture_stop_rx) = std::sync::mpsc::channel::<()>();
    if let Err(msg) = spawn_capture_thread(
        input_device,
        muted,
        noise_gate,
        frame_tx,
        capture_stop_rx,
    ) {
        fail(&mut output, msg).await;
        let _ = playback_stop_tx.send(());
        let _ = pc.close().await;
        return;
    }

    let track_for_send = Arc::clone(&local_track);
    tokio::spawn(async move {
        while let Some(chunk) = frame_rx.recv().await {
            let _ = track_for_send
                .write_sample(&Sample {
                    data: Bytes::from(chunk),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await;
        }
    });

    // Trickle ICE: forward each locally-discovered candidate to the peer via
    // Convex as soon as it's found, instead of batching them all into the
    // initial offer/answer. `call_id` isn't known yet at this point for the
    // caller (it only exists once `calls:startCall` returns, further down),
    // so candidates found before then just sit in the unbounded channel --
    // the drain task below blocks on `call_id_rx` first and only starts
    // reading them once a call id has actually been sent through it.
    let (local_candidate_tx, mut local_candidate_rx) =
        tokio::sync::mpsc::unbounded_channel::<RTCIceCandidateInit>();
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        if let Some(candidate) = candidate {
            if let Ok(init) = candidate.to_json() {
                let _ = local_candidate_tx.send(init);
            }
        }
        Box::pin(async {})
    }));
    let (call_id_tx, call_id_rx) = tokio::sync::oneshot::channel::<String>();
    {
        let mut client_for_candidates = client.clone();
        let session_token_for_candidates = session_token.clone();
        tokio::spawn(async move {
            let Ok(call_id) = call_id_rx.await else {
                return;
            };
            while let Some(candidate) = local_candidate_rx.recv().await {
                let Ok(candidate_json) = serde_json::to_string(&candidate) else {
                    continue;
                };
                let _ = client_for_candidates
                    .mutation(
                        "calls:addIceCandidate",
                        btreemap! {
                            "sessionToken".to_string() =>
                                Value::String(session_token_for_candidates.clone()),
                            "callId".to_string() => Value::String(call_id.clone()),
                            "candidate".to_string() => Value::String(candidate_json),
                        },
                    )
                    .await;
            }
        });
    }

    // Tracked purely for diagnostics: if the connect-timeout watchdog below
    // fires, reporting the last ICE state distinguishes "stuck checking
    // candidate pairs" (a real NAT-traversal failure -- needs a TURN
    // relay, see `turn_ice_server`) from other failure modes.
    let last_ice_state: Arc<Mutex<RTCIceConnectionState>> =
        Arc::new(Mutex::new(RTCIceConnectionState::Unspecified));
    let last_ice_state_for_handler = Arc::clone(&last_ice_state);
    pc.on_ice_connection_state_change(Box::new(move |s: RTCIceConnectionState| {
        if let Ok(mut state) = last_ice_state_for_handler.lock() {
            *state = s;
        }
        Box::pin(async {})
    }));

    let connected_flag = Arc::new(AtomicBool::new(false));
    let state_tx = output.clone();
    let connected_flag_for_state = Arc::clone(&connected_flag);
    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let mut tx = state_tx.clone();
        let connected_flag = Arc::clone(&connected_flag_for_state);
        Box::pin(async move {
            let event = match s {
                RTCPeerConnectionState::Connected => {
                    connected_flag.store(true, Ordering::Relaxed);
                    Some(CallEvent::Connected)
                }
                RTCPeerConnectionState::Failed => {
                    Some(CallEvent::Failed("Connection lost".to_string()))
                }
                RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed => {
                    Some(CallEvent::Ended)
                }
                _ => None,
            };
            if let Some(event) = event {
                let _ = tx.send(event).await;
            }
        })
    }));

    // Watchdog: `Connected` normally arrives via the state-change handler
    // above, driven by ICE/DTLS progress that has no single "waitable"
    // point in this function. If it never arrives -- STUN unreachable,
    // the other side never answers, a NAT combination that just can't
    // punch through -- this is what turns that into a visible failure
    // instead of "Connecting..." forever.
    let connect_timeout_flag = Arc::clone(&connected_flag);
    let connect_timeout_ice_state = Arc::clone(&last_ice_state);
    let mut connect_timeout_output = output.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CONNECT_TIMEOUT).await;
        if !connect_timeout_flag.load(Ordering::Relaxed) {
            let ice_state = connect_timeout_ice_state
                .lock()
                .map(|state| *state)
                .unwrap_or(RTCIceConnectionState::Unspecified);
            let _ = connect_timeout_output
                .send(CallEvent::Failed(format!(
                    "Connection timed out (ICE state: {ice_state})"
                )))
                .await;
        }
    });

    let _ = output.send(CallEvent::Connecting).await;

    let resolved_call_id: String;
    let mut answer_applied;

    if is_caller {
        // Must be created before the offer so it's included in this
        // negotiation. A failure here just means screen sharing won't be
        // available -- the call itself shouldn't be blocked by a bonus
        // feature.
        if let Ok(dc) = pc.create_data_channel("screenshare", None).await {
            attach_data_channel_handlers(Arc::clone(&dc), output.clone());
            let _ = data_channel_tx.send(Some(dc));
        }

        let offer = match pc.create_offer(None).await {
            Ok(o) => o,
            Err(err) => {
                fail(&mut output, format!("{err}")).await;
                return;
            }
        };
        // Trickle ICE: send the offer the moment it's set, rather than
        // waiting for `gathering_complete_promise()` here -- candidates
        // (including this one's) travel separately via
        // `calls:addIceCandidate`/`calls:listPeerIceCandidates` below.
        if pc.set_local_description(offer).await.is_err() {
            fail(&mut output, "Could not start call").await;
            return;
        }
        let Some(local_desc) = pc.local_description().await else {
            fail(&mut output, "Could not start call").await;
            return;
        };
        let Ok(offer_json) = serde_json::to_string(&local_desc) else {
            fail(&mut output, "Could not start call").await;
            return;
        };
        let (Some(conversation_id), Some(callee_id)) = (conversation_id, callee_id) else {
            fail(&mut output, "Missing call target").await;
            return;
        };

        let result = client
            .mutation(
                "calls:startCall",
                btreemap! {
                    "sessionToken".to_string() => Value::String(session_token.clone()),
                    "conversationId".to_string() => Value::String(conversation_id),
                    "calleeId".to_string() => Value::String(callee_id),
                    "offerSdp".to_string() => Value::String(offer_json),
                },
            )
            .await;

        match result {
            Ok(FunctionResult::Value(Value::String(id))) => {
                resolved_call_id = id.clone();
                let _ = call_id_tx.send(id);
                let _ = output.send(CallEvent::Created).await;
            }
            Ok(FunctionResult::ErrorMessage(msg)) => {
                fail(&mut output, msg).await;
                return;
            }
            _ => {
                fail(&mut output, "Could not start call").await;
                return;
            }
        }
        answer_applied = false;
    } else {
        let Some(id) = call_id else {
            fail(&mut output, "Missing call").await;
            return;
        };
        resolved_call_id = id.clone();
        // Unlike the caller, the callee already knows its call id up front
        // (it came from the incoming-call row), so the candidate-sending
        // task can start draining immediately.
        let _ = call_id_tx.send(id.clone());
        let Some(offer_json) = offer_sdp else {
            fail(&mut output, "Missing call offer").await;
            return;
        };
        // Answerer receives the caller's screenshare data channel via this
        // handler — without it, only the caller can share (and receive).
        {
            let data_channel_tx = data_channel_tx.clone();
            let output_for_dc = output.clone();
            pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                attach_data_channel_handlers(Arc::clone(&dc), output_for_dc.clone());
                let tx = data_channel_tx.clone();
                Box::pin(async move {
                    let _ = tx.send(Some(dc));
                })
            }));
        }
        let offer: RTCSessionDescription = match serde_json::from_str(&offer_json) {
            Ok(o) => o,
            Err(_) => {
                fail(&mut output, "Bad call offer").await;
                return;
            }
        };
        if pc.set_remote_description(offer).await.is_err() {
            fail(&mut output, "Could not join call").await;
            return;
        }
        let answer = match pc.create_answer(None).await {
            Ok(a) => a,
            Err(err) => {
                fail(&mut output, format!("{err}")).await;
                return;
            }
        };
        // Trickle ICE: send the answer immediately -- see the matching
        // comment on the caller's `set_local_description` above.
        if pc.set_local_description(answer).await.is_err() {
            fail(&mut output, "Could not join call").await;
            return;
        }
        let Some(local_desc) = pc.local_description().await else {
            fail(&mut output, "Could not join call").await;
            return;
        };
        let Ok(answer_json) = serde_json::to_string(&local_desc) else {
            fail(&mut output, "Could not join call").await;
            return;
        };

        let result = client
            .mutation(
                "calls:respond",
                btreemap! {
                    "sessionToken".to_string() => Value::String(session_token.clone()),
                    "callId".to_string() => Value::String(id),
                    "accept".to_string() => Value::Boolean(true),
                    "answerSdp".to_string() => Value::String(answer_json),
                },
            )
            .await;
        if let Ok(FunctionResult::ErrorMessage(msg)) = result {
            fail(&mut output, msg).await;
            return;
        }
        answer_applied = true;
    }

    let sub = client
        .subscribe(
            "calls:myCall",
            btreemap! { "sessionToken".to_string() => Value::String(session_token.clone()) },
        )
        .await;
    // The peer's half of trickle ICE: a reactive list of candidates they've
    // discovered so far. Delivered as a full snapshot each time (not a
    // diff), so `applied_candidates` tracks which rows have already been
    // handed to `add_ice_candidate` to avoid re-adding the same one.
    let candidate_sub = client
        .subscribe(
            "calls:listPeerIceCandidates",
            btreemap! {
                "sessionToken".to_string() => Value::String(session_token.clone()),
                "callId".to_string() => Value::String(resolved_call_id.clone()),
            },
        )
        .await;

    let mut share_stop_flag: Option<Arc<AtomicBool>> = None;
    let mut applied_candidates: HashSet<String> = HashSet::new();

    if let (Ok(mut sub), Ok(mut candidate_sub)) = (sub, candidate_sub) {
        'watch: loop {
            tokio::select! {
                next = sub.next() => {
                    let Some(result) = next else { break 'watch; };
                    let obj = match result {
                        FunctionResult::Value(Value::Object(obj)) => obj,
                        _ => break 'watch,
                    };

                    let this_call_id = match obj.get("callId") {
                        Some(Value::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    if this_call_id != resolved_call_id {
                        break 'watch;
                    }

                    let status = match obj.get("status") {
                        Some(Value::String(s)) => s.clone(),
                        _ => String::new(),
                    };

                    if !answer_applied {
                        if let Some(Value::String(answer_json)) = obj.get("answerSdp") {
                            if let Ok(answer) =
                                serde_json::from_str::<RTCSessionDescription>(answer_json)
                            {
                                if pc.set_remote_description(answer).await.is_ok() {
                                    answer_applied = true;
                                }
                            }
                        }
                    }

                    if status == "ended" || status == "declined" {
                        break 'watch;
                    }
                }
                next = candidate_sub.next() => {
                    let Some(result) = next else { break 'watch; };
                    let rows = match result {
                        FunctionResult::Value(Value::Array(items)) => items,
                        _ => continue,
                    };
                    for item in rows {
                        let Value::Object(row) = item else { continue; };
                        let row_id = match row.get("id") {
                            Some(Value::String(s)) => s.clone(),
                            _ => continue,
                        };
                        if !applied_candidates.insert(row_id) {
                            continue;
                        }
                        let Some(Value::String(candidate_json)) = row.get("candidate") else {
                            continue;
                        };
                        if let Ok(init) =
                            serde_json::from_str::<RTCIceCandidateInit>(candidate_json)
                        {
                            let _ = pc.add_ice_candidate(init).await;
                        }
                    }
                }
                cmd = share_rx.recv() => {
                    match cmd {
                        Some(ShareCommand::Start(target)) => {
                            if let Some(flag) = share_stop_flag.take() {
                                flag.store(true, Ordering::Relaxed);
                            }
                            let dc = data_channel_rx.borrow().clone();
                            match dc {
                                Some(dc) => {
                                    let (raw_tx, mut raw_rx) =
                                        tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                                    let stop_flag = Arc::new(AtomicBool::new(false));
                                    screenshare::spawn_capture_thread(
                                        target,
                                        raw_tx,
                                        Arc::clone(&stop_flag),
                                    );
                                    let stop_flag_for_forward = Arc::clone(&stop_flag);
                                    let mut output_for_forward = output.clone();
                                    tokio::spawn(async move {
                                        let mut frame_id: u32 = 0;
                                        while let Some(jpeg) = raw_rx.recv().await {
                                            frame_id = frame_id.wrapping_add(1);
                                            send_share_frame(&dc, frame_id, &jpeg).await;
                                        }
                                        // The channel only closes on its own (rather
                                        // than via the Stop command below setting the
                                        // flag first) if the capture thread gave up --
                                        // the target window/monitor became unavailable.
                                        if !stop_flag_for_forward.load(Ordering::Relaxed) {
                                            let _ = output_for_forward
                                                .send(CallEvent::ScreenShareFailed(
                                                    "Screen sharing stopped: the selected \
                                                     window or screen is no longer available."
                                                        .to_string(),
                                                ))
                                                .await;
                                        }
                                    });
                                    share_stop_flag = Some(stop_flag);
                                }
                                None => {
                                    let _ = output
                                        .send(CallEvent::ScreenShareFailed(
                                            "Screen sharing isn't ready yet -- try again in a moment."
                                                .to_string(),
                                        ))
                                        .await;
                                }
                            }
                        }
                        Some(ShareCommand::Stop) => {
                            if let Some(flag) = share_stop_flag.take() {
                                flag.store(true, Ordering::Relaxed);
                            }
                            let dc = data_channel_rx.borrow().clone();
                            if let Some(dc) = dc {
                                let _ = dc.send(&Bytes::from(vec![MSG_KIND_STOP])).await;
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    }

    if let Some(flag) = share_stop_flag.take() {
        flag.store(true, Ordering::Relaxed);
    }

    // If we never reached Connected (timeout, decline before answer, mid-setup
    // failure after the Convex row existed), make sure the server row is not
    // left as ringing/active — that blocks every subsequent call for both
    // peers with "You're already in a call".
    if !connected_flag.load(Ordering::Relaxed) {
        let _ = client
            .mutation(
                "calls:endCall",
                btreemap! {
                    "sessionToken".to_string() => Value::String(session_token.clone()),
                    "callId".to_string() => Value::String(resolved_call_id.clone()),
                },
            )
            .await;
    }

    let _ = output.send(CallEvent::Ended).await;
    let _ = capture_stop_tx.send(());
    let _ = playback_stop_tx.send(());
    let _ = pc.close().await;
}
