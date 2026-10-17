# Builds a portable ZIP; does not install or register a camera. Rust + MSVC Build Tools required.
param([string]$OutputDir = "$PSScriptRoot\..\target\diagnostics")
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
Push-Location $Root
try {
    rustup target add i686-pc-windows-msvc
    if ($LASTEXITCODE) { throw "rustup failed: $LASTEXITCODE" }
    cargo build --release --locked -p deskcam-diagnostics
    if ($LASTEXITCODE) { throw "x64 diagnostics build failed: $LASTEXITCODE" }
    cargo build --release --locked --target i686-pc-windows-msvc -p deskcam-diagnostics
    if ($LASTEXITCODE) { throw "x86 diagnostics build failed: $LASTEXITCODE" }
    $OutputDir = [IO.Path]::GetFullPath($OutputDir)
    $Bundle = Join-Path $OutputDir 'DeskCam-Diagnostics'
    New-Item -ItemType Directory -Force "$Bundle\x86" | Out-Null
    Copy-Item 'target\release\deskcam-diagnostics.exe' "$Bundle\deskcam-diagnostics.exe" -Force
    Copy-Item 'target\i686-pc-windows-msvc\release\deskcam-diagnostics.exe' "$Bundle\x86\deskcam-diagnostics.exe" -Force
    $Zip = Join-Path $OutputDir 'DeskCam-Diagnostics.zip'
    Compress-Archive -Path $Bundle -DestinationPath $Zip -Force
    "$((Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower())  DeskCam-Diagnostics.zip" |
        Set-Content (Join-Path $OutputDir 'SHA256SUMS.txt') -Encoding ascii
    Write-Host "Send $Zip. Extract all files, then run DeskCam-Diagnostics\deskcam-diagnostics.exe as the affected user."
} finally {
    Pop-Location
}
