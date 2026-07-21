//! OS notification toasts (`notify-rust`) and embedded MP3 sounds
//! (notification ping + looping ringtone) via `rodio` on a dedicated audio
//! thread — works on Windows and Linux (including AppImage).

use std::io::Cursor;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rodio::source::Source;
use rodio::{Decoder, OutputStream, Sink};

/// Fire-and-forget OS notification (Windows toast / Linux notify-send).
pub(crate) fn notify_desktop(summary: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .appname("HexaTalk")
        .summary(summary)
        .body(body)
        .show();
}

/// Rate-limited variant of `notify_desktop`.
pub(crate) fn notify_desktop_throttled(
    summary: &str,
    body: &str,
    min_interval: std::time::Duration,
) {
    static LAST_SHOWN: OnceLock<Mutex<std::collections::HashMap<String, Instant>>> =
        OnceLock::new();
    let now = Instant::now();
    let map = LAST_SHOWN.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(mut times) = map.lock() {
        if let Some(last) = times.get(summary) {
            if now.duration_since(*last) < min_interval {
                return;
            }
        }
        times.insert(summary.to_string(), now);
    }
    notify_desktop(summary, body);
}

const RINGTONE_BYTES: &[u8] = include_bytes!("../../assets/sounds/callsound.mp3");
const NOTIFICATION_BYTES: &[u8] = include_bytes!("../../assets/sounds/notification.mp3");

enum AudioCmd {
    Notify,
    RingtoneStart,
    RingtoneStop,
}

fn audio_tx() -> Option<&'static Sender<AudioCmd>> {
    static TX: OnceLock<Option<Sender<AudioCmd>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<AudioCmd>();
        std::thread::Builder::new()
            .name("hexatalk-audio".into())
            .spawn(move || audio_thread(rx))
            .ok()?;
        Some(tx)
    })
    .as_ref()
}

fn audio_thread(rx: mpsc::Receiver<AudioCmd>) {
    // OutputStream is !Send — lives only on this thread.
    let Ok((_stream, handle)) = OutputStream::try_default() else {
        // Drain forever so senders never block on a dead channel full of backlog.
        while rx.recv().is_ok() {}
        return;
    };

    let mut ring_sink: Option<Sink> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            AudioCmd::Notify => {
                let Ok(sink) = Sink::try_new(&handle) else {
                    continue;
                };
                let Ok(source) = Decoder::new(Cursor::new(NOTIFICATION_BYTES)) else {
                    continue;
                };
                sink.append(source);
                sink.detach();
            }
            AudioCmd::RingtoneStart => {
                // Stop previous loop first.
                if let Some(prev) = ring_sink.take() {
                    prev.stop();
                }
                let Ok(sink) = Sink::try_new(&handle) else {
                    continue;
                };
                let Ok(source) = Decoder::new(Cursor::new(RINGTONE_BYTES)) else {
                    continue;
                };
                sink.append(source.repeat_infinite());
                sink.play();
                ring_sink = Some(sink);
            }
            AudioCmd::RingtoneStop => {
                if let Some(sink) = ring_sink.take() {
                    sink.stop();
                }
            }
        }
    }
}

/// Plays the notification sound once (throttled).
pub(crate) fn notification_sound() {
    static LAST_PLAYED: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    const MIN_INTERVAL: Duration = Duration::from_millis(800);
    let slot = LAST_PLAYED.get_or_init(|| Mutex::new(None));
    if let Ok(mut last) = slot.lock() {
        if last.is_some_and(|t| t.elapsed() < MIN_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    if let Some(tx) = audio_tx() {
        let _ = tx.send(AudioCmd::Notify);
    }
}

/// Starts looping the ringtone (restarts if already playing).
pub(crate) fn ringtone_start() {
    if let Some(tx) = audio_tx() {
        let _ = tx.send(AudioCmd::RingtoneStart);
    }
}

/// Stops the ringtone.
pub(crate) fn ringtone_stop() {
    if let Some(tx) = audio_tx() {
        let _ = tx.send(AudioCmd::RingtoneStop);
    }
}
