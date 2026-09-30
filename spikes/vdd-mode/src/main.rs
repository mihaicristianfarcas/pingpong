//! Probe: can we put a virtual display into a requested mode on Windows 11 25H2?
//!
//! SCOPE CHANGE from the plan's Task 1. The plan assumed
//! `VirtualDrivers/Virtual-Display-Driver` driven by `ChangeDisplaySettingsEx`,
//! and asked whether that driver's issue #471 bites on 24H2/25H2. It does not
//! apply: the host already runs **SudoVDA** (SudoMaker Virtual Display Adapter,
//! installed by Apollo 0.4.6), which takes the mode as a *creation parameter*
//! over a private IOCTL. There is no mode-change call to fail.
//!
//! So this probe answers the questions that actually gate Phase 1:
//!   1. Can we open the SudoVDA control device and speak its protocol?
//!   2. Does ADD_VIRTUAL_DISPLAY produce a desktop-attached display at exactly
//!      the width/height/refresh we asked for?
//!   3. Does the watchdog really tear it down when we stop pinging?
//!   4. Can the virtual display be made PRIMARY? (If not, games launch on the
//!      physical monitor and are never captured -- a blocking finding.)
//!
//! Contract source: SudoMaker/SudoVDA `Common/Include/sudovda-ioctl.h` and
//! `Virtual Display Driver (HDR)/SudoVDA/Driver.cpp`, cross-checked against the
//! installed driver (oem35.inf, SudoVDA 1.10.9.289).
//!
//! MUST RUN IN THE INTERACTIVE DESKTOP SESSION. An SSH shell lands in session 0,
//! whose desktop shows a synthetic 1024x768 `WinDisc` -- display enumeration
//! there tells you nothing about the real desktop. Use spikes/run-interactive.ps1.
//!
//! Usage: vdd-mode.exe [width height refresh_mhz]

use std::ffi::c_void;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::DeviceIoControl;

// ---------------------------------------------------------------------------
// SudoVDA protocol. Mirrors sudovda-ioctl.h exactly.
// ---------------------------------------------------------------------------

/// {e5bcc234-1e0c-418a-a0d4-ef8b7501414d}
const SUVDA_INTERFACE_GUID: GUID = GUID::from_values(
    0xe5bcc234,
    0x1e0c,
    0x418a,
    [0xa0, 0xd4, 0xef, 0x8b, 0x75, 0x01, 0x41, 0x4d],
);

/// `CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0)`
/// expands to `(0x22 << 16) | (func << 2)`.
const fn ctl_code(function: u32) -> u32 {
    (0x22 << 16) | (function << 2)
}

