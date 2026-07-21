//! Media and device I/O: 1:1 WebRTC calls, room voice channels, the ADPCM
//! wire codec, screen-share capture, UI-thread image decoding, and sound /
//! OS-notification side effects.

pub(crate) mod activity;
pub(crate) mod adpcm;
pub(crate) mod call;
pub(crate) mod img_cache;
pub(crate) mod notify;
pub(crate) mod room_voice;
pub(crate) mod screenshare;
