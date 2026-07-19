//! End-to-end encryption for direct-conversation messages (protocol TKR3).
//!
//! ## Threat model
//!
//! - Convex (and anyone with DB access) must not read DM bodies or attachment
//!   bytes. Ciphertext + metadata only.
//! - Compromising a device *after* messages were read should not expose older
//!   message keys still sitting on the server (symmetric ratchet: old message
//!   keys are discarded; a local decrypt cache keeps plaintext for the UI
//!   only on this device).
//! - This is still not a full Signal stack (no DH ratchet / post-compromise
//!   healing, no prekey server, no sealed sender). It is a reliable step up
//!   from v1 static ECDH, and deliberately simpler than the broken TKR2
//!   Double Ratchet init that desynced both sides.
//!
//! ## Construction (TKR3 dual chain)
//!
//! 1. Long-term X25519 identity keys (private key only on device).
//! 2. Shared root `SK = HKDF(X25519(IKa, IKb))`.
//! 3. Two independent sending chains, one per peer, derived deterministically
//!    from `SK` and the sorted pair of user ids. Both sides can send first;
//!    no handshake round, no initiator/responder split that can desync.
//! 4. AES-256-GCM per message; AAD binds conversation + both user ids.
//! 5. Payload is a small JSON envelope so attachment keys travel inside the
//!    same E2EE blob as the text.
//!
//! Wire blob (base64):
//! `TKR3 || chain_id(u8) || n(u32 LE) || nonce(12) || ciphertext+tag`
//!
//! `chain_id` is 0 when the sender has the lexicographically smaller user id,
//! else 1. Receiver advances that peer's chain (with skip keys for gaps).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const MAX_SKIP: u32 = 512;

/// Wire magic for dual-chain blobs (`T`alkyss `K`rypto `R`atchet `3`).
const WIRE_MAGIC: &[u8; 4] = b"TKR3";
/// Reject leftover TKR2 sessions on disk.
const SESSION_VERSION: u32 = 3;

const HKDF_INFO_ROOT: &[u8] = b"talkyss-root-v3";
const HKDF_INFO_CHAIN0: &[u8] = b"talkyss-chain0-v3";
const HKDF_INFO_CHAIN1: &[u8] = b"talkyss-chain1-v3";
const HKDF_INFO_CK: &[u8] = b"talkyss-ck-v3";

/// A user's long-term X25519 identity key pair. The private half never leaves
/// this device; only the public half is uploaded to Convex.
#[derive(Clone)]
pub(crate) struct IdentityKeyPair {
    secret: StaticSecret,
}

impl IdentityKeyPair {
    pub(crate) fn generate() -> Self {
        Self {
            secret: StaticSecret::random(),
        }
    }

    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(bytes),
        }
    }

    pub(crate) fn to_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    pub(crate) fn public_key_bytes(&self) -> [u8; 32] {
        *PublicKey::from(&self.secret).as_bytes()
    }

    pub(crate) fn public_key_base64(&self) -> String {
        BASE64_STANDARD.encode(self.public_key_bytes())
    }

    pub(crate) fn shared_secret_with(&self, their_public_key_base64: &str) -> Option<[u8; 32]> {
        let their = decode_public_b64(their_public_key_base64)?;
        Some(
            self.secret
                .diffie_hellman(&their.get_public_key())
                .to_bytes(),
        )
    }
}

