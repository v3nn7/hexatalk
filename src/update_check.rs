//! Self-update: detects a new build via `version.txt`, downloads it in the
//! background, verifies the ed25519 signature of the downloaded .exe
//! against an embedded release public key, and stages it to replace the
//! running .exe the next time the app actually quits -- no click, no
//! browser, no separate installer. Fully silent; the only visible trace is
//! a toast once the download finishes and the "About" status line.

use crate::net::rt::Task;
use crate::state::message::Message;
use base64::prelude::{BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

// astrakit.pro is set up as a public custom domain in front of the R2
// bucket, so plain anonymous GETs work here (unlike the raw
// *.r2.cloudflarestorage.com S3-API endpoint, which needs signed
// requests). On every release upload `version.txt` (just the version
// string, e.g. "1.1.0"), the latest `HexaTalk.exe` and its detached
// signature `HexaTalk.exe.sig` (see below) to the bucket root.
pub(crate) const CURRENT_APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hard caps so a malicious or broken update host can't exhaust memory or
/// hang the background task forever: the version string and signature are
/// tiny, and the exe itself is tens of MB, so anything past 256 MB is
/// refused outright.
const MAX_VERSION_TXT_BYTES: u64 = 1024;
const MAX_SIGNATURE_BYTES: u64 = 4096;
const MAX_EXE_BYTES: u64 = 256 * 1024 * 1024;

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
///   3. Upload `version.txt`, `HexaTalk.exe` and `HexaTalk.exe.sig`
///      together -- never bump version.txt before the .sig is in place.

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

    let bytes = match download_bounded(crate::obf::update_download_url(), MAX_EXE_BYTES, 300).await
    {
        Ok(bytes) => bytes,
        Err(err) => return UpdateOutcome::Failed(err),
    };

    // Hard gate: refuse to stage anything that doesn't carry a valid
    // signature from the release key baked into the binary.
    let sig_bytes = match download_bounded(crate::obf::update_signature_url(), MAX_SIGNATURE_BYTES, 30)
        .await
    {
        Ok(sig) => sig,
        Err(err) => return UpdateOutcome::Failed(format!("update refused: couldn't download signature: {err}")),
    };
    if let Err(reason) = verify_bytes(&bytes, &sig_bytes) {
        return UpdateOutcome::Failed(format!("update refused: {reason}"));
    }

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
