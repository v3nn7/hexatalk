; HexaTalk installer — Inno Setup 6 script.
; Builds a per-user installer (no admin rights needed):
;   - installs into %LOCALAPPDATA%\Programs\HexaTalk
;   - Start Menu + optional Desktop shortcut
;   - optional "launch at Windows startup" task
;   - registered uninstaller in "Add or remove programs"
;
; Build:  iscc installer\hexatalk.iss
; Output: installer\Output\HexaTalkSetup.exe
;
; NOTE: after install, HexaTalk updates ITSELF (see src/update_check.rs) —
; this installer is only needed for the first-time setup.

#define AppName "HexaTalk"
#define AppVersion "0.1.0"
#define AppPublisher "HexaTalk"
#define AppExeName "HexaTalk.exe"

[Setup]
AppId={{7E4A9C2D-3F1B-4A8E-9D5C-2B6F0E1A7C44}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
; Per-user install: no UAC prompt, no admin needed.
PrivilegesRequired=lowest
OutputDir=Output
OutputBaseFilename=HexaTalkSetup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; Close a running HexaTalk before overwriting its exe.
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no
UninstallDisplayIcon={app}\{#AppExeName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "polish"; MessagesFile: "compiler:Languages\Polish.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "autostart"; Description: "Launch HexaTalk when Windows starts"; GroupDescription: "Startup:"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion

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