pub(crate) fn fingerprint_for_public_b64(public_key_base64: &str) -> String {
    let Ok(raw) = BASE64_STANDARD.decode(public_key_base64) else {
        return String::new();
    };
    let digest = Sha256::digest(&raw);
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

#[repr(align(64))]
struct AlignedKey {
    key: [u8; 32],
    _padding: [u8; 32],
}

impl AlignedKey {
    pub fn get_public_key(&self) -> PublicKey {
        PublicKey::from(self.key)
    }
}

fn decode_public_b64(public_key_base64: &str) -> Option<AlignedKey> {
    let bytes = AlignedKey {
        key: BASE64_STANDARD
            .decode(public_key_base64)
            .ok()?
            .try_into()
            .ok()?,
        _padding: [0u8; 32],
    };

    Some(bytes)
}

fn hkdf_32(ikm: &[u8], salt: Option<&[u8]>, info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(salt, ikm);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

fn kdf_ck(ck: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<Sha256>::new(None, ck);
    let mut out = [0u8; 64];
    hk.expand(HKDF_INFO_CK, &mut out)
        .expect("64 bytes is a valid HKDF-SHA256 output length");
    let mut next_ck = [0u8; 32];
    let mut mk = [0u8; 32];
    next_ck.copy_from_slice(&out[..32]);
    mk.copy_from_slice(&out[32..]);
    (next_ck, mk)
}

/// Plaintext envelope stored inside the ratchet ciphertext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MessagePayload {
    pub(crate) text: String,
    /// Random AES-256 key for the attachment blob (base64), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) att_key: Option<String>,
    /// GCM nonce for the attachment blob (base64), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) att_nonce: Option<String>,
}

impl MessagePayload {
    pub(crate) fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            att_key: None,
            att_nonce: None,
        }
    }

    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).expect("MessagePayload serializes")
    }

    pub(crate) fn decode(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

/// Encrypt raw attachment bytes. Returns (ciphertext, key_b64, nonce_b64).
pub(crate) fn encrypt_attachment(plain: &[u8]) -> (Vec<u8>, String, String) {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32-byte key");
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, plain).expect("attachment encryption");
    (
        ct,
        BASE64_STANDARD.encode(key),
        BASE64_STANDARD.encode(nonce_bytes),
    )
}

