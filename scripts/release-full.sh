#!/usr/bin/env bash
# HexaTalk full release: builds + signs Linux (AppImage) and Windows (exe,
# cross-compiled via cargo-xwin) update artifacts, generates encrypted
# deltas from the previous release, and (optionally) uploads the update
# feed to the vyrapp.pro server over rsync/SSH.
#
# This uploads ONLY the auto-update feed (version.txt, the raw HexaTalk.exe /
# AppImage + .sig, deltas/) -- never the Windows *installer*
# (installer/Output/HexaTalkSetup.exe), which is a separate, first-install-only
# artifact the running app's self-updater never fetches.
#
# Usage:
#   ./scripts/release-full.sh                 # bump patch version, dry-run upload
#   ./scripts/release-full.sh 0.2.0            # explicit version, dry-run upload
#   PUSH=1 ./scripts/release-full.sh 0.2.0     # actually rsync to the server
#   SKIP_WINDOWS=1 ./scripts/release-full.sh   # Linux-only (e.g. no wine/xwin set up)
#   SKIP_LINUX=1 ./scripts/release-full.sh     # Windows-only
#
# Needs (one-time setup, see the session that first wrote this script):
#   rustup target add x86_64-pc-windows-msvc
#   cargo install cargo-xwin
#   wine + Inno Setup 6 installed under it (ISCC.exe) -- only needed if you
#     also want installer/Output/HexaTalkSetup.exe rebuilt; not required for
#     just the update feed.
#   RELEASE_SIGNING_KEY_HEX   -- ed25519 private seed (32 bytes hex), secret.
#   RELEASE_DELTA_KEY_HEX     -- AES-256 key (32 bytes hex) for delta frames,
#                                must match UPDATE_DELTA_KEY_B64 in .env.local.
#   NO_STRIP=1, ICON_SRC_OVERRIDE=<path to a square icon> if this machine's
#     linuxdeploy/icon needs the same workarounds as when this was written.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RELEASES="$ROOT/releases"
DELTAS="$ROOT/deltas"
UPLOAD="$RELEASES/upload"
UPLOAD_DELTAS="$UPLOAD/deltas"
LINUX_STEM="HexaTalk-linux-x86_64.AppImage"
WINDOWS_STEM="HexaTalk"

mkdir -p "$RELEASES" "$DELTAS" "$UPLOAD" "$UPLOAD_DELTAS"

if [[ -f "$ROOT/.env.local" ]]; then
  set -a
  # shellcheck disable=SC1091
  source <(grep -v '^\s*#' "$ROOT/.env.local" | grep -E '^[A-Za-z_][A-Za-z0-9_]*=' || true)
  set +a
fi

# Deploy target lives ONLY in .env.local (gitignored, sourced above) or the
# environment -- this repo is open source, so the server's user@host and
# path never get hardcoded here as a default. Missing either one just skips
# the upload step (build+sign still happens) rather than silently deploying
# nowhere or leaking a target in a public commit.
SSH_TARGET="${DEPLOY_SSH_TARGET:-}"
REMOTE_WEBROOT="${DEPLOY_REMOTE_PATH:-}"

# ---------- version ----------

