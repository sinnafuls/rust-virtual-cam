#Requires -RunAsAdministrator
# Removes DeskCam. Keeps C:\ProgramData\DeskCam\config.ini.
$ErrorActionPreference = 'Stop'

$Inst = "$env:ProgramFiles\DeskCam"
$Data = "$env:ProgramData\DeskCam"

if (Test-Path "$Inst\deskcam.exe") {
    & "$Inst\deskcam.exe" stop
    Start-Sleep -Seconds 2
}
Stop-Process -Name deskcam -Force -ErrorAction SilentlyContinue

Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name DeskCam -ErrorAction SilentlyContinue

if (Test-Path "$Inst\deskcam_source.dll") {
    $reg = Start-Process regsvr32.exe -ArgumentList '/u', '/s', "`"$Inst\deskcam_source.dll`"" -Wait -PassThru
    if ($reg.ExitCode -ne 0) { Write-Warning "regsvr32 /u failed: $($reg.ExitCode)" }
}

# Unload the DLL so the folder can be deleted.
Stop-Service FrameServerMonitor, FrameServer -Force -ErrorAction SilentlyContinue

Remove-Item $Inst -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item "$Data\stream.bin" -Force -ErrorAction SilentlyContinue

Write-Host "DeskCam removed. Settings kept in $Data\config.ini"
