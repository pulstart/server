param(
    [ValidateSet("debug", "release")]
    [string]$Configuration = "release",
    [string]$Platform = "windows-x64",
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$FfmpegDir = $env:FFMPEG_DIR
)

$ErrorActionPreference = "Stop"
$serverRoot = Split-Path -Parent $PSScriptRoot

if (-not $FfmpegDir) {
    throw "Set FFMPEG_DIR or pass -FfmpegDir."
}

if (-not (Test-Path $FfmpegDir)) {
    throw "FFMPEG_DIR '$FfmpegDir' does not exist."
}

$profile = if ($Configuration -eq "release") { "release" } else { "debug" }
$stageDir = Join-Path $serverRoot "dist\$Platform\$Configuration"
$binaryPath = Join-Path $serverRoot "target\$Target\$profile\st-server.exe"

Push-Location $serverRoot
try {
    $env:FFMPEG_DIR = $FfmpegDir

    & rustup target add $Target
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $Target failed."
    }

    $cargoArgs = @("build", "--target", $Target, "--locked")
    if ($Configuration -eq "release") {
        $cargoArgs += "--release"
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed."
    }

    if (-not (Test-Path $binaryPath)) {
        throw "Built binary missing at '$binaryPath'."
    }

    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null
    Copy-Item $binaryPath $stageDir -Force

    $ffmpegBinDir = Join-Path $FfmpegDir "bin"
    if (Test-Path $ffmpegBinDir) {
        Get-ChildItem (Join-Path $ffmpegBinDir "*.dll") | Copy-Item -Destination $stageDir -Force
    } else {
        Write-Warning "FFmpeg bin directory missing at '$ffmpegBinDir'; runtime DLLs were not staged."
    }

    Write-Host "Staged Windows build at $stageDir"
} finally {
    Pop-Location
}
