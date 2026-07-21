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
                    // Most common production miss: version.txt points at a new
                    // build, but neither the from→to delta nor HexaTalk.exe is
                    // on the CDN (or the client is several versions behind and
                    // only the latest hop was uploaded).
                    let hint = if err.contains("404") {
                        format!(
                            "Update v{remote_version_str} is advertised, but no package for \
                             your build (v{CURRENT_APP_VERSION}) is on the server. \
                             Need deltas/HexaTalk-{CURRENT_APP_VERSION}-{remote_version_str}.delta \
                             or HexaTalk.exe (404)."
                        )
                    } else {
                        format!(
                            "No usable delta for {CURRENT_APP_VERSION}→{remote_version_str}, \
                             and full download failed: {err}"
                        )
                    };
                    return UpdateOutcome::Failed(hint);
                }
            };
            // Optional ed25519: verify when .sig matches the baked public key.
            // Missing or non-verifying sig → still stage (unsigned path). A
            // signed CDN must not brick clients that ship without / with a
            // different public key.
            let sig_bytes = match download_bounded(
                crate::obf::update_signature_url(),
                MAX_SIGNATURE_BYTES,
                30,
            )
            .await
            {
                Ok(sig)
                    if sig.len() == ED25519_SIG_LEN && verify_bytes(&bytes, &sig).is_ok() =>
                {
                    sig
                }
                _ => Vec::new(),
            };
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
    // Sidecar next to staged exe: 64-byte ed25519 when signed, empty file when
    // unsigned. `stage_exe_swap` re-checks this at quit time.
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
/// Optional ed25519 may be embedded as a 64-byte trailer on the delta, or
/// fetched from `HexaTalk.exe.sig`. If neither is present the patch is still
/// accepted after successful HTD1 decrypt + bspatch (unsigned releases).
///
/// Returns `None` on missing/bad delta so the caller can try full download.
/// Returns `(patched_exe, sig_bytes)` where `sig_bytes` is empty when unsigned.
async fn try_delta_patch(remote_version: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    // Windows: HexaTalk-<from>-<to>.delta (historical)
    // Linux:   HexaTalk-linux-x86_64.AppImage-<from>-<to>.delta
    // (stem already includes the .AppImage suffix on Linux)
    let stem = update_artifact_stem();
    let delta_url = if stem == "HexaTalk" {
        format!(
            "{}HexaTalk-{}-{}.delta",
            crate::obf::update_delta_base_url(),
            CURRENT_APP_VERSION,
            remote_version
        )
    } else {
        format!(
            "{}{}-{}-{}.delta",
            crate::obf::update_delta_base_url(),
            stem,
            CURRENT_APP_VERSION,
            remote_version
        )
    };
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

    // ed25519 is best-effort:
    // - verifies when the client has the matching public key
    // - if the release is signed but this client can't verify (no/wrong key),
    //   still accept the patch: HTD1 AES-GCM already authenticated the bytes
    //   with the baked delta key
    let sig_bytes = match resolve_optional_sig(&patched, embedded_sig).await {
        Some(sig) => sig,
        None => Vec::new(),
    };

    Some((patched, sig_bytes))
}

/// Returns a verified 64-byte ed25519 sig when available, else `None`
/// (caller installs as unsigned). Never fails the update solely because
/// a signature is present but not verifiable by this build.
async fn resolve_optional_sig(exe: &[u8], embedded: Option<Vec<u8>>) -> Option<Vec<u8>> {
    if let Some(sig) = embedded {
        if sig.len() == ED25519_SIG_LEN && verify_bytes(exe, &sig).is_ok() {
            return Some(sig);
        }
    }
    match download_bounded(crate::obf::update_signature_url(), MAX_SIGNATURE_BYTES, 30).await {
        Ok(remote) if remote.len() == ED25519_SIG_LEN && verify_bytes(exe, &remote).is_ok() => {
            Some(remote)
        }
        _ => None,
    }
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

/// Re-reads the staged exe + its sidecar before swap.
/// - 64-byte sidecar → must verify ed25519 (signed release)
/// - empty sidecar → unsigned release (HTD1-only), allow install
/// - missing/corrupt → refuse
fn staged_file_signature_valid(staged_path: &std::path::Path) -> bool {
    let sig_path = format!("{}.sig", staged_path.display());
    match (std::fs::read(staged_path), std::fs::read(&sig_path)) {
        (Ok(exe), Ok(sig)) if sig.is_empty() => !exe.is_empty(),
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

/// Linux / Unix: shell helper waits for this PID to exit, then replaces the
/// binary (or AppImage) in place. When running inside an AppImage the
/// `$APPIMAGE` env var points at the real file on disk — `current_exe()` is
/// only the FUSE mount and must not be overwritten.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn stage_exe_swap(staged_path: &std::path::Path, relaunch: bool) {
    let exe_path = std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok());
    let Some(exe_path) = exe_path else {
        return;
    };
    if !staged_file_signature_valid(staged_path) {
        let _ = std::fs::remove_file(staged_path);
        let _ = std::fs::remove_file(format!("{}.sig", staged_path.display()));
        return;
    }
    let staged = staged_path.display().to_string();
    let exe = exe_path.display().to_string();
    let pid = std::process::id();
    // Pass paths via env so we never have to shell-escape them.
    let mut cmd = std::process::Command::new("sh");
    cmd.env("HT_STAGED", &staged)
        .env("HT_EXE", &exe)
        .env("HT_PID", pid.to_string())
        .env("HT_RELAUNCH", if relaunch { "1" } else { "0" })
        .arg("-c")
        .arg(
            r#"
while kill -0 "$HT_PID" 2>/dev/null; do sleep 0.2; done
sleep 0.3
mv -f "$HT_STAGED" "$HT_EXE"
chmod +x "$HT_EXE"
rm -f "${HT_STAGED}.sig"
if [ "$HT_RELAUNCH" = "1" ]; then
  nohup "$HT_EXE" >/dev/null 2>&1 &
fi
"#,
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = cmd.spawn();
}

#[cfg(target_os = "macos")]
pub(crate) fn stage_exe_swap(_staged_path: &std::path::Path, _relaunch: bool) {
    // macOS packaging (app bundle) is a separate release path; self-replace
    // of a bare binary is not supported yet.
}

/// Filename stem used in CDN paths (without extension).
/// Windows keeps the historical `HexaTalk` / `HexaTalk.exe` names so existing
/// deltas keep working; Linux uses an explicit platform tag.
pub(crate) fn update_artifact_stem() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "HexaTalk"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        // AppImage is the supported portable Linux artefact.
        "HexaTalk-linux-x86_64.AppImage"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "HexaTalk-linux-aarch64.AppImage"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        "HexaTalk"
    }
}

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
