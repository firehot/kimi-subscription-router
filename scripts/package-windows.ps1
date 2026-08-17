param(
    [ValidateSet("release", "debug")]
    [string]$Profile = "release"
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

$CargoArgs = @(
    "build",
    "--manifest-path", (Join-Path $Root "Cargo.toml"),
    "-p", "kimi-subscription-router-gui",
    "-p", "kimi-switch-cli",
    "-p", "kimi-subscription-router"
)
if ($Profile -eq "release") {
    $CargoArgs += "--release"
}

& cargo @CargoArgs
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$Source = Join-Path $TargetDir "$Profile/kimi-subscription-router-gui.exe"
$Destination = Join-Path $TargetDir "$Profile/Kimi Subscription Router.exe"
$CliSource = Join-Path $TargetDir "$Profile/kimi-switch-cli.exe"
$CliDestination = Join-Path $TargetDir "$Profile/Kimi Subscription Router CLI.exe"
Copy-Item -Force $Source $Destination
Copy-Item -Force $CliSource $CliDestination
Write-Output $Destination
Write-Output $CliDestination
