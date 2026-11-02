; Windows installer for Lume, built with Inno Setup 6:
;   ISCC.exe installer\lume.iss
; It installs for the current user only, so it needs no administrator rights. User data
; (settings, login, library) lives elsewhere and is kept on uninstall.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{6F3A2E1C-8B4D-4C7A-9E21-5D0B7C3F9A14}
AppName=Lume
AppVersion={#AppVersion}
AppVerName=Lume {#AppVersion}
AppPublisher=Lume
DefaultDirName={localappdata}\Programs\Lume
DefaultGroupName=Lume
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
OutputDir=..\target\installer
OutputBaseFilename=Lume-Setup-{#AppVersion}
SetupIconFile=..\crates\ui\ui\lume.ico
UninstallDisplayIcon={app}\lume.exe
UninstallDisplayName=Lume
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; An open Lume is closed before its files are replaced.
CloseApplications=yes

[Languages]
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\lume.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Lume"; Filename: "{app}\lume.exe"
Name: "{autodesktop}\Lume"; Filename: "{app}\lume.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lume.exe"; Description: "{cm:LaunchProgram,Lume}"; Flags: nowait postinstall skipifsilent
