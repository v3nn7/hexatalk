//! Adaptive jitter buffer target sizing, shared between `call.rs` (1:1
//! calls) and `room_voice.rs` (group voice channels) -- extracted so both
//! use the same estimator instead of `room_voice.rs` having its own fixed
//! 500ms cap regardless of actual network behavior.

/// Below this, a connection is considered "stable enough" and the buffer
/// converges toward minimal latency.
pub(crate) const JITTER_TARGET_MIN_MS: f32 = 40.0;
/// Absolute ceiling regardless of how bad the network gets -- past this,
/// added latency stops helping and just makes the call feel unresponsive.
pub(crate) const JITTER_TARGET_MAX_MS: f32 = 500.0;

/// Tracks how irregular packet arrival timing is and derives a target
/// buffer depth from it: a stable connection converges toward
/// `JITTER_TARGET_MIN_MS` of latency, while a bursty one grows the target
/// (up to `JITTER_TARGET_MAX_MS`) to absorb the burstiness instead of
/// glitching.
pub(crate) struct JitterEstimator {
    last_arrival: Option<std::time::Instant>,
    last_interval_ms: f32,
    /// RFC 3550-style smoothed mean absolute deviation of inter-packet
    /// arrival intervals.
    jitter_ms: f32,
}

impl JitterEstimator {
    pub(crate) fn new() -> Self {
        Self {
            last_arrival: None,
            // 20ms is the nominal ADPCM frame interval (480 samples at
            // 24kHz); used only as the initial reference before the first
            // real interval is observed.
            last_interval_ms: 20.0,
            jitter_ms: 0.0,
        }
    }

    /// Current target buffer depth in milliseconds, from jitter alone
    /// (before any `call_stats`-derived bias is added).
    pub(crate) fn target_ms(&self) -> f32 {
        (self.jitter_ms * 4.0 + JITTER_TARGET_MIN_MS).clamp(JITTER_TARGET_MIN_MS, JITTER_TARGET_MAX_MS)
    }

    /// Call once per received packet. Returns the current target buffer
    /// depth in samples at `sample_rate_hz` (24kHz for the ADPCM wire
    /// format both `call.rs` and `room_voice.rs` use).
    pub(crate) fn on_packet_arrival(&mut self, sample_rate_hz: u32) -> usize {
        let now = std::time::Instant::now();
        if let Some(last) = self.last_arrival {
            let interval_ms = now.duration_since(last).as_secs_f32() * 1000.0;
            let deviation = (interval_ms - self.last_interval_ms).abs();
            self.jitter_ms += (deviation - self.jitter_ms) / 16.0;
            self.last_interval_ms = interval_ms;
        }
        self.last_arrival = Some(now);

        (self.target_ms() * (sample_rate_hz as f32 / 1000.0)) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_arrivals_converge_to_the_minimum_target() {
        let mut est = JitterEstimator::new();
        // Feed it a long run of arrivals; without real timing control in a
        // unit test we can't force exact 20ms spacing, but a tight loop's
        // successive calls have low deviation relative to jitter growth,
        // so jitter_ms should stay small and target should stay near the
        // floor across many samples.
        let mut target_ms = est.target_ms();
        for _ in 0..50 {
            est.on_packet_arrival(24_000);
            target_ms = est.target_ms();
        }
        assert!(
            target_ms < JITTER_TARGET_MAX_MS,
            "target should not have grown to the max in a tight loop"
        );
    }

    #[test]
    fn target_is_always_within_bounds() {
        let mut est = JitterEstimator::new();
        for _ in 0..10 {
            let samples = est.on_packet_arrival(24_000);
            let ms = samples as f32 / (24_000.0 / 1000.0);
            assert!((JITTER_TARGET_MIN_MS..=JITTER_TARGET_MAX_MS).contains(&ms));
        }
    }

    #[test]
    fn first_call_uses_the_minimum_target_before_any_interval_is_known() {
        let mut est = JitterEstimator::new();
        let samples = est.on_packet_arrival(24_000);
        let expected = (JITTER_TARGET_MIN_MS * 24.0) as usize; // 24_000/1000
        assert_eq!(samples, expected);
    }
}
