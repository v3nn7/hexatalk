<#
.SYNOPSIS
  Full desktop release pipeline for HexaTalk auto-updates:
    1. Bump version in Cargo.toml
    2. cargo build --release (bakes CONVEX_URL from .env.local)
    3. Archive releases\HexaTalk-<ver>.exe (local only; next delta base)
    4. Sign with ed25519 (RELEASE_SIGNING_KEY_HEX) — 64 raw bytes
    5. Generate qbsdiff deltas, HTD1-encrypt, append target-exe sig
       -> deltas\HexaTalk-<from>-<to>.delta
    6. Stage delta-only upload: releases\upload\{version.txt, deltas\}

.PARAMETER Version
  Explicit version, e.g. "1.2.0". Must be > current unless -Force.
  If omitted, patch +1 (0.1.0 -> 0.1.1).

.PARAMETER Force
  Allow non-increasing version.

.PARAMETER SkipBuild
  Only bump version; no build / sign / delta.

.PARAMETER SkipSign
  Build + delta, but skip ed25519 signing (not for production).

.PARAMETER AllDeltas
  Generate a delta from EVERY archived HexaTalk-*.exe in releases\
  to the new build (not only the latest previous). Users a few
  versions behind can still incremental-update.

.PARAMETER SkipDelta
  Skip qbsdiff entirely.

.PARAMETER SkipEncrypt
  Keep deltas as plain qbsdiff (legacy). Not for production - clients still
  accept plain patches as a fallback, but release.ps1 normally ships HTD1.

.PARAMETER VerifyDelta
  After generating a delta: decrypt (if HTD1) + qbspatch + byte compare.

.EXAMPLE
  $env:RELEASE_SIGNING_KEY_HEX = "<64 hex chars private seed>"
  # optional; defaults match the baked UPDATE_DELTA_KEY_B64 in build.rs
  $env:RELEASE_DELTA_KEY_HEX = "<64 hex chars AES-256 key>"
  .\scripts\release.ps1

.EXAMPLE
  .\scripts\release.ps1 -Version 1.0.0 -AllDeltas -VerifyDelta
#>
param(
    [string]$Version,
    [switch]$Force,
    [switch]$SkipBuild,
    [switch]$SkipSign,
    [switch]$AllDeltas,
    [switch]$SkipDelta,
    [switch]$SkipEncrypt,
    [switch]$VerifyDelta
)

$ErrorActionPreference = "Stop"
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$cargoToml = Join-Path $repoRoot "Cargo.toml"
$signPy = Join-Path $PSScriptRoot "sign_release.py"
$encryptPy = Join-Path $PSScriptRoot "encrypt_delta.py"
$releasesDir = Join-Path $repoRoot "releases"
$deltasDir = Join-Path $repoRoot "deltas"
$uploadDir = Join-Path $releasesDir "upload"

# Default AES-256 key (hex) matching UPDATE_DELTA_KEY_B64 baked in build.rs.
# Override with RELEASE_DELTA_KEY_HEX (and matching UPDATE_DELTA_KEY_B64 env
# when building the client) for a private/rotated key.
if (-not $env:RELEASE_DELTA_KEY_HEX) {
    $env:RELEASE_DELTA_KEY_HEX = "c14e32b623c2318ee6bf67cc85d88ba546a220b269e1af52487eec20274a554f"
}

function Get-Python {
    $python = Get-Command python -ErrorAction SilentlyContinue
    if (-not $python) { $python = Get-Command py -ErrorAction SilentlyContinue }
    if (-not $python) { throw "python not found on PATH (needed for sign/encrypt helpers)" }
    return $python
}

function Compare-Version([int[]]$a, [int[]]$b) {
    for ($i = 0; $i -lt 3; $i++) {
        if ($a[$i] -ne $b[$i]) { return $a[$i] - $b[$i] }
    }
    return 0
}

function Get-VersionFromName([string]$baseName) {
    # HexaTalk-1.2.3 -> 1.2.3
    if ($baseName -match '^HexaTalk-(\d+\.\d+\.\d+)$') { return $Matches[1] }
    return $null
}

