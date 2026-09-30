# Change the primary display's resolution.
#
# For measurements on a host with no physical monitor attached, which may sit
# at a low fallback mode (e.g. 1024x768): capture-rate figures (v1 design §7.1)
# mean nothing below the resolution being measured.
#
# MUST run in the interactive desktop session -- ChangeDisplaySettingsEx called
# from an SSH session (session 0) cannot affect the logged-on desktop. Drive it
# through spikes/run-interactive.ps1.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File tools\set-resolution.ps1 -Width 1920 -Height 1080
#   powershell -ExecutionPolicy Bypass -File tools\set-resolution.ps1 -ListOnly

param(
    [int]$Width = 1920,
    [int]$Height = 1080,
    [int]$RefreshHz = 0,   # 0 = leave the driver to pick
    [switch]$ListOnly
)

$ErrorActionPreference = "Stop"

# All of the marshalling lives in C# rather than PowerShell: DEVMODE has fixed
# char arrays and a union, and constructing it from PowerShell leaves the string
# fields null, which makes both Marshal.SizeOf and the call itself fail.
Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class Disp {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public ushort dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
        public uint dmFields;
        public int dmPositionX, dmPositionY;
        public uint dmDisplayOrientation, dmDisplayFixedOutput;
        public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public ushort dmLogPixels;
        public uint dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags,
                    dmDisplayFrequency, dmICMMethod, dmICMIntent, dmMediaType,
                    dmDitherType, dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DISPLAY_DEVICE {
        public int cb;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]  public string DeviceName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceString;
        public uint StateFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceID;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceKey;
    }

    [DllImport("user32.dll", CharSet = CharSet.Ansi)]
    static extern bool EnumDisplayDevices(string dev, uint devNum, ref DISPLAY_DEVICE d, uint flags);
    [DllImport("user32.dll", CharSet = CharSet.Ansi)]
    static extern int EnumDisplaySettings(string dev, int modeNum, ref DEVMODE dm);
    [DllImport("user32.dll", CharSet = CharSet.Ansi)]
    static extern int ChangeDisplaySettingsEx(string dev, ref DEVMODE dm, IntPtr hwnd,
                                              uint flags, IntPtr lParam);

    const int ENUM_CURRENT_SETTINGS = -1;
    const uint DM_PELSWIDTH = 0x80000, DM_PELSHEIGHT = 0x100000, DM_DISPLAYFREQUENCY = 0x400000;
    const uint ATTACHED_TO_DESKTOP = 0x1, PRIMARY_DEVICE = 0x4;

    static DEVMODE NewDevmode() {
        DEVMODE dm = new DEVMODE();
        dm.dmDeviceName = "";
        dm.dmFormName = "";
        dm.dmSize = (ushort)Marshal.SizeOf(typeof(DEVMODE));
        return dm;
    }

    /// Attached adapters, primary first.
    public static List<string> Adapters() {
        var attached = new List<string>();
        var primary = new List<string>();
        for (uint i = 0; ; i++) {
            DISPLAY_DEVICE d = new DISPLAY_DEVICE();
            d.cb = Marshal.SizeOf(typeof(DISPLAY_DEVICE));
            if (!EnumDisplayDevices(null, i, ref d, 0)) break;
            if ((d.StateFlags & ATTACHED_TO_DESKTOP) == 0) continue;
            if ((d.StateFlags & PRIMARY_DEVICE) != 0) primary.Add(d.DeviceName);
            else attached.Add(d.DeviceName);
        }
        primary.AddRange(attached);
        return primary;
    }

    public static string Describe(string dev) {
        DEVMODE dm = NewDevmode();
        if (EnumDisplaySettings(dev, ENUM_CURRENT_SETTINGS, ref dm) == 0)
            return dev + ": <EnumDisplaySettings failed>";
        return string.Format("{0}: {1}x{2} @ {3}Hz {4}bpp",
            dev, dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency, dm.dmBitsPerPel);
    }

    /// Returns the ChangeDisplaySettingsEx result code.
    public static int SetMode(string dev, uint w, uint h, uint hz) {
        DEVMODE dm = NewDevmode();
        if (EnumDisplaySettings(dev, ENUM_CURRENT_SETTINGS, ref dm) == 0) return -100;
        dm.dmPelsWidth = w;
        dm.dmPelsHeight = h;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        if (hz > 0) { dm.dmDisplayFrequency = hz; dm.dmFields |= DM_DISPLAYFREQUENCY; }
        return ChangeDisplaySettingsEx(dev, ref dm, IntPtr.Zero, 0, IntPtr.Zero);
    }
}
"@

$adapters = [Disp]::Adapters()
if ($adapters.Count -eq 0) { throw "No display adapters attached to the desktop." }

Write-Host "attached adapters (primary first):"
foreach ($a in $adapters) { Write-Host ("  " + [Disp]::Describe($a)) }

if ($ListOnly) { return }

$target = $adapters[0]
Write-Host ""
Write-Host "setting $target to ${Width}x${Height}..."
$rc = [Disp]::SetMode($target, $Width, $Height, $RefreshHz)
switch ($rc) {
    0    { Write-Host "OK" }
    1    { Write-Host "OK, but a restart is required" }
    -2   { throw "DISP_CHANGE_BADMODE: ${Width}x${Height} is not supported on $target" }
    -100 { throw "EnumDisplaySettings failed for $target" }
    default { throw "ChangeDisplaySettingsEx returned $rc" }
}
Write-Host ("now: " + [Disp]::Describe($target))
