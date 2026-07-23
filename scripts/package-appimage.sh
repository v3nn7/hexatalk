#!/usr/bin/env bash
# Build a portable HexaTalk AppImage (x86_64) that runs on most Linux distros.
#
# Requires (one-time):
#   - cargo build --release already done (or we build)
#   - appimagetool  (downloaded automatically if missing)
#   - linuxdeploy + linuxdeploy-plugin-gtk (downloaded if missing)
#
# Usage:
#   ./scripts/package-appimage.sh
#   ./scripts/package-appimage.sh 0.1.3
#   SKIP_BUILD=1 ./scripts/package-appimage.sh
#
# Output:
#   releases/HexaTalk-linux-x86_64.AppImage
#   releases/upload/HexaTalk-linux-x86_64.AppImage

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
STEM="HexaTalk-linux-${ARCH}.AppImage"
TOOLS="$ROOT/target/appimage-tools"
APPDIR="$ROOT/target/HexaTalk.AppDir"
OUT_DIR="$ROOT/releases"
UPLOAD="$OUT_DIR/upload"
VERSION="${1:-}"

mkdir -p "$TOOLS" "$OUT_DIR" "$UPLOAD"

if [[ -z "$VERSION" ]]; then
  VERSION="$(python3 - <<'PY'
import re, pathlib
text = pathlib.Path("Cargo.toml").read_text(encoding="utf-8")
m = re.search(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', text)
vm = re.search(r'(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"\s*$', m.group(1))
print(vm.group(1))
PY
)"
fi
echo "AppImage version: $VERSION"

# ---- build binary ----
if [[ -z "${SKIP_BUILD:-}" ]]; then
  if [[ -f "$ROOT/.env.local" ]]; then
    set -a
    # shellcheck disable=SC1091
    source <(grep -v '^\s*#' "$ROOT/.env.local" | grep -E '^[A-Za-z_][A-Za-z0-9_]*=' || true)
    set +a
  fi
  echo "cargo build --release..."
  cargo build --release
fi

BIN=""
for c in \
  "$ROOT/target/release/HexaTalk" \
  "$ROOT/target/release/hexatalk" \
  "$ROOT/target/x86_64-unknown-linux-gnu/release/HexaTalk" \
  "$ROOT/target/x86_64-unknown-linux-gnu/release/hexatalk"
do
  if [[ -f "$c" ]]; then BIN="$c"; break; fi
done
if [[ -z "$BIN" ]]; then
  echo "error: release binary not found — run cargo build --release" >&2
  exit 1
fi
chmod +x "$BIN"
echo "Binary: $BIN"

# ---- fetch tools ----
download() {
  local url="$1" dest="$2"
  if [[ -x "$dest" ]]; then return 0; fi
  echo "Downloading $(basename "$dest")..."
  curl -fsSL -o "$dest" "$url"
  chmod +x "$dest"
}

# Official continuous builds (static, work on most distros)
download \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${ARCH}.AppImage" \
  "$TOOLS/linuxdeploy"
download \
  "https://github.com/linuxdeploy/linuxdeploy-plugin-gtk/raw/master/linuxdeploy-plugin-gtk.sh" \
  "$TOOLS/linuxdeploy-plugin-gtk.sh"
download \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage" \
  "$TOOLS/appimagetool"

# linuxdeploy plugin must be named linuxdeploy-plugin-gtk and on PATH / next to binary
cp -f "$TOOLS/linuxdeploy-plugin-gtk.sh" "$TOOLS/linuxdeploy-plugin-gtk"
chmod +x "$TOOLS/linuxdeploy-plugin-gtk"

# ---- AppDir ----
rm -rf "$APPDIR"
mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
  "$APPDIR/usr/share/metainfo"

cp -f "$BIN" "$APPDIR/usr/bin/HexaTalk"
chmod +x "$APPDIR/usr/bin/HexaTalk"

# Icon
ICON_SRC="$ROOT/assets/textures/hexatalkicon.png"
if [[ -f "$ICON_SRC" ]]; then
  cp -f "$ICON_SRC" "$APPDIR/usr/share/icons/hicolor/256x256/apps/hexatalk.png"
  # AppImage top-level icon (required by some tools)
  cp -f "$ICON_SRC" "$APPDIR/hexatalk.png"