# Read ONLY the [package] table version (not dependency `version = "..."` lines).
# Returns @{ Text = "x.y.z"; Major = n; Minor = n; Patch = n } or $null.
function Get-PackageVersion([string]$tomlText) {
    # From [package] until the next [section], find version = "x.y.z"
    $m = [regex]::Match(
        $tomlText,
        '(?ms)^\[package\]\s*\r?\n(.*?)(?=^\[|\z)'
    )
    if (-not $m.Success) { return $null }
    $body = $m.Groups[1].Value
    $vm = [regex]::Match($body, '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"\s*$')
    if (-not $vm.Success) { return $null }
    return @{
        Text  = "$($vm.Groups[1].Value).$($vm.Groups[2].Value).$($vm.Groups[3].Value)"
        Major = [int]$vm.Groups[1].Value
        Minor = [int]$vm.Groups[2].Value
        Patch = [int]$vm.Groups[3].Value
    }
}

# Replace [package].version only; leave dependency version constraints alone.
function Set-PackageVersion([string]$tomlText, [string]$newVer) {
    $regex = [regex]'(?ms)(^\[package\]\s*\r?\n(?:(?!^\[).)*?^version\s*=\s*")\d+\.\d+\.\d+(")'
    $evaluator = {
        param($match)
        return $match.Groups[1].Value + $newVer + $match.Groups[2].Value
    }
    $once = $regex.Replace($tomlText, $evaluator, 1)
    return $once
}

function Write-Utf8NoBom([string]$path, [string]$text) {
    $enc = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($path, $text, $enc)
}

