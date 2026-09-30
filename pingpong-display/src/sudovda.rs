//! Raw SudoVDA ioctl client (v2 design §6.1).
//!
//! SudoVDA is the SudoMaker Virtual Display Adapter, present on the host
//! because Apollo installs it. It creates virtual monitors on demand over a
//! private ioctl interface, taking the mode as a *creation parameter* — which
//! is why nothing here calls `ChangeDisplaySettingsEx`, and why the driver bug
//! v1 design §15.3 worried about (mode changes on a virtual display driver)
//! cannot apply.
//!
//! This module is transport only: open the device, send an ioctl, map the
//! error. Policy — when to add, when to set primary, when to ping — is
//! `super::windows`.
//!
//! Contract mirrored from `SudoMaker/SudoVDA`: `Common/Include/sudovda-ioctl.h`
//! and `Virtual Display Driver (HDR)/SudoVDA/Driver.cpp`. Verified against the
//! installed driver by `spikes/vdd-mode`; see its README before changing
//! anything here.

use crate::{DisplayError, DisplayMode};
use std::ffi::c_void;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::DeviceIoControl;

/// {e5bcc234-1e0c-418a-a0d4-ef8b7501414d}
const SUVDA_INTERFACE_GUID: GUID = GUID::from_values(
    0xe5bcc234,
    0x1e0c,
    0x418a,
    [0xa0, 0xd4, 0xef, 0x8b, 0x75, 0x01, 0x41, 0x4d],
);

/// Fixed for the whole project, and deliberately so.
///
/// `IOCTL_ADD_VIRTUAL_DISPLAY` is idempotent by `MonitorGuid`: a second ADD
/// with the same GUID returns the existing monitor instead of creating one.
/// A stable GUID therefore (a) gives `DisplayControl::activate` its required
/// idempotency for free and (b) stops repeated runs leaking connector slots,
/// which SudoVDA exhausts with `STATUS_TOO_MANY_NODES`.
const PINGPONG_MONITOR_GUID: GUID = GUID::from_values(
    0x7a3c1f2e,
    0x9b4d,
    0x4c8a,
    [0xa1, 0xe6, 0x5d, 0x2f, 0x8b, 0x0c, 0x3e, 0x91],
);

/// `CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0)`
const fn ctl_code(function: u32) -> u32 {
    (0x22 << 16) | (function << 2)
}

const IOCTL_ADD_VIRTUAL_DISPLAY: u32 = ctl_code(0x800);
const IOCTL_REMOVE_VIRTUAL_DISPLAY: u32 = ctl_code(0x801);
const IOCTL_GET_WATCHDOG: u32 = ctl_code(0x803);
const IOCTL_DRIVER_PING: u32 = ctl_code(0x888);

/// `ERROR_NOT_FOUND` as an HRESULT. REMOVE returns this when the watchdog has
/// already reaped the monitor.
const HRESULT_NOT_FOUND: u32 = 0x8007_0490;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AddParams {
    width: u32,
    height: u32,
    /// The driver does `if (VSync < 1000) VSync *= 1000`, so this takes Hz or
    /// millihertz. We always pass millihertz, matching the wire protocol
    /// (v2 design §4.4), so no conversion happens anywhere.
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

/// Identity of the created monitor. This is the only unambiguous handle on it:
/// it maps straight onto `DISPLAYCONFIG_PATH_INFO::targetInfo`, so no
/// name-matching or "last enumerated display" guessing is needed.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct AddOut {
    pub adapter_luid_low: u32,
    pub adapter_luid_high: i32,
    pub target_id: u32,
}

fn cstr14(s: &str) -> [i8; 14] {
    let mut out = [0i8; 14];
    for (i, b) in s.bytes().take(13).enumerate() {
        out[i] = b as i8;
    }
    out
}

/// An open handle to the SudoVDA control device.
pub struct Device {
    handle: HANDLE,
}

