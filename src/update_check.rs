//! Self-update: detects a new build via `version.txt`, downloads it in the
//! background, verifies the ed25519 signature of the downloaded .exe
//! against an embedded release public key, and stages it to replace the
//! running .exe the next time the app actually quits -- no click, no
//! browser, no separate installer. Fully silent; the only visible trace is
//! a toast once the download finishes and the "About" status line.

use crate::net::rt::Task;
use crate::state::message::Message;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use qbsdiff::Bspatch;

/// Encrypted delta frame written by `scripts/encrypt_delta.py` /
/// `scripts/release.ps1`. Layout:
///   magic || nonce(12) || AES-256-GCM(ct||tag over raw qbsdiff)
///   [optional trailing 64-byte ed25519 sig of the *target* exe]
/// The trailing sig lets the CDN host only `version.txt` + deltas (no full
/// `HexaTalk.exe` / detached `.sig`). The AES key is baked into the binary
/// (see `UPDATE_DELTA_KEY_B64` in build.rs / `obf::update_delta_key_b64`).
const DELTA_MAGIC: &[u8; 4] = b"HTD1";
const DELTA_NONCE_LEN: usize = 12;
const DELTA_TAG_LEN: usize = 16;
const ED25519_SIG_LEN: usize = 64;

// astrakit.pro is set up as a public custom domain in front of the R2
// bucket, so plain anonymous GETs work here (unlike the raw
// *.r2.cloudflarestorage.com S3-API endpoint, which needs signed
// requests). Minimal release upload: `version.txt` +
// `deltas/HexaTalk-<from>-<to>.delta` (HTD1 with trailing sig). Full
// `HexaTalk.exe` + `HexaTalk.exe.sig` are optional fallbacks for clients
// that cannot delta (skipped versions, corrupt patch, etc.).
pub(crate) const CURRENT_APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hard caps so a malicious or broken update host can't exhaust memory or
/// hang the background task forever: the version string and signature are
/// tiny, and the exe itself is tens of MB, so anything past 256 MB is
/// refused outright.
const MAX_VERSION_TXT_BYTES: u64 = 1024;
const MAX_SIGNATURE_BYTES: u64 = 4096;
const MAX_EXE_BYTES: u64 = 256 * 1024 * 1024;
/// Deltas should normally be tens of KB to a few MB; this cap just stops a
/// broken/hostile host from streaming something exe-sized in as a "delta".
const MAX_DELTA_BYTES: u64 = 64 * 1024 * 1024;

/// The ed25519 public key that release binaries are signed with is baked
/// into the exe (obfuscated, see src/obf.rs — the key itself is public, the
/// obfuscation just keeps the binary's string table clean). A downloaded
/// update is staged ONLY if `HexaTalk.exe.sig` verifies against this key --
/// a compromised download host alone can no longer push a rogue binary to
/// every client. The version check decides WHEN to download; the signature
/// decides WHETHER the download is trusted.
///
/// Release signing procedure:
///   1. The matching 32-byte private seed exists only offline (printed
///      once when the keypair was generated) -- it is NOT in this repo.
///   2. Build the release exe, then produce the detached signature, e.g.:
///        python -c "from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey; open('HexaTalk.exe.sig','wb').write(Ed25519PrivateKey.from_private_bytes(bytes.fromhex('<PRIVATE_SEED_HEX>')).sign(open('HexaTalk.exe','rb').read()))"
///      (any ed25519 tool works as long as the .sig holds the raw
///      64-byte signature over the exact exe bytes).
///   3. Upload at least `version.txt` + signed deltas. Optionally also
///      `HexaTalk.exe` + `HexaTalk.exe.sig` for full-download fallback.
///      Never bump version.txt before the matching delta (or full exe) is live.

#[derive(Debug, Clone)]
pub(crate) enum UpdateOutcome {
    UpToDate,
    /// Downloaded and staged next to the running exe (`HexaTalk.exe.new`),
    /// ready for `stage_exe_swap` to install on the next real quit.
    Downloaded {
        path: std::path::PathBuf,
        version: String,
    },
    Failed(String),
}

pub(crate) fn check_for_update_task() -> Task<Message> {
    Task::perform(run_update_check(), Message::UpdateCheckFinished)
}

