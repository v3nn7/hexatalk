<#
.SYNOPSIS
  Build HexaTalkSetup.exe with Inno Setup 6, optionally Authenticode-signed.

.DESCRIPTION
  Finds `iscc` and (when a cert is configured) `signtool`, then compiles
  installer\hexatalk.iss. Signing is wired through Inno's SignTool=hexatalk
  so the setup, uninstaller, and packed HexaTalk.exe are all signed.

.ENVIRONMENT
  CODE_SIGN_PFX              Path to .pfx / .p12 (software cert)
  CODE_SIGN_PFX_PASSWORD     PFX password
  CODE_SIGN_THUMBPRINT       SHA1 thumbprint of a cert already in the
                             Windows cert store (CurrentUser\My or LocalMachine\My).
                             Used when CODE_SIGN_PFX is not set.
  CODE_SIGN_TIMESTAMP_URL    RFC3161 timestamp server
                             (default: http://timestamp.digicert.com)
  CODE_SIGN_SUBJECT          Optional /n "Subject Name" for store certs

.PARAMETER Version
  Override AppVersion in the ISS (default: read [package].version from Cargo.toml).

.PARAMETER NoSign
  Force unsigned build even if cert env vars are set.

.PARAMETER SkipBuildExe
  Do not run cargo build --release first (use existing target\release\HexaTalk.exe).

.EXAMPLE
  $env:CODE_SIGN_PFX = "D:\certs\hexatalk.pfx"
  $env:CODE_SIGN_PFX_PASSWORD = "secret"
  .\installer\build.ps1

.EXAMPLE
  $env:CODE_SIGN_THUMBPRINT = "AABBCC..."
  .\installer\build.ps1 -Version 0.1.3
#>
param(
    [string]$Version,
    [switch]$NoSign,
    [switch]$SkipBuildExe
)

$ErrorActionPreference = "Stop"
$installerDir = $PSScriptRoot
$repoRoot = Resolve-Path (Join-Path $installerDir "..")
$iss = Join-Path $installerDir "hexatalk.iss"
$outDir = Join-Path $installerDir "Output"

function Find-Iscc {
    $cmd = Get-Command iscc -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "${env:LocalAppData}\Programs\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $candidates) {
        if (Test-Path $p) { return $p }
    }
    return $null
}

function Find-Signtool {
    $cmd = Get-Command signtool -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    # Prefer newest Windows SDK
    $kits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (Test-Path $kits) {
        $found = Get-ChildItem -Path $kits -Recurse -Filter "signtool.exe" -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

function Get-CargoPackageVersion([string]$cargoToml) {
    $text = [System.IO.File]::ReadAllText($cargoToml)
    $m = [regex]::Match($text, '(?ms)^\[package\]\s*\r?\n(.*?)(?=^\[|\z)')
    if (-not $m.Success) { return $null }
    $vm = [regex]::Match($m.Groups[1].Value, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
    if (-not $vm.Success) { return $null }
    return $vm.Groups[1].Value
}

function Build-SignToolCommand([string]$signtool) {
    $ts = $env:CODE_SIGN_TIMESTAMP_URL
    if (-not $ts) { $ts = "http://timestamp.digicert.com" }

    # Inno Setup expands $f to the file path to sign. Must stay as literal $f
    # in the string passed to iscc (/Shexatalk=...). Escape `$ for PowerShell.
    # Prefer $q...$q quoting recommended by Inno docs for paths with spaces.
    if ($env:CODE_SIGN_PFX) {
        $pfx = (Resolve-Path $env:CODE_SIGN_PFX).Path
        $pass = $env:CODE_SIGN_PFX_PASSWORD
        if (-not $pass) {
            throw "CODE_SIGN_PFX is set but CODE_SIGN_PFX_PASSWORD is empty"
        }
        return "$q$signtool$q sign /fd SHA256 /tr $ts /td SHA256 /f $q$pfx$q /p $q$pass$q `$f".Replace(
            '$q', [string][char]34
        )
    }

    if ($env:CODE_SIGN_THUMBPRINT) {
        $thumb = ($env:CODE_SIGN_THUMBPRINT -replace '\s', '').ToUpperInvariant()
        $subject = $env:CODE_SIGN_SUBJECT
        if ($subject) {
            return ("$q$signtool$q sign /fd SHA256 /tr $ts /td SHA256 /sha1 $thumb /n $q$subject$q `$f").Replace(
                '$q', [string][char]34
            )
        }
        return ("$q$signtool$q sign /fd SHA256 /tr $ts /td SHA256 /sha1 $thumb `$f").Replace(
            '$q', [string][char]34
        )
    }

    return $null
}

# ---------- version ----------
if (-not $Version) {
    $Version = Get-CargoPackageVersion (Join-Path $repoRoot "Cargo.toml")
    if (-not $Version) { $Version = "0.0.0" }
}
Write-Host "Installer version: $Version" -ForegroundColor Cyan

# ---------- exe ----------
$exePath = Join-Path $repoRoot "target\release\HexaTalk.exe"
if (-not $SkipBuildExe) {
    Write-Host "Building HexaTalk.exe (release)..." -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        # Bake API_URL / TURN_* if .env.local is present
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
        }
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }
    } finally {
        Pop-Location
    }
}
if (-not (Test-Path $exePath)) {
    throw "Missing $exePath - build first or drop -SkipBuildExe"
}