# ---------- version bump ----------
# Prefer raw bytes over Get-Content: Set-Content historically rewrote Cargo.toml
# as UTF-16 on Windows, after which cargo still built but later runs + editors
# could disagree about what "current version" is.
$content = [System.IO.File]::ReadAllText($cargoToml)
$pkg = Get-PackageVersion $content
if (-not $pkg) {
    throw "Could not find [package] version = `"x.y.z`" in Cargo.toml"
}
$current = [int[]]@($pkg.Major, $pkg.Minor, $pkg.Patch)
$currentStr = $pkg.Text

if ($Version) {
    if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)$') {
        throw "Version must look like 1.2.3, got '$Version'"
    }
    # Capture groups immediately — $Matches is overwritten by later -match ops.
    $newParts = [int[]]@([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
    $newVersion = "$($newParts[0]).$($newParts[1]).$($newParts[2])"
    if (-not $Force -and (Compare-Version $newParts $current) -le 0) {
        throw "New version $newVersion is not greater than current $currentStr. Pass -Force to override."
    }
} else {
    $newParts = @($current[0], $current[1], $current[2] + 1)
    $newVersion = "$($newParts[0]).$($newParts[1]).$($newParts[2])"
}

Write-Host "Bumping version: $currentStr -> $newVersion" -ForegroundColor Cyan
if ($currentStr -eq $newVersion) {
    Write-Host "  (same version, -Force: Cargo.toml left unchanged)" -ForegroundColor DarkGray
} else {
    $updated = Set-PackageVersion $content $newVersion
    $check = Get-PackageVersion $updated
    if (-not $check -or $check.Text -ne $newVersion) {
        throw "Failed to rewrite [package].version to $newVersion in memory (still $($check.Text))"
    }
    Write-Utf8NoBom $cargoToml $updated
    # Verify on disk — never continue a release if the bump didn't stick.
    $onDisk = Get-PackageVersion ([System.IO.File]::ReadAllText($cargoToml))
    if (-not $onDisk -or $onDisk.Text -ne $newVersion) {
        throw "Cargo.toml on disk still reports version='$($onDisk.Text)' after bump to $newVersion"
    }
    Write-Host "  Cargo.toml [package].version = $newVersion (verified)" -ForegroundColor Green
}

if ($SkipBuild) {
    Write-Host "Version bumped to $newVersion. Skipping build (-SkipBuild)." -ForegroundColor Yellow
    exit 0
}

# ---------- build (bake CONVEX_URL from .env.local) ----------
Write-Host "Building release ($newVersion)..." -ForegroundColor Cyan
# Prefer .env.local so build.rs bakes production URL when set.
$envLocal = Join-Path $repoRoot ".env.local"
if (Test-Path $envLocal) {
    Get-Content $envLocal | ForEach-Object {
        if ($_ -match '^\s*#') { return }
        if ($_ -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$') {
            $k = $Matches[1]
            $v = $Matches[2].Trim().Trim('"').Split('#')[0].Trim()
            if ($v) { Set-Item -Path "env:$k" -Value $v }
        }
    }
    Write-Host "  Loaded env from .env.local (CONVEX_URL=$($env:CONVEX_URL))" -ForegroundColor DarkGray
}

Push-Location $repoRoot
try {
    # Force rebuild of build.rs secrets bake
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$exePath = Join-Path $repoRoot "target\release\HexaTalk.exe"
if (-not (Test-Path $exePath)) {
    throw "Build succeeded but $exePath was not found."
}

New-Item -ItemType Directory -Force -Path $releasesDir | Out-Null
New-Item -ItemType Directory -Force -Path $deltasDir | Out-Null
New-Item -ItemType Directory -Force -Path $uploadDir | Out-Null
$uploadDeltasDir = Join-Path $uploadDir "deltas"
New-Item -ItemType Directory -Force -Path $uploadDeltasDir | Out-Null

$archivePath = Join-Path $releasesDir "HexaTalk-$newVersion.exe"
Copy-Item -Path $exePath -Destination $archivePath -Force
Write-Host "Archived: $archivePath" -ForegroundColor Green

# ---------- sign (ed25519 raw 64-byte .sig) ----------
$sigPath = Join-Path $repoRoot "target\release\HexaTalk.exe.sig"
$sigArchive = Join-Path $releasesDir "HexaTalk-$newVersion.exe.sig"
$signed = $false

if (-not $SkipSign) {
    $keyHex = $env:RELEASE_SIGNING_KEY_HEX
    if (-not $keyHex) {
        throw @"
RELEASE_SIGNING_KEY_HEX is not set.

Set the 64-char hex ed25519 private seed (same key whose public half is baked
into the app - see src/update_check.rs), then re-run:

  `$env:RELEASE_SIGNING_KEY_HEX = '<64 hex chars>'
  .\scripts\release.ps1

Or pass -SkipSign to build without a signature (not for production).
"@
    }
    if (-not (Test-Path $signPy)) {
        throw "Missing $signPy"
    }
    $python = Get-Python

    Write-Host "Signing $exePath ..." -ForegroundColor Cyan
    & $python.Source $signPy $exePath $sigPath
    if ($LASTEXITCODE -ne 0) { throw "sign_release.py failed with exit code $LASTEXITCODE" }
    if (-not (Test-Path $sigPath)) { throw "Signature file not written: $sigPath" }
    $sigLen = (Get-Item $sigPath).Length
    if ($sigLen -ne 64) {
        throw "Signature must be exactly 64 raw bytes, got $sigLen (check sign_release.py / key)"
    }
    Copy-Item -Path $sigPath -Destination $sigArchive -Force
    # Detached .sig is optional on CDN when deltas embed the same 64 bytes
    # (default). Keep a local archive copy for debugging / full-fallback packs.
    $signed = $true
    Write-Host "  Signature: $sigPath ($sigLen bytes) [embedded into deltas, not required on R2]" -ForegroundColor Green
} else {
    Write-Warning "Skipping signature (-SkipSign). Deltas will not embed a sig; auto-update will reject."
}

# ---------- version.txt ----------
$versionTxt = Join-Path $uploadDir "version.txt"
Write-Utf8NoBom $versionTxt $newVersion
# Keep a copy at releases\version.txt so local tooling can see last ship.
Write-Utf8NoBom (Join-Path $releasesDir "version.txt") $newVersion
# Full HexaTalk.exe is NOT staged for upload by default: delta-only R2
# (version.txt + deltas/* with embedded ed25519). Local archive still has
# releases\HexaTalk-<ver>.exe for the next delta generation.
Write-Host "version.txt -> $newVersion" -ForegroundColor Green

