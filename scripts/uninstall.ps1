#Requires -RunAsAdministrator
# Removes DeskCam (Windows 11 media source and/or Windows 10 DirectShow filter).
# Keeps C:\ProgramData\DeskCam\config.ini.
$ErrorActionPreference = 'Stop'

$Inst = "$env:ProgramFiles\DeskCam"
$Data = "$env:ProgramData\DeskCam"

if (Test-Path "$Inst\deskcam.exe") {
    & "$Inst\deskcam.exe" stop
    Start-Sleep -Seconds 2
}
Stop-Process -Name deskcam -Force -ErrorAction SilentlyContinue

Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name DeskCam -ErrorAction SilentlyContinue

$Registered = @(
    @("$env:WINDIR\System32\regsvr32.exe", "$Inst\deskcam_source.dll"),
    @("$env:WINDIR\System32\regsvr32.exe", "$Inst\deskcam_dshow.dll"),
    @("$env:WINDIR\SysWOW64\regsvr32.exe", "$Inst\x86\deskcam_dshow.dll")
)
foreach ($entry in $Registered) {
    $exe, $dll = $entry
    if (Test-Path $dll) {
        $reg = Start-Process $exe -ArgumentList '/u', '/s', "`"$dll`"" -Wait -PassThru
        if ($reg.ExitCode -ne 0) { Write-Warning "regsvr32 /u $dll failed: $($reg.ExitCode)" }
    }
}

# Unload the media source so the folder can be deleted.
Stop-Service FrameServerMonitor, FrameServer -Force -ErrorAction SilentlyContinue

Remove-Item $Inst -Recurse -Force -ErrorAction SilentlyContinue
if (Test-Path $Inst) {
    # The DirectShow filter stays loaded in apps that used the camera until they exit.
    Write-Warning "Some files in $Inst are still in use (close apps that used the camera, then delete the folder)."
}
Remove-Item "$Data\stream.bin" -Force -ErrorAction SilentlyContinue

Write-Host "DeskCam removed. Settings kept in $Data\config.ini"
