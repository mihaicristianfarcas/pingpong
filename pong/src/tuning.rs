//! What a Windows host asks of Windows while it streams, as Sunshine asks (`src/platform/windows/misc.cpp`, `streaming_will_start`),
//! all put back when the session ends:
//!
//! - **A 1 ms timer.** Waits with a timeout (the session thread's 8 ms
//!   tick, a held key's repeats, Desktop Duplication's frame wait) wake on
//!   the system timer, 15.6 ms apart by default. Ping's Windows client asks
//!   for the same.
//! - **DWM scheduled by MMCSS**, so composition keeps its pace while a game
//!   loads the CPU.
//! - **Wi-Fi in media-streaming mode**: a host on Wi-Fi stops its
//!   adapters' background scans, which stall the radio for tens of
//!   milliseconds. Undone when the handle that asked closes. `wlanapi.dll`
//!   is loaded when needed: Windows Server can lack it.
//! - **Mouse Keys on a host with no mouse**: Windows hides the pointer
//!   when no mouse is attached, and the picture would have none (Pong draws
//!   the pointer Desktop Duplication reports). Mouse Keys makes Windows show
//!   it.

use std::ffi::c_void;

use windows::core::{s, w};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_connected, wlan_intf_opcode_media_streaming_mode, WLAN_INTERFACE_INFO,
    WLAN_INTERFACE_INFO_LIST, WLAN_INTF_OPCODE,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::UI::Accessibility::MOUSEKEYS;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SystemParametersInfoW, SM_MOUSEPRESENT, SPI_GETMOUSEKEYS, SPI_SETMOUSEKEYS,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// `winuser.h`: Mouse Keys on, and available.
const MKF_MOUSEKEYSON: u32 = 0x1;
const MKF_AVAILABLE: u32 = 0x2;

/// Undoes what `start` asked for when dropped.
pub struct Streaming {
    timer: bool,
    wlan: Option<Wlan>,
    /// Mouse Keys as they were, when this turned them on.
    mouse_keys: Option<MOUSEKEYS>,
}

impl Streaming {
    pub fn start() -> Streaming {
        // SAFETY: process-wide settings, each undone in `drop`.
        let timer = unsafe { windows::Win32::Media::timeBeginPeriod(1) } == 0;
        if let Err(e) = unsafe { windows::Win32::Graphics::Dwm::DwmEnableMMCSS(true) } {
            tracing::debug!(error = %e, "DwmEnableMMCSS");
        }
        Streaming {
            timer,
            wlan: Wlan::streaming_mode(),
            mouse_keys: show_pointer_without_mouse(),
        }
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        // SAFETY: undoing `start`, once.
        unsafe {
            if self.timer {
                windows::Win32::Media::timeEndPeriod(1);
            }
            let _ = windows::Win32::Graphics::Dwm::DwmEnableMMCSS(false);
        }
        drop(self.wlan.take());
        if let Some(mut was) = self.mouse_keys.take() {
            // SAFETY: a MOUSEKEYS read by SPI_GETMOUSEKEYS, cbSize set.
            let restored = unsafe {
                SystemParametersInfoW(
                    SPI_SETMOUSEKEYS,
                    0,
                    Some(&mut was as *mut MOUSEKEYS as *mut c_void),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
            };
            if let Err(e) = restored {
                tracing::warn!(error = %e, "Mouse Keys not put back");
            }
        }
    }
}

/// With no mouse attached, turn Mouse Keys on (returning how they were).
fn show_pointer_without_mouse() -> Option<MOUSEKEYS> {
    // SAFETY: reads a system metric.
    if unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0 {
        return None;
    }
    let mut was = MOUSEKEYS {
        cbSize: std::mem::size_of::<MOUSEKEYS>() as u32,
        ..Default::default()
    };
    let none = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
    // SAFETY: MOUSEKEYS structures with cbSize set, live for the calls.
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSEKEYS,
            was.cbSize,
            Some(&mut was as *mut MOUSEKEYS as *mut c_void),
            none,
        )
        .ok()?;
        let mut on = MOUSEKEYS {
            cbSize: was.cbSize,
            dwFlags: MKF_MOUSEKEYSON | MKF_AVAILABLE,
            iMaxSpeed: 10,
            iTimeToMaxSpeed: 1000,
            ..Default::default()
        };
        match SystemParametersInfoW(
            SPI_SETMOUSEKEYS,
            on.cbSize,
            Some(&mut on as *mut MOUSEKEYS as *mut c_void),
            none,
        ) {
            Ok(()) => {
                tracing::info!("no mouse on the host: Mouse Keys on, so the pointer shows");
                Some(was)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Mouse Keys not turned on");
                None
            }
        }
    }
}

