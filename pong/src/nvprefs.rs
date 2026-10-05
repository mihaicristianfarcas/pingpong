//! The NVIDIA driver's settings a streaming host wants, set as Sunshine sets
//! them (`src/platform/windows/nvprefs/driver_settings.cpp`),
//! through NVAPI's driver settings (DRS) in `nvapi64.dll`:
//!
//! - **Pong prefers maximum performance** (`PREFERRED_PSTATE`, in a profile
//!   of Pong's own for `pong.exe`). The driver's adaptive power states clock
//!   the GPU down between frames of a light scene, and the encoder then
//!   takes longer per frame (Sunshine's `nvenc_latency_over_power`).
//! - **OpenGL and Vulkan present through DXGI** (the global profile's
//!   "Vulkan/OpenGL present method: prefer layered on DXGI swapchain").
//!   Otherwise a full-screen OpenGL or Vulkan game presents in a way
//!   Desktop Duplication does not see at its full frame rate (Sunshine's
//!   `nvenc_opengl_vulkan_on_dxgi`). This one is system-wide: Pong puts the
//!   previous value back when it stops, and keeps it in a file meanwhile,
//!   so a Pong that died is undone by the next start.
//!
//! NVAPI's functions are found by number through `nvapi_QueryInterface`;
//! the numbers, structures and setting ids are NVIDIA's (`nvapi.h`,
//! `nvapi_interface.h`, `NvApiDriverSettings.h`, MIT-licensed). Nothing
//! happens on a host without an NVIDIA driver.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type Status = i32;
const OK: Status = 0;
const SETTING_NOT_FOUND: Status = -160;

// Function numbers (nvapi_interface.h).
const INITIALIZE: u32 = 0x0150_E828;
const DRS_CREATE_SESSION: u32 = 0x0694_D52E;
const DRS_DESTROY_SESSION: u32 = 0xDAD9_CFF8;
const DRS_LOAD_SETTINGS: u32 = 0x375D_BD6B;
const DRS_SAVE_SETTINGS: u32 = 0xFCBC_7E14;
const DRS_CREATE_PROFILE: u32 = 0xCC17_6068;
const DRS_FIND_PROFILE_BY_NAME: u32 = 0x7E4A_9A0B;
const DRS_CREATE_APPLICATION: u32 = 0x4347_A9DE;
const DRS_GET_APPLICATION_INFO: u32 = 0xED1F_8C69;
const DRS_SET_SETTING: u32 = 0x577D_D202;
const DRS_GET_SETTING: u32 = 0x73BF_8338;
const DRS_DELETE_PROFILE_SETTING: u32 = 0xE4A2_6362;
const DRS_GET_BASE_PROFILE: u32 = 0xDA84_66A0;

// Settings (NvApiDriverSettings.h).
const PREFERRED_PSTATE: u32 = 0x1057_EB71;
const PREFERRED_PSTATE_PREFER_MAX: u32 = 1;
const OGL_CPL_PREFER_DXPRESENT: u32 = 0x20D6_90F8;
const OGL_CPL_PREFER_DXPRESENT_ENABLED: u32 = 1;

const DWORD_TYPE: u32 = 0;
const CURRENT_PROFILE_LOCATION: u32 = 0;

/// Pong's own application profile.
const PROFILE_NAME: &str = "PongStream";

const UNICODE_MAX: usize = 2048;
type UnicodeString = [u16; UNICODE_MAX];

/// `NVDRS_SETTING_V1`, packed to 4 as in `nvapi.h`; the two unions are
/// 4100 bytes (a length and 4096 bytes of binary value) and only their
/// first four (a DWORD) are read here.
#[repr(C, packed(4))]
struct Setting {
    version: u32,
    name: UnicodeString,
    id: u32,
    kind: u32,
    location: u32,
    is_current_predefined: u32,
    is_predefined_valid: u32,
    predefined: [u8; 4100],
    current: [u8; 4100],
}
const _: () = assert!(std::mem::size_of::<Setting>() == 12320);
const SETTING_VER1: u32 = 12320 | (1 << 16);