fi

# Desktop entry (Name= must match for linuxdeploy)
cat > "$APPDIR/usr/share/applications/hexatalk.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=HexaTalk
Comment=Private chat, servers, and voice
Exec=HexaTalk %u
Icon=hexatalk
Terminal=false
Categories=Network;InstantMessaging;
MimeType=x-scheme-handler/vyrapp;
StartupNotify=true
X-AppImage-Version=$VERSION
EOF
cp -f "$APPDIR/usr/share/applications/hexatalk.desktop" "$APPDIR/hexatalk.desktop"

# AppStream metainfo (optional but nice)
cat > "$APPDIR/usr/share/metainfo/pro.vyrapp.hexatalk.appdata.xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <id>pro.vyrapp.hexatalk</id>
  <name>HexaTalk</name>
  <summary>Private chat, servers, and voice</summary>
  <metadata_license>FSFAP</metadata_license>
  <project_license>LicenseRef-proprietary</project_license>
  <description>
    <p>HexaTalk — end-to-end encrypted messenger with servers and voice.</p>
  </description>
  <launchable type="desktop-id">hexatalk.desktop</launchable>
  <url type="homepage">https://vyrapp.pro/</url>
  <releases>
    <release version="$VERSION" date="$(date -u +%Y-%m-%d)"/>
  </releases>
</component>
EOF

# ---- bundle libs (GTK, etc.) ----
export LINUXDEPLOY_OUTPUT_VERSION="$VERSION"
export LDAI_OUTPUT="$OUT_DIR/$STEM"
export PATH="$TOOLS:$PATH"
export APPIMAGE_EXTRACT_AND_RUN=1

echo "Running linuxdeploy (bundle dependencies)..."
# --appimage-extract-and-run: tools themselves are AppImages; works in CI/docker
"$TOOLS/linuxdeploy" --appimage-extract-and-run \
  --appdir "$APPDIR" \
  --executable "$APPDIR/usr/bin/HexaTalk" \
  --desktop-file "$APPDIR/usr/share/applications/hexatalk.desktop" \
  --icon-file "$APPDIR/usr/share/icons/hicolor/256x256/apps/hexatalk.png" \
  --plugin gtk \
  || {
    echo "warning: linuxdeploy --plugin gtk failed; retrying without gtk plugin..."
    "$TOOLS/linuxdeploy" --appimage-extract-and-run \
      --appdir "$APPDIR" \
      --executable "$APPDIR/usr/bin/HexaTalk" \
      --desktop-file "$APPDIR/usr/share/applications/hexatalk.desktop" \
      --icon-file "$APPDIR/usr/share/icons/hicolor/256x256/apps/hexatalk.png"
  }

# AppRun: prefer generated by linuxdeploy; if missing, write a minimal one
if [[ ! -f "$APPDIR/AppRun" ]]; then
  cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
SELF=$(readlink -f "$0")
HERE=${SELF%/*}
export PATH="${HERE}/usr/bin:${PATH}"
export LD_LIBRARY_PATH="${HERE}/usr/lib:${LD_LIBRARY_PATH:-}"
exec "${HERE}/usr/bin/HexaTalk" "$@"
EOF
  chmod +x "$APPDIR/AppRun"
fi

echo "Building AppImage..."
rm -f "$OUT_DIR/$STEM"
ARCH="$ARCH" "$TOOLS/appimagetool" --appimage-extract-and-run \
  "$APPDIR" "$OUT_DIR/$STEM"

chmod +x "$OUT_DIR/$STEM"
cp -f "$OUT_DIR/$STEM" "$UPLOAD/$STEM"

# Also keep a versioned archive copy for delta generation
cp -f "$OUT_DIR/$STEM" "$OUT_DIR/HexaTalk-linux-${ARCH}.AppImage-${VERSION}"

SIZE=$(du -h "$OUT_DIR/$STEM" | awk '{print $1}')
echo ""
echo "========== APPIMAGE READY =========="
echo "  $OUT_DIR/$STEM  ($SIZE)"
echo "  staged: $UPLOAD/$STEM"
echo "Run:  chmod +x $OUT_DIR/$STEM && $OUT_DIR/$STEM"
echo ""
