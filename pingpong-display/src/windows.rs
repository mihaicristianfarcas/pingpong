//! Windows display control over SudoVDA (v2 design §6).
//!
//! Five things here are load-bearing and look removable. Read
//! `spikes/vdd-mode/README.md` before simplifying any of them:
//!
//! 1. **Primary is set with the CCD API, not `ChangeDisplaySettingsEx`.**
//!    `CDS_SET_PRIMARY` returns `DISP_CHANGE_FAILED` on an indirect display
//!    while succeeding for a physical one — so the obvious call looks like it
//!    works right up until it silently doesn't.
//! 2. **A keepalive thread pings the driver every second.** SudoVDA reaps every
//!    monitor after ~3 s without an ioctl. Measured: the display vanishes 2 s
//!    after the last ping.
//! 3. **A mode change is REMOVE-then-ADD.** ADD is idempotent by monitor GUID
//!    and returns the *existing* monitor even when the requested mode differs,
//!    so an ADD alone would silently serve the old mode.
//! 4. **The mode is forced again after the monitor attaches.** Windows persists
//!    a configuration against the monitor's EDID identity and reapplies it over
//!    the one ADD asked for — measured: a request for 1920x1080@60 came up as
//!    2560x1440@120. A plain mode change *does* work here; only `CDS_SET_PRIMARY`
//!    does not.
//! 5. **Our display is found by CCD identity, never by diffing GDI names.**
//!    Removal is asynchronous, so on a mode change the outgoing display is still
//!    in any "before" snapshot and a name diff never observes the new one.

use crate::state::{self, SavedState};
use crate::sudovda::Device;
use crate::{ActiveDisplay, DisplayControl, DisplayError, DisplayMode};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Devices::Display::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    EXECUTION_STATE,
};

/// Ping interval. The watchdog is 3 s and decrements once a second, so this has
/// two whole intervals of slack before anything is reaped.
const PING_EVERY: Duration = Duration::from_secs(1);

/// How long to poll for an added monitor to attach to the desktop, not
/// counting the time the display calls themselves block (see
/// `wait_for_arrival`). Measured from under 1 s to 3.9 s on the test host
/// (Apollo's takes ~3 s there too); this is a ceiling, not a typical wait.
const ARRIVAL_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the wait looks.
const ARRIVAL_POLL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// CCD helpers
// ---------------------------------------------------------------------------

/// A display's CCD identity: adapter LUID plus target id.
///
/// This is what survives re-enumeration. GDI names (`\\.\DISPLAY1`) do not —
/// they are positional and shuffle when displays come and go, which is exactly
/// what happens here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CcdId {
    pub adapter_low: u32,
    pub adapter_high: i32,
    pub target_id: u32,
}

fn query_config(
) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), DisplayError> {
    query_config_with(QDC_ONLY_ACTIVE_PATHS)
}

fn query_config_with(
    flags: QUERY_DISPLAY_CONFIG_FLAGS,
) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), DisplayError> {
    unsafe {
        let mut n_paths = 0u32;
        let mut n_modes = 0u32;
        GetDisplayConfigBufferSizes(flags, &mut n_paths, &mut n_modes)
            .ok()
            .map_err(|e| DisplayError::Os(format!("GetDisplayConfigBufferSizes: {e}")))?;

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        QueryDisplayConfig(
            flags,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .map_err(|e| DisplayError::Os(format!("QueryDisplayConfig: {e}")))?;

        paths.truncate(n_paths as usize);
        modes.truncate(n_modes as usize);
        Ok((paths, modes))
    }
}

/// The CCD identity of whichever display is currently primary.
///
/// "Primary" is defined as the source positioned at (0,0); there is no flag for
/// it in the CCD world.
pub fn primary_id() -> Option<CcdId> {
    let (paths, modes) = query_config().ok()?;
    for p in &paths {
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx };
        if idx == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
            continue;
        }
        let m = modes.get(idx as usize)?;
        let pos = unsafe { m.Anonymous.sourceMode.position };
        if pos.x == 0 && pos.y == 0 {
            return Some(CcdId {
                adapter_low: p.targetInfo.adapterId.LowPart,
                adapter_high: p.targetInfo.adapterId.HighPart,
                target_id: p.targetInfo.id,
            });
        }
    }
    None
}

/// The GDI device name (`\\.\DISPLAY5`) for a CCD target, if it is attached.
///
/// Attach the calling thread to the desktop that has input.
///
/// Windows changes display settings only for a thread on the input desktop:
/// from another, `ChangeDisplaySettingsExW` returns DISP_CHANGE_FAILED. After
/// sleep the host sits at the lock screen, whose desktop is Winlogon's, and
/// every session failed so (seen after a Wake-on-LAN wake).
/// Opening Winlogon's desktop takes SYSTEM, which the service is. The thread
/// must own no windows or hooks: the session manager's does not.
fn follow_input_desktop() {
    use windows::Win32::Foundation::GENERIC_ALL;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
        DF_ALLOWOTHERACCOUNTHOOK,
    };
    unsafe {
        match OpenInputDesktop(
            DF_ALLOWOTHERACCOUNTHOOK,
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        ) {
            Ok(desk) => {
                if let Err(e) = SetThreadDesktop(desk) {
                    tracing::debug!(error = %e, "could not move to the input desktop");
                }
                let _ = CloseDesktop(desk);
            }
            Err(e) => tracing::debug!(error = %e, "cannot open the input desktop"),
        }
    }
}

