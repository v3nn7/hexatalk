# Release + encrypted delta (incremental update)

## Building BOTH platforms from one Linux machine (no Windows needed)

`scripts/release-full.sh` supersedes running `release.ps1` (Windows) and
`release-linux.sh` (Linux) separately -- it builds, signs, deltas, *and
uploads* both platforms in one command, from a single Linux box, including
producing the Windows installer.

One-time machine setup (done once per dev machine, not per release):

```bash
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin        # auto-downloads the Windows SDK/CRT the
                                 # first time you build for the msvc target
                                 # (needs clang/lld -- already on most distros)

# For the Inno Setup installer (installer/Output/HexaTalkSetup.exe) --
# only needed if you also want the first-install installer refreshed, not
# for the auto-update feed itself:
sudo pacman -S wine              # or your distro's equivalent
wine <inno-setup-6-installer.exe> /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
# ISCC.exe ends up at:
#   ~/.wine/drive_c/Program Files (x86)/Inno Setup 6/ISCC.exe

# For python signing/encryption scripts:
sudo pacman -S python-cryptography   # (or: pip install cryptography)
cargo install qbsdiff --features cmd
```

Optional, faster iterative rebuilds on this machine (NOT committed --
machine-local `~/.cargo/config.toml`, outside any repo, so it never affects
other contributors or CI):

```toml
[build]
rustc-wrapper = "sccache"        # compile cache; modest hit-rate on this
                                  # dependency graph but harmless
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]   # much faster linking
```
Do **not** also set `[profile.release] incremental = true` in `Cargo.toml`
-- it actively fights `sccache`'s caching and made builds slower, not
faster, when tried.

Every release, once set up:

```bash
export RELEASE_SIGNING_KEY_HEX=<64 hex chars — password manager, never a file>
export RELEASE_DELTA_KEY_HEX=<64 hex chars — password manager, never a file>

./scripts/release-full.sh 0.2.0            # dry-run: builds + signs + stages, no upload
PUSH=1 ./scripts/release-full.sh 0.2.0     # also uploads (rsync, no --delete)

SKIP_WINDOWS=1 ./scripts/release-full.sh   # Linux only
SKIP_LINUX=1   ./scripts/release-full.sh   # Windows only
```

The upload target (`DEPLOY_SSH_TARGET`, `DEPLOY_REMOTE_PATH`) and the
build-time-baked update URLs (`UPDATE_VERSION_URL`,
`UPDATE_DOWNLOAD_URL_WINDOWS`/`_LINUX`, `UPDATE_SIGNATURE_URL_WINDOWS`/`_LINUX`,
`UPDATE_DELTA_BASE_URL`, `UPDATE_PUBLIC_KEY_B64`, `UPDATE_DELTA_KEY_B64`)
all live in `.env.local` (gitignored) -- **not** hardcoded anywhere in the
repo, since this repo is open source. Copy the shape of your own
`.env.local` from `build.rs`'s `emit_obfuscated_secrets()` if it's missing;
every value there has a safe astrakit.pro-pointing default if you don't
override it, so a build without any `.env.local` still works, it just
points at the original project's update feed instead of your own.

The release script *also* keeps the marketing site's own "Pobierz"/download
buttons in sync (`assets/app/windows/HexaTalkSetup.exe`,
`assets/app/linux/HexaTalk-linux-x86_64.AppImage` under the deploy
webroot) -- a **separate** pair of files from the raw update-feed
artifacts below, since the update feed and the "download it for the first
time" page are different consumers of different files. Set
`SKIP_DOWNLOAD_PAGE=1` if your deploy target doesn't have that page
structure.

---

`scripts\release.ps1` does the full pipeline:

1. Bump version in `Cargo.toml`
2. `cargo build --release` (bakes `API_URL` from `.env.local` / default api.vyrapp.pro)
3. Archive `releases\HexaTalk-<ver>.exe` (local only — next delta base)
4. **Sign** → ed25519, 64 raw bytes (embedded into each delta)
5. **Deltas** → qbsdiff, **HTD1 AES-256-GCM**, then **trailing target-exe sig**
6. Stage upload folder `releases\upload\` → **delta-only R2**

## Encrypted delta format (HTD1 + embedded sig)

```
magic  : "HTD1"           (4 bytes)
nonce  : 12 random bytes  (AES-GCM)
body   : ciphertext || 16-byte GCM tag over the raw qbsdiff patch
sig    : 64-byte ed25519 signature of the *target* HexaTalk.exe  (optional trailer)
```

Client flow (`src/update_check.rs`):

1. Download `HexaTalk-<current>-<remote>.delta`
2. Strip trailing 64-byte sig when present; AES-256-GCM decrypt HTD1 with baked key
3. `Bspatch` against the running exe
4. Verify reconstructed bytes against **embedded** sig (or detached `HexaTalk.exe.sig` for old uploads)
5. On any failure → try full `HexaTalk.exe` download (only if you still host it)

Legacy plain qbsdiff / HTD1 without trailer still work if `HexaTalk.exe.sig` is on the CDN.

## One-time setup

```powershell
cargo install qbsdiff --features cmd   # qbsdiff + qbspatch
pip install cryptography               # for sign_release.py + encrypt_delta.py
```

Private signing seed (32 bytes → 64 hex chars) lives **only offline**:

```powershell
$env:RELEASE_SIGNING_KEY_HEX = "<64 hex chars>"
```

Optional AES key override (defaults already match the baked client key):

```powershell
# Default (matches UPDATE_DELTA_KEY_B64 in build.rs):
$env:RELEASE_DELTA_KEY_HEX = "c14e32b623c2318ee6bf67cc85d88ba546a220b269e1af52487eec20274a554f"
```

Public ed25519 key + delta AES key are baked into the client
(`UPDATE_PUBLIC_KEY_B64`, `UPDATE_DELTA_KEY_B64` in `build.rs`).

## Every release

```powershell
# set keys first (same shell)
$env:RELEASE_SIGNING_KEY_HEX = "<64 hex chars>"

