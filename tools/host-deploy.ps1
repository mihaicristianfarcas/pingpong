# Install (or update) the freshly built pong.exe as the PongService, and
# Pong's window (pong-app.exe, as "Pong Control.exe") with a Start menu entry
# and its icon in the notification area from the next sign-in on (for the
# user running this; "Show Pong's icon at login" in the window turns it off).
#
#   powershell -File host-deploy.ps1 [-Source <path to pong.exe>] [-NoStart]
#
# In a release archive (tools/package-windows.ps1) this script is
# install.ps1, beside pong.exe and Pong Control.exe, and takes them from
# there; in a checkout, from target\release.
param(
    [string]$Source = $(if (Test-Path (Join-Path $PSScriptRoot 'pong.exe')) { Join-Path $PSScriptRoot 'pong.exe' }
        else { Join-Path $PSScriptRoot '..\target\release\pong.exe' }),
    [string]$Window = $(if (Test-Path (Join-Path $PSScriptRoot 'Pong Control.exe')) { Join-Path $PSScriptRoot 'Pong Control.exe' }
        else { Join-Path $PSScriptRoot '..\target\release\pong-app.exe' }),
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'
# A service is installed by an administrator. From a PowerShell that is not
# one (the release archive's "Install Pong.cmd"), start again as one, which
# Windows asks the user to allow, in a window that stays open to show how it
# went.
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $again = '-NoProfile -ExecutionPolicy Bypass -NoExit -File "{0}" -Source "{1}" -Window "{2}"' -f $PSCommandPath, $Source, $Window
    if ($NoStart) { $again += ' -NoStart' }
    Start-Process powershell -Verb RunAs -ArgumentList $again
    return
}
$dest = 'C:\Program Files\Pong'
New-Item -ItemType Directory -Force -Path $dest | Out-Null

# Windows holds a program's file for a moment after its process is killed,
# and a copy straight after fails ("being used by another process"): the
# installer stops a program, waits for it to be gone, and copies over it
# with a few tries.
function Stop-Program([string]$Name) {
    $running = @(Get-Process $Name -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) { return }
    $running | Stop-Process -Force -ErrorAction SilentlyContinue
    $running | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
}
function Copy-Program([string]$From, [string]$To) {
    for ($try = 1; ; $try++) {
        try { Copy-Item -Force $From $To; return }
        catch { if ($try -ge 20) { throw }; Start-Sleep -Milliseconds 250 }
    }
}

$svc = Get-Service PongService -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Stop-Service PongService -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}
try {
    # The host child may outlive a forced stop by a moment.
    Stop-Program pong
    Copy-Program $Source (Join-Path $dest 'pong.exe')
    if (Test-Path $Window) {
        Stop-Program 'Pong Control'
        Copy-Program $Window (Join-Path $dest 'Pong Control.exe')
    }
} catch {
    # Leave the host running, on whichever pong.exe is in place, rather than
    # stopped until someone notices.
    if ($svc) { Start-Service PongService -ErrorAction SilentlyContinue }
    throw
}
# A download's files carry the internet's mark, and SmartScreen would
# question Pong's window at its first start: the copies are trusted as this
# installer is.
Unblock-File (Join-Path $dest 'pong.exe')
if (Test-Path $Window) {
    $control = Join-Path $dest 'Pong Control.exe'
    Unblock-File $control
    # For everyone on this PC: Start menu, "Pong".
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut((Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Pong.lnk'))
    $link.TargetPath = $control
    $link.WorkingDirectory = $dest
    $link.Save()
    # Pong's icon at sign-in, without its window: this user's Run key, the
    # value the window's own setting reads and writes. Not started from here:
    # this shell is elevated, and the icon is the signed-in user's.
    Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Pong' -Value ('"{0}" --background' -f $control)
}

& (Join-Path $dest 'pong.exe') install
if ($NoStart) { Stop-Service PongService -Force }
Get-Service PongService | Format-Table Name, Status, StartType