# ---------- deltas (qbsdiff) ----------
$deltaPaths = @()
if (-not $SkipDelta) {
    $qbsdiff = Get-Command qbsdiff -ErrorAction SilentlyContinue
    if (-not $qbsdiff) {
        Write-Warning "qbsdiff not on PATH. Install: cargo install qbsdiff --features cmd"
        Write-Warning "Skipping deltas (clients will full-download .exe)."
    } else {
        $prevArchives = Get-ChildItem -Path $releasesDir -Filter "HexaTalk-*.exe" |
            Where-Object {
                $_.Name -ne "HexaTalk-$newVersion.exe" -and
                (Get-VersionFromName $_.BaseName)
            } |
            Sort-Object {
                $v = Get-VersionFromName $_.BaseName
                [version]$v
            }

        if (-not $prevArchives) {
            Write-Host "No previous HexaTalk-*.exe in releases\ - first release, no delta." -ForegroundColor Yellow
        } else {
            $targets = if ($AllDeltas) {
                $prevArchives
            } else {
                @($prevArchives | Select-Object -Last 1)
            }

            $pythonForDelta = $null
            if (-not $SkipEncrypt) {
                if (-not (Test-Path $encryptPy)) {
                    throw "Missing $encryptPy (needed to AES-GCM encrypt deltas)"
                }
                $pythonForDelta = Get-Python
            }

            foreach ($prev in $targets) {
                $prevVersion = Get-VersionFromName $prev.BaseName
                if (-not $prevVersion) { continue }
                $deltaName = "HexaTalk-$prevVersion-$newVersion.delta"
                $deltaPath = Join-Path $deltasDir $deltaName
                $plainDeltaPath = Join-Path $deltasDir "$deltaName.plain"
                Write-Host "Generating delta $prevVersion -> $newVersion ..." -ForegroundColor Cyan
                & qbsdiff $prev.FullName $archivePath $plainDeltaPath
                if ($LASTEXITCODE -ne 0) {
                    Write-Warning "qbsdiff failed for $prevVersion (exit $LASTEXITCODE) - skip."
                    Remove-Item $plainDeltaPath -Force -ErrorAction SilentlyContinue
                    continue
                }
                if (-not (Test-Path $plainDeltaPath)) {
                    Write-Warning "Delta not written: $plainDeltaPath"
                    continue
                }

                # Encrypt plain qbsdiff -> HTD1 frame, optionally + trailing
                # 64-byte ed25519 of the *target* exe so R2 needs no full .exe/.sig.
                if (-not $SkipEncrypt) {
                    Write-Host "  Encrypting HTD1 (AES-256-GCM)$(if ($signed) { ' + embedded .sig' }) ..." -ForegroundColor Cyan
                    if ($signed) {
                        & $pythonForDelta.Source $encryptPy encrypt $plainDeltaPath $deltaPath $sigPath
                    } else {
                        & $pythonForDelta.Source $encryptPy encrypt $plainDeltaPath $deltaPath
                    }
                    if ($LASTEXITCODE -ne 0) {
                        Write-Warning "encrypt_delta.py failed for $prevVersion - skip."
                        Remove-Item $plainDeltaPath -Force -ErrorAction SilentlyContinue
                        Remove-Item $deltaPath -Force -ErrorAction SilentlyContinue
                        continue
                    }
                    Remove-Item $plainDeltaPath -Force -ErrorAction SilentlyContinue
                } else {
                    Move-Item -Path $plainDeltaPath -Destination $deltaPath -Force
                    Write-Warning "  Plain (unencrypted) delta - not for delta-only R2 (needs HTD1 + embedded sig)."
                }

                if (-not (Test-Path $deltaPath)) {
                    Write-Warning "Delta not written: $deltaPath"
                    continue
                }

                if ($VerifyDelta) {
                    $qbspatch = Get-Command qbspatch -ErrorAction SilentlyContinue
                    if ($qbspatch) {
                        $patchInput = $deltaPath
                        $tmpPlain = $null
                        # Decrypt HTD1 back to raw qbsdiff before qbspatch.
                        if (-not $SkipEncrypt) {
                            if (-not $pythonForDelta) { $pythonForDelta = Get-Python }
                            $tmpPlain = Join-Path $env:TEMP "HexaTalk-delta-plain-$prevVersion-$newVersion.delta"
                            & $pythonForDelta.Source $encryptPy decrypt $deltaPath $tmpPlain
                            if ($LASTEXITCODE -ne 0) {
                                Write-Warning "decrypt for verify failed ($prevVersion) - delta discarded."
                                Remove-Item $deltaPath -Force -ErrorAction SilentlyContinue
                                continue
                            }
                            $patchInput = $tmpPlain
                        }
                        $testOut = Join-Path $env:TEMP "HexaTalk-patch-test-$newVersion.exe"
                        & qbspatch $prev.FullName $patchInput $testOut
                        $patchOk = ($LASTEXITCODE -eq 0)
                        if ($tmpPlain) { Remove-Item $tmpPlain -Force -ErrorAction SilentlyContinue }
                        if (-not $patchOk) {
                            Write-Warning "qbspatch failed for $prevVersion - delta discarded."
                            Remove-Item $deltaPath -Force -ErrorAction SilentlyContinue
                            continue
                        }
                        $a = Get-FileHash $archivePath -Algorithm SHA256
                        $b = Get-FileHash $testOut -Algorithm SHA256
                        Remove-Item $testOut -Force -ErrorAction SilentlyContinue
                        if ($a.Hash -ne $b.Hash) {
                            Write-Warning "Delta verify mismatch for $prevVersion - delta discarded."
                            Remove-Item $deltaPath -Force -ErrorAction SilentlyContinue
                            continue
                        }
                        Write-Host "  Verified OK (decrypt + SHA256 match)" -ForegroundColor Green
                    } else {
                        Write-Warning "qbspatch not on PATH - skip -VerifyDelta check."
                    }
                }

                Copy-Item -Path $deltaPath -Destination (Join-Path $uploadDeltasDir $deltaName) -Force
                $deltaPaths += $deltaPath
                $sizeMb = [math]::Round((Get-Item $deltaPath).Length / 1MB, 2)
                Write-Host "  $deltaPath ($sizeMb MB, encrypted=$([bool](-not $SkipEncrypt)))" -ForegroundColor Green
            }
        }
    }
}

