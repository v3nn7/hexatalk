//! Self-update: detects a new build either via `version.txt` or by noticing
//! the hosted .exe's byte size differs from the running one (catches a
//! rebuild pushed to the download URL without remembering to bump
//! `version.txt`), downloads it in the background, and stages it to replace
//! the running .exe the next time the app actually quits -- no click, no
//! browser, no separate installer. Fully silent; the only visible trace is
//! a toast once the download finishes and the "About" status line.

use crate::rt::Task;
use crate::*;

// astrakit.pro is set up as a public custom domain in front of the R2
// bucket, so plain anonymous GETs work here (unlike the raw
// *.r2.cloudflarestorage.com S3-API endpoint, which needs signed
// requests). Upload `version.txt` (just the version string, e.g. "1.1.0")
// and the latest `Talkyss.exe` to the bucket root on every release.
pub(crate) const CURRENT_APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const UPDATE_VERSION_URL: &str = "https://astrakit.pro/version.txt";
pub(crate) const UPDATE_DOWNLOAD_URL: &str = "https://astrakit.pro/Talkyss.exe";

#[derive(Debug, Clone)]
pub(crate) enum UpdateOutcome {
    UpToDate,
    /// Downloaded and staged next to the running exe (`Talkyss.exe.new`),
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

async fn run_update_check() -> UpdateOutcome {
    let remote_version: Option<String> = match reqwest::get(UPDATE_VERSION_URL).await {
        Ok(resp) => resp
            .text()
            .await
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|v| !v.is_empty()),
        Err(_) => None,
    };

    let remote_size: Option<u64> = match reqwest::Client::new().head(UPDATE_DOWNLOAD_URL).send().await {
        Ok(resp) => resp.content_length(),
        Err(_) => None,
    };
    let local_size: Option<u64> = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());

    let version_says_newer = remote_version
        .as_deref()
        .is_some_and(|v| is_newer_version(v, CURRENT_APP_VERSION));
    // Different size in EITHER direction still means "the hosted build
    // doesn't match what's running" -- a real signal even if nobody
    // bumped version.txt for this push.
    let size_differs = matches!((remote_size, local_size), (Some(r), Some(l)) if r != l);

    if !version_says_newer && !size_differs {
        return UpdateOutcome::UpToDate;
    }

    let bytes = match reqwest::get(UPDATE_DOWNLOAD_URL).await {
        Ok(resp) => match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => return UpdateOutcome::Failed(err.to_string()),
        },
        Err(err) => return UpdateOutcome::Failed(err.to_string()),
    };

    let Ok(exe_path) = std::env::current_exe() else {
        return UpdateOutcome::Failed("couldn't locate the running executable".to_string());
    };
    let staged_path = std::path::PathBuf::from(format!("{}.new", exe_path.display()));
    if let Err(err) = std::fs::write(&staged_path, &bytes) {
        return UpdateOutcome::Failed(format!("couldn't save downloaded update: {err}"));
    }

    UpdateOutcome::Downloaded {
        path: staged_path,
        version: remote_version.unwrap_or_else(|| "?".to_string()),
    }
}

/// Compares two `major.minor.patch`-style version strings numerically
/// (falls back to treating an unparsable remote version as "not newer",
/// so a malformed `version.txt` can't trigger a bogus update notice).
pub(crate) fn is_newer_version(remote: &str, local: &str) -> bool {
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

/// Spawns a detached helper that waits for this process to fully exit (so
/// Windows releases its lock on the running .exe), then moves the staged
/// download over it. Not launched immediately -- called right before the
/// app's own `iced::exit()` call sites so it never races the still-running
/// process. Does not relaunch the app; the swap just means the *next*
/// launch (shortcut, Start Menu, ...) picks up the new build.
#[cfg(windows)]
pub(crate) fn stage_exe_swap(staged_path: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(exe_path) = std::env::current_exe() else {
        return;
    };
    let script = format!(
        "ping -n 2 127.0.0.1 >nul & move /Y \"{}\" \"{}\"",
        staged_path.display(),
        exe_path.display(),
    );
    let _ = std::process::Command::new("cmd")
        .args(["/C", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(windows))]
pub(crate) fn stage_exe_swap(_staged_path: &std::path::Path) {}
