<#
.SYNOPSIS
  Generates the Android release-signing keystore for HexaTalk mobile
  (crates/hexatalk-mobile) with a random password, and prints the password
  ONCE to the console.

.DESCRIPTION
  Release APK signing is intentionally not configured in
  crates/hexatalk-mobile/Cargo.toml -- `cargo apk build --release` reads it
  from environment variables instead:
    CARGO_APK_RELEASE_KEYSTORE          -- path to the keystore file
    CARGO_APK_RELEASE_KEYSTORE_PASSWORD -- keystore password
  cargo-apk 0.10 signs with the keystore's single key directly (no
  key_alias/key_password override), so this script creates a keystore with
  one key and uses the same random password for store and key.

  The password is printed exactly once -- store it in a password manager
  immediately. Losing the keystore or its password means losing the ability
  to ship updates to already-installed release builds (Android refuses
  updates signed with a different key).

  The keystore lands next to this script's repo root by default and matches
  the `*.keystore` rule in .gitignore, so it is never committed.

.EXAMPLE
  .\scripts\gen_release_keystore.ps1
  Creates .\hexatalk-release.keystore with a random password.

.EXAMPLE
  .\scripts\gen_release_keystore.ps1 -Output D:\secrets\hexatalk-release.keystore
  Creates the keystore at an explicit path (e.g. outside the repo).
#>
param(
    [string]$Output = (Join-Path (Resolve-Path (Join-Path $PSScriptRoot "..")) "hexatalk-release.keystore")
)

$ErrorActionPreference = "Stop"

if (Test-Path $Output) {
    throw "Keystore already exists at '$Output'. Refusing to overwrite it -- back it up first, or pass a different -Output."
}

# Random password, printed once below. 24 base64ish chars = ~144 bits.
$alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789"
$password = -join (1..24 | ForEach-Object { $alphabet[(Get-Random -Maximum $alphabet.Length)] })

# keytool ships with the JDK / Android Studio JBR.
$keytool = Get-Command keytool -ErrorAction SilentlyContinue
if (-not $keytool) {
    $jbr = Join-Path $env:LOCALAPPDATA "Programs\Android Studio\jbr\bin\keytool.exe"
    if (Test-Path $jbr) { $keytool = $jbr } else { throw "keytool not found on PATH and no Android Studio JBR found. Install a JDK or pass keytool via PATH." }
}

& $keytool -genkeypair -v `
    -keystore $Output `
    -alias hexatalk-release `
    -keyalg RSA -keysize 4096 -validity 10000 `
    -storepass $password -keypass $password `
    -dname "CN=HexaTalk Release, OU=Mobile, O=HexaTalk, C=PL"
if ($LASTEXITCODE -ne 0) { throw "keytool failed with exit code $LASTEXITCODE" }

Write-Host ""
Write-Host "Release keystore created: $Output" -ForegroundColor Green
Write-Host "Password (shown ONCE -- store it in a password manager NOW):" -ForegroundColor Yellow
Write-Host "  $password" -ForegroundColor Yellow
Write-Host ""
Write-Host "To build a signed release APK, set (same shell session):"
Write-Host "  `$env:CARGO_APK_RELEASE_KEYSTORE = '$Output'"
Write-Host "  `$env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD = '<password from above>'"
Write-Host "  cargo apk build --release --manifest-path crates/hexatalk-mobile/Cargo.toml"