/// Identity-based lookup, deliberately. Detecting our display by diffing the
/// attached-name list against a "before" snapshot looks simpler and is wrong:
/// removal is asynchronous, so during a mode change the outgoing display is
/// still listed when the snapshot is taken, and no new name ever appears.
pub fn gdi_name_for_target(id: CcdId) -> Option<String> {
    let (paths, _) = query_config().ok()?;
    let path = paths.iter().find(|p| {
        p.targetInfo.adapterId.LowPart == id.adapter_low
            && p.targetInfo.adapterId.HighPart == id.adapter_high
            && p.targetInfo.id == id.target_id
    })?;

    let mut req = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
        return None;
    }
    let name = String::from_utf16_lossy(&req.viewGdiDeviceName)
        .trim_end_matches('\0')
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Make `id` the primary display.
///
/// Translates every active source so `id`'s lands on (0,0), then commits the
/// whole topology at once. Doing it per-display, or without the translation,
/// is what makes `ChangeDisplaySettingsEx` appear to succeed while changing
/// nothing.
pub fn set_primary(id: CcdId) -> Result<(), DisplayError> {
    let (paths, mut modes) = query_config()?;

    let target = paths
        .iter()
        .find(|p| {
            p.targetInfo.adapterId.LowPart == id.adapter_low
                && p.targetInfo.adapterId.HighPart == id.adapter_high
                && p.targetInfo.id == id.target_id
        })
        .ok_or(DisplayError::NoSuchDisplay)?;

    let idx = unsafe { target.sourceInfo.Anonymous.modeInfoIdx };
    if idx == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        return Err(DisplayError::NoSuchDisplay);
    }
    let pos = unsafe { modes[idx as usize].Anonymous.sourceMode.position };
    let (dx, dy) = (pos.x, pos.y);
    if dx == 0 && dy == 0 {
        return Ok(()); // already at the origin, so already primary
    }

    for m in modes.iter_mut() {
        if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            unsafe {
                m.Anonymous.sourceMode.position.x -= dx;
                m.Anonymous.sourceMode.position.y -= dy;
            }
        }
    }

    let rc = unsafe {
        SetDisplayConfig(
            Some(&paths),
            Some(&modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(DisplayError::Os(format!("SetDisplayConfig returned {rc}")))
    }
}

/// What ONE named display is set to right now, as Windows reports it.
///
/// Polled by the session loop so the session can follow a mode change it did
/// not make. Three things do that: a game taking exclusive fullscreen (CS2
/// moved a 3600x2260 display to 3840x2160 and froze the stream), someone
/// changing the refresh rate in Windows settings while watching, and someone
/// changing the resolution there -- which on this virtual display also moves
/// the *rate*, because the mode list at 120 Hz does not contain every size the
/// list at 60 Hz does.
///
/// **Asked of a specific display, never of "the primary".** An earlier version
/// passed a NULL device name, meaning "the default display", on the reasoning
/// that `activate` makes the virtual display primary. That reasoning holds only
/// while the session's isolation holds, and the moment this exists to detect is
/// exactly the moment it does not: a topology reapply puts the host's own
/// monitor back at (0,0), and the NULL query then reports the PHYSICAL
/// display's mode. The session would retune itself to a display it is not
/// capturing.
pub fn mode_for_target(id: CcdId) -> Option<DisplayMode> {
    let name = gdi_name_for_target(id)?;
    let name_w = wide(&name);
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: dmSize is set and `name_w` is a NUL-terminated device name that
    // `gdi_name_for_target` just read back from the CCD path.
    if !unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
        .as_bool()
    {
        return None;
    }
    Some(DisplayMode {
        width: dm.dmPelsWidth as u16,
        height: dm.dmPelsHeight as u16,
        // dmDisplayFrequency is whole Hz. 59.94 comes back as 59 or 60 and the
        // session's own rounding (`encode_fps_for`) absorbs the difference.
        refresh_mhz: dm.dmDisplayFrequency * 1000,
    })
}

/// Leave `keep` as the only active display, deactivating every other path.
///
/// **Why a session owns the whole desktop.** Making the virtual display primary
/// is not enough: the physical displays stay attached, so apps still open on
/// them, and any app that restores its last window position goes straight back
/// there. From the client that looks like windows escaping to a monitor you
/// cannot see. Apollo deactivates the other outputs for the duration of a
/// session, which is exactly why it does not have this problem.
///
/// Returns the topology as it was, to be handed back to [`apply_topology`] on
/// teardown. Restoring the saved paths verbatim is what puts the arrangement
/// back -- positions and all -- rather than approximating it with "extend".
fn isolate_to(keep: CcdId) -> Result<Topology, DisplayError> {
    let (paths, modes) = query_config()?;
    let saved = Topology {
        paths: paths.clone(),
        modes: modes.clone(),
    };

    let mut next = paths.clone();
    let mut kept = false;
    for p in next.iter_mut() {
        let is_ours = p.targetInfo.adapterId.LowPart == keep.adapter_low
            && p.targetInfo.adapterId.HighPart == keep.adapter_high
            && p.targetInfo.id == keep.target_id;
        if is_ours {
            kept = true;
        } else {
            p.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
        }
    }
    // Refuse rather than apply a topology with NOTHING active: that is a desktop
    // with no displays at all, and on a headless host there would be no way to
    // get it back without a remote shell.
    if !kept {
        return Err(DisplayError::NoSuchDisplay);
    }

    // SDC_SAVE_TO_DATABASE is deliberately NOT set here, unlike `set_primary`.
    // This arrangement is the session's, not the user's, and persisting it would
    // teach Windows to reapply a single-display topology the next time these
    // monitors are seen -- long after the session is over.
    let rc = unsafe {
        SetDisplayConfig(
            Some(&next),
            Some(&modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    };
    if rc != 0 {
        return Err(DisplayError::Os(format!(
            "SetDisplayConfig(isolate) returned {rc}"
        )));
    }
    Ok(saved)
}

/// What [`WindowsDisplay::reassert`] found, and what the caller must do about
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reassert {
    /// The desktop is still the session's alone. Nothing to do.
    Held,
    /// Something had changed it and it has been put back.
    Reasserted,
    /// The session's display is no longer on the desktop, so there is nothing
    /// left to assert anything about. Only a fresh `activate` recovers.
    DisplayLost,
}

/// How many display paths Windows currently has active.
///
/// One, during a session, because `isolate_to` deactivated the rest. More than
/// one means something put them back -- see [`WindowsDisplay::reassert`].
pub fn active_path_count() -> usize {
    query_config().map(|(paths, _)| paths.len()).unwrap_or(1)
}

/// The active paths and modes at a point in time.
#[derive(Clone)]
struct Topology {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Whether a monitor of the host's own is connected (whether or not a
/// session has switched it off): a target on another adapter than the
/// virtual display's with a monitor plugged into it.
fn host_has_own_display(vdd: CcdId) -> bool {
    let Ok((paths, _)) = query_config_with(QDC_ALL_PATHS) else {
        return true;
    };
    paths.iter().any(|p| {
        let on_vdd = p.targetInfo.adapterId.LowPart == vdd.adapter_low
            && p.targetInfo.adapterId.HighPart == vdd.adapter_high;
        !on_vdd && p.targetInfo.targetAvailable.as_bool() && monitor_plugged_in(p)
    })
}

/// Whether a monitor is behind this path's target: Windows gives a connected
/// monitor a device path. The placeholders it keeps when the last display
/// goes are "available" too, but have none: seen for a GPU output whose
/// monitor was just unplugged, and for a departed virtual display.
fn monitor_plugged_in(p: &DISPLAYCONFIG_PATH_INFO) -> bool {
    let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: p.targetInfo.adapterId,
            id: p.targetInfo.id,
        },
        ..Default::default()
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
        return true; // unknown: restoring is the safe mistake
    }
    req.monitorDevicePath[0] != 0
}

fn apply_topology(t: &Topology) -> Result<(), DisplayError> {
    let rc = unsafe {
        SetDisplayConfig(
            Some(&t.paths),
            Some(&t.modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(DisplayError::Os(format!(
            "SetDisplayConfig(restore topology) returned {rc}"
        )))
    }
}

/// Turn every attached display back on, without a saved topology to work from.
///
/// The crash path: a killed server leaves the host's monitors dark and its
/// in-memory `Topology` is gone with it. Windows keeps its own database of
/// arrangements: first the one it has for the monitors connected now (the
/// user's own, as the isolation was never saved to it), then the remembered
/// extended desktop. Extend alone fails on a host with a single monitor
/// (error 31), where the database's arrangement is the one that works.
fn reactivate_all_displays() -> Result<(), DisplayError> {
    let current = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT) };
    if current == 0 {
        return Ok(());
    }
    let rc = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) };
    if rc == 0 {
        Ok(())
    } else {
        Err(DisplayError::Os(format!(
            "SetDisplayConfig: database arrangement returned {current}, extend {rc}"
        )))
    }
}

