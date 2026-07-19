//! Sound + OS notification side effects: the Windows `MessageBeep` alert
//! sounds and the cross-platform toast via `notify-rust`.

// Plain u32 values matching the Win32 MB_ICON* MessageBeep flags, kept as
// our own constants (rather than referencing `windows_sys` at call sites)
// so the call sites stay platform-agnostic even though the actual sound
// only plays on Windows.
pub(crate) const BEEP_MESSAGE: u32 = 0; // MB_OK: new message / unread chat
pub(crate) const BEEP_FRIEND_REQUEST: u32 = 32; // MB_ICONQUESTION
pub(crate) const BEEP_INCOMING_CALL: u32 = 48; // MB_ICONEXCLAMATION
pub(crate) const BEEP_CALL_CONNECTED: u32 = 64; // MB_ICONASTERISK

#[cfg(windows)]
pub(crate) fn play_beep(beep_type: u32) {
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(beep_type);
    }
}

#[cfg(not(windows))]
pub(crate) fn play_beep(_beep_type: u32) {}

/// Fire-and-forget OS notification (Windows toast via `notify-rust`). Mirrors
/// `play_beep`'s style: synchronous, errors swallowed -- a missed toast
/// isn't worth failing an update over, and the accompanying beep already
/// covers the "something happened" signal if the toast doesn't show.
pub(crate) fn notify_desktop(summary: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .appname("Talkyss")
        .summary(summary)
        .body(body)
        .show();
}

/// Incoming-call ringtone: `assets/sounds/callsound.mp3`, embedded into the
/// binary at compile time and looped through the Win32 MCI (which plays MP3
/// natively, so no decoder dependency is needed). The file is materialized
/// to the temp dir once because MCI needs a real path.
#[cfg(windows)]
const RINGTONE_BYTES: &[u8] = include_bytes!("../../assets/sounds/callsound.mp3");

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
    let path = std::env::temp_dir().join("talkyss_callsound.mp3");
    if !path.exists() && std::fs::write(&path, RINGTONE_BYTES).is_err() {
        return;
    }
    mci("close talkyss_ring");
    mci(&format!(
        "open \"{}\" alias talkyss_ring type mpegvideo",
        path.to_string_lossy()
    ));
    mci("play talkyss_ring repeat");
}

/// Stops the ringtone and releases the MCI device.
#[cfg(windows)]
pub(crate) fn ringtone_stop() {
    mci("stop talkyss_ring");
    mci("close talkyss_ring");
}

#[cfg(not(windows))]
pub(crate) fn ringtone_start() {}

#[cfg(not(windows))]
pub(crate) fn ringtone_stop() {}
