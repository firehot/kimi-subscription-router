#ifndef MyAppVersion
  #error MyAppVersion must be provided
#endif

#ifndef MySourceDir
  #error MySourceDir must be provided
#endif

#ifndef MyOutputDir
  #error MyOutputDir must be provided
#endif

[Setup]
AppId={{0C9082E7-95DC-4CFB-AC73-66A0A71D53E1}
AppName=Kimi Subscription Router
AppVersion={#MyAppVersion}
AppPublisher=firehot
AppPublisherURL=https://github.com/firehot/kimi-subscription-router
AppSupportURL=https://github.com/firehot/kimi-subscription-router/issues
AppUpdatesURL=https://github.com/firehot/kimi-subscription-router/releases
DefaultDirName={localappdata}\Programs\Kimi Subscription Router
DefaultGroupName=Kimi Subscription Router
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir={#MyOutputDir}
OutputBaseFilename=Kimi-Subscription-Router-{#MyAppVersion}-Windows-x86_64-Setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\Kimi Subscription Router.exe
CloseApplications=yes
RestartApplications=no

[Files]
Source: "{#MySourceDir}\Kimi Subscription Router.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MySourceDir}\Kimi Subscription Router CLI.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MySourceDir}\kimi-subscription-router.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Kimi Subscription Router"; Filename: "{app}\Kimi Subscription Router.exe"
Name: "{group}\Kimi Subscription Router CLI"; Filename: "{app}\Kimi Subscription Router CLI.exe"
Name: "{userdesktop}\Kimi Subscription Router"; Filename: "{app}\Kimi Subscription Router.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Run]
Filename: "{app}\Kimi Subscription Router.exe"; Description: "Launch Kimi Subscription Router"; Flags: nowait postinstall skipifsilent
