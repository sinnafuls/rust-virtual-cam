; DeskCam installer (Inno Setup 6.5+).
;
; Installs the right camera for the running Windows:
;   Windows 11 (build 22000+): the Media Foundation virtual camera source (deskcam_source.dll)
;   Windows 10 (build 18362+): the DirectShow capture filter, 64-bit and 32-bit (deskcam_dshow.dll)
; Mirrors scripts/install.ps1 and scripts/uninstall.ps1.
;
; Build (after cargo release builds for x64 and i686):
;   ISCC.exe /DAppVersion=0.1.0 installer\deskcam.iss

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#ifndef BuildDir
  #define BuildDir "..\target\release"
#endif
#ifndef BuildDir32
  #define BuildDir32 "..\target\i686-pc-windows-msvc\release"
#endif

#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
AppId={{6E095512-BE9A-4490-8A21-CF843DFCC74C}
AppName=DeskCam
AppVersion={#AppVersion}
AppVerName=DeskCam {#AppVersion}
AppPublisher=DeskCam
AppPublisherURL=https://github.com/sinnafuls/rust-virtual-cam
AppSupportURL=https://github.com/sinnafuls/rust-virtual-cam/issues
AppUpdatesURL=https://github.com/sinnafuls/rust-virtual-cam/releases
; The camera DLL is loaded by Windows services and other users' apps: it must live in Program Files.
DefaultDirName={autopf}\DeskCam
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=no
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 version 1903: first build with the screen capture APIs DeskCam needs.
MinVersion=10.0.18362
; Apps that use the camera on Windows 10 keep the filter DLL loaded; offer to close them.
CloseApplications=yes
RestartApplications=no
OutputDir=..\target\installer
OutputBaseFilename=DeskCam-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\deskcam.exe
UninstallDisplayName=DeskCam
SetupLogging=yes

[Messages]
WelcomeLabel2=This will install [name/ver] on your computer.%n%nDeskCam adds a camera called "DeskCam" that shows your desktop, for Discord, OBS, browsers and other apps.%n%nIt runs in the background with a tray icon and only captures while an app is using the camera.

[Tasks]
Name: autostart; Description: "Start DeskCam automatically when I sign in"
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#BuildDir}\deskcam.exe"; DestDir: "{app}"; Flags: ignoreversion restartreplace
Source: "{#BuildDir}\deskcam_source.dll"; DestDir: "{app}"; Check: IsWin11; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "{#BuildDir}\deskcam_dshow.dll"; DestDir: "{app}"; Check: not IsWin11; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "{#BuildDir32}\deskcam_dshow.dll"; DestDir: "{app}\x86"; Check: not IsWin11; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Dirs]
; Settings survive uninstall.
Name: "{commonappdata}\DeskCam"; Flags: uninsneveruninstall
; Remove the program folders when empty, even ones an older scripts\install.ps1 created.
Name: "{app}"; Flags: uninsalwaysuninstall
Name: "{app}\x86"; Flags: uninsalwaysuninstall; Check: UsesX86Folder

[Icons]
Name: "{autoprograms}\DeskCam"; Filename: "{app}\deskcam.exe"; Comment: "Share your desktop as a camera"
Name: "{autodesktop}\DeskCam"; Filename: "{app}\deskcam.exe"; Comment: "Share your desktop as a camera"; Tasks: desktopicon

[Run]
; Users edit config.ini and the app writes stream.bin; LocalService (Frame Server), AppContainer
; and LPAC consumers read them.
Filename: "{sys}\icacls.exe"; Parameters: """{commonappdata}\DeskCam"" /grant *S-1-5-32-545:(OI)(CI)M *S-1-5-19:(OI)(CI)RX *S-1-15-2-1:(OI)(CI)RX *S-1-15-2-2:(OI)(CI)RX"; Flags: runhidden; StatusMsg: "Setting folder permissions..."
Filename: "{sys}\regsvr32.exe"; Parameters: "/s ""{app}\deskcam_source.dll"""; Check: IsWin11; Flags: runhidden; StatusMsg: "Registering the camera..."
Filename: "{sys}\regsvr32.exe"; Parameters: "/s /n /i:""{code:CameraName}"" ""{app}\deskcam_dshow.dll"""; Check: not IsWin11; Flags: runhidden; StatusMsg: "Registering the camera..."
Filename: "{syswow64}\regsvr32.exe"; Parameters: "/s /n /i:""{code:CameraName}"" ""{app}\x86\deskcam_dshow.dll"""; Check: not IsWin11; Flags: runhidden 32bit; StatusMsg: "Registering the camera for 32-bit apps..."
; Autostart goes into the signed-in user's own registry, not the elevated account's.
Filename: "{sys}\reg.exe"; Parameters: "add ""HKCU\{#RunKey}"" /v DeskCam /t REG_SZ /d ""\""{app}\deskcam.exe\"""" /f"; Tasks: autostart; Flags: runhidden runasoriginaluser
Filename: "{sys}\reg.exe"; Parameters: "delete ""HKCU\{#RunKey}"" /v DeskCam /f"; Tasks: not autostart; Flags: runhidden runasoriginaluser
Filename: "{app}\deskcam.exe"; Description: "Start DeskCam now"; Flags: postinstall nowait runasoriginaluser skipifsilent

[UninstallRun]
Filename: "{app}\deskcam.exe"; Parameters: "stop"; Flags: runhidden; RunOnceId: "StopApp"
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im deskcam.exe"; Flags: runhidden; RunOnceId: "KillApp"
Filename: "{sys}\regsvr32.exe"; Parameters: "/u /s ""{app}\deskcam_source.dll"""; Flags: runhidden; RunOnceId: "UnregSource"
Filename: "{sys}\regsvr32.exe"; Parameters: "/u /s ""{app}\deskcam_dshow.dll"""; Flags: runhidden; RunOnceId: "UnregDShow64"
Filename: "{syswow64}\regsvr32.exe"; Parameters: "/u /s ""{app}\x86\deskcam_dshow.dll"""; Flags: runhidden 32bit; RunOnceId: "UnregDShow32"
; Unload the media source from the camera services so its file can be deleted.
Filename: "{sys}\net.exe"; Parameters: "stop FrameServerMonitor /y"; Flags: runhidden; RunOnceId: "StopFSM"
Filename: "{sys}\net.exe"; Parameters: "stop FrameServer /y"; Flags: runhidden; RunOnceId: "StopFS"

[UninstallDelete]
Type: files; Name: "{commonappdata}\DeskCam\stream.bin"
Type: files; Name: "{commonappdata}\DeskCam\stream.bin.tmp"
; Renamed in-use DLLs left by scripts\install.ps1.
Type: files; Name: "{app}\*.old-*"
Type: files; Name: "{app}\x86\*.old-*"

[Code]
function IsWin11: Boolean;
var
  Version: TWindowsVersion;
begin
  GetWindowsVersionEx(Version);
  Result := Version.Build >= 22000;
end;

{ The 32-bit filter folder: Windows 10 installs, or left over from an earlier one. }
function UsesX86Folder: Boolean;
begin
  Result := (not IsWin11) or DirExists(ExpandConstant('{app}\x86'));
end;

function ConfigPath: String;
begin
  Result := ExpandConstant('{commonappdata}\DeskCam\config.ini');
end;

{ Camera name for the Windows 10 registration: `name =` from config.ini, else "DeskCam". }
function CameraName(Param: String): String;
var
  Lines: TArrayOfString;
  I, Eq: Integer;
  Line: String;
begin
  Result := 'DeskCam';
  if not LoadStringsFromFile(ConfigPath, Lines) then Exit;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    Line := Trim(Lines[I]);
    Eq := Pos('=', Line);
    if (Eq > 0) and (CompareText(Trim(Copy(Line, 1, Eq - 1)), 'name') = 0) then
    begin
      Line := Trim(Copy(Line, Eq + 1, Length(Line)));
      StringChangeEx(Line, '"', '', True);
      if Line <> '' then Result := Line;
      Exit;
    end;
  end;
end;

procedure RunHidden(const Exe, Params: String);
var
  Code: Integer;
begin
  Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

{ Stops the running app and unloads the camera DLL that is about to be replaced. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  App: String;
begin
  Result := '';
  App := ExpandConstant('{app}');
  if FileExists(App + '\deskcam.exe') then
  begin
    RunHidden(App + '\deskcam.exe', 'stop');
    Sleep(1500);
  end;
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/f /im deskcam.exe');

  if IsWin11 then
  begin
    { Drop a Windows 10 registration left from an older install so apps don't list DeskCam twice. }
    if FileExists(App + '\deskcam_dshow.dll') then
      RunHidden(ExpandConstant('{sys}\regsvr32.exe'), '/u /s "' + App + '\deskcam_dshow.dll"');
    if FileExists(App + '\x86\deskcam_dshow.dll') then
      RunHidden(ExpandConstant('{syswow64}\regsvr32.exe'), '/u /s "' + App + '\x86\deskcam_dshow.dll"');
    { The camera services keep the old media source loaded; they restart on demand. }
    RunHidden(ExpandConstant('{sys}\net.exe'), 'stop FrameServerMonitor /y');
    RunHidden(ExpandConstant('{sys}\net.exe'), 'stop FrameServer /y');
  end
  else if FileExists(App + '\deskcam_source.dll') then
    RunHidden(ExpandConstant('{sys}\regsvr32.exe'), '/u /s "' + App + '\deskcam_source.dll"');
end;

{ The installer always registers the camera for this Windows version, so the app must pick it too:
  reset a `backend = mf|dshow` override (e.g. from `install.ps1 -Backend`) to `auto`. }
procedure ResetBackendOverride;
var
  Lines: TArrayOfString;
  I, Eq: Integer;
  Changed: Boolean;
begin
  if not LoadStringsFromFile(ConfigPath, Lines) then Exit;
  Changed := False;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    Eq := Pos('=', Lines[I]);
    if (Eq > 0) and (CompareText(Trim(Copy(Lines[I], 1, Eq - 1)), 'backend') = 0)
      and (CompareText(Trim(Copy(Lines[I], Eq + 1, Length(Lines[I]))), 'auto') <> 0) then
    begin
      Lines[I] := 'backend = auto';
      Changed := True;
    end;
  end;
  if Changed then SaveStringsToUTF8FileWithoutBOM(ConfigPath, Lines, False);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then ResetBackendOverride;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  { Uninstall runs as the same (elevated) user who normally set the autostart value. }
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKCU, '{#RunKey}', 'DeskCam');
end;