// ---------------------------------------------------------------------------
// GDI enumeration (only for "has the monitor turned up yet?")
// ---------------------------------------------------------------------------

/// A display attached to the desktop, as GDI sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attached {
    name: String,
    width: u32,
    height: u32,
    hz: u32,
}

fn attached_displays() -> Vec<Attached> {
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
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let name_w = wide(&name);
        let (width, height, hz) = if unsafe {
            EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
        }
        .as_bool()
        {
            (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency)
        } else {
            (0, 0, 0)
        };
        out.push(Attached {
            name,
            width,
            height,
            hz,
        });
    }
    out
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Force `mode` onto an already-attached display, and verify by read-back.
///
/// This is NOT redundant with the mode passed to ADD. Windows persists a
/// per-monitor configuration keyed on the monitor's EDID identity and restores
/// it when that monitor reappears, overriding the driver's preferred mode.
/// Measured: an ADD for 1920x1080@60 produced a display at 2560x1440@120,
/// because a previous session had left that saved for this monitor.
///
/// Unlike `CDS_SET_PRIMARY`, a plain mode change *does* work on an indirect
/// display -- verified return 0 with a matching read-back.
fn force_mode(name: &str, mode: DisplayMode) -> Result<(), DisplayError> {
    let want_hz = if mode.refresh_mhz >= 1000 {
        mode.refresh_mhz / 1000
    } else {
        mode.refresh_mhz
    };
    let dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        dmPelsWidth: mode.width as u32,
        dmPelsHeight: mode.height as u32,
        dmDisplayFrequency: want_hz,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        ..Default::default()
    };
    let name_w = wide(name);
    let rc = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name_w.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
        .0
    };
    if rc != 0 {
        return Err(DisplayError::Os(format!(
            "ChangeDisplaySettingsExW({name}) returned {rc}"
        )));
    }

    // Read back rather than trust the return code: v1 design §15.3's whole
    // concern was an API that reports success while the desktop mode does not
    // move.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(d) = attached_displays().into_iter().find(|d| d.name == name) {
            if d.width == mode.width as u32 && d.height == mode.height as u32 && d.hz == want_hz {
                return Ok(());
            }
        }
    }
    Err(DisplayError::ModeRejected)
}

