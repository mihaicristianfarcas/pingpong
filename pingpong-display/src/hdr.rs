//! A Windows display's HDR ("advanced colour"), switched on for a session
//! that streams HDR and off for one that does not, as Sunshine's display
//! handling does (libdisplaydevice's `win_api_layer.cpp`): Windows 11 24H2
//! has a call of its own for HDR (`DISPLAYCONFIG_SET_HDR_STATE`, device
//! info type 16; advanced colour there also means colour management for
//! SDR), and older Windows only advanced colour. The newer call is tried
//! first and the older one where Windows refuses it. SudoVDA's monitor
//! offers HDR, so its display can be switched like a real HDR monitor.
//!
//! Also the display's SDR white ("SDR content brightness"): what SDR
//! content is shown at inside an HDR desktop, which the stream's client
//! maps to its own SDR white.

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_TYPE,
};
use windows::Win32::Foundation::LUID;

use crate::windows::CcdId;
use crate::DisplayError;

/// Device info types (`wingdi.h`; 15 and 16 from Windows 11 24H2's SDK).
const GET_ADVANCED_COLOR_INFO: i32 = 9;
const SET_ADVANCED_COLOR_STATE: i32 = 10;
const GET_SDR_WHITE_LEVEL: i32 = 11;
const GET_ADVANCED_COLOR_INFO_2: i32 = 15;
const SET_HDR_STATE: i32 = 16;

/// `DISPLAYCONFIG_SET_HDR_STATE` and `DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE`
/// have the same shape: the header and a 32-bit field whose lowest bit
/// switches it on.
#[repr(C)]
struct SetState {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    enable: u32,
}

/// `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO(_2)`: the header, a field of
/// flags, the colour encoding, bits per channel (and, for `_2`, the active
/// colour mode).
#[repr(C)]
struct ColorInfo {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    encoding: u32,
    bits_per_channel: u32,
    active_mode: u32,
}

/// `DISPLAYCONFIG_SDR_WHITE_LEVEL`.
#[repr(C)]
struct SdrWhite {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    /// Thousandths of 80 cd/m² (1000 is 80).
    level: u32,
}

fn header<T>(kind: i32, id: CcdId) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(kind),
        size: std::mem::size_of::<T>() as u32,
        adapterId: LUID {
            LowPart: id.adapter_low,
            HighPart: id.adapter_high,
        },
        id: id.target_id,
    }
}

/// Whether the display is in HDR now; `None` when it cannot be (or Windows
/// does not say).
pub fn hdr_enabled(id: CcdId) -> Option<bool> {
    let mut v2 = ColorInfo {
        header: header::<ColorInfo>(GET_ADVANCED_COLOR_INFO_2, id),
        flags: 0,
        encoding: 0,
        bits_per_channel: 0,
        active_mode: 0,
    };
    // SAFETY: a structure of the size its header says, for the call.
    if unsafe { DisplayConfigGetDeviceInfo(&mut v2.header) } == 0 {
        // highDynamicRangeSupported is bit 4, highDynamicRangeUserEnabled 5.
        return (v2.flags & (1 << 4) != 0).then_some(v2.flags & (1 << 5) != 0);
    }
    let mut v1 = ColorInfo {
        header: header::<ColorInfo>(GET_ADVANCED_COLOR_INFO, id),
        flags: 0,
        encoding: 0,
        bits_per_channel: 0,
        active_mode: 0,
    };
    // The older structure is one field shorter.
    v1.header.size -= 4;
    // SAFETY: as above; the size said is the older structure's.
    if unsafe { DisplayConfigGetDeviceInfo(&mut v1.header) } != 0 {
        return None;
    }
    // advancedColorSupported is bit 0, advancedColorEnabled bit 1.
    (v1.flags & 1 != 0).then_some(v1.flags & 2 != 0)
}

/// Switch the display's HDR on or off. Returns whether it changed.
pub fn set_hdr(id: CcdId, on: bool) -> Result<bool, DisplayError> {
    match hdr_enabled(id) {
        None if on => return Err(DisplayError::Os("the display does not offer HDR".into())),
        None => return Ok(false),
        Some(now) if now == on => return Ok(false),
        Some(_) => {}
    }
    for kind in [SET_HDR_STATE, SET_ADVANCED_COLOR_STATE] {
        let set = SetState {
            header: header::<SetState>(kind, id),
            enable: on as u32,
        };
        // SAFETY: a structure of the size its header says, for the call.
        if unsafe { DisplayConfigSetDeviceInfo(&set.header) } == 0 {
            // Give the display a moment: it changes mode to switch.
            for _ in 0..20 {
                if hdr_enabled(id) == Some(on) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            return Ok(true);
        }
    }
    Err(DisplayError::Os(format!(
        "Windows refused to switch HDR {}",
        if on { "on" } else { "off" }
    )))
}

/// SDR white inside the display's HDR desktop, cd/m².
pub fn sdr_white_nits(id: CcdId) -> Option<u16> {
    let mut white = SdrWhite {
        header: header::<SdrWhite>(GET_SDR_WHITE_LEVEL, id),
        level: 0,
    };
    // SAFETY: a structure of the size its header says, for the call.
    if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != 0 || white.level == 0 {
        return None;
    }
    Some(((white.level as u64 * 80 / 1000).clamp(1, u16::MAX as u64)) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structures_are_the_sizes_windows_expects() {
        // wingdi.h: the header is 20 bytes; the set-state structures 24, the
        // colour info 32 (36 for _2), the SDR white level 24.
        assert_eq!(std::mem::size_of::<DISPLAYCONFIG_DEVICE_INFO_HEADER>(), 20);
        assert_eq!(std::mem::size_of::<SetState>(), 24);
        assert_eq!(std::mem::size_of::<ColorInfo>(), 36);
        assert_eq!(std::mem::size_of::<SdrWhite>(), 24);
    }
}