# ---------- iscc ----------
$iscc = Find-Iscc
if (-not $iscc) {
    throw @"
Inno Setup 6 (iscc) not found.
Install from https://jrsoftware.org/isinfo.php or add ISCC.exe to PATH.
"@
}
Write-Host "Using ISCC: $iscc" -ForegroundColor DarkGray

# ---------- signing ----------
$signArgs = @()
$doSign = -not $NoSign
$signCmd = $null
if ($doSign) {
    $signtool = Find-Signtool
    if (-not $signtool) {
        Write-Warning "signtool.exe not found (Windows SDK). Building UNSIGNED installer."
        $doSign = $false
    } else {
        try {
            $signCmd = Build-SignToolCommand $signtool
        } catch {
            Write-Warning $_
            $doSign = $false
        }
        if (-not $signCmd) {
            Write-Warning "No CODE_SIGN_PFX / CODE_SIGN_THUMBPRINT - building UNSIGNED installer."
            Write-Warning "Set those env vars to Authenticode-sign setup + exe (kills most SmartScreen FP)."
            $doSign = $false
        }
    }
}

$isccArgs = @(
    "/DAppVersion=$Version"
    "/DAppName=HexaTalk"
    "/DAppPublisher=v3nn7"
)

if ($doSign) {
    Write-Host "Authenticode: ON" -ForegroundColor Green
    Write-Host "  SignTool command template configured for Inno (hexatalk)" -ForegroundColor DarkGray
    # /DUseSign enables SignTool=hexatalk in the .iss; /Shexatalk= defines it.
    $isccArgs += "/DUseSign=1"
    $isccArgs += "/Shexatalk=$signCmd"
} else {
    Write-Host "Authenticode: OFF (unsigned)" -ForegroundColor Yellow
}

$isccArgs += $iss

Write-Host "Compiling installer..." -ForegroundColor Cyan
& $iscc @isccArgs
if ($LASTEXITCODE -ne 0) {
    throw "iscc failed with exit code $LASTEXITCODE"
}

$setup = Get-Item (Join-Path $outDir "HexaTalkSetup.exe") -ErrorAction SilentlyContinue
if (-not $setup) {
    # Legacy name with version in filename (older scripts)
    $setup = Get-ChildItem -Path $outDir -Filter "HexaTalkSetup-*.exe" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
}
if (-not $setup) {
    throw "Installer output not found under $outDir"
}

Write-Host ""
Write-Host "========== INSTALLER READY ==========" -ForegroundColor Green
Write-Host "  $($setup.FullName)"
Write-Host "  Size: $([math]::Round($setup.Length / 1MB, 2)) MB"
if ($doSign) {
    Write-Host "  Signed: yes (setup + uninstaller + HexaTalk.exe)" -ForegroundColor Green
    # Optional verify
    $signtool = Find-Signtool
    if ($signtool) {
        & $signtool verify /pa /v $setup.FullName 2>&1 | Out-Host
    }
} else {
    Write-Host "  Signed: no - SmartScreen/Defender may warn until you use a code-signing cert." -ForegroundColor Yellow
}
Write-Host ""
Write-Host "Env for signing next time:" -ForegroundColor DarkGray
Write-Host '  $env:CODE_SIGN_PFX = "C:\path\to\cert.pfx"'
Write-Host '  $env:CODE_SIGN_PFX_PASSWORD = "***"'
Write-Host '  # or: $env:CODE_SIGN_THUMBPRINT = "SHA1 thumbprint from certmgr"'
