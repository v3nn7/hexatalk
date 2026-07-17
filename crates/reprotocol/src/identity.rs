//! Long-term peer identity (X25519 static keys for Noise) and TOFU store.

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use snow::Builder;
use snow::params::NoiseParams;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Noise pattern used only to generate X25519 keypairs via `snow`.
const KP_PATTERN: &str = "Noise_NN_25519_ChaChaPoly_BLAKE2s";

/// Local long-term identity used in `Noise_XXpsk3` handshakes.
#[derive(Clone)]
pub struct Identity {
    /// 32-byte X25519 private key.
    pub private: [u8; 32],
    /// 32-byte X25519 public key.
    pub public: [u8; 32],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("public", &self.fingerprint())
            .field("private", &"<redacted>")
            .finish()
    }
}

impl Identity {
    /// Generate a fresh random identity.
    pub fn generate() -> Result<Self> {
        let params: NoiseParams = KP_PATTERN
            .parse()
            .map_err(|e| Error::Crypto(format!("params: {e}")))?;
        let kp = Builder::new(params)
            .generate_keypair()
            .map_err(|e| Error::Crypto(format!("generate_keypair: {e}")))?;
        let mut private = [0u8; 32];
        let mut public = [0u8; 32];
        if kp.private.len() != 32 || kp.public.len() != 32 {
            return Err(Error::Crypto("unexpected key size".into()));
        }
        private.copy_from_slice(&kp.private);
        public.copy_from_slice(&kp.public);
        Ok(Self { private, public })
    }

    /// Build from raw key material.
    pub fn from_parts(private: [u8; 32], public: [u8; 32]) -> Self {
        Self { private, public }
    }

    /// SHA-256 fingerprint of the public key (full 64 hex chars).
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.public)
    }

    /// Short fingerprint for UI (first 16 hex chars).
    pub fn short_fingerprint(&self) -> String {
        let f = self.fingerprint();
        f[..16.min(f.len())].to_string()
    }

    /// Persist as two hex lines: public then private (0600 recommended by caller on Unix).
    pub fn save_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let body = format!(
            "{}\n{}\n",
            hex_encode(&self.public),
            hex_encode(&self.private)
        );
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, body)?;
        Ok(())
    }

    /// Load identity written by [`Self::save_file`].
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let pub_hex = lines
            .next()
            .ok_or_else(|| Error::Identity("missing public key line".into()))?;
        let priv_hex = lines
            .next()
            .ok_or_else(|| Error::Identity("missing private key line".into()))?;
        let public = hex_decode_32(pub_hex)?;
        let private = hex_decode_32(priv_hex)?;
        Ok(Self { private, public })
    }

    /// Load from path or generate + save if missing.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            Self::load_file(path)
        } else {
            let id = Self::generate()?;
            id.save_file(path)?;
            Ok(id)
        }
    }
}

/// SHA-256 hex fingerprint of a 32-byte public key.
pub fn fingerprint_of(public: &[u8]) -> String {
    let dig = Sha256::digest(public);
    hex_encode(&dig)
}

/// Short (16 hex) fingerprint.
pub fn short_fingerprint_of(public: &[u8]) -> String {
    let f = fingerprint_of(public);
    f[..16.min(f.len())].to_string()
}

/// Trust-on-first-use store: maps remote fingerprint → first-seen unix seconds.
#[derive(Debug, Default, Clone)]
pub struct TofuStore {
    entries: HashMap<String, u64>,
    path: Option<PathBuf>,
}

/// Result of checking a remote identity against TOFU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TofuCheck {
    /// Never seen before — recorded as trusted.
    FirstSeen,
    /// Matches previously stored fingerprint.
    KnownMatch,
    /// Same peer id slot not used; we only key by fingerprint so this is unused.
    /// Provided for API clarity if you track by room.
    Mismatch,
}

impl TofuStore {
    /// Empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from a simple text file (`fingerprint timestamp` per line).
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut entries = HashMap::new();
        if path.exists() {
            let text = fs::read_to_string(&path)?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let Some(fp) = parts.next() else { continue };
                let ts = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                entries.insert(fp.to_string(), ts);
            }
        }
        Ok(Self {
            entries,
            path: Some(path),
        })
    }

    /// Persist if a path is configured.
    pub fn save(&self) -> Result<()> {
        let Some(ref path) = self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::from("# peerseal TOFU store\n");
        for (fp, ts) in &self.entries {
            body.push_str(&format!("{fp} {ts}\n"));
        }
        fs::write(path, body)?;
        Ok(())
    }

    /// Check remote public key; records first seen automatically.
    pub fn check_and_remember(&mut self, public: &[u8]) -> Result<TofuCheck> {
        let fp = fingerprint_of(public);
        if let Some(_) = self.entries.get(&fp) {
            return Ok(TofuCheck::KnownMatch);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.entries.insert(fp, now);
        self.save()?;
        Ok(TofuCheck::FirstSeen)
    }

    /// Whether this fingerprint was already trusted.
    pub fn is_known(&self, public: &[u8]) -> bool {
        self.entries.contains_key(&fingerprint_of(public))
    }

    /// Number of trusted peers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode_32(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return Err(Error::Identity(format!(
            "expected 64 hex chars, got {}",
            s.len()
        )));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| Error::Identity(format!("hex: {e}")))?;
        out[i] = byte;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generate_and_fingerprint() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.public, b.public);
        assert_eq!(a.fingerprint().len(), 64);
        assert_eq!(a.short_fingerprint().len(), 16);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("id.key");
        let id = Identity::generate().unwrap();
        id.save_file(&path).unwrap();
        let loaded = Identity::load_file(&path).unwrap();
        assert_eq!(id.public, loaded.public);
        assert_eq!(id.private, loaded.private);
    }

    #[test]
    fn tofu_first_then_known() {
        let mut store = TofuStore::new();
        let id = Identity::generate().unwrap();
        assert_eq!(
            store.check_and_remember(&id.public).unwrap(),
            TofuCheck::FirstSeen
        );
        assert_eq!(
            store.check_and_remember(&id.public).unwrap(),
            TofuCheck::KnownMatch
        );
    }
}