impl Setting {
    fn empty() -> Box<Setting> {
        // SAFETY: plain integers and arrays of them: all zero is valid.
        let mut s: Box<Setting> = unsafe { Box::new_zeroed().assume_init() };
        s.version = SETTING_VER1;
        s
    }

    fn dword(id: u32, value: u32) -> Box<Setting> {
        let mut s = Setting::empty();
        s.id = id;
        s.kind = DWORD_TYPE;
        s.location = CURRENT_PROFILE_LOCATION;
        s.current[..4].copy_from_slice(&value.to_le_bytes());
        s
    }

    fn value(&self) -> u32 {
        let current = self.current;
        u32::from_le_bytes([current[0], current[1], current[2], current[3]])
    }
}

/// `NVDRS_PROFILE_V1`.
#[repr(C)]
struct Profile {
    version: u32,
    name: UnicodeString,
    gpu_support: u32,
    is_predefined: u32,
    apps: u32,
    settings: u32,
}
const _: () = assert!(std::mem::size_of::<Profile>() == 4116);
const PROFILE_VER1: u32 = 4116 | (1 << 16);

/// `NVDRS_APPLICATION_V1`, as Sunshine uses it.
#[repr(C)]
struct Application {
    version: u32,
    is_predefined: u32,
    name: UnicodeString,
    friendly_name: UnicodeString,
    launcher: UnicodeString,
}
const _: () = assert!(std::mem::size_of::<Application>() == 12296);
const APPLICATION_VER1: u32 = 12296 | (1 << 16);

fn unicode(text: &str) -> Box<UnicodeString> {
    let mut out = Box::new([0u16; UNICODE_MAX]);
    for (slot, unit) in out
        .iter_mut()
        .zip(text.encode_utf16().take(UNICODE_MAX - 1))
    {
        *slot = unit;
    }
    out
}

type QueryInterface = unsafe extern "C" fn(u32) -> *const c_void;
type Handle = *mut c_void;

/// An open DRS session.
struct Drs {
    query: QueryInterface,
    session: Handle,
}

/// Call NVAPI function `$id` with the signature `fn($($t),*) -> Status`.
macro_rules! nvapi {
    ($drs:expr, $id:expr, fn($($t:ty),*), $($arg:expr),*) => {{
        let f = ($drs.query)($id);
        if f.is_null() {
            Err(format!("NVAPI function {:#x} missing", $id))
        } else {
            let f: unsafe extern "C" fn($($t),*) -> Status = std::mem::transmute(f);
            Ok(f($($arg),*))
        }
    }};
}

impl Drs {
    /// NVAPI's settings, loaded; `None` without an NVIDIA driver.
    fn open() -> Result<Option<Drs>, String> {
        // SAFETY: loading a DLL by name and looking up an export; the
        // function pointers are called with the signatures nvapi.h gives.
        unsafe {
            let Ok(lib) = LoadLibraryW(w!("nvapi64.dll")) else {
                return Ok(None);
            };
            let Some(query) = GetProcAddress(lib, s!("nvapi_QueryInterface")) else {
                return Err("nvapi64.dll has no nvapi_QueryInterface".into());
            };
            let query: QueryInterface = std::mem::transmute(query);
            let mut drs = Drs {
                query,
                session: std::ptr::null_mut(),
            };
            let status = nvapi!(drs, INITIALIZE, fn(),)?;
            if status != OK {
                // No NVIDIA GPU driving a display, typically.
                tracing::debug!(status, "NVAPI not available");
                return Ok(None);
            }
            let mut session: Handle = std::ptr::null_mut();
            check(
                nvapi!(drs, DRS_CREATE_SESSION, fn(*mut Handle), &mut session)?,
                "NvAPI_DRS_CreateSession",
            )?;
            drs.session = session;
            check(
                nvapi!(drs, DRS_LOAD_SETTINGS, fn(Handle), drs.session)?,
                "NvAPI_DRS_LoadSettings",
            )?;
            Ok(Some(drs))
        }
    }

