//! Local session/identity persistence: the Convex connection bootstrap
//! task, the session-token file next to the app (so login survives a
//! restart), and the on-disk E2EE identity keypair (generated once per
//! account, never uploaded).

use std::env;

use convex::ConvexClient;
use iced::Task;

use crate::*;

pub(crate) fn connect_task(deployment_url: String) -> Task<Message> {
    Task::perform(
        async move {
            ConvexClient::new(&deployment_url)
                .await
                .map_err(|err| err.to_string())
        },
        |result| match result {
            Ok(client) => Message::Connected(client),
            Err(err) => Message::ConnectFailed(err),
        },
    )
}

pub(crate) fn session_file_path() -> std::path::PathBuf {
    let base = env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::Path::new(&base).join("Talkyss").join("session.txt")
}

pub(crate) fn save_session_to_disk(session: &Session) {
    let path = session_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &session.token);
}

pub(crate) fn load_session_token_from_disk() -> Option<String> {
    std::fs::read_to_string(session_file_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn clear_session_file() {
    let _ = std::fs::remove_file(session_file_path());
}

pub(crate) fn talkyss_data_dir() -> std::path::PathBuf {
    let base = env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::Path::new(&base).join("Talkyss")
}

pub(crate) fn identity_key_path(user_id: &str) -> std::path::PathBuf {
    talkyss_data_dir().join(format!("identity_{user_id}.key"))
}

pub(crate) fn panel_prefs_path() -> std::path::PathBuf {
    talkyss_data_dir().join("panel_prefs.txt")
}

/// Loads (channel_list_width, members_panel_preferred_width), falling back
/// to the app's defaults if the file is missing or malformed.
pub(crate) fn load_panel_prefs() -> (f32, f32) {
    let defaults = (260.0, 220.0);
    let Ok(contents) = std::fs::read_to_string(panel_prefs_path()) else {
        return defaults;
    };
    let mut parts = contents.trim().split(',');
    let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
        return defaults;
    };
    match (a.parse::<f32>(), b.parse::<f32>()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => defaults,
    }
}

pub(crate) fn save_panel_prefs(channel_list_width: f32, members_panel_preferred_width: f32) {
    let path = panel_prefs_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{channel_list_width},{members_panel_preferred_width}"));
}

/// Loads this account's local E2EE identity key from disk, generating and
/// persisting a new one on first use. The private key never leaves this
/// file; only its public half is ever sent to the server.
pub(crate) fn load_or_create_identity_key(user_id: &str) -> crypto::IdentityKeyPair {
    let path = identity_key_path(user_id);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(raw) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return crypto::IdentityKeyPair::from_bytes(raw);
        }
    }

    let key = crypto::IdentityKeyPair::generate();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, key.to_bytes());
    key
}
