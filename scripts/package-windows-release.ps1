param(
    [ValidateSet("debug", "release")]
    [string]$Configuration = "release",
    [string]$Platform = "windows-x64"
)

$ErrorActionPreference = "Stop"
$serverRoot = Split-Path -Parent $PSScriptRoot
$cargoToml = Join-Path $serverRoot "Cargo.toml"
$version = Select-String -Path $cargoToml -Pattern '^version = "(.+)"$' | Select-Object -First 1

if (-not $version) {
    throw "Unable to resolve server version from Cargo.toml."
}

$packageVersion = $version.Matches[0].Groups[1].Value
$stageDir = Join-Path $serverRoot "dist\$Platform\$Configuration"
$exePath = Join-Path $stageDir "st-server.exe"

if (-not (Test-Path $exePath)) {
    throw "Windows stage directory is missing '$exePath'. Run build-windows-msvc.ps1 first."
}

$packageName = "st-server-v$packageVersion-$Platform"
$stagingRoot = Join-Path $serverRoot "dist\staging"
$packageRoot = Join-Path $stagingRoot $packageName
$archivePath = Join-Path $serverRoot "dist\$packageName.zip"

Remove-Item -Recurse -Force $packageRoot -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null

Copy-Item (Join-Path $stageDir "*") $packageRoot -Recurse -Force
@"
This archive contains the Windows x64 build of st-server.

The executable and FFmpeg runtime DLLs staged by build-windows-msvc.ps1 are included in this
package. No extra setup is required beyond the normal Windows desktop/runtime stack.

Set ST_SERVER_NO_TRAY=1 if you want to force headless mode.
"@ | Set-Content (Join-Path $packageRoot "README.txt")

Remove-Item $archivePath -Force -ErrorAction SilentlyContinue
Compress-Archive -Path $packageRoot -DestinationPath $archivePath -Force

Write-Host "Packaged Windows artifact at $archivePath"
