# Run a command in the interactive console session and wait for it.
#
# SSH lands in session 0, where there is no desktop to duplicate and no display
# to configure. This registers a one-shot scheduled task that runs as the
# logged-on user in their session, waits for it to finish, and prints its
# output. Development helper; the installed host runs as a service instead.
#
#   powershell -File host-run.ps1 -Command '<cmd line>' [-TimeoutSec 120] [-NoWait] [-Limited]
#
# -Limited: not elevated (what the user's own programs get), e.g. to raise a
# UAC prompt.
param(
    [Parameter(Mandatory = $true)][string]$Command,
    [int]$TimeoutSec = 120,
    [switch]$NoWait,
    [switch]$Limited,
    [string]$Name = 'pingpong-dev-run'
)
$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:LOCALAPPDATA 'pingpong-dev'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$log = Join-Path $dir "$Name.log"
$cmdFile = Join-Path $dir "$Name.cmd"
Remove-Item $log -ErrorAction SilentlyContinue
Set-Content -Path $cmdFile -Encoding ASCII -Value "@echo off`r`n$Command > `"$log`" 2>&1"

$user = (Get-CimInstance Win32_ComputerSystem).UserName
if (-not $user) { throw 'nobody is logged on to the console' }
$action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c `"$cmdFile`""
$level = if ($Limited) { 'Limited' } else { 'Highest' }
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel $level
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Hours 0) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $Name -Action $action -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $Name
if ($NoWait) { "started $Name; log: $log"; return }

$deadline = (Get-Date).AddSeconds($TimeoutSec)
Start-Sleep -Milliseconds 500
while ((Get-ScheduledTask -TaskName $Name).State -eq 'Running') {
    if ((Get-Date) -gt $deadline) {
        Stop-ScheduledTask -TaskName $Name
        "TIMEOUT after $TimeoutSec s"
        break
    }
    Start-Sleep -Milliseconds 300
}
if (Test-Path $log) { Get-Content $log }