/// Decrypt attachment bytes produced by [`encrypt_attachment`].
pub(crate) fn decrypt_attachment(
    key_b64: &str,
    nonce_b64: &str,
    ciphertext: &[u8],
) -> Option<Vec<u8>> {
    let key = BASE64_STANDARD.decode(key_b64).ok()?;
    let nonce_bytes = BASE64_STANDARD.decode(nonce_b64).ok()?;
    if key.len() != 32 || nonce_bytes.len() != NONCE_LEN {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    cipher.decrypt(nonce, ciphertext).ok()
}

#[derive(Clone, Serialize, Deserialize)]
struct SkippedKey {
    n: u32,
    mk: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedSession {
    version: u32,
    peer_user_id: String,
    peer_public_key: String,
    /// 0 if local user id < peer, else 1.
    my_chain_id: u8,
    send_ck: String,
    send_n: u32,
    recv_ck: String,
    recv_n: u32,
    skipped: Vec<SkippedKey>,
}

/// Dual-chain ratchet session for one DM peer, persisted under APPDATA.
#[derive(Clone)]
pub(crate) struct RatchetSession {
    path: PathBuf,
    peer_user_id: String,
    peer_public_key: [u8; 32],
    my_chain_id: u8,
    send_ck: [u8; 32],
    send_n: u32,
    recv_ck: [u8; 32],
    recv_n: u32,
    /// n -> message key for the peer's chain
    mkskipped: HashMap<u32, [u8; 32]>,
}

impl RatchetSession {
    fn session_path(base_dir: &Path, local_user_id: &str, peer_user_id: &str) -> PathBuf {
        base_dir.join(format!("ratchet_v3_{local_user_id}_{peer_user_id}.json"))
    }

    /// Drop persisted ratchet state for one DM pair (and any legacy TKR2 file).
    pub(crate) fn clear(base_dir: &Path, local_user_id: &str, peer_user_id: &str) {
        let _ = std::fs::remove_file(Self::session_path(base_dir, local_user_id, peer_user_id));
        let legacy = base_dir.join(format!("ratchet_{local_user_id}_{peer_user_id}.json"));
        let _ = std::fs::remove_file(legacy);
    }

    /// Delete every ratchet_*.json under the Talkyss data dir.
    pub(crate) fn clear_all(base_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(base_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if (name.starts_with("ratchet_") || name.starts_with("ratchet_v3_"))
                && name.ends_with(".json")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Load an existing session or create a fresh one from identity DH.
    pub(crate) fn load_or_create(
        base_dir: &Path,
        local_user_id: &str,
        peer_user_id: &str,
        identity: &IdentityKeyPair,
        peer_public_key_b64: &str,
    ) -> Option<Self> {
        // Drop broken TKR2 session files if present.
        let legacy = base_dir.join(format!("ratchet_{local_user_id}_{peer_user_id}.json"));
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }

        let path = Self::session_path(base_dir, local_user_id, peer_user_id);
        if path.exists() {
            if let Some(mut session) = Self::load_from_path(&path) {
                if session.peer_public_key_b64() != peer_public_key_b64 {
                    let _ = std::fs::remove_file(&path);
                } else {
                    session.path = path;
                    return Some(session);
                }
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }

        let sk_raw = identity.shared_secret_with(peer_public_key_b64)?;
        let root = hkdf_32(&sk_raw, None, HKDF_INFO_ROOT);
        let peer_public_key = decode_public_b64(peer_public_key_b64)?.key;

        // Both peers derive identical chain seeds from the same root + the
        // sorted user-id pair (encoded into the HKDF infos via order only).
        let chain0 = hkdf_32(&root, None, HKDF_INFO_CHAIN0);
        let chain1 = hkdf_32(&root, None, HKDF_INFO_CHAIN1);

        let my_chain_id: u8 = if local_user_id < peer_user_id { 0 } else { 1 };
        let (send_ck, recv_ck) = if my_chain_id == 0 {
            (chain0, chain1)
        } else {
            (chain1, chain0)
        };

        let session = Self {
            path: path.clone(),
            peer_user_id: peer_user_id.to_string(),
            peer_public_key,
            my_chain_id,
            send_ck,
            send_n: 0,
            recv_ck,
            recv_n: 0,
            mkskipped: HashMap::new(),
        };
        let _ = session.save();
        Some(session)
    }

    fn peer_public_key_b64(&self) -> String {
        BASE64_STANDARD.encode(self.peer_public_key)
    }

    fn load_from_path(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let p: PersistedSession = serde_json::from_str(&raw).ok()?;
        if p.version != SESSION_VERSION {
            return None;
        }
        let mut mkskipped = HashMap::new();
        for entry in p.skipped {
            let mk_bytes = BASE64_STANDARD.decode(&entry.mk).ok()?;
            let mk: [u8; 32] = mk_bytes.try_into().ok()?;
            mkskipped.insert(entry.n, mk);
        }
        Some(Self {
            path: path.to_path_buf(),
            peer_user_id: p.peer_user_id,
            peer_public_key: decode_public_b64(&p.peer_public_key)?.key,
            my_chain_id: p.my_chain_id,
            send_ck: {
                let b = BASE64_STANDARD.decode(&p.send_ck).ok()?;
                b.try_into().ok()?
            },
            send_n: p.send_n,
            recv_ck: {
                let b = BASE64_STANDARD.decode(&p.recv_ck).ok()?;
                b.try_into().ok()?
            },
            recv_n: p.recv_n,
            mkskipped,
        })
    }

    pub(crate) fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let skipped: Vec<SkippedKey> = self
            .mkskipped
            .iter()
            .map(|(n, mk)| SkippedKey {
                n: *n,
                mk: BASE64_STANDARD.encode(mk),
            })
            .collect();
        let p = PersistedSession {
            version: SESSION_VERSION,
            peer_user_id: self.peer_user_id.clone(),
            peer_public_key: self.peer_public_key_b64(),
            my_chain_id: self.my_chain_id,
            send_ck: BASE64_STANDARD.encode(self.send_ck),
            send_n: self.send_n,
            recv_ck: BASE64_STANDARD.encode(self.recv_ck),
            recv_n: self.recv_n,
            skipped,
        };
        let json = serde_json::to_string(&p).expect("session serializes");
        std::fs::write(&self.path, json)
    }

    fn skip_message_keys(&mut self, until: u32) -> Result<(), ()> {
        if until.saturating_sub(self.recv_n) > MAX_SKIP {
            return Err(());
        }
        while self.recv_n < until {
            let (next, mk) = kdf_ck(&self.recv_ck);
            self.recv_ck = next;
            self.mkskipped.insert(self.recv_n, mk);
            self.recv_n += 1;
        }
        if self.mkskipped.len() > MAX_SKIP as usize {
            // Keep the most recent keys only.
            let mut keys: Vec<u32> = self.mkskipped.keys().copied().collect();
            keys.sort_unstable();
            let drop_n = keys.len().saturating_sub(MAX_SKIP as usize / 2);
            for n in keys.into_iter().take(drop_n) {
                self.mkskipped.remove(&n);
            }
        }
        Ok(())
    }

    /// Encrypt a payload. `aad` should be stable per conversation.
    pub(crate) fn encrypt(&mut self, plaintext: &str, aad: &[u8]) -> Option<String> {
        let (next_ck, mk) = kdf_ck(&self.send_ck);
        self.send_ck = next_ck;
        let n = self.send_n;
        self.send_n = self.send_n.saturating_add(1);

        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let cipher = Aes256Gcm::new_from_slice(&mk).ok()?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad,
                },
            )
            .ok()?;

        let mut out = Vec::with_capacity(4 + 1 + 4 + NONCE_LEN + ct.len());
        out.extend_from_slice(WIRE_MAGIC);
        out.push(self.my_chain_id);
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        let _ = self.save();
        Some(BASE64_STANDARD.encode(out))
    }

    pub(crate) fn decrypt(&mut self, blob_b64: &str, aad: &[u8]) -> Option<String> {
        let blob = BASE64_STANDARD.decode(blob_b64).ok()?;
        if blob.len() < 4 + 1 + 4 + NONCE_LEN + 16 {
            return None;
        }
        if &blob[0..4] != WIRE_MAGIC {
            return None;
        }
        let chain_id = blob[4];
        let n = u32::from_le_bytes(blob[5..9].try_into().ok()?);
        let nonce_bytes: [u8; NONCE_LEN] = blob[9..9 + NONCE_LEN].try_into().ok()?;
        let ct = &blob[9 + NONCE_LEN..];

        // Peer's chain is the opposite of ours.
        let expected_peer_chain = 1 - self.my_chain_id;
        if chain_id != expected_peer_chain {
            // Own echo (or corrupt). Caller should use the outbound cache.
            return None;
        }

        if let Some(mk) = self.mkskipped.remove(&n) {
            let plain = decrypt_with_mk(&mk, &nonce_bytes, ct, aad)?;
            let _ = self.save();
            return Some(plain);
        }

        if n < self.recv_n {
            // Already consumed and not in skip map -- need cache.
            return None;
        }

        self.skip_message_keys(n).ok()?;
        let (next_ck, mk) = kdf_ck(&self.recv_ck);
        self.recv_ck = next_ck;
        self.recv_n = self.recv_n.saturating_add(1);

        let plain = decrypt_with_mk(&mk, &nonce_bytes, ct, aad)?;
        let _ = self.save();
        Some(plain)
    }
}

fn decrypt_with_mk(
    mk: &[u8; 32],
    nonce_bytes: &[u8; NONCE_LEN],
    ct: &[u8],
    aad: &[u8],
) -> Option<String> {
    let cipher = Aes256Gcm::new_from_slice(mk).ok()?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plain = cipher.decrypt(nonce, Payload { msg: ct, aad }).ok()?;
    String::from_utf8(plain).ok()
}

/// Local cache so the UI can re-render history without replaying the ratchet.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct DecryptCache {
    /// key: `message_id + ":" + sha256(ciphertext)[0..16 hex]`
    entries: HashMap<String, String>,
    /// Ciphertext-hash → plaintext, for reply-quote lookups without message id.
    #[serde(default)]
    by_ciphertext: HashMap<String, String>,
}

