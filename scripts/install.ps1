#Requires -RunAsAdministrator
# Installs DeskCam: copies binaries to Program Files, registers the camera (HKLM), prepares
# C:\ProgramData\DeskCam, adds logon autostart and starts the app unelevated.
#
# Windows 11 registers the Media Foundation source (deskcam_source.dll). Windows 10 has no
# virtual camera API, so it registers the DirectShow capture filter (deskcam_dshow.dll) instead,
# in both its 64-bit and 32-bit builds because apps load it in-process.
# -Backend dshow forces the DirectShow filter on Windows 11 too (to test it).
param(
    [string]$BuildDir = "$PSScriptRoot\..\target\release",
    [string]$BuildDir32 = "$PSScriptRoot\..\target\i686-pc-windows-msvc\release",
    [ValidateSet('auto', 'mf', 'dshow')][string]$Backend = 'auto'
)
$ErrorActionPreference = 'Stop'

$Inst = "$env:ProgramFiles\DeskCam"
$Data = "$env:ProgramData\DeskCam"
$OsBuild = [int](Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').CurrentBuildNumber

if ($OsBuild -lt 18362) { throw "DeskCam needs Windows 10 version 1903 (build 18362) or newer; this is build $OsBuild." }
if (-not [Environment]::Is64BitOperatingSystem) { throw "DeskCam needs 64-bit Windows." }
if ($Backend -eq 'mf' -and $OsBuild -lt 22000) { throw "-Backend mf needs Windows 11; this is build $OsBuild." }
$UseDShow = $Backend -eq 'dshow' -or ($Backend -eq 'auto' -and $OsBuild -lt 22000)

$Required = @('deskcam.exe', $(if ($UseDShow) { 'deskcam_dshow.dll' } else { 'deskcam_source.dll' }))
foreach ($file in $Required) {
    if (-not (Test-Path "$BuildDir\$file")) { throw "$BuildDir\$file not found. Run 'cargo build --release' first." }
}
$Has32 = $UseDShow -and (Test-Path "$BuildDir32\deskcam_dshow.dll")
if ($UseDShow -and -not $Has32) {
    Write-Warning "32-bit filter not found in $BuildDir32; 32-bit apps won't list the camera. Build it with:"
    Write-Warning "  rustup target add i686-pc-windows-msvc; cargo build --release --target i686-pc-windows-msvc -p deskcam-dshow"
}

# Camera name from config.ini, used for the DirectShow registration (Windows 11 reads it at runtime).
$Name = 'DeskCam'
if (Test-Path "$Data\config.ini") {
    $line = Select-String -Path "$Data\config.ini" -Pattern '^\s*name\s*=\s*(.+?)\s*$' | Select-Object -First 1
    if ($line) { $Name = $line.Matches[0].Groups[1].Value }
}

function Invoke-Regsvr([string]$Exe, [string[]]$Arguments) {
    $p = Start-Process $Exe -ArgumentList $Arguments -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "$Exe $($Arguments -join ' ') failed: $($p.ExitCode)" }
}

# A DLL loaded by a running app (Discord, a browser...) can't be overwritten but can be renamed;
# the renamed copy is deleted on a later install once nothing uses it.
function Install-File([string]$Source, [string]$Dir) {
    New-Item -ItemType Directory -Force $Dir | Out-Null
    Get-ChildItem $Dir -Filter '*.old-*' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    $target = Join-Path $Dir (Split-Path $Source -Leaf)
    if (Test-Path $target) {
        try { Remove-Item $target -Force }
        catch { Rename-Item $target "$(Split-Path $target -Leaf).old-$([guid]::NewGuid().ToString('N'))" }
    }
    Copy-Item $Source $target -Force
}

# Stop a running instance so its files can be replaced.
if (Test-Path "$Inst\deskcam.exe") {
    & "$Inst\deskcam.exe" stop
    Start-Sleep -Seconds 2
}
Stop-Process -Name deskcam -Force -ErrorAction SilentlyContinue

if (-not $UseDShow) {
    # Unload any previously registered media source; both services restart on demand.
    Stop-Service FrameServerMonitor, FrameServer -Force -ErrorAction SilentlyContinue
}

New-Item -ItemType Directory -Force $Inst, $Data | Out-Null
Install-File "$BuildDir\deskcam.exe" $Inst

# Users edit config.ini and the app writes stream.bin; LocalService (Frame Server) and
# AppContainer consumers read them.
icacls $Data /grant '*S-1-5-32-545:(OI)(CI)M' '*S-1-5-19:(OI)(CI)RX' '*S-1-15-2-1:(OI)(CI)RX' | Out-Null
if ($LASTEXITCODE -ne 0) { throw "icacls failed: $LASTEXITCODE" }

if ($UseDShow) {
    # Drop a Windows 11 registration so apps don't list the camera twice.
    if (Test-Path "$Inst\deskcam_source.dll") {
        Invoke-Regsvr "$env:WINDIR\System32\regsvr32.exe" @('/u', '/s', "`"$Inst\deskcam_source.dll`"")
    }
    Install-File "$BuildDir\deskcam_dshow.dll" $Inst
    Invoke-Regsvr "$env:WINDIR\System32\regsvr32.exe" @('/s', '/n', "/i:`"$Name`"", "`"$Inst\deskcam_dshow.dll`"")
    if ($Has32) {
        Install-File "$BuildDir32\deskcam_dshow.dll" "$Inst\x86"
        Invoke-Regsvr "$env:WINDIR\SysWOW64\regsvr32.exe" @('/s', '/n', "/i:`"$Name`"", "`"$Inst\x86\deskcam_dshow.dll`"")
    }
    $Mode = "DirectShow camera '$Name' (restart apps that were open to see it)"
} else {
    foreach ($dll in "$Inst\deskcam_dshow.dll", "$Inst\x86\deskcam_dshow.dll") {
        if (Test-Path $dll) {
            $exe = if ($dll -like '*\x86\*') { "$env:WINDIR\SysWOW64\regsvr32.exe" } else { "$env:WINDIR\System32\regsvr32.exe" }
            Invoke-Regsvr $exe @('/u', '/s', "`"$dll`"")
        }
    }
    Install-File "$BuildDir\deskcam_source.dll" $Inst
    Invoke-Regsvr "$env:WINDIR\System32\regsvr32.exe" @('/s', "`"$Inst\deskcam_source.dll`"")
    $Mode = 'Windows 11 virtual camera'
}

Set-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name DeskCam -Value "`"$Inst\deskcam.exe`""

# explorer.exe launches the app with the user's unelevated token.
& explorer.exe "$Inst\deskcam.exe"

Write-Host "DeskCam installed to $Inst as $Mode"
Write-Host "Config: $Data\config.ini (tray menu > Restart after editing)"
Write-Host "Log:    $env:LOCALAPPDATA\DeskCam\deskcam.log"