const IOCTL_ADD_VIRTUAL_DISPLAY: u32 = ctl_code(0x800);
const IOCTL_REMOVE_VIRTUAL_DISPLAY: u32 = ctl_code(0x801);
const IOCTL_GET_WATCHDOG: u32 = ctl_code(0x803);
const IOCTL_DRIVER_PING: u32 = ctl_code(0x888);
const IOCTL_GET_PROTOCOL_VERSION: u32 = ctl_code(0x8FF);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AddParams {
    width: u32,
    height: u32,
    /// Driver does `if (VSync < 1000) VSync *= 1000`, so this field takes Hz
    /// OR millihertz. We always pass millihertz, which is what the wire
    /// protocol carries (spec 4.4).
    refresh: u32,
    monitor_guid: GUID,
    device_name: [i8; 14],
    serial_number: [i8; 14],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RemoveParams {
    monitor_guid: GUID,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct AddOut {
    adapter_luid_low: u32,
    adapter_luid_high: i32,
    target_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct WatchdogOut {
    timeout: u32,
    countdown: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct ProtocolVersion {
    major: u8,
    minor: u8,
    incremental: u8,
    test_build: bool,
}

/// Stable across runs on purpose: ADD_VIRTUAL_DISPLAY is idempotent by GUID, so
/// a fixed GUID means a restart re-attaches to the same monitor instead of
/// leaking a connector slot on every run.
const PINGPONG_MONITOR_GUID: GUID = GUID::from_values(
    0x7a3c1f2e,
    0x9b4d,
    0x4c8a,
    [0xa1, 0xe6, 0x5d, 0x2f, 0x8b, 0x0c, 0x3e, 0x91],
);

fn cstr14(s: &str) -> [i8; 14] {
    let mut out = [0i8; 14];
    for (i, b) in s.bytes().take(13).enumerate() {
        out[i] = b as i8;
    }
    out
}

// ---------------------------------------------------------------------------
// Device open
// ---------------------------------------------------------------------------

/// Find the SudoVDA control device path via SetupAPI and open it.
fn open_sudovda() -> Result<HANDLE, String> {
    unsafe {
        let devinfo = SetupDiGetClassDevsW(
            Some(&SUVDA_INTERFACE_GUID),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(|e| format!("SetupDiGetClassDevsW: {e}"))?;

        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        if SetupDiEnumDeviceInterfaces(devinfo, None, &SUVDA_INTERFACE_GUID, 0, &mut iface).is_err()
        {
            let _ = SetupDiDestroyDeviceInfoList(devinfo);
            return Err(
                "no SudoVDA interface present -- is the driver installed and started?".into(),
            );
        }

        // Two-call pattern: ask for the size, then fetch. The detail struct is
        // variable-length (a cbSize followed by the path), so it lives in a
        // byte buffer we align to the struct.
        let mut needed = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(devinfo, &iface, None, 0, Some(&mut needed), None);
        if needed == 0 {
            let _ = SetupDiDestroyDeviceInfoList(devinfo);
            return Err("SetupDiGetDeviceInterfaceDetailW reported zero size".into());
        }

        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

        let got =
            SetupDiGetDeviceInterfaceDetailW(devinfo, &iface, Some(detail), needed, None, None);
        let _ = SetupDiDestroyDeviceInfoList(devinfo);
        got.map_err(|e| format!("SetupDiGetDeviceInterfaceDetailW: {e}"))?;

        let path = PCWSTR((*detail).DevicePath.as_ptr());
        println!("device path: {}", path.to_string().unwrap_or_default());

        CreateFileW(
            path,
            (GENERIC_READ.0 | GENERIC_WRITE.0) as u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| format!("CreateFileW on the control device: {e}"))
    }
}

/// Thin `DeviceIoControl` wrapper: `In` in, `Out` out, both plain `repr(C)`.
fn ioctl<I: Copy, O: Copy + Default>(h: HANDLE, code: u32, input: Option<&I>) -> Result<O, String> {
    let mut out = O::default();
    let mut returned = 0u32;
    let (in_ptr, in_len) = match input {
        Some(i) => (
            i as *const I as *const c_void,
            std::mem::size_of::<I>() as u32,
        ),
        None => (std::ptr::null(), 0),
    };
    unsafe {
        DeviceIoControl(
            h,
            code,
            Some(in_ptr),
            in_len,
            Some(&mut out as *mut O as *mut c_void),
            std::mem::size_of::<O>() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| format!("DeviceIoControl(0x{code:08x}): {e}"))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Display enumeration
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Clone, Debug)]
struct Attached {
    name: String,
    string: String,
    primary: bool,
    width: u32,
    height: u32,
    hz: u32,
    /// Desktop position. Needed because making a display primary means moving
    /// EVERY display so the new primary sits at (0,0).
    x: i32,
    y: i32,
}

fn enumerate() -> Vec<Attached> {
    let mut out = Vec::new();
    let mut i = 0u32;
    loop {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        i += 1;
        if dd.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 == 0 {
            continue;
        }
        let name = String::from_utf16_lossy(&dd.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        let string = String::from_utf16_lossy(&dd.DeviceString)
            .trim_end_matches('\0')
            .to_string();
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let name_w = wide(&name);
        if unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
            .as_bool()
        {
            let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
            out.push(Attached {
                name,
                string,
                primary: dd.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0 != 0,
                width: dm.dmPelsWidth,
                height: dm.dmPelsHeight,
                hz: dm.dmDisplayFrequency,
                x: pos.x,
                y: pos.y,
            });
        }
    }
    out
}

fn print_displays(label: &str) -> Vec<Attached> {
    let list = enumerate();
    println!("=== {label} ===");
    for d in &list {
        println!(
            "  {}  primary={}  {}x{} @ {} Hz  [{}]",
            d.name, d.primary, d.width, d.height, d.hz, d.string
        );
    }
    list
}

/// Make `device` the primary display. Returns (staged return codes, apply code).
///
/// Windows will not do this in one call. The primary is BY DEFINITION the
/// display at (0,0), so every display has to be translated by the target's old
/// offset, each staged with `CDS_NORESET`, and then committed by a single
/// `ChangeDisplaySettingsExW(NULL, NULL, ...)`. Repositioning only the target
/// -- the obvious implementation -- returns DISP_CHANGE_SUCCESSFUL and does
/// nothing, which is a false negative that looks exactly like a driver
/// limitation.
fn set_primary(device: &str) -> (Vec<i32>, i32) {
    let displays = enumerate();
    let Some(target) = displays.iter().find(|d| d.name == device) else {
        return (vec![], -1);
    };
    let (dx, dy) = (target.x, target.y);

    let mut staged = Vec::new();
    for d in &displays {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmFields: DM_POSITION,
            ..Default::default()
        };
        dm.Anonymous1.Anonymous2.dmPosition = POINTL {
            x: d.x - dx,
            y: d.y - dy,
        };
        let flags = if d.name == device {
            CDS_SET_PRIMARY | CDS_UPDATEREGISTRY | CDS_NORESET
        } else {
            CDS_UPDATEREGISTRY | CDS_NORESET
        };
        let name_w = wide(&d.name);
        let rc = unsafe {
            ChangeDisplaySettingsExW(PCWSTR(name_w.as_ptr()), Some(&mut dm), None, flags, None).0
        };
        staged.push(rc);
    }

    // Commit everything staged above.
    let apply =
        unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None).0 };
    (staged, apply)
}

/// Make the display identified by `(luid, target_id)` primary via the CCD API.
///
/// This is the modern path -- what Settings and Apollo actually drive -- and
/// spec 6.3 marks it [P]. `ChangeDisplaySettingsExW(CDS_SET_PRIMARY)` returns
/// DISP_CHANGE_FAILED on an IddCx indirect display, so if anything can do this,
/// it is this.
///
/// "Primary" is the source positioned at (0,0), so this translates every active
/// source by the target's current offset and commits the whole topology at once.
fn set_primary_ccd(luid_low: u32, luid_high: i32, target_id: u32) -> Result<(), String> {
    unsafe {
        let mut n_paths = 0u32;
        let mut n_modes = 0u32;
        let rc = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes);
        if rc.is_err() {
            return Err(format!("GetDisplayConfigBufferSizes: {rc:?}"));
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        let rc = QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            None,
        );
        if rc.is_err() {
            return Err(format!("QueryDisplayConfig: {rc:?}"));
        }
        paths.truncate(n_paths as usize);
        modes.truncate(n_modes as usize);

        // Locate our virtual display by the LUID + target id the ADD ioctl gave us.
        let target_path = paths
            .iter()
            .find(|p| {
                p.targetInfo.adapterId.LowPart == luid_low
                    && p.targetInfo.adapterId.HighPart == luid_high
                    && p.targetInfo.id == target_id
            })
            .ok_or_else(|| {
                format!("no active path for luid {luid_high}:{luid_low} target {target_id}")
            })?;

        let idx = target_path.sourceInfo.Anonymous.modeInfoIdx;
        if idx == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
            return Err("target path has no source mode".into());
        }
        let pos = modes[idx as usize].Anonymous.sourceMode.position;
        let (dx, dy) = (pos.x, pos.y);
        if dx == 0 && dy == 0 {
            return Ok(()); // already at the origin: already primary
        }

        // Translate every source so ours lands on (0,0).
        for m in modes.iter_mut() {
            if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                m.Anonymous.sourceMode.position.x -= dx;
                m.Anonymous.sourceMode.position.y -= dy;
            }
        }

        let rc = SetDisplayConfig(
            Some(&paths),
            Some(&modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
        );
        if rc != 0 {
            return Err(format!("SetDisplayConfig returned {rc}"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------

/// `cleanup`: remove our monitor and hand primary back to a real display.
/// `arrival W H MHZ`: add, then log the display list every 250ms so we can see
/// WHEN it arrives and in WHAT mode -- Windows may restore a saved
/// per-monitor configuration instead of honouring the requested one.
fn subcommand(cmd: &str, args: &[String]) {
    let h_dev = match open_sudovda() {
        Ok(h) => h,
        Err(e) => {
            println!("FAIL: {e}");
            return;
        }
    };

    let physical: Vec<Attached> = enumerate()
        .into_iter()
        .filter(|d| !d.string.contains("SudoMaker"))
        .collect();

    // Always start clean: remove our monitor, put primary back on a real one.
    let remove = RemoveParams {
        monitor_guid: PINGPONG_MONITOR_GUID,
    };
    let rc = ioctl::<RemoveParams, u8>(h_dev, IOCTL_REMOVE_VIRTUAL_DISPLAY, Some(&remove));
    println!("cleanup REMOVE: {:?}", rc.map(|_| "ok"));
    std::thread::sleep(std::time::Duration::from_millis(2000));
    if let Some(p) = physical.first() {
        let (s, a) = set_primary(&p.name);
        println!("cleanup primary -> {}: staged={s:?} apply={a}", p.name);
    }
    std::thread::sleep(std::time::Duration::from_millis(1000));
    print_displays("after cleanup");

    if cmd == "cleanup" {
        unsafe {
            let _ = CloseHandle(h_dev);
        }
        return;
    }

    let w: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(1920);
    let h: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1080);
    let mhz: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60_000);

    let before = enumerate();
    println!("\n=== ADD {w}x{h} @ {mhz} mHz ===");
    let params = AddParams {
        width: w,
        height: h,
        refresh: mhz,
        monitor_guid: PINGPONG_MONITOR_GUID,
        device_name: cstr14("pingpong"),
        serial_number: cstr14("pp000001"),
    };
    match ioctl::<AddParams, AddOut>(h_dev, IOCTL_ADD_VIRTUAL_DISPLAY, Some(&params)) {
        Ok(o) => println!(
            "ADD ok: luid={}:{} target_id={}",
            o.adapter_luid_high, o.adapter_luid_low, o.target_id
        ),
        Err(e) => {
            println!("ADD FAILED: {e}");
            unsafe {
                let _ = CloseHandle(h_dev);
            }
            return;
        }
    }

    let start = std::time::Instant::now();
    let mut last = String::new();
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let _ = ioctl::<(), u8>(h_dev, IOCTL_DRIVER_PING, None);
        let now = enumerate();
        let snapshot = now
            .iter()
            .map(|d| {
                format!(
                    "{}:{}x{}@{}{}",
                    d.name,
                    d.width,
                    d.height,
                    d.hz,
                    if d.primary { "*" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        if snapshot != last {
            println!("  t={:>6.2}s  {snapshot}", start.elapsed().as_secs_f32());
            last = snapshot;
        }
        let arrived = now.iter().any(|d| !before.iter().any(|b| b.name == d.name));
        if arrived && start.elapsed().as_secs_f32() > 3.0 {
            break;
        }
    }

    // Windows restores a SAVED configuration for this monitor identity, which
    // overrides the mode ADD asked for. Can we force it back afterwards?
    let target = enumerate()
        .into_iter()
        .find(|d| !before.iter().any(|b| b.name == d.name));
    if let Some(t) = target {
        println!(
            "\n=== forcing mode on {} (currently {}x{}@{}) ===",
            t.name, t.width, t.height, t.hz
        );
        let want_hz = if mhz >= 1000 { mhz / 1000 } else { mhz };
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmPelsWidth: w,
            dmPelsHeight: h,
            dmDisplayFrequency: want_hz,
            dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
            ..Default::default()
        };
        let name_w = wide(&t.name);
        let rc = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name_w.as_ptr()),
                Some(&mut dm),
                None,
                CDS_UPDATEREGISTRY,
                None,
            )
            .0
        };
        println!("ChangeDisplaySettingsExW(mode) returned {rc} (0 = success)");
        std::thread::sleep(std::time::Duration::from_millis(2000));
        let _ = ioctl::<(), u8>(h_dev, IOCTL_DRIVER_PING, None);
        let after_mode = enumerate().into_iter().find(|d| d.name == t.name);
        match after_mode {
            Some(a) if a.width == w && a.height == h && a.hz == want_hz => println!(
                "VERIFIED: mode forced to {}x{}@{} after arrival",
                a.width, a.height, a.hz
            ),
            Some(a) => println!(
                "STILL WRONG: {}x{}@{} (wanted {w}x{h}@{want_hz})",
                a.width, a.height, a.hz
            ),
            None => println!("display vanished"),
        }
    }

    println!("\n=== teardown ===");
    if let Some(p) = physical.first() {
        let _ = set_primary(&p.name);
    }
    let _ = ioctl::<RemoveParams, u8>(h_dev, IOCTL_REMOVE_VIRTUAL_DISPLAY, Some(&remove));
    std::thread::sleep(std::time::Duration::from_millis(1500));
    print_displays("final");

    unsafe {
        let _ = CloseHandle(h_dev);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1) {
        if cmd == "cleanup" || cmd == "arrival" {
            subcommand(cmd, &args[2..]);
            return;
        }
    }
    let (w, h, mhz) = if args.len() >= 4 {
        (
            args[1].parse().unwrap(),
            args[2].parse().unwrap(),
            args[3].parse().unwrap(),
        )
    } else {
        (2560u32, 1440u32, 120_000u32)
    };

    let before = print_displays("attached displays BEFORE");
    let before_primary = before.iter().find(|d| d.primary).map(|d| d.name.clone());

    let h_dev = match open_sudovda() {
        Ok(h) => h,
        Err(e) => {
            println!("FAIL: could not open SudoVDA: {e}");
            return;
        }
    };
    println!("opened SudoVDA control device");

    // Q1: do we speak the protocol?
    match ioctl::<(), ProtocolVersion>(h_dev, IOCTL_GET_PROTOCOL_VERSION, None) {
        Ok(v) => println!(
            "protocol version {}.{}.{} test_build={}",
            v.major, v.minor, v.incremental, v.test_build
        ),
        Err(e) => println!("WARN: GET_PROTOCOL_VERSION failed: {e}"),
    }
    match ioctl::<(), WatchdogOut>(h_dev, IOCTL_GET_WATCHDOG, None) {
        Ok(wd) => println!(
            "watchdog timeout={}s countdown={}s",
            wd.timeout, wd.countdown
        ),
        Err(e) => println!("WARN: GET_WATCHDOG failed: {e}"),
    }

    // Q2: does ADD give us exactly the mode we asked for?
    println!("\n=== requesting {w}x{h} @ {mhz} mHz ===");
    let params = AddParams {
        width: w,
        height: h,
        refresh: mhz,
        monitor_guid: PINGPONG_MONITOR_GUID,
        device_name: cstr14("pingpong"),
        serial_number: cstr14("pp000001"),
    };
    let added: AddOut = match ioctl(h_dev, IOCTL_ADD_VIRTUAL_DISPLAY, Some(&params)) {
        Ok(o) => o,
        Err(e) => {
            println!("FAIL: ADD_VIRTUAL_DISPLAY: {e}");
            unsafe {
                let _ = CloseHandle(h_dev);
            }
            return;
        }
    };
    println!(
        "ADD ok: adapter_luid={}:{} target_id={}",
        added.adapter_luid_high, added.adapter_luid_low, added.target_id
    );

    // The monitor arrives asynchronously (IddCx monitor arrival + desktop
    // reconfigure). Ping while we wait -- the watchdog is 3s by default and
    // ANY ioctl except GET_WATCHDOG resets it.
    let mut found = None;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = ioctl::<(), u8>(h_dev, IOCTL_DRIVER_PING, None);
        let now = enumerate();
        if let Some(d) = now
            .iter()
            .find(|d| !before.iter().any(|b| b.name == d.name))
        {
            found = Some(d.clone());
            break;
        }
    }

    let after = print_displays("attached displays AFTER add");

    match &found {
        Some(d) => {
            println!("\nnew display: {} [{}]", d.name, d.string);
            let want_hz = if mhz >= 1000 { mhz / 1000 } else { mhz };
            if d.width == w && d.height == h && d.hz == want_hz {
                println!(
                    "VERIFIED: virtual display came up at exactly {w}x{h} @ {want_hz} Hz \
                     -- mode is a CREATION parameter, no ChangeDisplaySettingsEx needed"
                );
            } else {
                println!(
                    "MISMATCH: asked {w}x{h} @ {want_hz} Hz, got {}x{} @ {} Hz",
                    d.width, d.height, d.hz
                );
            }
        }
        None => println!("\nFAIL: no new display appeared within 10s of ADD"),
    }

    // Q4: can it be primary? Blocking if not -- games would launch on the
    // physical monitor and never be captured.
    if let Some(d) = &found {
        println!("\n=== primary test ===");
        let (staged, apply) = set_primary(&d.name);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = ioctl::<(), u8>(h_dev, IOCTL_DRIVER_PING, None);
        let now = enumerate();
        let is_primary = now.iter().any(|x| x.name == d.name && x.primary);
        println!("staged rcs={staged:?} apply rc={apply} (0 = success)");
        for x in &now {
            println!("  {} primary={} at ({},{})", x.name, x.primary, x.x, x.y);
        }
        let mut is_primary = is_primary;
        if is_primary {
            println!("VERIFIED: primary via ChangeDisplaySettingsEx(CDS_SET_PRIMARY)");
        } else {
            // Spec 6.3 [P]: the CCD path is what Settings and Apollo drive.
            println!("ChangeDisplaySettingsEx could not do it; trying the CCD path");
            match set_primary_ccd(
                added.adapter_luid_low,
                added.adapter_luid_high,
                added.target_id,
            ) {
                Ok(()) => {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let _ = ioctl::<(), u8>(h_dev, IOCTL_DRIVER_PING, None);
                    is_primary = enumerate().iter().any(|x| x.name == d.name && x.primary);
                    if is_primary {
                        println!("VERIFIED: the virtual display CAN be made primary, via SetDisplayConfig");
                    } else {
                        println!("BLOCKING: SetDisplayConfig succeeded but the display is still not primary");
                    }
                }
                Err(e) => println!("BLOCKING: CCD path failed too: {e}"),
            }
            for x in &enumerate() {
                println!("  {} primary={} at ({},{})", x.name, x.primary, x.x, x.y);
            }
        }
        // Put the original primary back before we tear anything down.
        if let Some(p) = &before_primary {
            if p != &d.name {
                let (s2, a2) = set_primary(p);
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let restored = enumerate().iter().any(|x| &x.name == p && x.primary);
                println!("restored primary to {p}: staged={s2:?} apply={a2} ok={restored}");
            }
        }
    }

    // Q3: is the watchdog real? The driver decrements a GLOBAL countdown once a
    // second and calls DisconnectAllMonitors at zero; every ioctl except
    // GET_WATCHDOG resets it. GET_WATCHDOG is therefore the only way to observe
    // the countdown without disturbing it. If it never reaches 0, some OTHER
    // process is pinging the driver -- ApolloService is the obvious suspect,
    // and relying on it would be a silent dependency on Apollo staying alive.
    println!("\n=== watchdog test: polling GET_WATCHDOG (non-resetting) for 8s ===");
    let mut reaped_at = None;
    for i in 0..16 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let wd = ioctl::<(), WatchdogOut>(h_dev, IOCTL_GET_WATCHDOG, None).unwrap_or_default();
        let alive = enumerate()
            .iter()
            .any(|x| found.as_ref().is_some_and(|f| f.name == x.name));
        println!(
            "  t={:.1}s countdown={} display_alive={}",
            (i + 1) as f32 * 0.5,
            wd.countdown,
            alive
        );
        if !alive && reaped_at.is_none() {
            reaped_at = Some((i + 1) as f32 * 0.5);
            break;
        }
    }
    match reaped_at {
        Some(t) => println!(
            "VERIFIED: watchdog reaped the display after {t}s without a ping \
             -- a keepalive thread is MANDATORY"
        ),
        None => println!(
            "watchdog did NOT reap within 8s. If countdown above never fell to 0, \
             another process (ApolloService) is resetting it -- implement the \
             keepalive anyway rather than depend on that."
        ),
    }

    // Teardown.
    let remove = RemoveParams {
        monitor_guid: PINGPONG_MONITOR_GUID,
    };
    match ioctl::<RemoveParams, u8>(h_dev, IOCTL_REMOVE_VIRTUAL_DISPLAY, Some(&remove)) {
        Ok(_) => println!("\nREMOVE ok"),
        Err(e) => println!("\nREMOVE failed (already reaped?): {e}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let final_list = print_displays("attached displays AFTER remove");

    let clean = final_list.len() == before.len()
        && final_list
            .iter()
            .all(|d| before.iter().any(|b| b.name == d.name));
    println!(
        "\nteardown clean: {clean} (before={}, after={})",
        before.len(),
        final_list.len()
    );
    let _ = after;

    unsafe {
        let _ = CloseHandle(h_dev);
    }
}
