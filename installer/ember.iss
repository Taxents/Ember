; Ember's Windows installer (Inno Setup 6.3+ or 7). Builds EmberSetup.exe.
;
; Installs per user into %LOCALAPPDATA%\Programs\Ember, with no admin prompt, because the app
; updates itself by swapping ember.exe in place and so needs a folder the user can write to.
; Adds a Start menu entry (which Windows Search finds), an optional desktop icon and an entry
; in Settings > Apps that uninstalls it. Running a newer EmberSetup.exe over an install just
; replaces the files.
;
; Built by .github/workflows/release.yml:
;   ISCC /DAppVersion=1.2.3 /DExePath=C:\path\to\ember.exe /O<output dir> installer\ember.iss
; Both defines are optional for a local test build (paths are relative to this folder).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef ExePath
  #define ExePath "..\target\release\ember.exe"
#endif

[Setup]
; Never change AppId: it is how Windows and Setup recognise an existing install to upgrade.
AppId={{75470392-1290-427A-8DE7-8618F43DF2C5}
AppName=Ember
AppVersion={#AppVersion}
AppVerName=Ember {#AppVersion}
AppPublisher=Taxents
AppPublisherURL=https://github.com/Taxents/Ember
AppSupportURL=https://github.com/Taxents/Ember
AppUpdatesURL=https://github.com/Taxents/Ember/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName=Ember

; Per user, no UAC prompt; the folder stays writable for the self-updater.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\Ember
DisableProgramGroupPage=yes
; Fixed location: elsewhere (e.g. Program Files) a per-user install can't write, and the
; self-updater needs to.
DisableDirPage=yes
UsePreviousAppDir=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; Settings > Apps entry.
UninstallDisplayName=Ember
UninstallDisplayIcon={app}\ember.exe

SetupIconFile=..\app\icons\ember.ico
OutputBaseFilename=EmberSetup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

; A running Ember holds ember.exe open: close it (Restart Manager) before replacing it. It is
; started again by the [Run] entry below, not by Restart Manager.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#ExePath}"; DestDir: "{app}"; DestName: "ember.exe"; Flags: ignoreversion

[InstallDelete]
; Left behind by the in-app updater; harmless, but stale after a reinstall.
Type: files; Name: "{app}\ember.exe.old"
Type: files; Name: "{app}\ember.exe.new"

[Icons]
Name: "{userprograms}\Ember"; Filename: "{app}\ember.exe"; WorkingDir: "{app}"
Name: "{userdesktop}\Ember"; Filename: "{app}\ember.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\ember.exe"; Description: "{cm:LaunchProgram,Ember}"; Flags: postinstall nowait skipifsilent

[UninstallRun]
; The uninstaller doesn't close running apps itself; stop the installed Ember (only that one,
; not other copies such as a dev build) so its exe can be removed.
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -NonInteractive -Command ""Get-Process ember -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{app}\ember.exe' } | Stop-Process -Force"""; Flags: runhidden; RunOnceId: "StopEmber"

[UninstallDelete]
; The updater's leftovers, which the uninstaller didn't install and so wouldn't remove.
Type: files; Name: "{app}\ember.exe.old"
Type: files; Name: "{app}\ember.exe.new"
Type: dirifempty; Name: "{app}"

[Code]
// Settings, sign-in and cache live outside the install folder (%LOCALAPPDATA%\Ember and
// %APPDATA%\Ember; MusicApp before the rename, if an old copy couldn't move them). They are
// kept unless the user says otherwise; a silent uninstall always keeps them.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
  begin
    if MsgBox('Also delete your Ember settings, sign-in and cached data?' + #13#10 + #13#10 +
              'Choose No to keep them for a later reinstall.',
              mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      DelTree(ExpandConstant('{localappdata}\Ember'), True, True, True);
      DelTree(ExpandConstant('{userappdata}\Ember'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\MusicApp'), True, True, True);
      DelTree(ExpandConstant('{userappdata}\MusicApp'), True, True, True);
    end;
  end;
end;
