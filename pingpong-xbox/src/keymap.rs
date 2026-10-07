//! Keys for a console as Windows virtual-key codes, which is what the
//! input channel's keyboard reports carry.
//!
//! Ping's keys are positional ([`Key`], PS/2 set-1 scancodes on the wire);
//! the virtual-key code is the US layout's key at that position, which is
//! what a Windows PC would report for it with the US layout.

use pingpong_proto::input::{scancode, Key};

/// The key at a PS/2 set-1 scancode (with the E0 prefix in the high bit),
/// as Ping's input events carry it.
pub fn key_for_scancode(sc: u16) -> Option<Key> {
    Key::ALL.iter().copied().find(|&k| scancode(k) == sc)
}

/// A key's Windows virtual-key code.
pub fn vk(key: Key) -> u8 {
    match key {
        Key::Escape => 0x1B,
        Key::Digit1 => b'1',
        Key::Digit2 => b'2',
        Key::Digit3 => b'3',
        Key::Digit4 => b'4',
        Key::Digit5 => b'5',
        Key::Digit6 => b'6',
        Key::Digit7 => b'7',
        Key::Digit8 => b'8',
        Key::Digit9 => b'9',
        Key::Digit0 => b'0',
        Key::Minus => 0xBD,
        Key::Equal => 0xBB,
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::KeyQ => b'Q',
        Key::KeyW => b'W',
        Key::KeyE => b'E',
        Key::KeyR => b'R',
        Key::KeyT => b'T',
        Key::KeyY => b'Y',
        Key::KeyU => b'U',
        Key::KeyI => b'I',
        Key::KeyO => b'O',
        Key::KeyP => b'P',
        Key::BracketLeft => 0xDB,
        Key::BracketRight => 0xDD,
        Key::Enter | Key::NumpadEnter => 0x0D,
        Key::ControlLeft => 0xA2,
        Key::KeyA => b'A',
        Key::KeyS => b'S',
        Key::KeyD => b'D',
        Key::KeyF => b'F',
        Key::KeyG => b'G',
        Key::KeyH => b'H',
        Key::KeyJ => b'J',
        Key::KeyK => b'K',
        Key::KeyL => b'L',
        Key::Semicolon => 0xBA,
        Key::Quote => 0xDE,
        Key::Backquote => 0xC0,
        Key::ShiftLeft => 0xA0,
        Key::Backslash => 0xDC,
        Key::KeyZ => b'Z',
        Key::KeyX => b'X',
        Key::KeyC => b'C',
        Key::KeyV => b'V',
        Key::KeyB => b'B',
        Key::KeyN => b'N',
        Key::KeyM => b'M',
        Key::Comma => 0xBC,
        Key::Period => 0xBE,
        Key::Slash => 0xBF,
        Key::ShiftRight => 0xA1,
        Key::NumpadMultiply => 0x6A,
        Key::AltLeft => 0xA4,
        Key::Space => 0x20,
        Key::CapsLock => 0x14,
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::NumLock => 0x90,
        Key::ScrollLock => 0x91,
        Key::Numpad0 => 0x60,
        Key::Numpad1 => 0x61,
        Key::Numpad2 => 0x62,
        Key::Numpad3 => 0x63,
        Key::Numpad4 => 0x64,
        Key::Numpad5 => 0x65,
        Key::Numpad6 => 0x66,
        Key::Numpad7 => 0x67,
        Key::Numpad8 => 0x68,
        Key::Numpad9 => 0x69,
        Key::NumpadSubtract => 0x6D,
        Key::NumpadAdd => 0x6B,
        Key::NumpadDecimal => 0x6E,
        Key::NumpadDivide => 0x6F,
        Key::IntlBackslash => 0xE2,
        Key::ControlRight => 0xA3,
        Key::PrintScreen => 0x2C,
        Key::AltRight => 0xA5,
        Key::Home => 0x24,
        Key::ArrowUp => 0x26,
        Key::PageUp => 0x21,
        Key::ArrowLeft => 0x25,
        Key::ArrowRight => 0x27,
        Key::End => 0x23,
        Key::ArrowDown => 0x28,
        Key::PageDown => 0x22,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::SuperLeft => 0x5B,
        Key::SuperRight => 0x5C,
        Key::ContextMenu => 0x5D,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_its_own_virtual_key_but_the_two_enters() {
        let mut codes: Vec<u8> = Key::ALL
            .iter()
            .filter(|&&k| k != Key::NumpadEnter)
            .map(|&k| vk(k))
            .collect();
        let n = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), n);
        assert_eq!(vk(Key::NumpadEnter), vk(Key::Enter));
    }

    #[test]
    fn scancodes_lead_back_to_their_keys() {
        for &k in Key::ALL {
            assert_eq!(key_for_scancode(scancode(k)), Some(k));
        }
        assert_eq!(key_for_scancode(0x7f), None);
        assert_eq!(vk(key_for_scancode(0x1e).unwrap()), b'A');
        assert_eq!(vk(key_for_scancode(0x8048).unwrap()), 0x26);
    }
}
