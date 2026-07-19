//! Local encrypted DM history (owner-only vault).
//!
//! Messages never leave the device as plaintext. Ciphertext files live under
//! `%APPDATA%/Talkyss/vault/<owner_user_id>/` and are sealed with AES-256-GCM
//! using a key derived from the local **peerseal identity private key** via
//! HKDF-SHA256. Anyone without that key file cannot decrypt history — not
//! Convex, not another account on the same PC, not a stolen vault folder alone
//! (without the matching `peerseal_*.key`).

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const VAULT_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const HKDF_INFO: &[u8] = b"talkyss-local-history-vault-v1";
const MAX_MESSAGES: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredMessage {
    pub(crate) id: String,
    pub(crate) author_id: String,
    pub(crate) author_name: String,
    pub(crate) author_avatar_color: String,
    pub(crate) body: String,
    /// ms since epoch. Vaults written by older builds stored this as a
    /// float; the custom deserializer accepts both so those still load.
    #[serde(deserialize_with = "deserialize_ms")]
    pub(crate) sent_at: i64,
    /// If true, JPEG/PNG bytes live in `media/<id>.bin` (also encrypted).
    #[serde(default)]
    pub(crate) has_media: bool,
    #[serde(default)]
    pub(crate) media_content_type: String,
}

#[allow(dead_code)] // only referenced from the (currently dead) vault serde impl
fn deserialize_ms<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Ms {
        Int(i64),
        Float(f64),
    }
    Ok(match Ms::deserialize(deserializer)? {
        Ms::Int(i) => i,
        Ms::Float(f) => f as i64,
    })
}

fn talkyss_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("Talkyss")
}

fn vault_dir(owner_user_id: &str) -> PathBuf {
    talkyss_dir().join("vault").join(owner_user_id)
}

fn chat_path(owner_user_id: &str, conversation_id: &str) -> PathBuf {
    // Avoid path separators in Convex ids (they're usually safe alphanumerics).
    let safe: String = conversation_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    vault_dir(owner_user_id)
        .join("chats")
        .join(format!("{safe}.bin"))
}

fn media_path(owner_user_id: &str, conversation_id: &str, message_id: &str) -> PathBuf {
    let safe_c: String = conversation_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let safe_m: String = message_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    vault_dir(owner_user_id)
        .join("media")
        .join(safe_c)
        .join(format!("{safe_m}.bin"))
}

