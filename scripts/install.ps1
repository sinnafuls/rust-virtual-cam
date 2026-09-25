#Requires -RunAsAdministrator
# Installs DeskCam: copies binaries to Program Files, registers the media source DLL (HKLM),
# prepares C:\ProgramData\DeskCam, adds logon autostart and starts the app unelevated.
param([string]$BuildDir = "$PSScriptRoot\..\target\release")
$ErrorActionPreference = 'Stop'

$Inst = "$env:ProgramFiles\DeskCam"
$Data = "$env:ProgramData\DeskCam"

foreach ($file in 'deskcam.exe', 'deskcam_source.dll') {
    if (-not (Test-Path "$BuildDir\$file")) { throw "$BuildDir\$file not found. Run 'cargo build --release' first." }
}

# Stop a running instance so its files can be replaced.
if (Test-Path "$Inst\deskcam.exe") {
    & "$Inst\deskcam.exe" stop
    Start-Sleep -Seconds 2
}
Stop-Process -Name deskcam -Force -ErrorAction SilentlyContinue

# Unload any previously registered DLL; both services restart on demand.
Stop-Service FrameServerMonitor, FrameServer -Force -ErrorAction SilentlyContinue

New-Item -ItemType Directory -Force $Inst, $Data | Out-Null
Copy-Item "$BuildDir\deskcam.exe", "$BuildDir\deskcam_source.dll" $Inst -Force

# Users edit config.ini and the app writes stream.bin; LocalService (Frame Server) and
# AppContainer consumers read them.
icacls $Data /grant '*S-1-5-32-545:(OI)(CI)M' '*S-1-5-19:(OI)(CI)RX' '*S-1-15-2-1:(OI)(CI)RX' | Out-Null
if ($LASTEXITCODE -ne 0) { throw "icacls failed: $LASTEXITCODE" }

$reg = Start-Process regsvr32.exe -ArgumentList '/s', "`"$Inst\deskcam_source.dll`"" -Wait -PassThru
if ($reg.ExitCode -ne 0) { throw "regsvr32 failed: $($reg.ExitCode)" }

Set-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name DeskCam -Value "`"$Inst\deskcam.exe`""

# explorer.exe launches the app with the user's unelevated token.
& explorer.exe "$Inst\deskcam.exe"

Write-Host "DeskCam installed to $Inst"
Write-Host "Config: $Data\config.ini (tray menu > Restart after editing)"
Write-Host "Log:    $env:LOCALAPPDATA\DeskCam\deskcam.log"