/// GETs `url` with an explicit timeout and a hard body-size cap. The
/// default `reqwest::get` has neither -- a hung or hostile host could stall
/// the background update task forever or stream unbounded data into memory.
async fn download_bounded(url: &str, max_bytes: u64, timeout_secs: u64) -> Result<Vec<u8>, String> {
    use futures::StreamExt;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|err| format!("couldn't build HTTP client: {err}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("download failed: {err}"))?;
    if !resp.status().is_success() {
        return Err(format!("server returned HTTP {}", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(format!("download too large ({len} bytes)"));
        }
    }
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| format!("download failed: {err}"))?;
        if buf.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err("download exceeded the size cap".to_string());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

async fn run_update_check() -> UpdateOutcome {
    let remote_version: Option<String> = download_bounded(crate::obf::update_version_url(), MAX_VERSION_TXT_BYTES, 15)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|v| !v.is_empty());

    // `version.txt` is the ONLY update trigger. Anything weaker (e.g. the
    // hosted exe merely differing in byte size from the running one) would
    // let a compromised host silently force a download onto every client.
    let version_says_newer = remote_version
        .as_deref()
        .is_some_and(|v| is_newer_version(v, CURRENT_APP_VERSION));
    if !version_says_newer {
        return UpdateOutcome::UpToDate;
    }

    let remote_version_str = remote_version.clone().unwrap_or_default();

    // Prefer the small incremental patch (see try_delta_patch). A missing /
    // bad delta falls through to a full-exe download when the host still
    // serves one; delta-only CDN deploys simply fail closed here.
    let (bytes, sig_bytes) = match try_delta_patch(&remote_version_str).await {
        Some(patched) => patched,
        None => {
            let bytes = match download_bounded(crate::obf::update_download_url(), MAX_EXE_BYTES, 300)
                .await
            {
                Ok(bytes) => bytes,
                Err(err) => {
                    return UpdateOutcome::Failed(format!(
                        "no usable delta for {CURRENT_APP_VERSION}->{remote_version_str}, \
                         and full download failed: {err}"
                    ));
                }
            };
            // Hard gate: refuse to stage anything that doesn't carry a
            // valid signature from the release key baked into the binary.
            let sig_bytes =
                match download_bounded(crate::obf::update_signature_url(), MAX_SIGNATURE_BYTES, 30)
                    .await
                {
                    Ok(sig) => sig,
                    Err(err) => {
                        return UpdateOutcome::Failed(format!(
                            "update refused: couldn't download signature: {err}"
                        ));
                    }
                };
            if let Err(reason) = verify_bytes(&bytes, &sig_bytes) {
                return UpdateOutcome::Failed(format!("update refused: {reason}"));
            }
            (bytes, sig_bytes)
        }
    };

    let Ok(exe_path) = std::env::current_exe() else {
        return UpdateOutcome::Failed("couldn't locate the running executable".to_string());
    };
    let staged_path = std::path::PathBuf::from(format!("{}.new", exe_path.display()));
    if let Err(err) = std::fs::write(&staged_path, &bytes) {
        return UpdateOutcome::Failed(format!("couldn't save downloaded update: {err}"));
    }
    // Keep the verified signature next to the staged exe so
    // `stage_exe_swap` can RE-verify at quit time -- closes the window in
    // which something local could have tampered with the staged file
    // between download and install.
    if let Err(err) = std::fs::write(format!("{}.sig", staged_path.display()), &sig_bytes) {
        let _ = std::fs::remove_file(&staged_path);
        return UpdateOutcome::Failed(format!("couldn't save update signature: {err}"));
    }

    UpdateOutcome::Downloaded {
        path: staged_path,
        version: remote_version.unwrap_or_else(|| "?".to_string()),
    }
}

/// Attempts an incremental update: downloads the small
/// `HexaTalk-<current>-<remote>.delta` patch (if the release host has
/// uploaded one for this exact version pair), decrypts it when framed as
/// HTD1 (AES-256-GCM), applies the inner qbsdiff patch to the exe currently
/// on disk, and returns the reconstructed exe bytes together with the
/// (already-verified) release signature -- ready to stage exactly like a
/// full download.
///
/// The release signature is preferably embedded as a 64-byte trailer on the
/// delta blob (delta-only CDN). If absent, the client still tries the
/// detached `HexaTalk.exe.sig` URL for older uploads.
///
/// Returns `None` on ANY failure (no delta uploaded for this pair, bad
/// decrypt, corrupt patch, or a patched result that doesn't verify against
/// the release signature) so the caller falls back to the full-exe download.
async fn try_delta_patch(remote_version: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let delta_url = format!(
        "{}HexaTalk-{}-{}.delta",
        crate::obf::update_delta_base_url(),
        CURRENT_APP_VERSION,
        remote_version
    );
    let delta_blob = download_bounded(&delta_url, MAX_DELTA_BYTES, 60).await.ok()?;
    // Prefer HTD1-encrypted frames (what release.ps1 ships). Legacy plain
    // qbsdiff blobs are still accepted so older staged deltas keep working
    // until the next release rotates them out.
    let DecryptedDelta {
        patch: delta,
        embedded_sig,
    } = decrypt_delta_blob(&delta_blob)?;

    let exe_path = std::env::current_exe().ok()?;
    let current_exe = std::fs::read(&exe_path).ok()?;

    let patcher = Bspatch::new(&delta).ok()?;
    let mut patched = Vec::with_capacity(patcher.hint_target_size() as usize);
    patcher.apply(&current_exe, std::io::Cursor::new(&mut patched)).ok()?;

    let sig_bytes = if let Some(sig) = embedded_sig {
        sig
    } else {
        download_bounded(crate::obf::update_signature_url(), MAX_SIGNATURE_BYTES, 30)
            .await
            .ok()?
    };
    verify_bytes(&patched, &sig_bytes).ok()?;

    Some((patched, sig_bytes))
}

struct DecryptedDelta {
    patch: Vec<u8>,
    /// ed25519 sig of the target exe when the release embedded it as a
    /// trailing 64 bytes after the HTD1 frame.
    embedded_sig: Option<Vec<u8>>,
}

/// Decrypt an HTD1-framed delta blob into raw qbsdiff bytes (+ optional
/// embedded target-exe signature). If the blob does not start with the HTD1
/// magic it is treated as a legacy plaintext qbsdiff patch (returned as-is).
/// Any AEAD / framing error yields `None` so the caller falls back.
fn decrypt_delta_blob(blob: &[u8]) -> Option<DecryptedDelta> {
    if !blob.starts_with(DELTA_MAGIC) {
        // Legacy plain qbsdiff patch (no embedded sig).
        return Some(DecryptedDelta {
            patch: blob.to_vec(),
            embedded_sig: None,
        });
    }

    let min_frame = DELTA_MAGIC.len() + DELTA_NONCE_LEN + DELTA_TAG_LEN;
    if blob.len() < min_frame {
        return None;
    }

    let key_raw = BASE64_STANDARD
        .decode(crate::obf::update_delta_key_b64())
        .ok()?;
    let key: [u8; 32] = key_raw.try_into().ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;

    let try_frame = |frame: &[u8]| -> Option<Vec<u8>> {
        if frame.len() < min_frame || !frame.starts_with(DELTA_MAGIC) {
            return None;
        }
        let nonce_start = DELTA_MAGIC.len();
        let body_start = nonce_start + DELTA_NONCE_LEN;
        let nonce = Nonce::from_slice(&frame[nonce_start..body_start]);
        let ciphertext = &frame[body_start..];
        cipher.decrypt(nonce, ciphertext).ok()
    };

    // New format: HTD1 frame || ed25519(64). AEAD fails if the trailer is
    // included in the ciphertext, so try stripping it first when present.
    if blob.len() >= min_frame + ED25519_SIG_LEN {
        let (frame, sig) = blob.split_at(blob.len() - ED25519_SIG_LEN);
        if let Some(patch) = try_frame(frame) {
            return Some(DecryptedDelta {
                patch,
                embedded_sig: Some(sig.to_vec()),
            });
        }
    }

    // Legacy HTD1 without trailing signature.
    let patch = try_frame(blob)?;
    Some(DecryptedDelta {
        patch,
        embedded_sig: None,
    })
}

/// Verifies `sig_bytes` (64 raw ed25519 signature bytes) over `bytes` with
/// the embedded release key. Every failure -- malformed key or signature,
/// mismatch -- is a hard refusal, never a silent pass.
fn verify_bytes(bytes: &[u8], sig_bytes: &[u8]) -> Result<(), String> {
    let signature = Signature::try_from(sig_bytes)
        .map_err(|_| format!("signature must be 64 raw bytes, got {}", sig_bytes.len()))?;
    let key_raw = BASE64_STANDARD
        .decode(crate::obf::update_public_key_b64())
        .map_err(|err| format!("embedded update key is invalid: {err}"))?;
    let key_array: [u8; 32] = key_raw
        .try_into()
        .map_err(|_| "embedded update key must be 32 bytes".to_string())?;
    let key = VerifyingKey::from_bytes(&key_array)
        .map_err(|err| format!("embedded update key is invalid: {err}"))?;
    key.verify(bytes, &signature)
        .map_err(|_| "signature doesn't match the downloaded exe".to_string())
}

/// Compares two `major.minor.patch`-style version strings numerically
/// (falls back to treating an unparsable remote version as "not newer",
/// so a malformed `version.txt` can't trigger a bogus update notice).
fn is_newer_version(remote: &str, local: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect()
    }
    let remote_parts = parts(remote);
    if remote_parts.is_empty() {
        return false;
    }
    remote_parts > parts(local)
}