/// True if a SudoVDA-driven display is attached to the desktop right now.
///
/// Exists for the watchdog test: proving the keepalive works means proving the
/// display is still *there*, which no return code can tell you.
pub fn virtual_display_present() -> bool {
    let mut i = 0u32;
    loop {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            return false;
        }
        i += 1;
        if dd.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 == 0 {
            continue;
        }
        let s = String::from_utf16_lossy(&dd.DeviceString);
        if s.contains("SudoMaker") || s.contains("Virtual Display") {
            return true;
        }
    }
}

// ---------------------------------------------------------------------------
// Keepalive
// ---------------------------------------------------------------------------

struct Keepalive {
    stop: Arc<AtomicBool>,
    /// Keep the display on too (see `keep_display_on`).
    display_on: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Keepalive {
    /// The thread opens its OWN device handle. Any ioctl on any handle resets
    /// the driver's global countdown, and this avoids making `Device` `Send`.
    fn start() -> Keepalive {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let display_on = Arc::new(AtomicBool::new(false));
        let display_on_thread = Arc::clone(&display_on);
        let handle = std::thread::Builder::new()
            .name("vdd-keepalive".into())
            .spawn(move || {
                let device = match Device::open() {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::error!(error = %e, "keepalive could not open the VDD; \
                            the virtual display will be reaped in ~3s");
                        return;
                    }
                };
                // v2 design §6.5: a remote host has no reason to stay awake, so
                // Windows powers the display off at its idle timeout (900 s on
                // the test host), the desktop stops compositing, and
                // change-driven capture correctly has nothing to send. The
                // session looks perfectly healthy the whole time, which is what
                // makes it so hard to attribute.
                //
                // This lives HERE, on the keepalive thread, because the flags
                // are per-thread: asserting them on a thread that then exits
                // silently un-inhibits. This thread's lifetime is exactly the
                // session's -- started by activate, stopped by restore.
                //
                // The display itself only once the session's desktop is set up
                // (`keep_display_on`): asking for it wakes the host's own
                // monitors, and a DisplayPort monitor deep asleep takes seconds
                // to come up (12.5 s measured) -- all spent before the virtual
                // display is even usable, for monitors the session then turns
                // off. The system stays up from the start.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                let mut display_inhibited = false;

                let mut last = Instant::now() - PING_EVERY;
                while !stop_thread.load(Ordering::Relaxed) {
                    if !display_inhibited && display_on_thread.load(Ordering::Acquire) {
                        display_inhibited = true;
                        let previous = unsafe {
                            SetThreadExecutionState(
                                ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
                            )
                        };
                        if previous == EXECUTION_STATE(0) {
                            tracing::warn!(
                                "could not inhibit display sleep; the session will \
                                    end when the display powers off"
                            );
                        } else {
                            tracing::info!("display sleep inhibited for the session");
                        }
                    }
                    if last.elapsed() >= PING_EVERY {
                        if let Err(e) = device.ping() {
                            tracing::warn!(error = %e, "VDD keepalive ping failed");
                        }
                        last = Instant::now();
                    }
                    // Poll finer than the ping interval so stopping is prompt.
                    std::thread::sleep(Duration::from_millis(100));
                }

                // Drop the inhibit. Not strictly required -- the flags die with
                // the thread -- but doing it explicitly means the log says so
                // and a future refactor that keeps the thread alive longer does
                // not silently keep the display awake forever.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                tracing::info!("display sleep inhibit released");
            })
            .expect("spawning the keepalive thread");
        Keepalive {
            stop,
            display_on,
            handle: Some(handle),
        }
    }

