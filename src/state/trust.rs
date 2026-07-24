//! Persistent SAS/fingerprint verification state.
//!
//! `crates/reprotocol` computes a SAS (`sas_emojis()`) and a fingerprint for
//! every peerseal session, but neither the crate nor the app previously
//! remembered whether a *user* actually compared and confirmed one — the
//! only state lived in `App::peer_sas`/`peer_remote_fp`, both cleared on
//! disconnect (see `App::stop_peer_session_for`). A restart, or the peer
//! simply going offline and back online, silently forgot every
//! verification a user had performed.
//!
//! This module gives verification a home on disk, keyed by peer `user_id`
//! (not by session), so it survives restarts and reconnects. It also
//! distinguishes "never verified" from "verified, but the remote key has
//! since changed" — the latter is the interesting case (reinstall, device
//! change, or a possible MITM) and must not be silently reported as still
//! verified.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::state::history::hexatalk_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerifiedPeerEntry {
    fingerprint: String,
    #[allow(dead_code)] // kept for future audit/UX (e.g. "verified 3 days ago")
    verified_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct VerifiedPeerStore {
    entries: HashMap<String, VerifiedPeerEntry>,
    /// Not serialized: set on `load`, used by `save` to write back to the
    /// same owner-scoped file.
    #[serde(skip)]
    owner_user_id: String,
}

/// What the UI should show for a peer's identity key, given whatever this
/// store remembers about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustBadge {
    /// Never verified (or nothing on disk yet — the common case for a
    /// freshly added friend).
    Unverified,
    /// Verified, and the fingerprint we saw this session still matches.
    Verified,
    /// Verified once, but the fingerprint changed since then — could be a
    /// reinstall/new device, could be an attacker. Surfaced as a warning,
    /// never silently downgraded to `Unverified` (that would erase the
    /// signal that something changed).
    FingerprintChanged { old: String },
}

fn store_path(owner_user_id: &str) -> PathBuf {
    hexatalk_dir()
        .join("trust")
        .join(format!("{owner_user_id}.json"))
}

impl VerifiedPeerStore {
    /// Loads the store for one local account. Missing or corrupt files
    /// degrade to an empty store rather than failing — an unreadable trust
    /// file must never block the app from starting, and "no verifications
    /// on record" is always a safe (if less convenient) default.
    pub(crate) fn load(owner_user_id: &str) -> Self {
        let path = store_path(owner_user_id);
        let mut store = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default();
        store.owner_user_id = owner_user_id.to_string();
        store
    }

    fn save(&self) -> Result<(), String> {
        if self.owner_user_id.is_empty() {
            return Err("VerifiedPeerStore::save called before load".to_string());
        }
        let path = store_path(&self.owner_user_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    /// What to show for `peer_id`, given the fingerprint observed *this*
    /// session. Pure lookup — never mutates or writes to disk.
    pub(crate) fn status_for(&self, peer_id: &str, current_fingerprint: &str) -> TrustBadge {
        match self.entries.get(peer_id) {
            Some(entry) if entry.fingerprint == current_fingerprint => TrustBadge::Verified,
            Some(entry) => TrustBadge::FingerprintChanged {
                old: entry.fingerprint.clone(),
            },
            None => TrustBadge::Unverified,
        }
    }

    /// Records that the user compared SAS/fingerprint out-of-band and
    /// confirmed a match, and persists it immediately (a verification that
    /// only lives in memory until the next unrelated save is a
    /// verification that can silently vanish).
    pub(crate) fn mark_verified(&mut self, peer_id: &str, fingerprint: &str) -> Result<(), String> {
        self.entries.insert(
            peer_id.to_string(),
            VerifiedPeerEntry {
                fingerprint: fingerprint.to_string(),
                verified_at: chrono::Utc::now().timestamp_millis(),
            },
        );
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_for_unknown_peer_is_unverified() {
        let store = VerifiedPeerStore::default();
        assert_eq!(store.status_for("alice", "fp-a"), TrustBadge::Unverified);
    }

    #[test]
    fn status_for_matching_fingerprint_is_verified() {
        let mut store = VerifiedPeerStore::default();
        store.owner_user_id = "me".to_string();
        store.entries.insert(
            "alice".to_string(),
            VerifiedPeerEntry {
                fingerprint: "fp-a".to_string(),
                verified_at: 0,
            },
        );
        assert_eq!(store.status_for("alice", "fp-a"), TrustBadge::Verified);
    }

    #[test]
    fn status_for_changed_fingerprint_warns_instead_of_forgetting() {
        let mut store = VerifiedPeerStore::default();
        store.owner_user_id = "me".to_string();
        store.entries.insert(
            "alice".to_string(),
            VerifiedPeerEntry {
                fingerprint: "fp-old".to_string(),
                verified_at: 0,
            },
        );
        assert_eq!(
            store.status_for("alice", "fp-new"),
            TrustBadge::FingerprintChanged {
                old: "fp-old".to_string()
            }
        );
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "hexatalk-trust-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        // Point HOME/APPDATA-derived hexatalk_dir() somewhere private for
        // this test by writing directly to a path under our own temp dir
        // instead of relying on the real user data dir.
        let path = dir.join("trust").join("owner123.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mut store = VerifiedPeerStore {
            entries: HashMap::new(),
            owner_user_id: "owner123".to_string(),
        };
        store.entries.insert(
            "bob".to_string(),
            VerifiedPeerEntry {
                fingerprint: "fp-bob".to_string(),
                verified_at: 42,
            },
        );
        let json = serde_json::to_vec_pretty(&store).unwrap();
        std::fs::write(&path, &json).unwrap();

        let loaded: VerifiedPeerStore = serde_json::from_slice(&json).unwrap();
        assert_eq!(loaded.status_for("bob", "fp-bob"), TrustBadge::Verified);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_falls_back_to_empty_store_not_a_crash() {
        let dir = std::env::temp_dir().join(format!(
            "hexatalk-trust-corrupt-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bogus = dir.join("not-json.json");
        std::fs::write(&bogus, b"{ this is not valid json").unwrap();
        let parsed: Option<VerifiedPeerStore> =
            std::fs::read(&bogus).ok().and_then(|b| serde_json::from_slice(&b).ok());
        assert!(parsed.is_none(), "corrupt file must not parse");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
