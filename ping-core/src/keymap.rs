//! Local keys → protocol keys: macOS virtual key codes (`kVK_*`, from
//! Carbon's Events.h), and Windows scan codes.
//!
//! Positional, like the rest of the protocol: the host maps the scancode
//! through its own layout. Pure, so it is tested without AppKit or Win32.

use pingpong_proto::input::Key;

/// Command keys, which are only forwarded (as the Windows key) when the user
/// opts into capturing system shortcuts -- Moonlight's default, so that
/// pressing Cmd never opens the Start menu.
pub const COMMAND_LEFT: u16 = 0x37;
pub const COMMAND_RIGHT: u16 = 0x36;

pub fn key_for(code: u16, command_is_windows_key: bool) -> Option<Key> {
    match code {
        COMMAND_LEFT | COMMAND_RIGHT if !command_is_windows_key => None,
        _ => pingpong_proto::mackeys::key_for(code),
    }
}

/// The modifier keys, reported through flagsChanged rather than keyDown/keyUp.
/// Returns the device-dependent flag bit (NX_DEVICE*KEYMASK) that says whether
/// that particular key is down.
pub fn modifier_mask(code: u16) -> Option<u64> {
    Some(match code {
        0x3B => 0x0000_0001, // left control
        0x38 => 0x0000_0002, // left shift
        0x3C => 0x0000_0004, // right shift
        0x37 => 0x0000_0008, // left command
        0x36 => 0x0000_0010, // right command
        0x3A => 0x0000_0020, // left option
        0x3D => 0x0000_0040, // right option
        0x3E => 0x0000_2000, // right control
        0x39 => 0x0001_0000, // caps lock (NSEventModifierFlagCapsLock)
        _ => return None,
    })
}

/// Windows: a key's scan code and extended flag, as a low-level keyboard
/// hook (or a WM_KEYDOWN) reports them, and its virtual key → the protocol's
/// scancode. The protocol's codes are set-1 scan codes, so this is mostly the
/// identity; it drops what the protocol has no key for (Pause, media keys)
/// and the fake modifiers keyboards send around other keys.
pub fn windows_scancode(scan: u32, extended: bool, vk: u32) -> Option<u16> {
    const VK_PAUSE: u32 = 0x13;
    const VK_NUMLOCK: u32 = 0x90;
    const E0: u16 = 0x8000;
    match vk {
        // The hook reports Num Lock as extended; set 1 has it plain.
        VK_NUMLOCK => return Some(0x45),
        VK_PAUSE => return None,
        _ => {}
    }
    // AltGr's fake left Control (scan 0x21D).
    if scan & 0x200 != 0 {
        return None;
    }
    let code = (scan & 0xFF) as u16 | if extended { E0 } else { 0 };
    // The fake shifts some keyboards wrap extended keys in (E0 2A, E0 36).
    if code == E0 | 0x2A || code == E0 | 0x36 {
        return None;
    }
    Key::ALL
        .iter()
        .map(|&k| pingpong_proto::input::scancode(k))
        .find(|&c| c == code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::input::scancode;

    #[test]
    fn wasd_are_positional() {
        assert_eq!(key_for(0x0D, false), Some(Key::KeyW));
        assert_eq!(key_for(0x00, false), Some(Key::KeyA));
        assert_eq!(key_for(0x01, false), Some(Key::KeyS));
        assert_eq!(key_for(0x02, false), Some(Key::KeyD));
        assert_eq!(scancode(Key::KeyW), 0x11);
    }

    #[test]
    fn command_is_only_forwarded_on_request() {
        assert_eq!(key_for(COMMAND_LEFT, false), None);
        assert_eq!(key_for(COMMAND_LEFT, true), Some(Key::SuperLeft));
        assert_eq!(key_for(0x3A, false), Some(Key::AltLeft));
    }

    #[test]
    fn every_mapped_key_has_a_distinct_code() {
        let mut seen = std::collections::HashSet::new();
        for code in 0..=0x7Fu16 {
            if let Some(k) = key_for(code, true) {
                assert!(seen.insert(k), "{k:?} mapped twice");
            }
        }
    }

    #[test]
    fn windows_scancodes_pass_through_with_their_extended_bit() {
        assert_eq!(
            windows_scancode(0x11, false, 0x57),
            Some(scancode(Key::KeyW))
        );
        assert_eq!(
            windows_scancode(0x48, true, 0x26),
            Some(scancode(Key::ArrowUp))
        );
        assert_eq!(
            windows_scancode(0x48, false, 0x68),
            Some(scancode(Key::Numpad8))
        );
        assert_eq!(
            windows_scancode(0x5B, true, 0x5B),
            Some(scancode(Key::SuperLeft))
        );
        assert_eq!(
            windows_scancode(0x1D, true, 0xA3),
            Some(scancode(Key::ControlRight))
        );
        assert_eq!(
            windows_scancode(0x45, true, 0x90),
            Some(scancode(Key::NumLock))
        );
        assert_eq!(windows_scancode(0x45, false, 0x13), None, "Pause");
        assert_eq!(
            windows_scancode(0x21D, false, 0xA2),
            None,
            "AltGr's fake Control"
        );
        assert_eq!(windows_scancode(0x2A, true, 0x10), None, "a fake shift");
        assert_eq!(
            windows_scancode(0x2A, false, 0xA0),
            Some(scancode(Key::ShiftLeft))
        );
    }
}
