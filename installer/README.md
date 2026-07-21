# HexaTalk installer (Inno Setup 6)

Per-user setup → `%LOCALAPPDATA%\Programs\HexaTalk` (no admin).

## Build

```powershell
# Unsigned (SmartScreen will warn until you have a cert)
.\installer\build.ps1 -SkipBuildExe   # uses existing target\release\HexaTalk.exe
.\installer\build.ps1                 # cargo build --release first
```

Output: `installer\Output\HexaTalkSetup.exe`

## Authenticode (kills most Defender / SmartScreen FPs)

Buy a **code signing** certificate (OV/EV) from DigiCert, Sectigo, SSL.com, etc.  
This is **not** the same as `RELEASE_SIGNING_KEY_HEX` (ed25519 for auto-update).

### Option A — PFX file

```powershell
$env:CODE_SIGN_PFX = "D:\secrets\hexatalk-codesign.pfx"
$env:CODE_SIGN_PFX_PASSWORD = "your-pfx-password"
# optional:
# $env:CODE_SIGN_TIMESTAMP_URL = "http://timestamp.digicert.com"

.\installer\build.ps1
```

### Option B — cert already in Windows store

```powershell
# Certmgr → Personal → certificate → Details → Thumbprint
$env:CODE_SIGN_THUMBPRINT = "AABBCCDDEE..."
.\installer\build.ps1
```

Inno will sign:

1. `HexaTalk.exe` before packing (`Flags: sign`)
2. The setup `.exe`
3. The uninstaller (`SignedUninstaller=yes`)

## From full release pipeline

```powershell
.\scripts\release.ps1
# builds installer at the end unless -SkipInstaller
```

## Requirements

- [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`ISCC.exe` on PATH or default install dir)
- [Windows SDK](https://developer.microsoft.com/windows/downloads/windows-sdk/) for `signtool.exe` (only when signing)

## Manual iscc (unsigned)

```powershell
iscc /DAppVersion=0.1.3 installer\hexatalk.iss
```

(Signing: use `.\installer\build.ps1` with `CODE_SIGN_*` — do not set `SignTool=` alone in the IDE.)
