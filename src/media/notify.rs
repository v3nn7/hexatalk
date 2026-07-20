//! OS notification side effects: the cross-platform toast via `notify-rust`
//! and the incoming-call ringtone through the Win32 MCI.

/// Fire-and-forget OS notification (Windows toast via `notify-rust`).
/// Synchronous, errors swallowed -- a missed toast isn't worth failing an
/// update over.
pub(crate) fn notify_desktop(summary: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .appname("HexaTalk")
        .summary(summary)
        .body(body)
        .show();
}

/// Rate-limited variant of `notify_desktop`: toasts with the same `summary`
/// are suppressed when they arrive faster than `min_interval`, so a burst of
/// incoming messages can't flood the OS notification center. First toast of
/// each kind always goes through immediately.
pub(crate) fn notify_desktop_throttled(
    summary: &str,
    body: &str,
    min_interval: std::time::Duration,
) {
    use std::sync::{Mutex, OnceLock};
    static LAST_SHOWN: OnceLock<
        Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = OnceLock::new();
    let now = std::time::Instant::now();
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

/// Incoming-call ringtone: `assets/sounds/callsound.mp3`, embedded into the
/// binary at compile time and looped through the Win32 MCI (which plays MP3
/// natively, so no decoder dependency is needed). The file is materialized
/// to the temp dir once because MCI needs a real path.
#[cfg(windows)]
const RINGTONE_BYTES: &[u8] = include_bytes!("../../assets/sounds/callsound.mp3");

/// Notification ping: `assets/sounds/notification.mp3`, embedded and played
/// one-shot through the same MCI path as the ringtone.
#[cfg(windows)]
const NOTIFICATION_BYTES: &[u8] = include_bytes!("../../assets/sounds/notification.mp3");

/// Materializes an embedded sound asset to the temp dir (MCI needs a real
/// path). The write goes through a PID-unique staging file + rename so a
/// crash mid-write can never leave a truncated file that later runs would
/// mistake for the real thing. Losing a race against another instance is
/// fine -- the bytes are identical.
#[cfg(windows)]
fn materialize_sound(filename: &str, bytes: &[u8]) -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join(filename);
    if path.exists() {
        return Some(path);
    }
    let staging = std::env::temp_dir().join(format!("{filename}.{}.tmp", std::process::id()));
    if std::fs::write(&staging, bytes).is_err() {
        return None;
    }
    if std::fs::rename(&staging, &path).is_err() {
        let _ = std::fs::remove_file(&staging);
        if !path.exists() {
            return None;
        }
    }
    Some(path)
}

/// Plays the notification sound once. Rapid consecutive notifications are
/// throttled (one playback per `NOTIFY_SOUND_MIN_INTERVAL`): each play is a
/// close/open/play MCI sequence, and retriggering it faster than that just
/// restarts the first milliseconds of the sample over and over, which reads
/// as stutter rather than "more notifications".
#[cfg(windows)]
pub(crate) fn notification_sound() {
    use std::sync::{Mutex, OnceLock};
    static LAST_PLAYED: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    const NOTIFY_SOUND_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(800);
    let slot = LAST_PLAYED.get_or_init(|| Mutex::new(None));
    if let Ok(mut last) = slot.lock() {
        if last.is_some_and(|t| t.elapsed() < NOTIFY_SOUND_MIN_INTERVAL) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let Some(path) = materialize_sound("hexatalk_notification.mp3", NOTIFICATION_BYTES) else {
        return;
    };
    mci("close hexatalk_notify");
    mci(&format!(
        "open \"{}\" alias hexatalk_notify type mpegvideo",
        path.to_string_lossy()
    ));
    mci("play hexatalk_notify");
}

#[cfg(not(windows))]
pub(crate) fn notification_sound() {}

#[cfg(windows)]
fn mci(command: &str) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new(command)
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        windows_sys::Win32::Media::Multimedia::mciSendStringW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
        );
    }
}

/// Starts looping the ringtone. Every call restarts playback cleanly.
#[cfg(windows)]
pub(crate) fn ringtone_start() {
    let Some(path) = materialize_sound("hexatalk_callsound.mp3", RINGTONE_BYTES) else {
        return;
    };
    mci("close hexatalk_ring");
    mci(&format!(
        "open \"{}\" alias hexatalk_ring type mpegvideo",
        path.to_string_lossy()
    ));
    mci("play hexatalk_ring repeat");
}

/// Stops the ringtone and releases the MCI device.
#[cfg(windows)]
pub(crate) fn ringtone_stop() {
    mci("stop hexatalk_ring");
    mci("close hexatalk_ring");
}

#[cfg(not(windows))]
pub(crate) fn ringtone_start() {}

#[cfg(not(windows))]
pub(crate) fn ringtone_stop() {}
