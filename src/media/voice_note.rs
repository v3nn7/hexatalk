//! Voice notes: record a short clip from the microphone and send it as a
//! regular chat attachment (`content_type: "audio/wav"`). The *only* new
//! integration point is producing the WAV bytes -- upload, E2EE
//! encryption, and delivery all go through the existing attachment
//! pipeline (`PendingAttachment` -> `crypto::encrypt_attachment` ->
//! `client.upload_file`) completely unchanged.
//!
//! Deliberately WAV/PCM16, not the ADPCM wire codec `adpcm.rs` uses for
//! live calls: that encoder is tuned for streaming 20ms frames over RTP,
//! not a single finished file, and PCM16 in a WAV container is trivial to
//! write correctly (and to play back anywhere, including outside the app).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};

use super::call::find_input_device;

/// Hard cap on recording length. Also keeps the encoded WAV comfortably
/// under the app's existing 5MB attachment limit (`AttachmentPick::TooLarge`
/// in `state/update.rs`) at typical device sample rates: 45s mono PCM16 is
/// ~4.3 MB even at 48kHz, ~3.9 MB at 44.1kHz.
const MAX_DURATION: Duration = Duration::from_secs(45);

fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}
fn u16_to_f32(s: u16) -> f32 {
    (s as f32 - 32768.0) / 32768.0
}
fn i32_to_f32(s: i32) -> f32 {
    s as f32 / i32::MAX as f32
}
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Downmixes interleaved multi-channel `f32` samples to mono by averaging
/// each frame's channels. A no-op copy when the device is already mono.
fn downmix_to_mono(data: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

struct RecorderState {
    samples: Vec<i16>,
    max_samples: usize,
}

fn push_mono(state: &Mutex<RecorderState>, mono: &[f32]) {
    if let Ok(mut st) = state.lock() {
        if st.samples.len() >= st.max_samples {
            return;
        }
        let remaining = st.max_samples - st.samples.len();
        st.samples
            .extend(mono.iter().take(remaining).copied().map(f32_to_i16));
    }
}

/// Owns the live microphone stream (via a dedicated thread -- `cpal::Stream`
/// is not `Send`) and the buffer it fills. Dropping this (or calling
/// `stop`) ends the recording; the OS stream is torn down when the thread
/// observes `stop_flag`.
pub(crate) struct VoiceNoteRecorder {
    state: Arc<Mutex<RecorderState>>,
    stop_flag: Arc<AtomicBool>,
    sample_rate: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VoiceNoteRecorder {
    /// Opens the default (or named) microphone and starts recording
    /// immediately. Fails cleanly -- no microphone, unreadable config, an
    /// unsupported sample format -- so the caller can surface an error
    /// instead of the record button silently doing nothing.
    pub(crate) fn start(device_name: Option<String>) -> Result<Self, String> {
        let device =
            find_input_device(&device_name).ok_or_else(|| "No microphone found.".to_string())?;
        let config = device
            .default_input_config()
            .map_err(|e| format!("Could not read microphone configuration: {e}"))?;
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
        let sample_rate = stream_config.sample_rate.0.max(8000);
        let max_samples = (sample_rate as u64 * MAX_DURATION.as_secs()) as usize;

        let state = Arc::new(Mutex::new(RecorderState {
            samples: Vec::new(),
            max_samples,
        }));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop_flag);

        let thread = std::thread::spawn(move || {
            let err_fn = |err| eprintln!("HexaTalk voice note: microphone stream error: {err}");
            let stream = match sample_format {
                cpal::SampleFormat::F32 => {
                    let state_for_stream = Arc::clone(&thread_state);
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            push_mono(&state_for_stream, &downmix_to_mono(data, channels));
                        },
                        err_fn,
                        None,
                    )
                }
                cpal::SampleFormat::I16 => {
                    let state_for_stream = Arc::clone(&thread_state);
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            let f: Vec<f32> = data.iter().copied().map(i16_to_f32).collect();
                            push_mono(&state_for_stream, &downmix_to_mono(&f, channels));
                        },
                        err_fn,
                        None,
                    )
                }
                cpal::SampleFormat::U16 => {
                    let state_for_stream = Arc::clone(&thread_state);
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            let f: Vec<f32> = data.iter().copied().map(u16_to_f32).collect();
                            push_mono(&state_for_stream, &downmix_to_mono(&f, channels));
                        },
                        err_fn,
                        None,
                    )
                }
                cpal::SampleFormat::I32 => {
                    let state_for_stream = Arc::clone(&thread_state);
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[i32], _: &cpal::InputCallbackInfo| {
                            let f: Vec<f32> = data.iter().copied().map(i32_to_f32).collect();
                            push_mono(&state_for_stream, &downmix_to_mono(&f, channels));
                        },
                        err_fn,
                        None,
                    )
                }
                cpal::SampleFormat::F64 => {
                    let state_for_stream = Arc::clone(&thread_state);
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[f64], _: &cpal::InputCallbackInfo| {
                            let f: Vec<f32> = data.iter().map(|&s| s as f32).collect();
                            push_mono(&state_for_stream, &downmix_to_mono(&f, channels));
                        },
                        err_fn,
                        None,
                    )
                }
                // Already rejected above -- unreachable, but no panic if cpal
                // ever adds a new variant this match hasn't seen.
                _ => {
                    let _ = ready_tx.send(Err("unsupported sample format".to_string()));
                    return;
                }
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("Could not open microphone: {e}")));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("Could not start microphone stream: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(stream); // stops the OS stream before the thread exits
        });

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                state,
                stop_flag,
                sample_rate,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err("Microphone did not start in time.".to_string())
            }
        }
    }

    /// Stops recording and returns the captured samples as a WAV file
    /// (mono, 16-bit PCM, native device sample rate). Empty (0-length)
    /// recordings return `None` rather than a technically-valid but
    /// pointless empty WAV.
    pub(crate) fn stop(mut self) -> Option<Vec<u8>> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let samples = self
            .state
            .lock()
            .map(|st| st.samples.clone())
            .unwrap_or_default();
        if samples.is_empty() {
            return None;
        }
        Some(encode_wav(&samples, self.sample_rate))
    }
}

