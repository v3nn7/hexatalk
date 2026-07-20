//! Wire audio codec: IMA-ADPCM, 4 bits/sample at 24 kHz mono (~96 kbps).
//!
//! Replaces the old G.711 µ-law @ 8 kHz payload (telephone quality) with
//! super-wideband speech while staying pure Rust — no native codec libs.
//!
//! Packet layout (one 20 ms frame per RTP packet, 244 bytes total):
//! ```text
//! [0]    MAGIC        format tag — peers on a different codec drop the
//!                     packet instead of playing garbage
//! [1..2] predictor    i16 LE, encoder state before this frame (= last
//!                     sample of the previous frame)
//! [3]    step_index   IMA step-table index (0..=88)
//! [4..]  240 bytes    480 nibbles, low nibble = earlier sample
//! ```
//! Each packet is fully self-describing, so decoding is stateless and a lost
//! packet costs exactly one frame (no error propagation).

/// Nominal sample rate of the wire format.
pub(super) const WIRE_SAMPLE_RATE: u32 = 24_000;

/// Samples per RTP packet: 20 ms at the wire rate.
pub(super) const FRAME_SAMPLES: usize = WIRE_SAMPLE_RATE as usize / 50;

/// Format tag. Bump if the payload layout ever changes.
const WIRE_MAGIC: u8 = 0xA7;

/// Total RTP payload size for one frame.
#[cfg_attr(not(test), allow(dead_code))] // format-spec constant, exercised by tests
const WIRE_FRAME_BYTES: usize = 4 + FRAME_SAMPLES / 2;

/// SDP/RTP identity of the codec. Deliberately NOT a standard mime type:
/// the media engine only advertises this codec, so a peer running an old
/// (PCMU-only) build finds no common codec and the call fails at
/// negotiation instead of playing garbage.
/// NOTE: wire-protocol constant — both call peers must agree on this exact
/// string, so it keeps the pre-rebrand name for old↔new client compat.
pub(super) const MIME_TYPE: &str = "audio/x-talkyss-adpcm";

/// Dynamic RTP payload type for the codec.
pub(super) const RTP_PAYLOAD_TYPE: u8 = 111;

/// IMA-ADPCM step size table (89 entries).
#[rustfmt::skip]
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17,
    19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118,
    130, 143, 157, 173, 190, 209, 230, 253, 279, 307,
    337, 371, 408, 449, 494, 544, 598, 658, 724, 796,
    876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358,
    5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899,
    15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// IMA-ADPCM index adjustment per nibble.
#[rustfmt::skip]
const INDEX_TABLE: [i32; 16] = [
    -1, -1, -1, -1, 2, 4, 6, 8,
    -1, -1, -1, -1, 2, 4, 6, 8,
];

/// Stateful encoder: predictor/step carry over between frames so the frame
/// boundary is inaudible. State is also written into every packet header,
/// which is what makes the decoder stateless.
pub(super) struct AdpcmEncoder {
    predictor: i32,
    step_index: i32,
}

impl AdpcmEncoder {
    pub(super) fn new() -> Self {
        Self {
            predictor: 0,
            step_index: 0,
        }
    }

    /// Encodes exactly one 20 ms frame (`pcm.len() == FRAME_SAMPLES`).
    pub(super) fn encode_frame(&mut self, pcm: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(WIRE_FRAME_BYTES);
        self.encode_frame_into(pcm, &mut out);
        out
    }

    /// Allocation-free variant of `encode_frame`: reuses `out`'s capacity so
    /// the 50 Hz capture path doesn't allocate a fresh `Vec` per frame.
    pub(super) fn encode_frame_into(&mut self, pcm: &[i16], out: &mut Vec<u8>) {
        debug_assert_eq!(pcm.len(), FRAME_SAMPLES);
        out.clear();
        out.reserve(4 + pcm.len() / 2);
        out.push(WIRE_MAGIC);
        out.extend_from_slice(&(self.predictor as i16).to_le_bytes());
        out.push(self.step_index as u8);

        let mut pending_nibble: Option<u8> = None;
        for &sample in pcm {
            let nibble = self.encode_sample(sample as i32);
            match pending_nibble.take() {
                None => pending_nibble = Some(nibble),
                Some(lo) => out.push(lo | (nibble << 4)),
            }
        }
        if let Some(lo) = pending_nibble {
            out.push(lo);
        }
    }

    fn encode_sample(&mut self, sample: i32) -> u8 {
        let step = STEP_TABLE[self.step_index as usize];
        let mut diff = sample - self.predictor;
        let mut nibble: u8 = 0;
        if diff < 0 {
            nibble = 8;
            diff = -diff;
        }

        // IMA "diff = step/8 + nibble-weighted fractions of step".
        let mut delta = step >> 3;
        if diff >= step {
            nibble |= 4;
            diff -= step;
            delta += step;
        }
        if diff >= step >> 1 {
            nibble |= 2;
            diff -= step >> 1;
            delta += step >> 1;
        }
        if diff >= step >> 2 {
            nibble |= 1;
            delta += step >> 2;
        }

        if nibble & 8 != 0 {
            self.predictor -= delta;
        } else {
            self.predictor += delta;
        }
        self.predictor = self.predictor.clamp(i16::MIN as i32, i16::MAX as i32);

        self.step_index = (self.step_index + INDEX_TABLE[nibble as usize]).clamp(0, 88);
        nibble
    }
}

