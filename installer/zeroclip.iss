; ZeroClip installer (Inno Setup 6). Built by .github/workflows/release.yml:
;   iscc /DAppVersion=0.1.3 /DStage=..\stage /DOutDir=..\dist installer\zeroclip.iss
; Stage holds zeroclip.exe, LICENSE, README.md and the ffmpeg files, the same as the zip.
;
; Per-user install (no admin) into %LOCALAPPDATA%\Programs\ZeroClip, because the app updates
; itself by renaming its own exe and unpacks ffmpeg beside it; both need a folder the user can
; write. Settings and clips live elsewhere (src/paths.rs) and are left alone on uninstall.

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z
#endif
#ifndef Stage
  #define Stage "..\stage"
#endif
#ifndef OutDir
  #define OutDir "..\dist"
#endif

[Setup]
; Never change AppId: it is how an install finds the previous one to upgrade.
AppId={{485F77F4-D627-4A6A-BA47-B230A7567BB3}
AppName=ZeroClip
AppVersion={#AppVersion}
AppPublisher=ZeroClip
AppPublisherURL=https://github.com/zeroclip-dev/zeroclip
DefaultDirName={userpf}\ZeroClip
DefaultGroupName=ZeroClip
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
OutputDir={#OutDir}
OutputBaseFilename=ZeroClip-v{#AppVersion}-setup-x64
SetupIconFile=..\assets\brand\zeroclip.ico
UninstallDisplayIcon={app}\zeroclip.exe
LicenseFile={#Stage}\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Close a running ZeroClip (it holds its exe open) and start it again afterwards.
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#Stage}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\ZeroClip"; Filename: "{app}\zeroclip.exe"
Name: "{autodesktop}\ZeroClip"; Filename: "{app}\zeroclip.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\zeroclip.exe"; Description: "Start ZeroClip"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; The app's tray keeps it running; stop it so its files can be removed.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM zeroclip.exe"; Flags: runhidden; RunOnceId: "StopZeroClip"

[UninstallDelete]
; Written beside the exe after install: ffmpeg fetched on first start, and updater leftovers.
Type: files; Name: "{app}\ffmpeg.exe"
Type: files; Name: "{app}\ffprobe.exe"
Type: files; Name: "{app}\ffplay.exe"
Type: files; Name: "{app}\LICENSE-ffmpeg.txt"
Type: files; Name: "{app}\ffmpeg-download.zip"
Type: filesandordirs; Name: "{app}\ffmpeg-download"
Type: files; Name: "{app}\zeroclip.exe.old"
Type: files; Name: "{app}\zeroclip.exe.new"
Type: dirifempty; Name: "{app}"

[Registry]
; "Start with Windows" (crates/zc-win/src/autostart.rs) writes this value; drop it on uninstall.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "ZeroClip"; Flags: uninsdeletevalue dontcreatekey