    fn save(&self) -> Result<(), String> {
        // SAFETY: a live session.
        unsafe {
            check(
                nvapi!(self, DRS_SAVE_SETTINGS, fn(Handle), self.session)?,
                "NvAPI_DRS_SaveSettings",
            )
        }
    }

    fn base_profile(&self) -> Result<Handle, String> {
        let mut profile: Handle = std::ptr::null_mut();
        // SAFETY: a live session and a valid out-param.
        unsafe {
            check(
                nvapi!(
                    self,
                    DRS_GET_BASE_PROFILE,
                    fn(Handle, *mut Handle),
                    self.session,
                    &mut profile
                )?,
                "NvAPI_DRS_GetBaseProfile",
            )?;
        }
        Ok(profile)
    }

    /// The setting as `profile` has it, or `None` when it is not set
    /// anywhere.
    fn get(&self, profile: Handle, id: u32) -> Result<Option<Box<Setting>>, String> {
        let mut setting = Setting::empty();
        // SAFETY: a live session and profile; `setting` is a full
        // NVDRS_SETTING_V1 with its version set.
        let status = unsafe {
            nvapi!(
                self,
                DRS_GET_SETTING,
                fn(Handle, Handle, u32, *mut Setting),
                self.session,
                profile,
                id,
                &mut *setting
            )?
        };
        match status {
            OK => Ok(Some(setting)),
            SETTING_NOT_FOUND => Ok(None),
            s => Err(format!("NvAPI_DRS_GetSetting({id:#x}): status {s}")),
        }
    }

    fn set(&self, profile: Handle, id: u32, value: u32) -> Result<(), String> {
        let mut setting = Setting::dword(id, value);
        // SAFETY: as `get`.
        unsafe {
            check(
                nvapi!(
                    self,
                    DRS_SET_SETTING,
                    fn(Handle, Handle, *mut Setting),
                    self.session,
                    profile,
                    &mut *setting
                )?,
                "NvAPI_DRS_SetSetting",
            )
        }
    }

    fn delete(&self, profile: Handle, id: u32) -> Result<(), String> {
        // SAFETY: a live session and profile.
        let status = unsafe {
            nvapi!(
                self,
                DRS_DELETE_PROFILE_SETTING,
                fn(Handle, Handle, u32),
                self.session,
                profile,
                id
            )?
        };
        match status {
            OK | SETTING_NOT_FOUND => Ok(()),
            s => Err(format!(
                "NvAPI_DRS_DeleteProfileSetting({id:#x}): status {s}"
            )),
        }
    }

    /// Pong's own profile, holding the executable `exe_name`; made if it
    /// is not there.
    fn app_profile(&self, exe_name: &str) -> Result<Handle, String> {
        let name = unicode(PROFILE_NAME);
        let mut profile: Handle = std::ptr::null_mut();
        // SAFETY: a live session; the strings are full NvAPI_UnicodeStrings
        // and the structures full V1 ones with their versions set.
        unsafe {
            let found = nvapi!(
                self,
                DRS_FIND_PROFILE_BY_NAME,
                fn(Handle, *const u16, *mut Handle),
                self.session,
                name.as_ptr(),
                &mut profile
            )?;
            if found != OK {
                let mut p: Box<Profile> = Box::new_zeroed().assume_init();
                p.version = PROFILE_VER1;
                p.name = *name;
                check(
                    nvapi!(
                        self,
                        DRS_CREATE_PROFILE,
                        fn(Handle, *mut Profile, *mut Handle),
                        self.session,
                        &mut *p,
                        &mut profile
                    )?,
                    "NvAPI_DRS_CreateProfile",
                )?;
            }
            let app_name = unicode(exe_name);
            let mut app: Box<Application> = Box::new_zeroed().assume_init();
            app.version = APPLICATION_VER1;
            let known = nvapi!(
                self,
                DRS_GET_APPLICATION_INFO,
                fn(Handle, Handle, *const u16, *mut Application),
                self.session,
                profile,
                app_name.as_ptr(),
                &mut *app
            )?;
            if known != OK {
                let mut app: Box<Application> = Box::new_zeroed().assume_init();
                app.version = APPLICATION_VER1;
                app.name = *app_name;
                app.friendly_name = *app_name;
                check(
                    nvapi!(
                        self,
                        DRS_CREATE_APPLICATION,
                        fn(Handle, Handle, *mut Application),
                        self.session,
                        profile,
                        &mut *app
                    )?,
                    "NvAPI_DRS_CreateApplication",
                )?;
            }
        }
        Ok(profile)
    }
}