/// Hard upper bound on a wire payload. A legit frame is exactly
/// `WIRE_FRAME_BYTES` (244 B); anything dramatically larger is a corrupt or
/// hostile packet, and decoding it would let one packet force a large
/// `Vec` allocation on the receive path. Generous headroom (4x) so future
/// format tweaks within the same magic keep working.
const MAX_WIRE_PAYLOAD: usize = 4 + (FRAME_SAMPLES / 2) * 4;

/// Decodes one RTP payload into 24 kHz mono i16 samples. Returns `None`
/// (drop the packet) if the format tag doesn't match or the payload is
/// malformed — a mismatched peer produces silence, never garbage.
pub(super) fn decode_frame(payload: &[u8]) -> Option<Vec<i16>> {
    if payload.len() < 4 || payload.len() > MAX_WIRE_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity((payload.len() - 4) * 2);
    decode_frame_into(payload, &mut out).then_some(out)
}

/// Allocation-free variant of `decode_frame`: clears and refills `out`,
/// returning `false` (drop the packet) on a malformed/foreign payload.
pub(super) fn decode_frame_into(payload: &[u8], out: &mut Vec<i16>) -> bool {
    out.clear();
    if payload.len() < 4 || payload.len() > MAX_WIRE_PAYLOAD || payload[0] != WIRE_MAGIC {
        return false;
    }
    let mut predictor = i16::from_le_bytes([payload[1], payload[2]]) as i32;
    let mut step_index = (payload[3] as i32).clamp(0, 88);

    out.reserve((payload.len() - 4) * 2);
    for &byte in &payload[4..] {
        for nibble in [byte & 0x0F, byte >> 4] {
            let step = STEP_TABLE[step_index as usize];
            let mut delta = step >> 3;
            if nibble & 4 != 0 {
                delta += step;
            }
            if nibble & 2 != 0 {
                delta += step >> 1;
            }
            if nibble & 1 != 0 {
                delta += step >> 2;
            }
            if nibble & 8 != 0 {
                predictor -= delta;
            } else {
                predictor += delta;
            }
            predictor = predictor.clamp(i16::MIN as i32, i16::MAX as i32);
            step_index = (step_index + INDEX_TABLE[nibble as usize]).clamp(0, 88);
            out.push(predictor as i16);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sine_tracks_closely() {
        let mut enc = AdpcmEncoder::new();
        let mut err_sum = 0.0f64;
        let mut n = 0usize;
        // 50 frames of a 440 Hz sine at 40% full scale.
        for f in 0..50 {
            let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (f * FRAME_SAMPLES + i) as f32 / WIRE_SAMPLE_RATE as f32;
                    ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.4 * i16::MAX as f32) as i16
                })
                .collect();
            let wire = enc.encode_frame(&pcm);
            assert_eq!(wire.len(), WIRE_FRAME_BYTES);
            let decoded = decode_frame(&wire).expect("magic must match");
            assert_eq!(decoded.len(), FRAME_SAMPLES);
            for (a, b) in pcm.iter().zip(decoded.iter()) {
                err_sum += (*a as f64 - *b as f64).abs();
                n += 1;
            }
        }
        let mean_abs_err = err_sum / n as f64;
        // 4-bit ADPCM on a smooth tone should average well under 2% FS error.
        assert!(mean_abs_err < i16::MAX as f64 * 0.02, "MAE {mean_abs_err}");
    }

    #[test]
    fn rejects_foreign_payload() {
        assert!(decode_frame(&[]).is_none());
        assert!(decode_frame(&[0x00, 0, 0, 0]).is_none());
        assert!(decode_frame(&[0xFF, 0, 0, 0, 1, 2, 3]).is_none());
    }

    #[test]
    fn rejects_oversized_payload() {
        // Right magic, but absurdly long: must be dropped, not decoded into
        // a giant allocation.
        let oversized = {
            let mut v = vec![WIRE_MAGIC, 0, 0, 0];
            v.resize(MAX_WIRE_PAYLOAD + 1, 0x55);
            v
        };
        assert!(decode_frame(&oversized).is_none());
        let mut scratch = Vec::new();
        assert!(!decode_frame_into(&oversized, &mut scratch));
    }

    #[test]
    fn silence_stays_silent() {
        let mut enc = AdpcmEncoder::new();
        let pcm = vec![0i16; FRAME_SAMPLES];
        let wire = enc.encode_frame(&pcm);
        let decoded = decode_frame(&wire).unwrap();
        assert!(decoded.iter().all(|&s| s.abs() <= 8));
    }
}