    /// Keep the display on from now on, and wake it if it is off: the
    /// session's desktop needs composing.
    fn keep_display_on(&self) {
        if !self.display_on.swap(true, Ordering::AcqRel) {
            wake_displays();
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A monitor asleep stays dark until there is input, and a desktop nobody
/// sees is not composed: a new virtual display would never produce an image.
/// A zero-length mouse move is that input.
fn wake_displays() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent != 1 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "could not wake the displays");
    }
}

/// Power the displays off (as Windows does at its idle timeout). A monitor
/// that is part of the desktop but asleep -- nobody at the host -- is woken
/// by any change to the desktop, and a DisplayPort monitor deep asleep takes
/// 12 s and more: every virtual display added then waited for it (12.5 s
/// measured), against 0.1 s with the displays off.
fn power_displays_off() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SC_MONITORPOWER, SMTO_ABORTIFHUNG, WM_SYSCOMMAND,
    };
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(2),
            SMTO_ABORTIFHUNG,
            500,
            None,
        );
    }
}

/// With the displays off, how long a new virtual display gets to arrive
/// before they are woken for it after all (see `activate`).
const ARRIVE_ASLEEP: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------

pub struct WindowsDisplay {
    state_path: PathBuf,
    /// Set while a virtual display we created is in force.
    active: Option<ActiveDisplay>,
    /// CCD identity of that display, for unambiguous lookup and teardown.
    active_id: Option<CcdId>,
    device: Option<Device>,
    keepalive: Option<Keepalive>,
    /// The desktop arrangement before this session deactivated the other
    /// displays, so teardown puts back exactly what was there rather than an
    /// approximation of it. `None` when nothing has been deactivated.
    saved_topology: Option<Topology>,
    /// The desktop as the user had it, before the session's virtual display
    /// existed: teardown applies it in one change, which brings the host's
    /// monitors back, at their positions, with their primary, and leaves the
    /// virtual display (not part of it) inactive.
    original_topology: Option<Topology>,
    /// Turn the host's own monitors off while the virtual display is active,
    /// so it is the whole desktop (Apollo's default).
    isolate: bool,
}

impl WindowsDisplay {
    pub fn new(state_path: PathBuf) -> WindowsDisplay {
        WindowsDisplay {
            state_path,
            active: None,
            active_id: None,
            device: None,
            keepalive: None,
            saved_topology: None,
            original_topology: None,
            isolate: true,
        }
    }

    /// Whether the next `activate` turns the host's own monitors off.
    pub fn set_isolate(&mut self, isolate: bool) {
        self.isolate = isolate;
    }

    /// If a previous run died with a session active, put the desktop back
    /// (v2 design §6.4). Call once at startup, before anything else touches
    /// the display.
    ///
    /// The virtual display itself usually self-heals — the watchdog reaps it
    /// within ~3 s of the process dying. What does not self-heal is the primary
    /// assignment, and a stray monitor can survive if something else (Apollo)
    /// kept the watchdog fed.
    pub fn restore_stale(state_path: &Path) {
        let Some(saved) = state::load(state_path) else {
            return;
        };
        follow_input_desktop();
        tracing::info!("stale display state found; restoring the previous primary");

        if let Ok(device) = Device::open() {
            let _ = device.remove();
        }
        // Before the primary, and before anything else: a crash that stranded
        // this left the host's monitors DARK, which is the one stranded state
        // someone standing at the machine cannot work around.
        if saved.displays_disabled {
            tracing::info!("a previous session left the other displays off; turning them back on");
            if let Err(e) = reactivate_all_displays() {
                tracing::warn!(error = %e, "could not reactivate the host's displays");
            }
        }
        if let Err(e) = set_primary(CcdId {
            adapter_low: saved.primary_adapter_low,
            adapter_high: saved.primary_adapter_high,
            target_id: saved.primary_target_id,
        }) {
            tracing::warn!(error = %e, "could not restore the previous primary display");
        }
        state::clear(state_path);
    }

