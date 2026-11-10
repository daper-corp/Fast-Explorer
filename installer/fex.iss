; Fast Explorer installer. Build with installer\build.ps1 (passes /DAppVersion=...).
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Target
  #define Target "..\target"
#endif

[Setup]
AppId={{B7A3F0C2-5D0E-4B8E-9A55-FA57E9B10E01}
AppName=Fast Explorer
AppVersion={#AppVersion}
AppPublisher=DAPER
DefaultDirName={autopf}\Fast Explorer
DefaultGroupName=Fast Explorer
UninstallDisplayIcon={app}\fex.exe
OutputDir={#Target}\installer
OutputBaseFilename=FastExplorer-Setup-{#AppVersion}
; Admin: the index service runs as SYSTEM and must live in an admin-only folder.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes
CloseApplications=yes
RestartApplications=no
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"

[CustomMessages]
english.DesktopIcon=Create a desktop shortcut
english.IndexService=Install the index service for whole-disk search (indexes file names only)
english.DefaultExplorer=Use Fast Explorer as the default file manager (handles opening folders instead of Explorer)
english.InstallingIndex=Installing the index service...
english.RunApp=Launch Fast Explorer
korean.DesktopIcon=바탕 화면 바로 가기 만들기
korean.IndexService=전체 검색용 인덱스 서비스 설치 (파일 이름만 색인)
korean.DefaultExplorer=Fast Explorer를 기본 파일 탐색기로 사용 (폴더 열기를 Explorer 대신 처리)
korean.InstallingIndex=인덱스 서비스 설치 중...
korean.RunApp=Fast Explorer 실행

[Tasks]
Name: "desktopicon"; Description: "{cm:DesktopIcon}"; Flags: unchecked
Name: "indexsvc"; Description: "{cm:IndexService}"
Name: "defaultexplorer"; Description: "{cm:DefaultExplorer}"; Flags: unchecked

[Files]
Source: "{#Target}\release\fex.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Target}\release\fex-shell-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Target}\release\fex-index.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Fast Explorer"; Filename: "{app}\fex.exe"
Name: "{autodesktop}\Fast Explorer"; Filename: "{app}\fex.exe"; Tasks: desktopicon

[Registry]
; Default verb for folders and drives -> ours. Turning it off (or uninstalling) puts back Windows'
; default "none" (see [Code]). Win+E and the taskbar icon still open Explorer.
Root: HKLM; Subkey: "Software\Classes\Directory\shell\fex"; ValueType: string; ValueData: "Fast Explorer"; Flags: uninsdeletekey; Tasks: defaultexplorer
Root: HKLM; Subkey: "Software\Classes\Directory\shell\fex\command"; ValueType: string; ValueData: """{app}\fex.exe"" ""%1"""; Tasks: defaultexplorer
Root: HKLM; Subkey: "Software\Classes\Directory\shell"; ValueType: string; ValueData: "fex"; Tasks: defaultexplorer
Root: HKLM; Subkey: "Software\Classes\Drive\shell\fex"; ValueType: string; ValueData: "Fast Explorer"; Flags: uninsdeletekey; Tasks: defaultexplorer
Root: HKLM; Subkey: "Software\Classes\Drive\shell\fex\command"; ValueType: string; ValueData: """{app}\fex.exe"" ""%1"""; Tasks: defaultexplorer
Root: HKLM; Subkey: "Software\Classes\Drive\shell"; ValueType: string; ValueData: "fex"; Tasks: defaultexplorer

[Run]
Filename: "{app}\fex-index.exe"; Parameters: "install"; Flags: runhidden; Tasks: indexsvc; StatusMsg: "{cm:InstallingIndex}"
Filename: "{app}\fex.exe"; Description: "{cm:RunApp}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\fex-index.exe"; Parameters: "uninstall"; Flags: runhidden; RunOnceId: "RemoveIndexService"

[Code]
// An upgrade: stop and remove the running service first so its exe can be replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var Code: Integer;
begin
  if FileExists(ExpandConstant('{app}\fex-index.exe')) then
    Exec(ExpandConstant('{app}\fex-index.exe'), 'uninstall', '', SW_HIDE, ewWaitUntilTerminated, Code);
  // Error 5 (access denied) on DeleteFile means a copy is still running; the shell-host child and
  // extra windows are not always caught by CloseApplications, so end them here.
  Exec(ExpandConstant('{sys}	askkill.exe'), '/F /IM fex-shell-host.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}	askkill.exe'), '/F /IM fex.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(500);
  Result := '';
end;

// Hand folders back to Explorer: our verb key goes, and the default verb returns to "none".
procedure RestoreExplorer;
var Kind, Value: String; I: Integer;
begin
  for I := 0 to 1 do begin
    if I = 0 then Kind := 'Directory' else Kind := 'Drive';
    if RegQueryStringValue(HKLM, 'Software\Classes\' + Kind + '\shell', '', Value) and (Value = 'fex') then
      RegWriteStringValue(HKLM, 'Software\Classes\' + Kind + '\shell', '', 'none');
    RegDeleteKeyIncludingSubkeys(HKLM, 'Software\Classes\' + Kind + '\shell\fex');
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and not WizardIsTaskSelected('defaultexplorer') then RestoreExplorer;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then RestoreExplorer;
end;
