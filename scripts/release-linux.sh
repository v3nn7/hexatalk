#!/usr/bin/env bash
# HexaTalk Linux release → portable AppImage + HTD1 deltas for auto-update.
#
# Produces:
#   releases/HexaTalk-linux-x86_64.AppImage-<ver>     (archive for next delta)
#   releases/upload/HexaTalk-linux-x86_64.AppImage    (CDN full binary)
#   releases/upload/HexaTalk-linux-x86_64.AppImage.sig
#   releases/upload/deltas/HexaTalk-linux-x86_64.AppImage-<from>-<to>.delta
#   releases/upload/version.txt
#
# Usage:
#   ./scripts/release-linux.sh
#   ./scripts/release-linux.sh 0.2.0
#   SKIP_SIGN=1 ALL_DELTAS=1 ./scripts/release-linux.sh
#
# Needs: cargo, python3, qbsdiff (optional), curl (AppImage tools),
#        libgtk-3-dev / fuse (for linuxdeploy / running AppImages).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RELEASES="$ROOT/releases"
DELTAS="$ROOT/deltas"
UPLOAD="$RELEASES/upload"
UPLOAD_DELTAS="$UPLOAD/deltas"
STEM="HexaTalk-linux-x86_64.AppImage"
ARCH="x86_64"

if [[ -z "${RELEASE_DELTA_KEY_HEX:-}" ]]; then
  export RELEASE_DELTA_KEY_HEX="c14e32b623c2318ee6bf67cc85d88ba546a220b269e1af52487eec20274a554f"
fi

if [[ -f "$ROOT/.env.local" ]]; then
  set -a
  # shellcheck disable=SC1091
  source <(grep -v '^\s*#' "$ROOT/.env.local" | grep -E '^[A-Za-z_][A-Za-z0-9_]*=' || true)
  set +a
  echo "Loaded .env.local (API_URL=${API_URL:-})"
fi

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
    body2, n = re.subn(
        r'(?m)^(version\s*=\s*")\d+\.\d+\.\d+(")\s*$',
        rf'\g<1>{ver}\2',
        body,
        count=1,
    )
    if n != 1:
        raise SystemExit("failed to rewrite version")
    return "[package]\n" + body2
new, n = re.subn(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', repl, text, count=1)
if n != 1:
    raise SystemExit("no [package] block")
path.write_text(new, encoding="utf-8")
print(f"Cargo.toml [package].version = {ver}")
PY
}

CURRENT="$(pkg_version)"
if [[ -n "${1:-}" ]]; then
  NEW="$1"
else
  IFS=. read -r MA MI PA <<<"$CURRENT"
  NEW="$MA.$MI.$((PA + 1))"
fi

echo "Bumping version: $CURRENT -> $NEW"
if [[ "$CURRENT" != "$NEW" ]]; then
  set_pkg_version "$NEW"
else
  echo "  same version — rebuilding $NEW"
fi

# ---- AppImage (build + bundle) ----
chmod +x "$ROOT/scripts/package-appimage.sh"
SKIP_BUILD="${SKIP_BUILD:-}" "$ROOT/scripts/package-appimage.sh" "$NEW"

APPIMAGE="$RELEASES/$STEM"
if [[ ! -f "$APPIMAGE" ]]; then
  echo "error: AppImage not produced at $APPIMAGE" >&2
  exit 1
fi

mkdir -p "$RELEASES" "$DELTAS" "$UPLOAD_DELTAS"
ARCHIVE="$RELEASES/${STEM}-${NEW}"
cp -f "$APPIMAGE" "$ARCHIVE"
chmod +x "$ARCHIVE"
echo "Archived: $ARCHIVE"

# Optional ed25519 over the AppImage bytes
SIG_PATH=""
SIGNED=0
if [[ -z "${SKIP_SIGN:-}" && -n "${RELEASE_SIGNING_KEY_HEX:-}" ]]; then
  if command -v python3 >/dev/null && [[ -f "$ROOT/scripts/sign_release.py" ]]; then
    SIG_PATH="${ARCHIVE}.sig"
    python3 "$ROOT/scripts/sign_release.py" "$ARCHIVE" "$SIG_PATH"
    if [[ -f "$SIG_PATH" ]]; then
      SIGNED=1
      cp -f "$SIG_PATH" "$UPLOAD/${STEM}.sig"
      echo "Signed: $SIG_PATH"
    fi
  fi
else
  echo "Unsigned release (set RELEASE_SIGNING_KEY_HEX to sign)"
fi

cp -f "$APPIMAGE" "$UPLOAD/$STEM"
printf '%s' "$NEW" > "$UPLOAD/version.txt"
printf '%s' "$NEW" > "$RELEASES/version.txt"
echo "version.txt -> $NEW"

# Deltas between AppImage archives
DELTA_COUNT=0
if command -v qbsdiff >/dev/null 2>&1; then
  mapfile -t PREV < <(
    find "$RELEASES" -maxdepth 1 -type f -name "${STEM}-*" \
      ! -name "*.sig" ! -name "${STEM}-${NEW}" | sort -V
  )
  if [[ ${#PREV[@]} -eq 0 ]]; then
    echo "No previous ${STEM}-* archives — first AppImage release, no delta."
  else
    if [[ -n "${ALL_DELTAS:-}" ]]; then
      TARGETS=("${PREV[@]}")
    else
      TARGETS=("${PREV[-1]}")
    fi
    for prev in "${TARGETS[@]}"; do
      base="$(basename "$prev")"
      prev_ver="${base#${STEM}-}"
      if [[ ! "$prev_ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        continue
      fi
      plain="$DELTAS/${STEM}-${prev_ver}-${NEW}.delta.plain"
      out="$DELTAS/${STEM}-${prev_ver}-${NEW}.delta"
      echo "Generating delta $prev_ver -> $NEW ..."
      if ! qbsdiff "$prev" "$ARCHIVE" "$plain"; then
        echo "  qbsdiff failed — skip"
        rm -f "$plain"
        continue
      fi
      if command -v python3 >/dev/null && [[ -f "$ROOT/scripts/encrypt_delta.py" ]]; then
        if [[ "$SIGNED" -eq 1 && -f "$SIG_PATH" ]]; then
          python3 "$ROOT/scripts/encrypt_delta.py" encrypt "$plain" "$out" "$SIG_PATH"
        else
          python3 "$ROOT/scripts/encrypt_delta.py" encrypt "$plain" "$out"
        fi
        rm -f "$plain"
      else
        mv -f "$plain" "$out"
      fi
      if [[ -f "$out" ]]; then
        cp -f "$out" "$UPLOAD_DELTAS/"
        sz=$(du -h "$out" | awk '{print $1}')
        echo "  $out ($sz)"
        DELTA_COUNT=$((DELTA_COUNT + 1))
      fi
    done
  fi
else
  echo "qbsdiff not on PATH — install: cargo install qbsdiff --features cmd"
fi

echo ""
echo "========== LINUX APPIMAGE RELEASE $NEW READY =========="
echo "Upload to astrakit.pro / R2:"
echo "  $UPLOAD/"
echo "    version.txt"
echo "    $STEM"
[[ "$SIGNED" -eq 1 ]] && echo "    ${STEM}.sig"
echo "    deltas/ ($DELTA_COUNT)"
ls -1 "$UPLOAD_DELTAS" 2>/dev/null | sed 's/^/      /' || true
echo ""
echo "Users run:"
echo "  chmod +x $STEM && ./$STEM"
echo "Works on most distros (bundled libs via linuxdeploy)."