# ---------- summary ----------
Write-Host ""
Write-Host "========== RELEASE $newVersion READY ==========" -ForegroundColor Green
Write-Host "Upload bundle (mirror to https://astrakit.pro / R2) — delta-only:" -ForegroundColor Cyan
Write-Host "  $uploadDir\"
Write-Host "    version.txt              = $newVersion"
if ($deltaPaths.Count -gt 0) {
    Write-Host "    deltas\                  (HTD1 + embedded ed25519 of target exe)"
    foreach ($d in $deltaPaths) {
        Write-Host "      $(Split-Path $d -Leaf)"
    }
} else {
    Write-Host "    deltas\                  = (none this release)" -ForegroundColor Yellow
    Write-Host "    !! Without deltas, clients cannot update unless you also host HexaTalk.exe + .sig" -ForegroundColor Yellow
}
Write-Host ""
Write-Host "Optional full-download fallback (NOT staged by default):" -ForegroundColor DarkGray
Write-Host "  HexaTalk.exe + HexaTalk.exe.sig  — only if you want skip-version recovery"
Write-Host "  Local archive still at: releases\HexaTalk-$newVersion.exe"
Write-Host ""
Write-Host "Client expects:" -ForegroundColor DarkGray
Write-Host "  GET .../version.txt"
Write-Host "  GET .../deltas/HexaTalk-<from>-<to>.delta   (HTD1 || AES(qbsdiff) || sig64)"
Write-Host "  GET .../HexaTalk.exe[.sig]                  (optional fallback only)"
Write-Host ""
Write-Host "Delta wire: HTD1 || nonce(12) || AES-256-GCM(qbsdiff) || ed25519(64)." -ForegroundColor DarkGray
Write-Host "  AES key = RELEASE_DELTA_KEY_HEX (must match baked UPDATE_DELTA_KEY_B64)." -ForegroundColor DarkGray
Write-Host "  ed25519 verifies reconstructed exe bytes (same as detached .sig)." -ForegroundColor DarkGray
Write-Host ""
if (-not $signed) {
    Write-Host "WARNING: unsigned build - auto-update will reject it." -ForegroundColor Red
    exit 2
}
if ($deltaPaths.Count -eq 0) {
    Write-Host "WARNING: no deltas produced — delta-only R2 will not update anyone." -ForegroundColor Red
}
