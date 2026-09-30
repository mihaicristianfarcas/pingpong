//! Protocol scancodes (set 1, E0 in the high bit) → Linux input key codes
//! (`KEY_*` in linux/input-event-codes.h), what the kernel, X (plus 8) and
//! Wayland all number keys by. Pure, so it is tested everywhere.

const E0: u16 = 0x8000;

pub fn key_code(sc: u16) -> Option<u16> {
    Some(match sc {
        // Escape through keypad '.': the kernel numbers these as set 1 does.
        0x01..=0x53 => sc,
        0x56 => 86, // KEY_102ND
        0x57 => 87, // KEY_F11
        0x58 => 88, // KEY_F12
        _ if sc & E0 != 0 => match sc & 0xFF {
            0x1C => 96,  // KEY_KPENTER
            0x1D => 97,  // KEY_RIGHTCTRL
            0x35 => 98,  // KEY_KPSLASH
            0x37 => 99,  // KEY_SYSRQ (Print Screen)
            0x38 => 100, // KEY_RIGHTALT
            0x47 => 102, // KEY_HOME
            0x48 => 103, // KEY_UP
            0x49 => 104, // KEY_PAGEUP
            0x4B => 105, // KEY_LEFT
            0x4D => 106, // KEY_RIGHT
            0x4F => 107, // KEY_END
            0x50 => 108, // KEY_DOWN
            0x51 => 109, // KEY_PAGEDOWN
            0x52 => 110, // KEY_INSERT
            0x53 => 111, // KEY_DELETE
            0x5B => 125, // KEY_LEFTMETA
            0x5C => 126, // KEY_RIGHTMETA
            0x5D => 127, // KEY_COMPOSE (the menu key)
            0x20 => 113, // KEY_MUTE
            0x2E => 114, // KEY_VOLUMEDOWN
            0x30 => 115, // KEY_VOLUMEUP
            0x22 => 164, // KEY_PLAYPAUSE
            0x24 => 166, // KEY_STOPCD
            0x10 => 165, // KEY_PREVIOUSSONG
            0x19 => 163, // KEY_NEXTSONG
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::input::{scancode, Key};

    #[test]
    fn every_key_has_its_own_code() {
        let mut seen = std::collections::HashSet::new();
        for &k in Key::ALL {
            let code = key_code(scancode(k)).unwrap_or_else(|| panic!("{k:?} has no key code"));
            assert!(seen.insert(code), "{k:?} shares {code}");
        }
        assert_eq!(key_code(scancode(Key::KeyA)), Some(30));
        assert_eq!(key_code(scancode(Key::ArrowUp)), Some(103));
        assert_eq!(key_code(scancode(Key::SuperLeft)), Some(125));
    }
}