impl DecryptCache {
    fn path(base_dir: &Path, local_user_id: &str, peer_user_id: &str) -> PathBuf {
        base_dir.join(format!("decrypt_cache_{local_user_id}_{peer_user_id}.json"))
    }

    pub(crate) fn load(base_dir: &Path, local_user_id: &str, peer_user_id: &str) -> Self {
        let path = Self::path(base_dir, local_user_id, peer_user_id);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, base_dir: &Path, local_user_id: &str, peer_user_id: &str) {
        let path = Self::path(base_dir, local_user_id, peer_user_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Drop cache for this pair (used after protocol upgrades / chat clear).
    pub(crate) fn clear(base_dir: &Path, local_user_id: &str, peer_user_id: &str) {
        let _ = std::fs::remove_file(Self::path(base_dir, local_user_id, peer_user_id));
    }

    /// Delete every decrypt_cache_*.json under the Talkyss data dir.
    pub(crate) fn clear_all(base_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(base_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("decrypt_cache_") && name.ends_with(".json") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn ct_hash(ciphertext: &str) -> String {
        let digest = Sha256::digest(ciphertext.as_bytes());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    fn entry_key(message_id: &str, ciphertext: &str) -> String {
        format!("{message_id}:{}", Self::ct_hash(ciphertext))
    }

    pub(crate) fn get(&self, message_id: &str, ciphertext: &str) -> Option<String> {
        self.entries
            .get(&Self::entry_key(message_id, ciphertext))
            .cloned()
            .or_else(|| self.get_by_ciphertext(ciphertext))
    }

    pub(crate) fn get_by_ciphertext(&self, ciphertext: &str) -> Option<String> {
        self.by_ciphertext.get(&Self::ct_hash(ciphertext)).cloned()
    }

    pub(crate) fn put(&mut self, message_id: &str, ciphertext: &str, plaintext: String) {
        self.entries
            .insert(Self::entry_key(message_id, ciphertext), plaintext.clone());
        self.by_ciphertext
            .insert(Self::ct_hash(ciphertext), plaintext);
        if self.entries.len() > 5000 {
            let excess = self.entries.len() - 4000;
            let keys: Vec<String> = self.entries.keys().take(excess).cloned().collect();
            for k in keys {
                self.entries.remove(&k);
            }
        }
        if self.by_ciphertext.len() > 5000 {
            let excess = self.by_ciphertext.len() - 4000;
            let keys: Vec<String> = self.by_ciphertext.keys().take(excess).cloned().collect();
            for k in keys {
                self.by_ciphertext.remove(&k);
            }
        }
    }
}

/// True when `s` is a base64-encoded TKR3 ratchet blob (not plaintext).
pub(crate) fn looks_like_ratchet_blob(s: &str) -> bool {
    BASE64_STANDARD
        .decode(s)
        .ok()
        .is_some_and(|b| b.len() > 20 && (b.starts_with(WIRE_MAGIC) || b.starts_with(b"TKR2")))
}

/// Build AAD for a DM: conversation id + both user ids (sorted).
fn dm_aad(conversation_id: &str, user_a: &str, user_b: &str) -> Vec<u8> {
    let (a, b) = if user_a < user_b {
        (user_a, user_b)
    } else {
        (user_b, user_a)
    };
    format!("talkyss-dm-v3|{conversation_id}|{a}|{b}").into_bytes()
}

// ---------------------------------------------------------------------------
// Group / channel conversation keys (protocol TGK1)
// ---------------------------------------------------------------------------
//
// Threat model: Convex stores ciphertext only for group and server-channel
// bodies. Every member holds the same AES-256 conversation key, sealed to
// their long-term X25519 public key (the same key published as `publicKey`
// / peerseal identity). Membership changes should rotate the epoch (force
// republish); current code bootstraps epoch 1 and can share with joiners.
//
// Wire message (base64):
//   TGK1 || epoch(u32 LE) || nonce(12) || AES-GCM(ciphertext+tag)
// AAD: talkyss-group-v1|{conversationId}|{epoch}

/// Wire magic for group-key message blobs.
const GROUP_WIRE_MAGIC: &[u8; 4] = b"TGK1";
const HKDF_INFO_GROUP_WRAP: &[u8] = b"talkyss-group-wrap-v1";
const HKDF_INFO_GROUP_MSG: &[u8] = b"talkyss-group-msg-v1";

fn group_aad(conversation_id: &str, epoch: u32) -> Vec<u8> {
    format!("talkyss-group-v1|{conversation_id}|{epoch}").into_bytes()
}

/// True when `s` looks like a TGK1 group ciphertext blob.
pub(crate) fn looks_like_group_blob(s: &str) -> bool {
    BASE64_STANDARD
        .decode(s)
        .ok()
        .is_some_and(|b| b.len() > 4 + 4 + NONCE_LEN + 16 && b.starts_with(GROUP_WIRE_MAGIC))
}

/// Generate a fresh 32-byte group conversation key.
pub(crate) fn generate_group_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

/// Seal a group key to a member's X25519 public key.
/// Returns (eph_public_b64, sealed_key_b64) where sealed is nonce||ct of the 32-byte key.
pub(crate) fn seal_group_key_for(
    recipient_public_b64: &str,
    group_key: &[u8; 32],
) -> Option<(String, String)> {
    let their = decode_public_b64(recipient_public_b64)?;
    let eph = StaticSecret::random();
    let eph_pub = PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&their.get_public_key());
    let wrap_key = hkdf_32(shared.as_bytes(), None, HKDF_INFO_GROUP_WRAP);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(&wrap_key).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, group_key.as_slice()).ok()?;
    let mut sealed = Vec::with_capacity(NONCE_LEN + ct.len());
    sealed.extend_from_slice(&nonce_bytes);
    sealed.extend_from_slice(&ct);
    Some((
        BASE64_STANDARD.encode(eph_pub.as_bytes()),
        BASE64_STANDARD.encode(sealed),
    ))
}

/// Unseal a group key using our long-term identity private key.
pub(crate) fn unseal_group_key(
    identity: &IdentityKeyPair,
    eph_public_b64: &str,
    sealed_key_b64: &str,
) -> Option<[u8; 32]> {
    let eph_bytes = decode_public_b64(eph_public_b64)?;
    let sealed = BASE64_STANDARD.decode(sealed_key_b64).ok()?;
    if sealed.len() < NONCE_LEN + 16 {
        return None;
    }
    let (nonce_bytes, ct) = sealed.split_at(NONCE_LEN);
    let shared = identity.secret.diffie_hellman(&eph_bytes.get_public_key());
    let wrap_key = hkdf_32(shared.as_bytes(), None, HKDF_INFO_GROUP_WRAP);
    let cipher = Aes256Gcm::new_from_slice(&wrap_key).ok()?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plain = cipher.decrypt(nonce, ct).ok()?;
    let key: [u8; 32] = plain.try_into().ok()?;
    Some(key)
}

fn group_message_key(group_key: &[u8; 32], epoch: u32) -> [u8; 32] {
    let mut ikm = Vec::with_capacity(36);
    ikm.extend_from_slice(group_key);
    ikm.extend_from_slice(&epoch.to_le_bytes());
    hkdf_32(&ikm, None, HKDF_INFO_GROUP_MSG)
}

/// Encrypt a plaintext envelope for a group/channel conversation.
pub(crate) fn encrypt_group_message(
    group_key: &[u8; 32],
    epoch: u32,
    conversation_id: &str,
    plaintext: &str,
) -> Option<String> {
    let mk = group_message_key(group_key, epoch);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(&mk).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let aad = group_aad(conversation_id, epoch);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: &aad,
            },
        )
        .ok()?;
    let mut out = Vec::with_capacity(4 + 4 + NONCE_LEN + ct.len());
    out.extend_from_slice(GROUP_WIRE_MAGIC);
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Some(BASE64_STANDARD.encode(out))
}

/// Decrypt a TGK1 blob. Returns plaintext string.
pub(crate) fn decrypt_group_message(
    group_key: &[u8; 32],
    conversation_id: &str,
    blob_b64: &str,
) -> Option<String> {
    let blob = BASE64_STANDARD.decode(blob_b64).ok()?;
    if blob.len() < 4 + 4 + NONCE_LEN + 16 {
        return None;
    }
    if &blob[0..4] != GROUP_WIRE_MAGIC {
        return None;
    }
    let epoch = u32::from_le_bytes(blob[4..8].try_into().ok()?);
    let nonce_bytes: [u8; NONCE_LEN] = blob[8..8 + NONCE_LEN].try_into().ok()?;
    let ct = &blob[8 + NONCE_LEN..];
    let mk = group_message_key(group_key, epoch);
    let cipher = Aes256Gcm::new_from_slice(&mk).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let aad = group_aad(conversation_id, epoch);
    let plain = cipher.decrypt(nonce, Payload { msg: ct, aad: &aad }).ok()?;
    String::from_utf8(plain).ok()
}

/// In-memory + on-disk cache of unsealed group keys per conversation.
#[derive(Clone)]
pub(crate) struct GroupKeyStore {
    /// conversation_id -> (epoch, key)
    keys: HashMap<String, (u32, [u8; 32])>,
    path: PathBuf,
}

impl GroupKeyStore {
    pub(crate) fn load(base_dir: &Path, local_user_id: &str) -> Self {
        let path = base_dir.join(format!("group_keys_{local_user_id}.json"));
        let keys = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, StoredGroupKey>>(&s).ok())
            .map(|map| {
                map.into_iter()
                    .filter_map(|(cid, sk)| {
                        let raw = BASE64_STANDARD.decode(&sk.key_b64).ok()?;
                        let key: [u8; 32] = raw.try_into().ok()?;
                        Some((cid, (sk.epoch, key)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { keys, path }
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let map: HashMap<String, StoredGroupKey> = self
            .keys
            .iter()
            .map(|(cid, (epoch, key))| {
                (
                    cid.clone(),
                    StoredGroupKey {
                        epoch: *epoch,
                        key_b64: BASE64_STANDARD.encode(key),
                    },
                )
            })
            .collect();
        if let Ok(json) = serde_json::to_string(&map) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    pub(crate) fn get(&self, conversation_id: &str) -> Option<(u32, [u8; 32])> {
        self.keys.get(conversation_id).copied()
    }

    pub(crate) fn put(&mut self, conversation_id: &str, epoch: u32, key: [u8; 32]) {
        self.keys.insert(conversation_id.to_string(), (epoch, key));
        self.save();
    }

    pub(crate) fn clear_conversation(&mut self, conversation_id: &str) {
        self.keys.remove(conversation_id);
        self.save();
    }
}

#[derive(Serialize, Deserialize)]
struct StoredGroupKey {
    epoch: u32,
    key_b64: String,
}

// ---------------------------------------------------------------------------
// Legacy v1 helpers (unused by the UI).
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) fn encrypt(shared_secret: &[u8; 32], plaintext: &str) -> String {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let key = hkdf_32(shared_secret, Some(&salt), b"talkyss-dm-message-v1");
    let cipher = Aes256Gcm::new_from_slice(&key).expect("key is exactly 32 bytes");
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .expect("AES-256-GCM encryption of a bounded plaintext cannot fail");
    let mut blob = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    BASE64_STANDARD.encode(blob)
}

#[allow(dead_code)]
pub(crate) fn decrypt(shared_secret: &[u8; 32], blob_base64: &str) -> Option<String> {
    let blob = BASE64_STANDARD.decode(blob_base64).ok()?;
    if blob.len() < SALT_LEN + NONCE_LEN {
        return None;
    }
    let (salt, rest) = blob.split_at(SALT_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);
    let key = hkdf_32(shared_secret, Some(salt), b"talkyss-dm-message-v1");
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("talkyss-crypto-test-{nanos}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn group_key_seal_and_message_roundtrip() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let group_key = generate_group_key();
        let (eph, sealed) = seal_group_key_for(&bob.public_key_base64(), &group_key).unwrap();
        let opened = unseal_group_key(&bob, &eph, &sealed).unwrap();
        assert_eq!(opened, group_key);
        // Alice cannot unseal Bob's package.
        assert!(unseal_group_key(&alice, &eph, &sealed).is_none());

        let conv = "conv_group_1";
        let epoch = 1u32;
        let payload = MessagePayload::text_only("hello group").encode();
        let ct = encrypt_group_message(&group_key, epoch, conv, &payload).unwrap();
        assert!(looks_like_group_blob(&ct));
        let plain = decrypt_group_message(&group_key, conv, &ct).unwrap();
        assert_eq!(MessagePayload::decode(&plain).unwrap().text, "hello group");
    }

    #[test]
    fn dual_chain_roundtrip_both_directions() {
        let dir_a = tmp_dir();
        let dir_b = tmp_dir();
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let alice_id = "user_a";
        let bob_id = "user_b";
        let conv = "conv123";
        let aad = dm_aad(conv, alice_id, bob_id);

        let mut sa = RatchetSession::load_or_create(
            &dir_a,
            alice_id,
            bob_id,
            &alice,
            &bob.public_key_base64(),
        )
        .expect("alice session");
        let mut sb = RatchetSession::load_or_create(
            &dir_b,
            bob_id,
            alice_id,
            &bob,
            &alice.public_key_base64(),
        )
        .expect("bob session");

        // Alice -> Bob
        let ct1 = sa.encrypt(r#"{"text":"hello bob"}"#, &aad).unwrap();
        let p1 = sb.decrypt(&ct1, &aad).unwrap();
        assert_eq!(p1, r#"{"text":"hello bob"}"#);

        // Bob -> Alice
        let ct2 = sb.encrypt(r#"{"text":"hi alice"}"#, &aad).unwrap();
        let p2 = sa.decrypt(&ct2, &aad).unwrap();
        assert_eq!(p2, r#"{"text":"hi alice"}"#);

        // Both send again (simultaneous-friendly)
        let ct3 = sa.encrypt(r#"{"text":"second"}"#, &aad).unwrap();
        let ct4 = sb.encrypt(r#"{"text":"also second"}"#, &aad).unwrap();
        assert_eq!(sb.decrypt(&ct3, &aad).unwrap(), r#"{"text":"second"}"#);
        assert_eq!(sa.decrypt(&ct4, &aad).unwrap(), r#"{"text":"also second"}"#);

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn out_of_order_receive() {
        let dir_a = tmp_dir();
        let dir_b = tmp_dir();
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let aad = dm_aad("c", "a", "b");

        let mut sa =
            RatchetSession::load_or_create(&dir_a, "a", "b", &alice, &bob.public_key_base64())
                .unwrap();
        let mut sb =
            RatchetSession::load_or_create(&dir_b, "b", "a", &bob, &alice.public_key_base64())
                .unwrap();

        let c0 = sa.encrypt("m0", &aad).unwrap();
        let c1 = sa.encrypt("m1", &aad).unwrap();
        let c2 = sa.encrypt("m2", &aad).unwrap();

        // Receive 2 first, then 0, then 1
        assert_eq!(sb.decrypt(&c2, &aad).unwrap(), "m2");
        assert_eq!(sb.decrypt(&c0, &aad).unwrap(), "m0");
        assert_eq!(sb.decrypt(&c1, &aad).unwrap(), "m1");

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}