impl Drop for Drs {
    fn drop(&mut self) {
        if !self.session.is_null() {
            // SAFETY: the session this opened, destroyed once.
            let _ = unsafe { nvapi!(self, DRS_DESTROY_SESSION, fn(Handle), self.session) };
        }
    }
}

fn check(status: Status, what: &str) -> Result<(), String> {
    if status == OK {
        Ok(())
    } else {
        Err(format!("{what}: status {status}"))
    }
}

/// What the global profile had before Pong changed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Undo {
    /// The present method's value, or `None` when it was not set (the
    /// driver's default applies).
    dxpresent: Option<u32>,
}

fn undo_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nvidia-undo.toml")
}

/// Put the global profile's present method back to `undo`, unless someone
/// changed it from Pong's value meanwhile (then theirs stays).
fn restore(drs: &Drs, undo: Undo) -> Result<(), String> {
    let base = drs.base_profile()?;
    let ours = drs.get(base, OGL_CPL_PREFER_DXPRESENT)?.is_some_and(|s| {
        s.location == CURRENT_PROFILE_LOCATION && s.value() == OGL_CPL_PREFER_DXPRESENT_ENABLED
    });
    if !ours {
        tracing::info!("the OpenGL/Vulkan present method was changed since; leaving it");
        return Ok(());
    }
    match undo.dxpresent {
        Some(v) => drs.set(base, OGL_CPL_PREFER_DXPRESENT, v)?,
        None => drs.delete(base, OGL_CPL_PREFER_DXPRESENT)?,
    }
    drs.save()?;
    tracing::info!("the OpenGL/Vulkan present method is back as it was");
    Ok(())
}

/// What `apply` changed system-wide, put back when dropped.
pub struct Applied {
    undo: Option<(Undo, PathBuf)>,
}

impl Drop for Applied {
    fn drop(&mut self) {
        let Some((undo, path)) = self.undo.take() else {
            return;
        };
        match Drs::open().and_then(|d| match d {
            Some(drs) => restore(&drs, undo),
            None => Ok(()),
        }) {
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => tracing::warn!(error = %e, "NVIDIA settings not put back"),
        }
    }
}

/// Set the driver up for streaming, as the settings say: `max_power` for
/// Pong's own profile, `dxgi_present` for the global one. A run that died
/// is undone first. `None` without an NVIDIA driver or on failure (logged:
/// streaming works without them).
pub fn apply(data_dir: &Path, max_power: bool, dxgi_present: bool) -> Option<Applied> {
    let drs = match Drs::open() {
        Ok(Some(drs)) => drs,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "NVIDIA driver settings unavailable");
            return None;
        }
    };
    let path = undo_path(data_dir);
    if let Some(stale) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str::<Undo>(&t).ok())
    {
        tracing::info!("undoing the NVIDIA settings of a Pong that did not stop cleanly");
        if let Err(e) = restore(&drs, stale) {
            tracing::warn!(error = %e, "could not undo the previous run's NVIDIA settings");
        }
        let _ = std::fs::remove_file(&path);
    }
    if let Err(e) = app_power(&drs, max_power) {
        tracing::warn!(error = %e, "NVIDIA power setting for Pong not set");
    }
    let undo = if dxgi_present {
        match global_present(&drs, &path) {
            Ok(undo) => undo.map(|u| (u, path)),
            Err(e) => {
                tracing::warn!(error = %e, "OpenGL/Vulkan present method not set");
                None
            }
        }
    } else {
        None
    };
    Some(Applied { undo })
}