impl Drop for VoiceNoteRecorder {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Minimal 44-byte-header WAV writer: mono, 16-bit PCM, little-endian.
/// No external crate needed -- the format is trivial and this makes it
/// easy to unit-test byte-for-byte.
pub(crate) fn encode_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let block_align = num_channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * block_align as u32;
    let data_len = (samples.len() * 2) as u32;
    let riff_len = 36 + data_len;

    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&num_channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// Playback goes through `media::notify`'s shared audio thread
// (`notify::play_voice_note`), not through this module -- see its doc
// comment for why a dedicated long-lived thread owns every `rodio`
// interaction in this app instead of each call site managing its own
// `!Send` `OutputStream`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_matches_riff_spec_for_a_known_sample_count() {
        let samples = vec![0i16, 100, -100, i16::MAX, i16::MIN];
        let wav = encode_wav(&samples, 16_000);

        assert_eq!(&wav[0..4], b"RIFF");
        let riff_len = u32::from_le_bytes(wav[4..8].try_into().unwrap());
        assert_eq!(riff_len as usize, wav.len() - 8);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
        assert_eq!(
            u32::from_le_bytes(wav[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16); // bits/sample
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(data_len as usize, samples.len() * 2);
        assert_eq!(wav.len(), 44 + samples.len() * 2);
    }

    #[test]
    fn wav_data_round_trips_sample_values_exactly() {
        let samples = vec![1234i16, -1234, 0, i16::MAX, i16::MIN];
        let wav = encode_wav(&samples, 8_000);
        let data = &wav[44..];
        let decoded: Vec<i16> = data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(decoded, samples);
    }

    #[test]
    fn empty_samples_still_produce_a_structurally_valid_header() {
        let wav = encode_wav(&[], 44_100);
        assert_eq!(wav.len(), 44);
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            0,
            "data chunk size must be 0 for no samples"
        );
    }

    #[test]
    fn f32_to_i16_clamps_out_of_range_input() {
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX); // clamp(-1.0) * MAX, not MIN
        assert_eq!(f32_to_i16(0.0), 0);
    }

    #[test]
    fn downmix_averages_stereo_to_mono() {
        let stereo = [1.0f32, -1.0, 0.5, 0.5]; // two frames: (1,-1), (0.5,0.5)
        let mono = downmix_to_mono(&stereo, 2);
        assert_eq!(mono, vec![0.0, 0.5]);
    }

    #[test]
    fn downmix_is_a_noop_for_mono_input() {
        let mono_in = [0.1f32, 0.2, 0.3];
        assert_eq!(downmix_to_mono(&mono_in, 1), mono_in.to_vec());
    }
}