/// Derive a 32-byte vault key from the peerseal identity private key.
pub(crate) fn vault_key_from_identity_private(private_key_32: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, private_key_32);
    let mut out = [0u8; 32];
    hk.expand(HKDF_INFO, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// Load peerseal private key bytes from the same file format as peerseal Identity.
pub(crate) fn load_peerseal_private(owner_user_id: &str) -> Option<[u8; 32]> {
    let path = talkyss_dir().join(format!("peerseal_{owner_user_id}.key"));
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let _pub_hex = lines.next()?;
    let priv_hex = lines.next()?;
    hex_decode_32(priv_hex.trim())
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn encrypt_blob(key: &[u8; 32], plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), plain).ok()?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(VAULT_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Some(out)
}

fn decrypt_blob(key: &[u8; 32], blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 1 + NONCE_LEN + 16 {
        return None;
    }
    if blob[0] != VAULT_VERSION {
        return None;
    }
    let nonce = &blob[1..1 + NONCE_LEN];
    let ct = &blob[1 + NONCE_LEN..];
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher.decrypt(Nonce::from_slice(nonce), ct).ok()
}

/// Load decrypted message list for a conversation (empty if missing / wrong key).
pub(crate) fn load_chat(
    owner_user_id: &str,
    conversation_id: &str,
    vault_key: &[u8; 32],
) -> Vec<StoredMessage> {
    let path = chat_path(owner_user_id, conversation_id);
    let Ok(blob) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Some(plain) = decrypt_blob(vault_key, &blob) else {
        return Vec::new();
    };
    serde_json::from_slice(&plain).unwrap_or_default()
}

/// Replace-save full chat history (encrypted).
pub(crate) fn save_chat(
    owner_user_id: &str,
    conversation_id: &str,
    vault_key: &[u8; 32],
    messages: &[StoredMessage],
) -> Result<(), String> {
    let path = chat_path(owner_user_id, conversation_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Soft cap so vault doesn't grow forever.
    let slice = if messages.len() > MAX_MESSAGES {
        &messages[messages.len() - MAX_MESSAGES..]
    } else {
        messages
    };
    let plain = serde_json::to_vec(slice).map_err(|e| e.to_string())?;
    let blob = encrypt_blob(vault_key, &plain).ok_or_else(|| "encrypt failed".to_string())?;
    std::fs::write(path, blob).map_err(|e| e.to_string())
}

/// Append one message and persist.
pub(crate) fn append_message(
    owner_user_id: &str,
    conversation_id: &str,
    vault_key: &[u8; 32],
    msg: StoredMessage,
) -> Result<(), String> {
    let mut list = load_chat(owner_user_id, conversation_id, vault_key);
    // Dedup by id
    if list.iter().any(|m| m.id == msg.id) {
        return Ok(());
    }
    list.push(msg);
    save_chat(owner_user_id, conversation_id, vault_key, &list)
}

/// Encrypt and store media bytes for a message.
pub(crate) fn save_media(
    owner_user_id: &str,
    conversation_id: &str,
    message_id: &str,
    vault_key: &[u8; 32],
    bytes: &[u8],
) -> Result<(), String> {
    let path = media_path(owner_user_id, conversation_id, message_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let blob = encrypt_blob(vault_key, bytes).ok_or_else(|| "encrypt media failed".to_string())?;
    std::fs::write(path, blob).map_err(|e| e.to_string())
}

/// Load and decrypt media for a message.
pub(crate) fn load_media(
    owner_user_id: &str,
    conversation_id: &str,
    message_id: &str,
    vault_key: &[u8; 32],
) -> Option<Vec<u8>> {
    let path = media_path(owner_user_id, conversation_id, message_id);
    let blob = std::fs::read(path).ok()?;
    decrypt_blob(vault_key, &blob)
}

/// Stable UI attachment URL for vault media (not a network URL).
pub(crate) fn media_url_tag(message_id: &str) -> String {
    format!("vaultmedia:{message_id}")
}

pub(crate) fn is_media_url_tag(url: &str) -> bool {
    url.starts_with("vaultmedia:")
}

pub(crate) fn media_id_from_url(url: &str) -> Option<&str> {
    url.strip_prefix("vaultmedia:")
}

/// Human path for settings / docs.
pub(crate) fn vault_root_display(owner_user_id: &str) -> String {
    vault_dir(owner_user_id).display().to_string()
}

/// Wipe one conversation's encrypted vault files (owner only, this device).
pub(crate) fn wipe_chat(owner_user_id: &str, conversation_id: &str) {
    let _ = std::fs::remove_file(chat_path(owner_user_id, conversation_id));
    let media_dir = media_path(owner_user_id, conversation_id, "_")
        .parent()
        .map(Path::to_path_buf);
    if let Some(dir) = media_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Drop the entire local vault tree for this account (all chats + media).
pub(crate) fn wipe_all_vaults(owner_user_id: &str) {
    let dir = vault_dir(owner_user_id);
    let _ = std::fs::remove_dir_all(dir);
}

pub(crate) fn talkyss_root() -> PathBuf {
    talkyss_dir()
}

/// Debug helper: fingerprint of vault key (not secret) for UI.
pub(crate) fn vault_key_fingerprint(vault_key: &[u8; 32]) -> String {
    use sha2::Digest;
    let d = Sha256::digest(vault_key);
    format!("{:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3])
}

/// Encode optional short note (unused helper for future export).
#[allow(dead_code)]
pub(crate) fn encode_export_marker(owner: &str) -> String {
    BASE64_STANDARD.encode(format!("talkyss-vault:{owner}"))
}
