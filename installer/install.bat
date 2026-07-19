@echo off
REM Talkyss one-click installer: runs install.ps1 with the Desktop shortcut.
REM Double-click after building (cargo build --release) or place next to Talkyss.exe.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" -DesktopIcon %*
pause