pkg_version() {
  python3 - <<'PY'
import re, pathlib
text = pathlib.Path("Cargo.toml").read_text(encoding="utf-8")
m = re.search(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', text)
vm = re.search(r'(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"\s*$', m.group(1))
print(f"{vm.group(1)}.{vm.group(2)}.{vm.group(3)}")
PY
}

set_pkg_version() {
  local ver="$1"
  python3 - "$ver" <<'PY'
import re, sys, pathlib
ver = sys.argv[1]
path = pathlib.Path("Cargo.toml")
text = path.read_text(encoding="utf-8")
def repl(m):
    body = m.group(1)
    body2, n = re.subn(r'(?m)^(version\s*=\s*")\d+\.\d+\.\d+(")\s*$', rf'\g<1>{ver}\2', body, count=1)
    if n != 1:
        raise SystemExit("failed to rewrite version")
    return "[package]\n" + body2
new, n = re.subn(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', repl, text, count=1)
if n != 1:
    raise SystemExit("no [package] block")
path.write_text(new, encoding="utf-8")
PY
}

CURRENT="$(pkg_version)"
if [[ -n "${1:-}" ]]; then
  NEW="$1"
else
  IFS=. read -r MA MI PA <<<"$CURRENT"
  NEW="$MA.$MI.$((PA + 1))"
fi
echo "Version: $CURRENT -> $NEW"
if [[ "$CURRENT" != "$NEW" ]]; then
  set_pkg_version "$NEW"
fi

sign_one() {
  local artifact="$1" sig_out="$2"
  if [[ -n "${RELEASE_SIGNING_KEY_HEX:-}" ]]; then
    python3 "$ROOT/scripts/sign_release.py" "$artifact" "$sig_out"
    echo "  signed -> $sig_out"
  else
    echo "  WARNING: RELEASE_SIGNING_KEY_HEX not set -- $artifact left unsigned"
  fi
}

make_delta() {
  # make_delta <stem> <prev_archive> <new_archive> <out_delta>
  local stem="$1" prev="$2" new_archive="$3" out="$4"
  if ! command -v qbsdiff >/dev/null 2>&1; then
    echo "  qbsdiff not on PATH -- skipping delta (cargo install qbsdiff --features cmd)"
    return
  fi
  local plain="${out}.plain"
  if ! qbsdiff "$prev" "$new_archive" "$plain"; then
    echo "  qbsdiff failed for $stem -- skipping delta"
    rm -f "$plain"
    return
  fi
  if [[ -n "${RELEASE_DELTA_KEY_HEX:-}" ]]; then
    local sig_for_delta=""
    [[ -f "${new_archive}.sig" ]] && sig_for_delta="${new_archive}.sig"
    if [[ -n "$sig_for_delta" ]]; then
      python3 "$ROOT/scripts/encrypt_delta.py" encrypt "$plain" "$out" "$sig_for_delta"
    else
      python3 "$ROOT/scripts/encrypt_delta.py" encrypt "$plain" "$out"
    fi
    rm -f "$plain"
  else
    echo "  WARNING: RELEASE_DELTA_KEY_HEX not set -- shipping unencrypted delta (client expects HTD1 framing and will reject this)"
    mv -f "$plain" "$out"
  fi
  cp -f "$out" "$UPLOAD_DELTAS/"
  echo "  delta -> $out"
}

# ---------- Linux (AppImage) ----------

if [[ -z "${SKIP_LINUX:-}" ]]; then
  echo ""
  echo "== Linux: build + package =="
  chmod +x "$ROOT/scripts/package-appimage.sh"
  "$ROOT/scripts/package-appimage.sh" "$NEW"

  APPIMAGE="$RELEASES/$LINUX_STEM"
  ARCHIVE="$RELEASES/${LINUX_STEM}-${NEW}"
  cp -f "$APPIMAGE" "$ARCHIVE"
  chmod +x "$ARCHIVE"

  sign_one "$ARCHIVE" "${ARCHIVE}.sig"
  [[ -f "${ARCHIVE}.sig" ]] && cp -f "${ARCHIVE}.sig" "$UPLOAD/${LINUX_STEM}.sig"
  cp -f "$APPIMAGE" "$UPLOAD/$LINUX_STEM"

  mapfile -t PREV_LINUX < <(
    find "$RELEASES" -maxdepth 1 -type f -name "${LINUX_STEM}-*" \
      ! -name "*.sig" ! -name "${LINUX_STEM}-${NEW}" | sort -V
  )
  if [[ ${#PREV_LINUX[@]} -gt 0 ]]; then
    prev="${PREV_LINUX[-1]}"
    prev_ver="${prev##*-${LINUX_STEM}-}"
    make_delta "$LINUX_STEM" "$prev" "$ARCHIVE" \
      "$DELTAS/${LINUX_STEM}-${prev_ver}-${NEW}.delta"
  else
    echo "  no previous Linux archive -- first release, no delta"
  fi
fi

# ---------- Windows (exe, cross-compiled) ----------

if [[ -z "${SKIP_WINDOWS:-}" ]]; then
  echo ""
  echo "== Windows: cross-compile (cargo-xwin) =="
  cargo xwin build --release --target x86_64-pc-windows-msvc

  WIN_BIN="$ROOT/target/x86_64-pc-windows-msvc/release/HexaTalk.exe"
  if [[ ! -f "$WIN_BIN" ]]; then
    echo "error: $WIN_BIN not produced" >&2
    exit 1
  fi
  ARCHIVE_WIN="$RELEASES/${WINDOWS_STEM}-${NEW}.exe"
  cp -f "$WIN_BIN" "$ARCHIVE_WIN"

  sign_one "$ARCHIVE_WIN" "${ARCHIVE_WIN}.sig"
  [[ -f "${ARCHIVE_WIN}.sig" ]] && cp -f "${ARCHIVE_WIN}.sig" "$UPLOAD/HexaTalk.exe.sig"
  cp -f "$WIN_BIN" "$UPLOAD/HexaTalk.exe"

  # Also refresh the installer's source binary + repack it locally (not
  # uploaded to the update feed -- see the header comment -- but kept in
  # sync for manual first-install distribution).
  if command -v wine >/dev/null 2>&1 && [[ -f "$ROOT/installer/hexatalk.iss" ]]; then
    cp -f "$WIN_BIN" "$ROOT/target/release/HexaTalk.exe"
    ISCC="C:\\Program Files (x86)\\Inno Setup 6\\ISCC.exe"
    (cd "$ROOT/installer" && WINEDEBUG=-all wine "$ISCC" "/DAppVersion=$NEW" hexatalk.iss) \
      && echo "  installer/Output/HexaTalkSetup.exe refreshed (not part of the update feed)"
  fi

  mapfile -t PREV_WIN < <(
    find "$RELEASES" -maxdepth 1 -type f -name "${WINDOWS_STEM}-*.exe" \
      ! -name "*.sig" ! -name "${WINDOWS_STEM}-${NEW}.exe" | sort -V
  )
  if [[ ${#PREV_WIN[@]} -gt 0 ]]; then
    prev="${PREV_WIN[-1]}"
    prev_ver="$(basename "$prev" .exe)"
    prev_ver="${prev_ver#${WINDOWS_STEM}-}"
    make_delta "$WINDOWS_STEM" "$prev" "$ARCHIVE_WIN" \
      "$DELTAS/${WINDOWS_STEM}-${prev_ver}-${NEW}.delta"
  else
    echo "  no previous Windows archive -- first release, no delta"
  fi
fi

# ---------- version.txt ----------

printf '%s' "$NEW" > "$UPLOAD/version.txt"
printf '%s' "$NEW" > "$RELEASES/version.txt"

echo ""
echo "========== RELEASE $NEW STAGED =========="
echo "$UPLOAD/"
ls -1 "$UPLOAD"

# ---------- upload ----------

echo ""
if [[ -z "$SSH_TARGET" || -z "$REMOTE_WEBROOT" ]]; then
  echo "== Upload skipped: DEPLOY_SSH_TARGET / DEPLOY_REMOTE_PATH not set =="
  echo "   Set them in .env.local (gitignored, never commit real infra details"
  echo "   in this open-source repo) or export them for this shell, e.g.:"
  echo "     DEPLOY_SSH_TARGET=user@host DEPLOY_REMOTE_PATH=/var/www/example.com"
  echo "   Artifacts are staged and ready in $UPLOAD/ for a manual upload."
elif [[ -n "${PUSH:-}" ]]; then
  echo "== Uploading to $SSH_TARGET:$REMOTE_WEBROOT (LIVE — no --delete, existing site files untouched) =="
  rsync -avz --no-owner --no-group \
    --rsync-path="sudo rsync" \
    "$UPLOAD/" "$SSH_TARGET:$REMOTE_WEBROOT/"
  echo "Uploaded."
else
  echo "== Dry run (set PUSH=1 to actually upload) =="
  rsync -avzn --no-owner --no-group \
    --rsync-path="sudo rsync" \
    "$UPLOAD/" "$SSH_TARGET:$REMOTE_WEBROOT/"
  echo "(nothing was uploaded — rerun with PUSH=1 ./scripts/release-full.sh $NEW)"
fi

# ---------- download-page copies ----------
#
# Separate from the update feed above: vyrapp.pro's own download section
# (index.html's #download, discovered 2026-07-24) links directly to
# assets/app/windows/HexaTalkSetup.exe (the Inno Setup *installer*, for a
# first-time install) and assets/app/linux/<AppImage> (the AppImage doubles
# as its own "installer" on Linux). These are separate copies with
# different filenames/roles than the raw update-feed artifacts above, so
# they don't get updated by that rsync and were found stale (this session)
# after being updated by hand once already. Set SKIP_DOWNLOAD_PAGE=1 if a
# future deploy target doesn't have this page structure.
if [[ -z "${SKIP_DOWNLOAD_PAGE:-}" && -n "$SSH_TARGET" && -n "$REMOTE_WEBROOT" ]]; then
  DL_ARGS=(-avz --no-owner --no-group --rsync-path="sudo rsync")
  [[ -z "${PUSH:-}" ]] && DL_ARGS+=(-n)
  echo ""
  echo "== Download-page copies $( [[ -z "${PUSH:-}" ]] && echo "(dry run)" || echo "(LIVE)" ) =="
  if [[ -f "$ROOT/installer/Output/HexaTalkSetup.exe" ]]; then
    rsync "${DL_ARGS[@]}" "$ROOT/installer/Output/HexaTalkSetup.exe" \
      "$SSH_TARGET:$REMOTE_WEBROOT/assets/app/windows/HexaTalkSetup.exe"
  fi
  if [[ -f "$UPLOAD/$LINUX_STEM" ]]; then
    rsync "${DL_ARGS[@]}" "$UPLOAD/$LINUX_STEM" \
      "$SSH_TARGET:$REMOTE_WEBROOT/assets/app/linux/$LINUX_STEM"
  fi
fi