/// Maximum performance for `pong.exe`, or Pong's own setting removed.
///
/// The profile names the executable by its file name, as Sunshine names
/// `sunshine.exe`. Looks interchangeable with the full path, is not: the
/// driver does not find an application it was given by full path again
/// (`NvAPI_DRS_GetApplicationInfo` fails), and adding it a second time is
/// `NVAPI_EXECUTABLE_ALREADY_IN_USE`, so after the first start Pong could
/// no longer change the setting.
fn app_power(drs: &Drs, max_power: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = exe
        .file_name()
        .ok_or_else(|| format!("{} has no file name", exe.display()))?;
    let profile = drs.app_profile(&name.to_string_lossy())?;
    let current = drs.get(profile, PREFERRED_PSTATE)?;
    let ours = current.as_ref().is_some_and(|s| {
        s.location == CURRENT_PROFILE_LOCATION && s.value() == PREFERRED_PSTATE_PREFER_MAX
    });
    if max_power && !ours {
        drs.set(profile, PREFERRED_PSTATE, PREFERRED_PSTATE_PREFER_MAX)?;
        drs.save()?;
        tracing::info!("the GPU runs at full power for Pong");
    } else if !max_power
        && current
            .as_ref()
            .is_some_and(|s| s.location == CURRENT_PROFILE_LOCATION)
    {
        drs.delete(profile, PREFERRED_PSTATE)?;
        drs.save()?;
        tracing::info!("the GPU's power is the driver's choice for Pong again");
    }
    Ok(())
}

/// OpenGL and Vulkan through DXGI, system-wide; what it was, when Pong
/// changed it (kept at `undo_path` until put back).
fn global_present(drs: &Drs, undo_path: &Path) -> Result<Option<Undo>, String> {
    let base = drs.base_profile()?;
    let current = drs.get(base, OGL_CPL_PREFER_DXPRESENT)?;
    if current
        .as_ref()
        .is_some_and(|s| s.value() == OGL_CPL_PREFER_DXPRESENT_ENABLED)
    {
        return Ok(None);
    }
    let undo = Undo {
        dxpresent: current.map(|s| s.value()),
    };
    // The way back is on disk before the change is.
    let text = toml::to_string(&undo).map_err(|e| e.to_string())?;
    std::fs::write(undo_path, text).map_err(|e| format!("{}: {e}", undo_path.display()))?;
    drs.set(
        base,
        OGL_CPL_PREFER_DXPRESENT,
        OGL_CPL_PREFER_DXPRESENT_ENABLED,
    )?;
    drs.save()?;
    tracing::info!(was = ?undo.dxpresent, "OpenGL and Vulkan present through DXGI while Pong runs");
    Ok(Some(undo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_undo_record_survives_a_round_trip_through_its_file() {
        for undo in [Undo { dxpresent: None }, Undo { dxpresent: Some(2) }] {
            let text = toml::to_string(&undo).unwrap();
            assert_eq!(toml::from_str::<Undo>(&text).unwrap(), undo);
        }
    }

    #[test]
    fn a_dword_setting_carries_its_value_little_endian() {
        let s = Setting::dword(PREFERRED_PSTATE, PREFERRED_PSTATE_PREFER_MAX);
        assert_eq!(s.value(), 1);
        assert_eq!({ s.version }, SETTING_VER1);
    }
}