    /// Take the desktop back if something changed it under the session.
    ///
    /// Observed with CS2 in exclusive fullscreen: the physical display came
    /// back mid-session. The likely mechanism is that a
    /// game-driven mode change makes Windows reapply its own remembered
    /// arrangement, and `isolate_to` deliberately does NOT write ours to that
    /// database -- a session's topology is not the user's, and persisting it
    /// would have Windows reapplying a one-display desktop long afterwards.
    ///
    /// So the session re-asserts instead of persisting: the arrangement holds
    /// for exactly as long as the session does, and nothing outlives it. The
    /// reappearance is measured, the reason for it is not (a hypothesis), and
    /// the log lines here are what will settle that.
    ///
    /// **Two things are re-asserted, not one.** Isolation alone is not enough:
    /// a reapply that puts the host's monitor back also puts it at (0,0) and
    /// pushes ours off the origin, and `isolate_to` deactivates the others
    /// without moving what it keeps. That leaves a single-display desktop whose
    /// one display is not primary -- so `primary_id` finds nothing, the input
    /// transform is built against the wrong rectangle, and anything that asks
    /// Windows for "the default display" gets an answer about a display this
    /// session is not capturing.
    pub fn reassert(&mut self) -> Reassert {
        // Only while we hold an isolated desktop of our own.
        if self.saved_topology.is_none() {
            return Reassert::Held;
        }
        follow_input_desktop();
        let Some(id) = self.active_id else {
            return Reassert::Held;
        };

        // Ours has to still BE there before anything can be asserted about it.
        // `isolate_to` and `set_primary` both look the target up among the
        // ACTIVE paths, so a display that has left the desktop makes both of
        // them fail with `NoSuchDisplay` -- which the previous version logged
        // as a warning twice a second, forever, while the host's monitors
        // stayed on and the client stared at a frozen picture. The caller can
        // do something about it; a warning cannot.
        if gdi_name_for_target(id).is_none() {
            return Reassert::DisplayLost;
        }

        let mut acted = false;
        if active_path_count() > 1 {
            tracing::warn!(
                "something reactivated the host's displays mid-session; isolating again"
            );
            // The ORIGINAL saved topology is kept, not overwritten: it is what
            // teardown restores, and replacing it with whatever Windows just did
            // would make the session's own change permanent.
            if let Err(e) = isolate_to(id) {
                tracing::warn!(error = %e, "could not re-isolate the display");
                return Reassert::DisplayLost;
            }
            acted = true;
        }
        if primary_id() != Some(id) {
            tracing::warn!("the session's display is no longer primary; moving it back");
            if let Err(e) = set_primary(id) {
                tracing::warn!(error = %e, "could not make the session's display primary again");
                return Reassert::DisplayLost;
            }
            acted = true;
        }

        if acted {
            Reassert::Reasserted
        } else {
            Reassert::Held
        }
    }

    /// What the session's OWN display is set to right now.
    ///
    /// `None` once it has left the desktop -- see [`WindowsDisplay::reassert`].
    pub fn current_mode(&self) -> Option<DisplayMode> {
        self.active_id.and_then(mode_for_target)
    }

    /// Accept that the display is in `mode` now, because something else put it
    /// there.
    ///
    /// Without this `activate` would treat the client's original mode as
    /// already active and return its idempotent no-op (v2 design §4.4) -- so a
    /// client renegotiating after someone changed the resolution in Windows
    /// settings would be acked the mode it asked for while the display stayed
    /// in the one it was moved to.
    pub fn adopt_mode(&mut self, mode: DisplayMode) {
        if let Some(active) = self.active.as_mut() {
            active.mode = mode;
        }
    }

    /// Stop the keepalive and remove our monitor. Leaves `state_path` alone.
    fn teardown_display(&mut self) {
        let t = Instant::now();
        if let Some(k) = self.keepalive.take() {
            k.stop();
        }
        let keepalive_ms = t.elapsed().as_millis();
        if let Some(d) = self.device.take() {
            if let Err(e) = d.remove() {
                tracing::warn!(error = %e, "removing the virtual display failed");
            }
        }
        let remove_ms = t.elapsed().as_millis();
        if let Some(id) = self.active_id.take() {
            Self::wait_for_departure(id);
        }
        tracing::info!(
            keepalive_ms,
            remove_ms,
            total_ms = t.elapsed().as_millis(),
            "virtual display torn down"
        );
        self.active = None;
    }

    /// Block until the display with CCD identity `id` is attached, and return
    /// its GDI device name.
    ///
    /// On timeout the error names what WAS attached, so a stale monitor left by
    /// an earlier run is distinguishable from a driver failure.
    fn wait_for_arrival(
        device: &Device,
        id: CcdId,
        keepalive: &Keepalive,
    ) -> Result<String, DisplayError> {
        let started = Instant::now();
        // The wait is counted in polls, not on the clock. The display calls
        // below block while Windows brings a sleeping monitor up, and the
        // virtual display attaches only once that is done: with the host's
        // own monitor kept on and asleep that took 12 s, and 26 s from a
        // DisplayPort monitor's deep sleep. On the clock, one blocked call
        // used the whole timeout up, and the wait gave up the moment the call
        // returned, a poll or two before the display was there.
        let polls = ARRIVAL_TIMEOUT.as_millis() / ARRIVAL_POLL.as_millis();
        for _ in 0..polls {
            std::thread::sleep(ARRIVAL_POLL);
            // Not there yet with the displays off: perhaps Windows waits for
            // them. Wake them, as before.
            if started.elapsed() >= ARRIVE_ASLEEP && !keepalive.display_on.load(Ordering::Acquire) {
                tracing::info!(
                    "the virtual display has not arrived with the displays off; waking them"
                );
                keepalive.keep_display_on();
            }
            let _ = device.ping();
            // The first of these blocks while Windows brings the monitor up
            // (measured 3.8 s here): the keepalive thread is what feeds the
            // watchdog meanwhile.
            if let Some(name) = gdi_name_for_target(id) {
                if attached_displays().iter().any(|d| d.name == name) {
                    return Ok(name);
                }
            }
        }
        let seen: Vec<String> = attached_displays().into_iter().map(|d| d.name).collect();
        Err(DisplayError::Os(format!(
            "display {id:?} did not attach within {}s of looking ({:.1}s in all); \
                attached={seen:?}",
            ARRIVAL_TIMEOUT.as_secs(),
            started.elapsed().as_secs_f32()
        )))
    }

