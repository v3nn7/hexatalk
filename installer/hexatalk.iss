; HexaTalk installer — Inno Setup 6 script.
; Builds a per-user installer (no admin rights needed):
;   - installs into %LOCALAPPDATA%\Programs\HexaTalk
;   - Start Menu + optional Desktop shortcut
;   - optional "Launch at Windows startup" task
;   - registered uninstaller in "Add or remove programs"
;
; Build (unsigned — works in Compiler IDE or plain iscc):
;   iscc installer\hexatalk.iss
;   iscc /DAppVersion=0.1.3 installer\hexatalk.iss
;
; Build (Authenticode): do NOT set SignTool by hand in the IDE.
;   .\installer\build.ps1
;   → passes  /DUseSign=1  /Shexatalk=<signtool command>
;   The name after SignTool= MUST match the /S name (hexatalk).
;
; Output: installer\Output\HexaTalkSetup-<version>.exe
;
; NOTE: after install, HexaTalk updates ITSELF (see src/update_check.rs) —
; this installer is only needed for the first-time setup.

#ifndef AppName
  #define AppName "HexaTalk"
#endif
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef AppPublisher
  #define AppPublisher "v3nn7"
#endif
#ifndef AppExeName
  #define AppExeName "HexaTalk.exe"
#endif

[Setup]
AppId={{7E4A9C2D-3F1B-4A8E-9D5C-2B6F0E1A7C44}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL=https://vyrapp.pro/
AppSupportURL=https://vyrapp.pro/
AppUpdatesURL=https://vyrapp.pro/download
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
; Per-user install: no UAC prompt, no admin needed.
PrivilegesRequired=lowest
OutputDir=Output
OutputBaseFilename=HexaTalkSetup
; Milder compression — fewer AV false positives than lzma2/ultra64 solid.
Compression=lzma
SolidCompression=no
WizardStyle=modern
; Close a running HexaTalk before overwriting its exe.
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no
UninstallDisplayIcon={app}\{#AppExeName}
; --- Authenticode (optional) ---
; Only when build.ps1 passes /DUseSign=1 AND /Shexatalk="signtool ... $f"
; Without that, SignTool= is omitted so plain iscc / Compiler IDE works.
#ifdef UseSign
SignTool=hexatalk
SignedUninstaller=yes
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "polish"; MessagesFile: "compiler:Languages\Polish.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "autostart"; Description: "Launch HexaTalk when Windows starts"; GroupDescription: "Startup:"; Flags: unchecked

[Files]
; `sign` only when UseSign is defined (otherwise "sign" flag errors without SignTool).
#ifdef UseSign
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion sign
#else
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
#endif

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; \
    ValueName: "{#AppName}"; ValueData: """{app}\{#AppExeName}"""; \
    Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; \
    Flags: nowait postinstall skipifsilent