impl Device {
    /// Locate the control device via SetupAPI and open it.
    ///
    /// Absence means the driver is not installed or not started, which is
    /// `VddUnavailable` rather than a generic OS error — the caller may fall
    /// back to the physical display on that (spec E10).
    pub fn open() -> Result<Device, DisplayError> {
        unsafe {
            let devinfo = SetupDiGetClassDevsW(
                Some(&SUVDA_INTERFACE_GUID),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
            .map_err(|_| DisplayError::VddUnavailable)?;

            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(devinfo, None, &SUVDA_INTERFACE_GUID, 0, &mut iface)
                .is_err()
            {
                let _ = SetupDiDestroyDeviceInfoList(devinfo);
                return Err(DisplayError::VddUnavailable);
            }

            // Two-call pattern: the detail struct is variable-length (a cbSize
            // then the path), so it lives in a buffer aligned to the struct.
            let mut needed = 0u32;
            let _ =
                SetupDiGetDeviceInterfaceDetailW(devinfo, &iface, None, 0, Some(&mut needed), None);
            if needed == 0 {
                let _ = SetupDiDestroyDeviceInfoList(devinfo);
                return Err(DisplayError::VddUnavailable);
            }

            let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

            let got =
                SetupDiGetDeviceInterfaceDetailW(devinfo, &iface, Some(detail), needed, None, None);
            let _ = SetupDiDestroyDeviceInfoList(devinfo);
            got.map_err(|e| DisplayError::Os(e.to_string()))?;

            let handle = CreateFileW(
                PCWSTR((*detail).DevicePath.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(|e| DisplayError::Os(e.to_string()))?;

            Ok(Device { handle })
        }
    }

    fn ioctl<I: Copy, O: Copy + Default>(
        &self,
        code: u32,
        input: Option<&I>,
    ) -> Result<O, windows::core::Error> {
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
                self.handle,
                code,
                Some(in_ptr),
                in_len,
                Some(&mut out as *mut O as *mut c_void),
                std::mem::size_of::<O>() as u32,
                Some(&mut returned),
                None,
            )?;
        }
        Ok(out)
    }

    /// Create the virtual monitor at `mode`, or return the existing one.
    pub fn add(&self, mode: DisplayMode) -> Result<AddOut, DisplayError> {
        let params = AddParams {
            width: mode.width as u32,
            height: mode.height as u32,
            refresh: mode.refresh_mhz,
            monitor_guid: PINGPONG_MONITOR_GUID,
            device_name: cstr14("pingpong"),
            serial_number: cstr14("pp000001"),
        };
        self.ioctl(IOCTL_ADD_VIRTUAL_DISPLAY, Some(&params))
            .map_err(|e| DisplayError::Os(format!("ADD_VIRTUAL_DISPLAY: {e}")))
    }

    /// Remove our virtual monitor.
    ///
    /// `ERROR_NOT_FOUND` is success: it means the watchdog already reaped the
    /// monitor, so the desired end state already holds. Treating it as an error
    /// would make every teardown-after-a-stall look like a failure.
    pub fn remove(&self) -> Result<(), DisplayError> {
        let params = RemoveParams {
            monitor_guid: PINGPONG_MONITOR_GUID,
        };
        match self.ioctl::<RemoveParams, u8>(IOCTL_REMOVE_VIRTUAL_DISPLAY, Some(&params)) {
            Ok(_) => Ok(()),
            Err(e) if e.code().0 as u32 == HRESULT_NOT_FOUND => Ok(()),
            Err(e) => Err(DisplayError::Os(format!("REMOVE_VIRTUAL_DISPLAY: {e}"))),
        }
    }

    /// Reset the driver's watchdog.
    ///
    /// SudoVDA disconnects every monitor if it goes ~3 s without an ioctl
    /// (v2 design §6.1, verified), so this is not optional bookkeeping — it is what
    /// keeps the streamed display alive.
    pub fn ping(&self) -> Result<(), DisplayError> {
        self.ioctl::<(), u8>(IOCTL_DRIVER_PING, None)
            .map(|_| ())
            .map_err(|e| DisplayError::Os(format!("DRIVER_PING: {e}")))
    }
}

impl Device {
    /// The driver's watchdog: (timeout, countdown) in seconds.
    pub fn watchdog(&self) -> Result<(u32, u32), DisplayError> {
        self.ioctl::<(), [u32; 2]>(IOCTL_GET_WATCHDOG, None)
            .map(|w| (w[0], w[1]))
            .map_err(|e| DisplayError::Os(format!("GET_WATCHDOG: {e}")))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
