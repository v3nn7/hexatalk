//! Local user-settings persistence: audio device choices, the mic noise
//! gate threshold, and per-peer voice volumes survive app restarts via a
//! JSON file next to the session token (`%APPDATA%/HexaTalk/settings.json`).
//!
//! The schema is deliberately tolerant: `#[serde(default)]` everywhere, so a
//! file written by an older/newer build (missing or unknown fields) loads
//! fine instead of crashing or resetting everything.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::session_store::hexatalk_data_dir;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct PersistedSettings {
    /// Preferred microphone (`None` = OS default).
    pub(super) input_device: Option<String>,
    /// Preferred speaker/headphones (`None` = OS default).
    pub(super) output_device: Option<String>,
    /// Mic noise gate threshold (linear amplitude 0..1). `None` = app default
    /// (`call::DEFAULT_NOISE_GATE`).
    pub(super) noise_gate: Option<f32>,
    /// Global UI scale multiplier (0.8..=1.4). `None` = 1.0.
    pub(super) ui_scale: Option<f32>,
    /// Per-peer voice volume gains (peer user_id -> gain, 0.0..=5.0). The
    /// 1:1 call remote uses the special key "*".
    pub(super) voice_gains: HashMap<String, f32>,
    /// When true (default), scan local processes and share a Discord-style
    /// "Playing / Active in …" activity on the presence heartbeat + profile.
    pub(super) share_activity: Option<bool>,
    /// When true (default), pad E2EE payloads to size buckets so the server
    /// cannot infer message length from ciphertext size.
    pub(super) e2ee_pad_messages: Option<bool>,
}

fn settings_file_path() -> std::path::PathBuf {
    hexatalk_data_dir().join("settings.json")
}

/// Loads persisted settings, falling back to defaults if the file is
/// missing or malformed.
pub(super) fn load_settings() -> PersistedSettings {
    let Ok(contents) = std::fs::read_to_string(settings_file_path()) else {
        return PersistedSettings::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Saves settings. Written via a temp file + rename so a crash mid-write
/// can't leave a truncated JSON behind (the old file stays intact).
pub(super) fn save_settings(settings: &PersistedSettings) {
    let path = settings_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_string_pretty(settings) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}