    /// Block until our monitor is really gone.
    ///
    /// `IOCTL_REMOVE_VIRTUAL_DISPLAY` returns as soon as the driver has asked
    /// for departure; the desktop reconfigure happens afterwards. Re-adding
    /// before that lands makes the new monitor race the old one's teardown.
    fn wait_for_departure(id: CcdId) {
        // Windows never detaches its last display: on a host with no monitor
        // of its own (asleep off the cable, unplugged), ours stays as a ghost
        // until another arrives, however long we wait. (A monitor merely
        // switched off for the session is still connected, and Windows
        // brings it back as ours goes: then the wait is worth it.)
        if !host_has_own_display(id) {
            tracing::debug!(
                ?id,
                "no display of the host's own; Windows keeps ours as a ghost"
            );
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if gdi_name_for_target(id).is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::warn!(?id, "virtual display still attached 5s after removal");
    }
}

impl DisplayControl for WindowsDisplay {
    fn activate(&mut self, mode: DisplayMode) -> Result<ActiveDisplay, DisplayError> {
        follow_input_desktop();
        if let Some(active) = self.active.as_ref() {
            if active.mode == mode {
                return Ok(active.clone()); // idempotent -- v2 design §4.4
            }
            // ADD would hand back the OLD monitor: same GUID, different mode.
            tracing::info!(
                ?mode,
                "mode changed; removing the old virtual display first"
            );
            self.teardown_display();
        }

        let opened_at = Instant::now();
        let device = Device::open()?;

        // Record who was primary BEFORE we move it (v2 design §6.4). Only on the
        // first activate of a session -- a mode change must not overwrite the
        // real pre-session primary with our own virtual display.
        let mut saved_state = false;
        if state::load(&self.state_path).is_none() {
            if let Some(prev) = primary_id() {
                saved_state = true;
                let _ = state::save(
                    &self.state_path,
                    &SavedState {
                        primary_adapter_low: prev.adapter_low,
                        primary_adapter_high: prev.adapter_high,
                        primary_target_id: prev.target_id,
                        // Set BEFORE isolating, not after: a crash in the
                        // window between the two would otherwise leave the
                        // monitors off with nothing on disk saying so.
                        displays_disabled: true,
                    },
                );
            }
        }

        // Only on the first activate of a session, like the primary above.
        let first = self.original_topology.is_none();
        if first {
            self.original_topology = query_config()
                .ok()
                .map(|(paths, modes)| Topology { paths, modes });
        }

        // The keepalive runs from before the ADD, on its own thread. The setup
        // below goes through display calls that block while Windows brings
        // the monitor up (3.8 s measured), and pinging only between them lets
        // the watchdog reap the monitor midway. Apollo's own pings hid this
        // whenever it was running.
        let keepalive = Keepalive::start();
        // Keeping the host's monitors: they are part of the desktop, wake
        // them now. Isolating: power them off first, and on again only once
        // they are out of the desktop (below), so that only the virtual
        // display has to come up.
        //
        // A kept monitor that is asleep or switched off makes this slow: the
        // wake blocks until Windows has brought it up, 12 to 38 s measured.
        // Powering off first, as when isolating, only moves that wait to the
        // wake after the setup (tried: 24 s there), so the order stays.
        if self.isolate {
            power_displays_off();
        } else {
            keepalive.keep_display_on();
        }
        let added = match device.add(mode) {
            Ok(added) => added,
            Err(e) => {
                keepalive.stop();
                if saved_state {
                    state::clear(&self.state_path);
                }
                if first {
                    self.original_topology = None;
                }
                return Err(e);
            }
        };
        tracing::info!(
            open_to_added_ms = opened_at.elapsed().as_millis(),
            "virtual display added"
        );
        let id = CcdId {
            adapter_low: added.adapter_luid_low,
            adapter_high: added.adapter_luid_high,
            target_id: added.target_id,
        };

        // Anything after ADD that fails must remove the monitor again. A
        // half-created display outlives the process -- the watchdog only reaps
        // it if nothing else is pinging, and Apollo is -- so leaving one behind
        // poisons the next activate().
        let mut isolated: Option<Topology> = None;
        let mut gdi_name = String::new();
        let added_at = Instant::now();
        let result = (|| {
            let name = Self::wait_for_arrival(&device, id, &keepalive)?;
            let arrived_ms = added_at.elapsed().as_millis();
            gdi_name = name.clone();
            force_mode(&name, mode)?;
            let moded_ms = added_at.elapsed().as_millis();
            set_primary(id)?;
            tracing::info!(
                arrived_ms,
                moded_ms,
                primary_ms = added_at.elapsed().as_millis(),
                "virtual display set up"
            );
            // LAST, and only once the display is real and primary: this is the
            // step that turns the host's own monitors off, and doing it before
            // the virtual display is known good could leave a desktop with no
            // usable output at all.
            if self.isolate {
                isolated = Some(isolate_to(id)?);
            }
            // The session's desktop is ready: now on, and composed.
            keepalive.keep_display_on();
            Ok(())
        })();
        if let Err(e) = result {
            if let Some(t) = isolated.as_ref() {
                let _ = apply_topology(t);
            }
            let _ = device.remove();
            keepalive.stop();
            Self::wait_for_departure(id);
            if first {
                self.original_topology = None;
            }
            // Nothing was changed for good: left behind, the state would have
            // the next start "restore" displays that were never turned off.
            if saved_state {
                state::clear(&self.state_path);
            }
            return Err(e);
        }
        self.saved_topology = isolated;

        self.keepalive = Some(keepalive);
        self.device = Some(device);
        self.active_id = Some(id);

        // `WgcSource::new(0)` means "the primary monitor", and we just made the
        // virtual display primary. That is why no index mapping is needed: GDI
        // order and windows-capture's own one-based order do not agree, and
        // this sidesteps having to reconcile them.
        let active = ActiveDisplay { gdi_name, mode };
        self.active = Some(active.clone());
        tracing::info!(
            width = mode.width,
            height = mode.height,
            refresh_mhz = mode.refresh_mhz,
            "virtual display active and primary"
        );
        Ok(active)
    }

