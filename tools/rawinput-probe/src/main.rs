//! Reports what raw input actually received, so injection can be verified
//! against the API layer games read rather than against a success return.
//!
//! "SendInput returned success" is not evidence a game saw anything. The
//! precedent is in v1 design §8.3 (docs/design/): a keyframe request silently
//! did nothing for 300 frames while reporting success.
//!
//! RIDEV_INPUTSINK means this receives input even without foreground focus,
//! which is what lets it watch while a game has the focus.

use std::mem::size_of;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
// RI_KEY_* live under WindowsAndMessaging in `windows` 0.61, not beside the
// raw-input types they describe.
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, HWND_MESSAGE,
    MSG, RI_KEY_BREAK, RI_KEY_E0, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WNDCLASSW,
};

const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg != WM_INPUT {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    let read = GetRawInputData(
        HRAWINPUT(l.0 as *mut _),
        RID_INPUT,
        Some(&mut raw as *mut _ as *mut _),
        &mut size,
        size_of::<RAWINPUTHEADER>() as u32,
    );
    if read == u32::MAX {
        return DefWindowProcW(hwnd, msg, w, l);
    }

    match raw.header.dwType {
        t if t == RIM_TYPEKEYBOARD.0 => {
            let kb = raw.data.keyboard;
            let e0 = kb.Flags & RI_KEY_E0 as u16 != 0;
            let up = kb.Flags & RI_KEY_BREAK as u16 != 0;
            println!(
                "key make=0x{:02x} e0={} {}",
                kb.MakeCode,
                e0,
                if up { "up" } else { "down" }
            );
        }
        t if t == RIM_TYPEMOUSE.0 => {
            let m = raw.data.mouse;
            // usFlags bit 0 is MOUSE_MOVE_ABSOLUTE. Its presence on what should
            // be relative motion is Apollo issue #1479's exact failure: a
            // raw-input game computes a delta against a cursor it is itself
            // re-centring, and the camera snaps to a corner.
            println!(
                "mouse dx={} dy={} flags=0x{:x} buttons=0x{:x}",
                m.lLastX, m.lLastY, m.usFlags.0, m.Anonymous.Anonymous.usButtonFlags
            );
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, w, l)
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            lpszClassName: w!("rawinput-probe"),
            ..Default::default()
        };
        RegisterClassW(&class);

        // A message-only window: it never appears, never takes focus, and so
        // never changes what the thing under test is talking to.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("rawinput-probe"),
            w!("rawinput-probe"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )?;

        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: HID_USAGE_PAGE_GENERIC,
                usUsage: HID_USAGE_GENERIC_KEYBOARD,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: HID_USAGE_PAGE_GENERIC,
                usUsage: HID_USAGE_GENERIC_MOUSE,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32)?;
        println!("watching raw input; Ctrl-C to stop");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
