param(
    [ValidateSet("release")]
    [string]$Profile = "release",
    [string]$Version
)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$TargetDir = if ($env:CARGO_TARGET_DIR) {
    if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $Root $env:CARGO_TARGET_DIR
    }
} else {
    Join-Path $Root "target"
}

if (-not $Version) {
    $PackageId = (& cargo pkgid --manifest-path (Join-Path $Root "Cargo.toml") -p kimi-subscription-router-gui)
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
    $Version = ($PackageId -split "@")[-1]
}

& (Join-Path $PSScriptRoot "package-windows.ps1") $Profile
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$ReleaseDir = Join-Path $TargetDir "dist/windows-x86_64"
$PortableDir = Join-Path $ReleaseDir "portable"
New-Item -ItemType Directory -Force -Path $PortableDir | Out-Null

$Gui = Join-Path $TargetDir "$Profile/Kimi Subscription Router.exe"
$Cli = Join-Path $TargetDir "$Profile/Kimi Subscription Router CLI.exe"
$Router = Join-Path $TargetDir "$Profile/kimi-subscription-router.exe"
foreach ($Path in @($Gui, $Cli, $Router)) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Missing Windows release binary: $Path"
    }
    Copy-Item -Force -LiteralPath $Path -Destination $PortableDir
}

$PortableZip = Join-Path $ReleaseDir "Kimi-Subscription-Router-$Version-Windows-x86_64-Portable.zip"
Compress-Archive -Path "$PortableDir/*" -DestinationPath $PortableZip -Force

$IsccCandidates = @(
    @(
        $env:ISCC_PATH,
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) }
)
if (-not $IsccCandidates) {
    throw "Inno Setup 6 compiler ISCC.exe was not found"
}
$Iscc = $IsccCandidates[0]
$InstallerScript = Join-Path $Root "packaging/windows/installer.iss"
& $Iscc "/DMyAppVersion=$Version" "/DMySourceDir=$PortableDir" "/DMyOutputDir=$ReleaseDir" $InstallerScript
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$Installer = Join-Path $ReleaseDir "Kimi-Subscription-Router-$Version-Windows-x86_64-Setup.exe"
foreach ($Path in @($PortableZip, $Installer)) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Missing Windows release package: $Path"
    }
}

Get-FileHash -Algorithm SHA256 -LiteralPath @($PortableZip, $Installer) |
    ForEach-Object { "$($_.Hash.ToLower())  $([System.IO.Path]::GetFileName($_.Path))" } |
    Set-Content -Encoding ascii -LiteralPath (Join-Path $ReleaseDir "SHA256SUMS-Windows-x86_64.txt")

Write-Output $PortableZip
Write-Output $Installer