    fn restore(&mut self) -> Result<(), DisplayError> {
        if self.active.is_none() {
            return Ok(()); // already restored; teardown can race a crash handler
        }
        follow_input_desktop();

        // A host with no display of its own connected (its monitor asleep
        // off the cable, or none at all): there is no arrangement to put
        // back and no primary to restore. Seen after a reboot: both attempts
        // failed (SetDisplayConfig 1610, "no such display").
        if let Some(id) = self.active_id {
            if !host_has_own_display(id) {
                tracing::info!(
                    "the host has no display of its own: unplugging the virtual \
                        one, nothing to restore"
                );
                self.original_topology = None;
                self.saved_topology = None;
                self.teardown_display();
                state::clear(&self.state_path);
                return Ok(());
            }
        }

        // The user's own arrangement, in one change: monitors, positions and
        // primary together. Putting back the isolated snapshot and then moving
        // the primary took two, and the second failed now and then
        // (SetDisplayConfig returned 31).
        //
        // Our monitor goes FIRST. Bringing back a DisplayPort monitor deep
        // asleep blocks the change for as long as it takes to wake (26 s
        // measured), and with the virtual display still part of
        // that change its driver hung and Windows killed it ("the process
        // hosting the driver has been terminated"), leaving no virtual display
        // for any later session. Gone first, it is not in the change at all;
        // Windows starts bringing the host's monitor back as it goes.
        if let Some(t) = self.original_topology.take() {
            let started = Instant::now();
            self.teardown_display();
            let unplugged_ms = started.elapsed().as_millis();
            match apply_topology(&t) {
                Ok(()) => {
                    self.saved_topology = None;
                    state::clear(&self.state_path);
                    tracing::info!(
                        unplugged_ms,
                        restored_ms = started.elapsed().as_millis(),
                        "the host's display arrangement is back"
                    );
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not put the original display \
                        arrangement back; trying step by step")
                }
            }
        }

        // The displays come back BEFORE the primary, and the order is
        // load-bearing: `set_primary` looks the target up among the ACTIVE
        // paths (`query_config` passes QDC_ONLY_ACTIVE_PATHS), so restoring the
        // primary while the host's monitors are still deactivated can only fail
        // with NoSuchDisplay -- a display that is off is a display that is not
        // there.
        if let Some(t) = self.saved_topology.take() {
            if let Err(e) = apply_topology(&t) {
                tracing::warn!(error = %e, "could not restore the display arrangement");
                // Fall back to the remembered extended desktop rather than
                // leaving the host dark. Less faithful, still usable.
                if let Err(e) = reactivate_all_displays() {
                    tracing::warn!(error = %e, "could not reactivate the host's displays either");
                }
            }
        }

        // Then the primary, so there is no window in which the desktop has no
        // sensible primary.
        if let Some(saved) = state::load(&self.state_path) {
            if let Err(e) = set_primary(CcdId {
                adapter_low: saved.primary_adapter_low,
                adapter_high: saved.primary_adapter_high,
                target_id: saved.primary_target_id,
            }) {
                tracing::warn!(error = %e, "could not restore the previous primary display");
            }
        }

        self.teardown_display();
        state::clear(&self.state_path);
        Ok(())
    }
}

impl Drop for WindowsDisplay {
    fn drop(&mut self) {
        if self.active.is_some() {
            let _ = self.restore();
        }
    }
}
