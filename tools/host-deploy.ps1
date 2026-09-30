# Install (or update) the freshly built pong.exe as the PongService, and
# Pong's window (pong-app.exe, as "Pong Control.exe") with a Start menu entry.
#
#   powershell -File host-deploy.ps1 [-Source <path to pong.exe>] [-NoStart]
param(
    [string]$Source = (Join-Path $PSScriptRoot '..\target\release\pong.exe'),
    [string]$Window = (Join-Path $PSScriptRoot '..\target\release\pong-app.exe'),
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'
$dest = 'C:\Program Files\Pong'
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$svc = Get-Service PongService -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Stop-Service PongService -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}
# The host child may outlive a forced stop by a moment.
Get-Process pong -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Copy-Item -Force $Source (Join-Path $dest 'pong.exe')
if (Test-Path $Window) {
    Get-Process 'Pong Control' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Copy-Item -Force $Window (Join-Path $dest 'Pong Control.exe')
    # For everyone on this PC: Start menu, "Pong".
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut((Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Pong.lnk'))
    $link.TargetPath = Join-Path $dest 'Pong Control.exe'
    $link.WorkingDirectory = $dest
    $link.Save()
}

& (Join-Path $dest 'pong.exe') install
if ($NoStart) { Stop-Service PongService -Force }
Get-Service PongService | Format-Table Name, Status, StartType
