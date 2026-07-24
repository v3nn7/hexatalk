//! Periodic WebRTC connection-quality polling (`RTCPeerConnection::get_stats`)
//! feeding an additive bias into the jitter buffer target. Packet loss or
//! high RTT reported by the remote side (RTCP receiver reports about the
//! stream *we* sent them) means the network is struggling right now --
//! this reacts sooner than `JitterEstimator` alone, which only grows the
//! buffer once it notices arrival-timing irregularity on packets already
//! received (necessarily a lagging signal by design, since it's a smoothed
//! average).
//!
//! Deliberately does *not* attempt adaptive ADPCM bitrate: the wire codec
//! (`adpcm.rs`) is a fixed ~96kbps 4-bit format with no variable-rate mode,
//! and introducing one would be a wire-format change touching every client
//! version at once. Widening/narrowing the jitter buffer is the adaptive
//! lever available without that.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use webrtc::peer_connection::RTCPeerConnection;
use webrtc::stats::StatsReportType;

use super::jitter::{JITTER_TARGET_MAX_MS, JITTER_TARGET_MIN_MS};

/// How often to poll `get_stats()`. Orders of magnitude cheaper than
/// per-packet jitter tracking, so there's no need to go faster than this.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Above this fraction lost, treat the network as actively lossy.
const LOSS_THRESHOLD: f64 = 0.05;
/// Above this RTT, treat the network as actively slow.
const RTT_THRESHOLD_MS: f64 = 150.0;
/// Extra buffer per ms of RTT above the threshold.
const RTT_BIAS_FACTOR: f32 = 0.5;
/// Flat extra buffer once loss crosses the threshold (on top of any RTT bias).
const LOSS_BIAS_MS: f32 = 60.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct CallStats {
    pub(crate) packet_loss_fraction: f64,
    pub(crate) rtt_ms: f64,
}

/// Reads whatever `RemoteInboundRTP` stats are present in the report --
/// the standard place WebRTC surfaces "how did our outbound audio actually
/// arrive on the other end" (RTCP receiver reports), which is what we want
/// to react to here. `None` if the connection has no such report yet
/// (e.g. right after connecting, before the first RTCP RR).
pub(crate) async fn poll_stats(pc: &RTCPeerConnection) -> Option<CallStats> {
    let report = pc.get_stats().await;
    report.reports.values().find_map(|r| match r {
        StatsReportType::RemoteInboundRTP(s) => Some(CallStats {
            packet_loss_fraction: s.fraction_lost,
            rtt_ms: s.round_trip_time.unwrap_or(0.0) * 1000.0,
        }),
        _ => None,
    })
}

/// Extra milliseconds to add on top of the jitter estimator's own target,
/// given the latest connection stats. Pure and synchronous -- no network,
/// no time -- so it's fully unit-testable.
pub(crate) fn adjust_jitter_policy(stats: &CallStats) -> f32 {
    let mut bias_ms = 0.0f32;
    if stats.packet_loss_fraction > LOSS_THRESHOLD {
        bias_ms += LOSS_BIAS_MS;
    }
    if stats.rtt_ms > RTT_THRESHOLD_MS {
        bias_ms += (stats.rtt_ms - RTT_THRESHOLD_MS) as f32 * RTT_BIAS_FACTOR;
    }
    bias_ms.clamp(0.0, JITTER_TARGET_MAX_MS - JITTER_TARGET_MIN_MS)
}

/// Shared bias storage: read on the per-packet jitter path, written by the
/// periodic polling task. An `AtomicU32` holding an `f32`'s bits -- read
/// far more often (every packet) than written (every `POLL_INTERVAL`), so
/// a lock-free atomic beats a `Mutex<f32>` here.
pub(crate) fn new_bias() -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(0f32.to_bits()))
}

fn store_bias_ms(bias: &AtomicU32, ms: f32) {
    bias.store(ms.to_bits(), Ordering::Relaxed);
}

/// Converts the current bias (milliseconds) to samples at `sample_rate_hz`.
/// Called on the per-packet hot path -- lock-free and allocation-free.
pub(crate) fn bias_samples(bias: &AtomicU32, sample_rate_hz: u32) -> usize {
    let ms = f32::from_bits(bias.load(Ordering::Relaxed));
    (ms * (sample_rate_hz as f32 / 1000.0)).max(0.0) as usize
}

/// Spawns the periodic polling task. Callers must wrap the returned handle
/// in `net::rt::AbortOnDrop` (or otherwise scope its lifetime) -- this
/// holds a strong `Arc<RTCPeerConnection>`, so an un-aborted task would
/// keep the connection alive and keep polling forever, well past the call
/// actually ending.
pub(crate) fn spawn_stats_poll(
    pc: Arc<RTCPeerConnection>,
    bias: Arc<AtomicU32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.tick().await; // skip the immediate first tick -- no stats yet
        loop {
            interval.tick().await;
            if let Some(stats) = poll_stats(&pc).await {
                store_bias_ms(&bias, adjust_jitter_policy(&stats));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_network_adds_no_bias() {
        let stats = CallStats {
            packet_loss_fraction: 0.0,
            rtt_ms: 20.0,
        };
        assert_eq!(adjust_jitter_policy(&stats), 0.0);
    }

    #[test]
    fn high_loss_adds_the_flat_bias() {
        let stats = CallStats {
            packet_loss_fraction: 0.10,
            rtt_ms: 20.0,
        };
        assert_eq!(adjust_jitter_policy(&stats), LOSS_BIAS_MS);
    }

    #[test]
    fn high_rtt_adds_a_proportional_bias() {
        let stats = CallStats {
            packet_loss_fraction: 0.0,
            rtt_ms: 250.0, // 100ms over the 150ms threshold
        };
        assert_eq!(adjust_jitter_policy(&stats), 100.0 * RTT_BIAS_FACTOR);
    }

    #[test]
    fn loss_and_rtt_biases_stack() {
        let stats = CallStats {
            packet_loss_fraction: 0.10,
            rtt_ms: 250.0,
        };
        let expected = LOSS_BIAS_MS + 100.0 * RTT_BIAS_FACTOR;
        assert_eq!(adjust_jitter_policy(&stats), expected);
    }

    #[test]
    fn bias_is_clamped_to_the_jitter_estimator_range() {
        let stats = CallStats {
            packet_loss_fraction: 1.0,
            rtt_ms: 10_000.0,
        };
        let bias = adjust_jitter_policy(&stats);
        assert!(bias <= JITTER_TARGET_MAX_MS - JITTER_TARGET_MIN_MS);
    }

    #[test]
    fn bias_samples_round_trips_through_the_atomic() {
        let bias = new_bias();
        store_bias_ms(&bias, 50.0);
        assert_eq!(bias_samples(&bias, 24_000), 50 * 24); // 50ms @ 24kHz
    }

    #[test]
    fn default_bias_is_zero_samples() {
        let bias = new_bias();
        assert_eq!(bias_samples(&bias, 24_000), 0);
    }
}