/// Re-reads the staged exe + its detached signature and re-verifies them
/// right before the swap. The download was already verified when staged,
/// but the staged file sits unlocked on disk between then and quit time --
/// this closes the local-tamper window without trusting anything that
/// happened while the app wasn't looking.
fn staged_file_signature_valid(staged_path: &std::path::Path) -> bool {
    let sig_path = format!("{}.sig", staged_path.display());
    match (std::fs::read(staged_path), std::fs::read(&sig_path)) {
        (Ok(exe), Ok(sig)) => verify_bytes(&exe, &sig).is_ok(),
        _ => false,
    }
}

/// Spawns a detached helper that waits for this process to fully exit (so
/// Windows releases its lock on the running .exe), then moves the staged
/// download over it. Not launched immediately -- called right before the
/// app's own quit call sites so it never races the still-running process.
/// With `relaunch` the helper also starts the fresh exe afterwards (the
/// "Restart & install" flow); without it the swap just means the *next*
/// launch (shortcut, Start Menu, ...) picks up the new build.
#[cfg(windows)]
pub(crate) fn stage_exe_swap(staged_path: &std::path::Path, relaunch: bool) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(exe_path) = std::env::current_exe() else {
        return;
    };
    if !staged_file_signature_valid(staged_path) {
        // Staged update was tampered with (or its signature is gone):
        // drop both files and skip the swap rather than installing it.
        let _ = std::fs::remove_file(staged_path);
        let _ = std::fs::remove_file(format!("{}.sig", staged_path.display()));
        return;
    }
    // `current_exe()` may return an extended-length path (`\\?\C:\...`) which
    // breaks `move`/`start` inside cmd (Windows reports it cannot find "\\").
    // Strip the prefix so the helper script sees plain DOS paths.
    fn dos_path(p: &std::path::Path) -> String {
        let s = p.display().to_string();
        s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
    }
    let staged = dos_path(staged_path);
    let exe = dos_path(&exe_path);
    let script = if relaunch {
        format!("ping -n 2 127.0.0.1 >nul & move /Y \"{staged}\" \"{exe}\" & del \"{staged}.sig\" & start \"\" \"{exe}\"",)
    } else {
        format!("ping -n 2 127.0.0.1 >nul & move /Y \"{staged}\" \"{exe}\" & del \"{staged}.sig\"",)
    };
    let _ = std::process::Command::new("cmd")
        .args(["/C", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(windows))]
