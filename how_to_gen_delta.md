# Release + encrypted delta (incremental update)

`scripts\release.ps1` does the full pipeline:

1. Bump version in `Cargo.toml`
2. `cargo build --release` (bakes `CONVEX_URL` from `.env.local`)
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

After the script finishes, upload **contents of** `releases\upload\`:

```
version.txt                           # just "1.2.3"
deltas/HexaTalk-1.0.0-1.2.3.delta     # HTD1 + trailing ed25519
deltas/HexaTalk-1.1.0-1.2.3.delta     # if -AllDeltas
```

**You do not need to upload `HexaTalk.exe`.** The 64-byte release signature
rides inside each delta. Full exe stays only in local `releases\` for the
next patch base.

### When to still host the full exe

| Situation | Need full `HexaTalk.exe` + `.sig`? |
|-----------|-------------------------------------|
| Users on last version, you ship one delta | **No** |
| Users may skip versions (0.1.0 → 0.1.3) | Yes, or ship `-AllDeltas` for every old archive |
| First install / reinstall from web | Yes (or a separate download page) |
| Corrupt / missing delta for a pair | Yes (fallback), otherwise update fails |

URLs the client hits:

- `https://astrakit.pro/version.txt` **(required)**
- `https://astrakit.pro/deltas/HexaTalk-<from>-<to>.delta` **(required for delta-only)**
- `https://astrakit.pro/HexaTalk.exe` + `.sig` *(optional fallback)*

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
