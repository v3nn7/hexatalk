# Talkyss zero-dependency installer (no Inno Setup needed).
# Copies Talkyss.exe into %LOCALAPPDATA%\Programs\Talkyss, creates Start Menu
# (+ optional Desktop) shortcuts and registers the uninstaller under HKCU.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File installer\install.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File installer\install.ps1 -Autostart -DesktopIcon
#   powershell -NoProfile -ExecutionPolicy Bypass -File installer\install.ps1 -Uninstall
#
# After install Talkyss updates ITSELF automatically; this script is only
# needed for the first-time setup.

[CmdletBinding()]
param(
    [string]$SourceExe = "target\release\Talkyss.exe",
    [switch]$DesktopIcon,
    [switch]$Autostart,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$AppName = "Talkyss"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\$AppName"
$ExePath = Join-Path $InstallDir "Talkyss.exe"
$StartMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\$AppName"
$UninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$AppName"
$RunKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"

function Stop-Talkyss {
    Get-Process -Name "Talkyss" -ErrorAction SilentlyContinue | Stop-Process -Force
}

function New-Shortcut([string]$Path, [string]$Target) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = $Target
    $shortcut.WorkingDirectory = Split-Path $Target
    $shortcut.IconLocation = "$Target,0"
    $shortcut.Save()
}

if ($Uninstall) {
    Write-Host "Uninstalling $AppName..."
    Stop-Talkyss
    Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $StartMenuDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $env:USERPROFILE "Desktop\$AppName.lnk") -Force -ErrorAction SilentlyContinue
    Remove-Item $UninstallKey -Recurse -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path $RunKey -Name $AppName -ErrorAction SilentlyContinue
    Write-Host "$AppName uninstalled."
    return
}

# Resolve the exe to install: parameter, next to this script, or repo build.
$candidates = @(
    $SourceExe,
    (Join-Path $PSScriptRoot "Talkyss.exe"),
    (Join-Path $PSScriptRoot "..\target\release\Talkyss.exe")
)
$Source = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Source) {
    throw "Talkyss.exe not found. Build it first (cargo build --release) or pass -SourceExe <path>."
}
$Source = (Resolve-Path $Source).Path

Write-Host "Installing $AppName from $Source"
Stop-Talkyss

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path $StartMenuDir | Out-Null
Copy-Item $Source $ExePath -Force

New-Shortcut (Join-Path $StartMenuDir "$AppName.lnk") $ExePath
if ($DesktopIcon) {
    New-Shortcut (Join-Path $env:USERPROFILE "Desktop\$AppName.lnk") $ExePath
}

# Register in "Add or remove programs" (HKCU -> no admin needed).
$uninstallCmd = "powershell -NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Uninstall"
New-Item -Path $UninstallKey -Force | Out-Null
Set-ItemProperty -Path $UninstallKey -Name "DisplayName" -Value $AppName
Set-ItemProperty -Path $UninstallKey -Name "DisplayIcon" -Value "$ExePath,0"
Set-ItemProperty -Path $UninstallKey -Name "InstallLocation" -Value $InstallDir
Set-ItemProperty -Path $UninstallKey -Name "UninstallString" -Value $uninstallCmd
Set-ItemProperty -Path $UninstallKey -Name "NoModify" -Value 1 -Type DWord
Set-ItemProperty -Path $UninstallKey -Name "NoRepair" -Value 1 -Type DWord

if ($Autostart) {
    Set-ItemProperty -Path $RunKey -Name $AppName -Value "`"$ExePath`""
}

Write-Host ""
Write-Host "$AppName installed to $InstallDir"
Write-Host "Start Menu shortcut created. Updates install automatically from now on."
