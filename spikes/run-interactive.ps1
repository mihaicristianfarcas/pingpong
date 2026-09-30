# Run a command in the INTERACTIVE desktop session and stream back its output.
#
# WHY THIS EXISTS: an SSH session on Windows lands in session 0 (services).
# Windows Graphics Capture requires a real desktop session, so any WGC code
# started over SSH fails with `ItemConvertFailed` before capturing anything.
# This wraps the command in a scheduled task registered with `/it`
# (interactive), which runs it in the logged-on user's session instead.
#
# Anything touching WGC -- the cuda-interop spike, pingpong-capture, the server
# binary -- must be launched this way when driven remotely.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File run-interactive.ps1 `
#       -WorkDir C:\path\to\dir -Exe .\target\release\thing.exe [-TimeoutSec 120]

param(
    [Parameter(Mandatory = $true)][string]$WorkDir,
    [Parameter(Mandatory = $true)][string]$Exe,
    [string]$Arguments = "",
    [int]$TimeoutSec = 120,
    [string]$TaskName = "pingpong-interactive-run"
)

$ErrorActionPreference = "Stop"
$log = Join-Path $WorkDir "interactive-run.log"
if (Test-Path $log) { Remove-Item $log -Force }

# cmd wrapper so stdout and stderr both land in the log.
$inner = "cd /d `"$WorkDir`" && `"$Exe`" $Arguments > `"$log`" 2>&1"

schtasks /create /tn $TaskName /tr "cmd /c $inner" /sc once /st 00:00 /ru $env:USERNAME /it /f | Out-Null
try {
    schtasks /run /tn $TaskName | Out-Null

    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    do {
        Start-Sleep -Milliseconds 500
        $status = (schtasks /query /tn $TaskName /fo list | Select-String "Status:").ToString()
        $running = $status -match "Running"
    } while ($running -and (Get-Date) -lt $deadline)

    if ($running) {
        Write-Host "=== TIMED OUT after ${TimeoutSec}s; killing task ==="
        schtasks /end /tn $TaskName | Out-Null
    }
} finally {
    schtasks /delete /tn $TaskName /f | Out-Null
}

Write-Host "=== output from interactive session ==="
if (Test-Path $log) { Get-Content $log } else { Write-Host "(no output produced)" }
