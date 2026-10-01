# Package Ping and Pong for Windows as a release: the programs built as one
# (PINGPONG_RELEASE=1: they then follow releases, not the main branch), in
# zip archives for a GitHub release and the website's downloads.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\package-windows.ps1 [-FFmpeg DIR]
#     -> target\dist\Ping-VERSION-windows-x86_64.zip  (Ping.exe beside FFmpeg's DLLs)
#        target\dist\Pong-VERSION-windows-x86_64.zip  (pong.exe, Pong Control.exe,
#                                                        install.ps1, Install Pong.cmd)
#
# VERSION is the workspace's. -FFmpeg (default: FFMPEG_DIR) and LLVM are what
# tools\build-ping-win.ps1 needs. The files sit at each archive's root, so
# Explorer's Extract All gives one folder. The programs are not signed:
# SmartScreen asks before Ping.exe first starts. The release workflow
# (.github/workflows/release.yml) runs this on a Windows runner.
param(
    [string]$FFmpeg = $env:FFMPEG_DIR
)
$ErrorActionPreference = 'Stop'
if (-not $FFmpeg) { throw 'Pass -FFmpeg DIR or set FFMPEG_DIR: an FFmpeg 8 shared build (include\, lib\, bin\).' }
$repo = Split-Path -Parent $PSScriptRoot
$manifest = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($manifest -notmatch '(?ms)^\[workspace\.package\].*?^version = "([^"]+)"') { throw 'no version in Cargo.toml [workspace.package]' }
$version = $Matches[1]
$env:FFMPEG_DIR = $FFmpeg
if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }
$env:PINGPONG_RELEASE = '1'

Push-Location $repo
try {
    # cargo's progress on stderr is not an error (Windows PowerShell makes
    # it one under 'Stop').
    $ErrorActionPreference = 'Continue'
    cargo build --release -p ping-app -p pong -p pong-app
    $ErrorActionPreference = 'Stop'
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }

    $release = Join-Path $repo 'target\release'
    $dist = Join-Path $repo 'target\dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    # zip NAME { fill the folder $args[0] }: the archive target\dist\NAME.zip.
    function zip([string]$name, [scriptblock]$fill) {
        $stage = Join-Path ([IO.Path]::GetTempPath()) $name
        if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
        New-Item -ItemType Directory $stage | Out-Null
        & $fill $stage
        $archive = Join-Path $dist "$name.zip"
        if (Test-Path $archive) { Remove-Item -Force $archive }
        [IO.Compression.ZipFile]::CreateFromDirectory($stage, $archive, 'Optimal', $false)
        Remove-Item -Recurse -Force $stage
        $sum = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLower()
        Write-Output "$sum  $name.zip"
    }

    zip "Ping-$version-windows-x86_64" {
        param($dir)
        Copy-Item (Join-Path $release 'ping-app.exe') (Join-Path $dir 'Ping.exe')
        # The DLLs tools\build-ping-win.ps1 puts beside Ping.exe: avcodec,
        # avutil, and swresample (some decoders).
        Get-ChildItem (Join-Path $FFmpeg 'bin') -Filter '*.dll' |
            Where-Object { $_.Name -match '^(avcodec|avutil|swresample)-\d+\.dll$' } |
            ForEach-Object { Copy-Item $_.FullName $dir }
        # FFmpeg is LGPL: its license goes with its DLLs.
        $license = Join-Path $FFmpeg 'LICENSE.txt'
        if (Test-Path $license) { Copy-Item $license (Join-Path $dir 'FFmpeg LICENSE.txt') }
    }

    zip "Pong-$version-windows-x86_64" {
        param($dir)
        Copy-Item (Join-Path $release 'pong.exe') $dir
        Copy-Item (Join-Path $release 'pong-app.exe') (Join-Path $dir 'Pong Control.exe')
        Copy-Item (Join-Path $PSScriptRoot 'host-deploy.ps1') (Join-Path $dir 'install.ps1')
        # What a person double-clicks: install.ps1 asks Windows for an
        # administrator itself.
        $cmd = "@echo off`r`nrem Install Pong from this folder: the host as PongService, its window in the Start menu.`r`n" +
            "powershell -NoProfile -ExecutionPolicy Bypass -File `"%~dp0install.ps1`"`r`n"
        [IO.File]::WriteAllText((Join-Path $dir 'Install Pong.cmd'), $cmd)
    }
} finally {
    Pop-Location
}