# patch bump, build, sign, encrypted delta(s) with embedded sig
.\scripts\release.ps1

# explicit version + deltas from ALL old archives + verify decrypt+patch
.\scripts\release.ps1 -Version 1.0.0 -AllDeltas -VerifyDelta
```

### Manual encrypt / decrypt (debug)

```powershell
$env:RELEASE_DELTA_KEY_HEX = "c14e32b623c2318ee6bf67cc85d88ba546a220b269e1af52487eec20274a554f"
python .\scripts\encrypt_delta.py encrypt plain.delta out.delta HexaTalk.exe.sig
python .\scripts\encrypt_delta.py decrypt out.delta plain2.delta
```

### Useful switches

| Switch | Meaning |
|--------|---------|
| `-Version 1.2.3` | Set exact version |
| `-Force` | Allow non-increasing version |
| `-AllDeltas` | Delta from every `releases\HexaTalk-*.exe` → new |
| `-VerifyDelta` | Decrypt HTD1 + `qbspatch` + SHA256 check |
| `-SkipSign` | Build without signature (**not for prod**) |
| `-SkipDelta` | No qbsdiff |
| `-SkipEncrypt` | Ship plain qbsdiff (+ still appends .sig if signed) |
| `-SkipBuild` | Only bump Cargo.toml |

## Upload to R2 / astrakit.pro (delta-only)

> The URLs below are the *original* project defaults baked in when
> `.env.local` doesn't override them. If you've pointed your own build at a
> different update host (see the `release-full.sh` section above), your
> actual upload target/URLs are whatever `.env.local` says instead --
> `scripts/release-full.sh` handles this automatically; the manual
> `release.ps1`/`release-linux.sh` steps below still apply the same way,
> just against your own host.

### Windows (`scripts/release.ps1`)

After the script finishes, upload **contents of** `releases\upload\`:

```
version.txt                           # just "1.2.3" (shared)
deltas/HexaTalk-1.0.0-1.2.3.delta     # HTD1 [+ optional ed25519 trailer]
deltas/HexaTalk-1.1.0-1.2.3.delta     # if -AllDeltas
HexaTalk.exe                          # optional full fallback
HexaTalk.exe.sig                      # optional
```

### Linux AppImage (`scripts/release-linux.sh`)

Portable build for most distros (libs bundled by linuxdeploy):

```bash
chmod +x scripts/release-linux.sh scripts/package-appimage.sh
./scripts/release-linux.sh            # patch +1 + AppImage + deltas
ALL_DELTAS=1 ./scripts/release-linux.sh
./scripts/package-appimage.sh         # AppImage only, no version bump
```

Upload:

```
version.txt                                                 # shared with Windows
HexaTalk-linux-x86_64.AppImage
HexaTalk-linux-x86_64.AppImage.sig                          # optional
deltas/HexaTalk-linux-x86_64.AppImage-1.0.0-1.2.3.delta
```

Users:

```bash
chmod +x HexaTalk-linux-x86_64.AppImage
./HexaTalk-linux-x86_64.AppImage
```

Linux notes:
- **Sounds**: rodio + embedded MP3 (notify + ringtone) — works in AppImage.
- **Self-update**: replaces `$APPIMAGE` when running as AppImage.
- **Tray**: StatusNotifier (KDE/XFCE OK; GNOME needs AppIndicator extension).
- **Deep links**: `~/.local/share/applications/hexatalk-vyrapp.desktop` on start.
- Build deps: `libgtk-3-dev`, `libasound2-dev`, `libxcb-*`, `curl`, `fuse3`/`libfuse2`.

### When to still host the full binary

| Situation | Need full binary + optional `.sig`? |
|-----------|-------------------------------------|
| Users on last version, you ship one delta | **No** |
| Users may skip versions | Yes, or ship `ALL_DELTAS` / `-AllDeltas` |
| First install from web | Yes (exe / AppImage on download page) |
| Corrupt / missing delta | Yes (fallback) |

URLs the client hits:

- `https://astrakit.pro/version.txt` **(required)**
- Windows: `deltas/HexaTalk-<from>-<to>.delta`, `HexaTalk.exe`
- Linux: `deltas/HexaTalk-linux-x86_64.AppImage-<from>-<to>.delta`, `HexaTalk-linux-x86_64.AppImage`

## Safety

- Authenticity is still **ed25519 of the reconstructed exe** (same key as
  detached `.sig`). AES only hides patch bytes on the CDN.
- Bad/missing/undecryptable delta → try full download; if you don't host the
  exe, update fails closed (no silent install).
- **Never** bump `version.txt` before the matching delta is live.

## First release after cloning

If `releases\` is empty, place the currently live `HexaTalk.exe` as
`releases\HexaTalk-<that_version>.exe` (from your machine / previous archive)
so the next run can make a delta. You do **not** need it on R2 for that.
