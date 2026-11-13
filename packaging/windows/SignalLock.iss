; SignalLock — Inno Setup installer script (Rust app, ADR-014).
; Adapted from the legacy windows/installer/SignalLock.iss.
;
; Build (from the app/ directory, after `cargo build --release`):
;   iscc /DMyAppVersion=<workspace version> packaging\windows\SignalLock.iss
; The CI release workflow passes the workspace version; the fallback below only
; matters when ISCC is invoked by hand for an ad-hoc test.
; Output: dist\SignalLock-<version>-setup.exe (paths in this file are relative
; to the directory of this script).

#ifndef MyAppVersion
  #define MyAppVersion "1.0.0"
#endif

#define MyAppName     "SignalLock"
#define MyAppPublisher "SignalLock"
#define MyAppURL      "https://signallock.app"
#define MyAppExeName  "SignalLock.exe"
; Cargo's output name for the binary crate; installed as MyAppExeName.
#define MyAppBuiltExe "signallock.exe"

[Setup]
; Immutable — never regenerate. Identifies upgrades / uninstalls of this product;
; unchanged from 0.1.0 so 1.0.0 upgrades an existing install in place.
AppId={{56B5F70A-5A1E-40B5-B9C0-31C437B72F54}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
VersionInfoVersion={#MyAppVersion}

DefaultDirName={autopf}\{#MyAppName}
DisableProgramGroupPage=yes
DisableDirPage=no
AllowNoIcons=yes

; Wizard asks user up-front: "Install for me only" (no UAC, %LocalAppData%)
; vs "Install for all users" (UAC, Program Files). {autopf} resolves
; correctly for either choice.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

OutputDir=..\..\dist
OutputBaseFilename=SignalLock-{#MyAppVersion}-setup
SetupIconFile=SignalLock.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}

WizardStyle=modern
Compression=lzma2/ultra
SolidCompression=yes
LZMAUseSeparateProcess=yes

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763

; If SignalLock is running, prompt the user to close it before continuing.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "turkish"; MessagesFile: "compiler:Languages\Turkish.isl"

[Files]
; Build pipeline: `cargo build --release` produces a single self-contained
; signallock.exe in target\release; it is installed as SignalLock.exe.
Source: "..\..\target\release\{#MyAppBuiltExe}"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"

[Run]
; Final-page checkbox (default checked, hidden in /SILENT mode).
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
; In-app update (ADR-015): the running app starts this installer with
; /SILENT ... /SignalLockUpdate=1 and exits, so nothing else would start the
; new version. This entry does, and only for that case: a plain silent install
; (no /SignalLockUpdate=1) still does not launch the app. runasoriginaluser keeps
; an all-users (elevated) install from starting the app elevated.
Filename: "{app}\{#MyAppExeName}"; Flags: nowait runasoriginaluser; Check: IsUpdateInstall

[UninstallRun]
; Remove the auto-start registry entry SignalLock may have written.
; Failure is fine (entry may not exist) — runhidden + don't break uninstall.
Filename: "{cmd}"; Parameters: "/c reg delete ""HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"" /v ""{#MyAppName}"" /f"; Flags: runhidden

[Code]
// True for the silent install the app itself starts to update itself (ADR-015).
function IsUpdateInstall(): Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:SignalLockUpdate|0}') = '1');
end;

function ConfirmDataDeletion(): Boolean;
var
  Msg: string;
begin
  if ActiveLanguage = 'turkish' then
    Msg :=
      '%APPDATA%\{#MyAppName} klasörü ayarlarınızı, güvenilir cihaz kaydınızı' #13#10 +
      've günlük dosyalarını barındırıyor.' #13#10 #13#10 +
      'Bu klasörü de silmek ister misiniz?' #13#10 #13#10 +
      'İleride yeniden kurarsanız ayarlarınızın korunmasını istiyorsanız "Hayır"a tıklayın.'
  else
    Msg :=
      'The %APPDATA%\{#MyAppName} folder holds your settings, trusted device' #13#10 +
      'and log files.' #13#10 #13#10 +
      'Do you want to delete this folder as well?' #13#10 #13#10 +
      'Click "No" to keep your settings for a future reinstall.';

  Result := MsgBox(Msg, mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AppData: string;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    AppData := ExpandConstant('{userappdata}\{#MyAppName}');
    if DirExists(AppData) and ConfirmDataDeletion() then
      DelTree(AppData, True, True, True);
  end;
end;
