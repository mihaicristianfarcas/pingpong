//! macOS virtual key codes (`kVK_*`, from Carbon's Events.h) and protocol
//! keys, both ways: the Mac client sends keys by position, and a Mac host
//! types them by position. Command is the Super (Windows) key.

use crate::input::{scancode, Key};

/// The protocol key at a Mac key code.
pub fn key_for(code: u16) -> Option<Key> {
    Some(match code {
        0x00 => Key::KeyA,
        0x01 => Key::KeyS,
        0x02 => Key::KeyD,
        0x03 => Key::KeyF,
        0x04 => Key::KeyH,
        0x05 => Key::KeyG,
        0x06 => Key::KeyZ,
        0x07 => Key::KeyX,
        0x08 => Key::KeyC,
        0x09 => Key::KeyV,
        0x0A => Key::IntlBackslash, // ISO section key
        0x0B => Key::KeyB,
        0x0C => Key::KeyQ,
        0x0D => Key::KeyW,
        0x0E => Key::KeyE,
        0x0F => Key::KeyR,
        0x10 => Key::KeyY,
        0x11 => Key::KeyT,
        0x12 => Key::Digit1,
        0x13 => Key::Digit2,
        0x14 => Key::Digit3,
        0x15 => Key::Digit4,
        0x16 => Key::Digit6,
        0x17 => Key::Digit5,
        0x18 => Key::Equal,
        0x19 => Key::Digit9,
        0x1A => Key::Digit7,
        0x1B => Key::Minus,
        0x1C => Key::Digit8,
        0x1D => Key::Digit0,
        0x1E => Key::BracketRight,
        0x1F => Key::KeyO,
        0x20 => Key::KeyU,
        0x21 => Key::BracketLeft,
        0x22 => Key::KeyI,
        0x23 => Key::KeyP,
        0x24 => Key::Enter,
        0x25 => Key::KeyL,
        0x26 => Key::KeyJ,
        0x27 => Key::Quote,
        0x28 => Key::KeyK,
        0x29 => Key::Semicolon,
        0x2A => Key::Backslash,
        0x2B => Key::Comma,
        0x2C => Key::Slash,
        0x2D => Key::KeyN,
        0x2E => Key::KeyM,
        0x2F => Key::Period,
        0x30 => Key::Tab,
        0x31 => Key::Space,
        0x32 => Key::Backquote,
        0x33 => Key::Backspace,
        0x35 => Key::Escape,
        0x36 => Key::SuperRight,
        0x37 => Key::SuperLeft,
        0x38 => Key::ShiftLeft,
        0x39 => Key::CapsLock,
        0x3A => Key::AltLeft,
        0x3B => Key::ControlLeft,
        0x3C => Key::ShiftRight,
        0x3D => Key::AltRight,
        0x3E => Key::ControlRight,
        0x41 => Key::NumpadDecimal,
        0x43 => Key::NumpadMultiply,
        0x45 => Key::NumpadAdd,
        0x47 => Key::NumLock, // keypad Clear sits where Num Lock does
        0x4B => Key::NumpadDivide,
        0x4C => Key::NumpadEnter,
        0x4E => Key::NumpadSubtract,
        0x52 => Key::Numpad0,
        0x53 => Key::Numpad1,
        0x54 => Key::Numpad2,
        0x55 => Key::Numpad3,
        0x56 => Key::Numpad4,
        0x57 => Key::Numpad5,
        0x58 => Key::Numpad6,
        0x59 => Key::Numpad7,
        0x5B => Key::Numpad8,
        0x5C => Key::Numpad9,
        0x60 => Key::F5,
        0x61 => Key::F6,
        0x62 => Key::F7,
        0x63 => Key::F3,
        0x64 => Key::F8,
        0x65 => Key::F9,
        0x67 => Key::F11,
        0x69 => Key::PrintScreen, // F13
        0x6B => Key::ScrollLock,  // F14
        0x6D => Key::F10,
        0x6E => Key::ContextMenu,
        0x6F => Key::F12,
        0x72 => Key::Insert, // Help sits where Insert does
        0x73 => Key::Home,
        0x74 => Key::PageUp,
        0x75 => Key::Delete,
        0x76 => Key::F4,
        0x77 => Key::End,
        0x78 => Key::F2,
        0x79 => Key::PageDown,
        0x7A => Key::F1,
        0x7B => Key::ArrowLeft,
        0x7C => Key::ArrowRight,
        0x7D => Key::ArrowDown,
        0x7E => Key::ArrowUp,
        _ => return None,
    })
}

/// The Mac key code of a protocol key.
pub fn keycode_for(key: Key) -> Option<u16> {
    (0..0x80u16).find(|&c| key_for(c) == Some(key))
}

/// The Mac key code at a PC scancode (set 1, E0 in the high bit).
pub fn keycode_for_scancode(sc: u16) -> Option<u16> {
    (0..0x80u16).find(|&c| key_for(c).map(scancode) == Some(sc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_goes_there_and_back() {
        for code in 0..0x80u16 {
            if let Some(k) = key_for(code) {
                assert_eq!(keycode_for(k), Some(code), "{k:?}");
                assert_eq!(keycode_for_scancode(scancode(k)), Some(code), "{k:?}");
            }
        }
        assert_eq!(keycode_for_scancode(scancode(Key::KeyW)), Some(0x0D));
        assert_eq!(keycode_for(Key::SuperLeft), Some(0x37));
    }
}
