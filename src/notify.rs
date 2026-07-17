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