type OpenHandle = unsafe extern "system" fn(u32, *const c_void, *mut u32, *mut HANDLE) -> u32;
type CloseHandle = unsafe extern "system" fn(HANDLE, *const c_void) -> u32;
type EnumInterfaces =
    unsafe extern "system" fn(HANDLE, *const c_void, *mut *mut WLAN_INTERFACE_INFO_LIST) -> u32;
type SetInterface = unsafe extern "system" fn(
    HANDLE,
    *const windows::core::GUID,
    WLAN_INTF_OPCODE,
    u32,
    *const c_void,
    *const c_void,
) -> u32;
type FreeMemory = unsafe extern "system" fn(*const c_void);

/// A WLAN client handle: the media-streaming mode it asked for lasts until
/// it closes.
struct Wlan {
    handle: HANDLE,
    close: CloseHandle,
}

impl Wlan {
    /// Every connected Wi-Fi adapter in media-streaming mode; `None` when
    /// there is none (or no WLAN service).
    fn streaming_mode() -> Option<Wlan> {
        // SAFETY: wlanapi's exports, called with the signatures wlanapi.h
        // gives; the interface list is freed with WlanFreeMemory.
        unsafe {
            let lib = LoadLibraryExW(w!("wlanapi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
            let open: OpenHandle = std::mem::transmute(GetProcAddress(lib, s!("WlanOpenHandle"))?);
            let close: CloseHandle =
                std::mem::transmute(GetProcAddress(lib, s!("WlanCloseHandle"))?);
            let list: EnumInterfaces =
                std::mem::transmute(GetProcAddress(lib, s!("WlanEnumInterfaces"))?);
            let set: SetInterface =
                std::mem::transmute(GetProcAddress(lib, s!("WlanSetInterface"))?);
            let free: FreeMemory = std::mem::transmute(GetProcAddress(lib, s!("WlanFreeMemory"))?);

            let mut version = 0u32;
            let mut handle = HANDLE::default();
            // Version 2: Windows Vista and later.
            if open(2, std::ptr::null(), &mut version, &mut handle) != 0 {
                return None;
            }
            let wlan = Wlan { handle, close };
            let mut interfaces: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if list(handle, std::ptr::null(), &mut interfaces) != 0 || interfaces.is_null() {
                return None;
            }
            let count = (*interfaces).dwNumberOfItems as usize;
            let first =
                std::ptr::addr_of!((*interfaces).InterfaceInfo) as *const WLAN_INTERFACE_INFO;
            let mut set_any = false;
            for i in 0..count {
                let info = &*first.add(i);
                if info.isState != wlan_interface_state_connected {
                    continue;
                }
                let on: i32 = 1;
                let status = set(
                    handle,
                    &info.InterfaceGuid,
                    wlan_intf_opcode_media_streaming_mode,
                    std::mem::size_of::<i32>() as u32,
                    &on as *const i32 as *const c_void,
                    std::ptr::null(),
                );
                if status == 0 {
                    set_any = true;
                } else {
                    tracing::debug!(status, "Wi-Fi media-streaming mode refused");
                }
            }
            free(interfaces as *const c_void);
            if set_any {
                tracing::info!("Wi-Fi in media-streaming mode for the session");
                Some(wlan)
            } else {
                None
            }
        }
    }
}

impl Drop for Wlan {
    fn drop(&mut self) {
        // SAFETY: the handle WlanOpenHandle gave, closed once.
        unsafe {
            (self.close)(self.handle, std::ptr::null());
        }
    }
}