pub(crate) fn stage_exe_swap(_staged_path: &std::path::Path, _relaunch: bool) {}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    #[test]
    fn decrypt_delta_accepts_legacy_plain() {
        let plain = b"not-a-real-qbsdiff-but-legacy-shape";
        let out = decrypt_delta_blob(plain).expect("plain should pass through");
        assert_eq!(out.patch, plain);
        assert!(out.embedded_sig.is_none());
    }

    #[test]
    fn decrypt_delta_roundtrip_htd1() {
        let plain = b"fake-qbsdiff-payload-for-unit-test";
        let key_raw = BASE64_STANDARD
            .decode(crate::obf::update_delta_key_b64())
            .expect("baked key b64");
        let key: [u8; 32] = key_raw.try_into().expect("32 bytes");
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce_bytes = [7u8; DELTA_NONCE_LEN];
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher.encrypt(nonce, plain.as_ref()).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(DELTA_MAGIC);
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ct);

        let out = decrypt_delta_blob(&blob).expect("HTD1 decrypt");
        assert_eq!(out.patch, plain);
        assert!(out.embedded_sig.is_none());
    }

    #[test]
    fn decrypt_delta_roundtrip_htd1_with_trailing_sig() {
        let plain = b"fake-qbsdiff-payload-for-unit-test";
        let sig = [0xABu8; ED25519_SIG_LEN];
        let key_raw = BASE64_STANDARD
            .decode(crate::obf::update_delta_key_b64())
            .expect("baked key b64");
        let key: [u8; 32] = key_raw.try_into().expect("32 bytes");
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce_bytes = [9u8; DELTA_NONCE_LEN];
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher.encrypt(nonce, plain.as_ref()).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(DELTA_MAGIC);
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ct);
        blob.extend_from_slice(&sig);

        let out = decrypt_delta_blob(&blob).expect("HTD1+sig decrypt");
        assert_eq!(out.patch, plain);
        assert_eq!(out.embedded_sig.as_deref(), Some(sig.as_slice()));
    }

    #[test]
    fn decrypt_delta_rejects_bad_tag() {
        let mut blob = Vec::from(DELTA_MAGIC.as_slice());
        blob.extend_from_slice(&[0u8; DELTA_NONCE_LEN + DELTA_TAG_LEN + 8]);
        assert!(decrypt_delta_blob(&blob).is_none());
    }
}
