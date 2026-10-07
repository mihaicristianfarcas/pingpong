//! The keyboard as a controller, for a console or game that takes no
//! keyboard.
//!
//! The layout is Greenlight's default (its README's "Keyboard controls"),
//! by position rather than by character, so it stays put on any layout;
//! Escape and Space join Backspace and Enter as B and A.

use pingpong_proto::gamepad::{button, GamepadState};
use pingpong_proto::input::Key;

/// What a key is on the keyboard-as-controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PadControl {
    /// XInput button bits.
    Button(u32),
    LeftTrigger,
    RightTrigger,
}

/// The keyboard-as-controller's layout.
const PAD_KEYS: [(Key, PadControl); 20] = {
    use PadControl::*;
    [
        (Key::Enter, Button(button::A)),
        (Key::NumpadEnter, Button(button::A)),
        (Key::Space, Button(button::A)),
        (Key::Backspace, Button(button::B)),
        (Key::Escape, Button(button::B)),
        (Key::KeyX, Button(button::X)),
        (Key::KeyY, Button(button::Y)),
        (Key::ArrowUp, Button(button::DPAD_UP)),
        (Key::ArrowDown, Button(button::DPAD_DOWN)),
        (Key::ArrowLeft, Button(button::DPAD_LEFT)),
        (Key::ArrowRight, Button(button::DPAD_RIGHT)),
        (Key::BracketLeft, Button(button::LEFT_SHOULDER)),
        (Key::BracketRight, Button(button::RIGHT_SHOULDER)),
        (Key::KeyL, Button(button::LEFT_THUMB)),
        (Key::KeyR, Button(button::RIGHT_THUMB)),
        (Key::KeyM, Button(button::START)),
        (Key::KeyV, Button(button::BACK)),
        (Key::KeyN, Button(button::GUIDE)),
        (Key::Minus, LeftTrigger),
        (Key::Equal, RightTrigger),
    ]
};

/// The keyboard as a controller: which of its keys are held. Merged with
/// the first controller, so both work at once (as Greenlight merges them,
/// `Channel/Input.ts` `mergeState`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardPad {
    /// Bit `i`: `PAD_KEYS[i]` is down.
    held: u32,
}

impl KeyboardPad {
    /// A key went down or up. Whether it is one of the controller's (and so
    /// not a key the console should see).
    pub fn key(&mut self, key: Key, down: bool) -> bool {
        let Some(i) = PAD_KEYS.iter().position(|&(k, _)| k == key) else {
            return false;
        };
        if down {
            self.held |= 1 << i;
        } else {
            self.held &= !(1 << i);
        }
        true
    }

    pub fn is_idle(&self) -> bool {
        *self == KeyboardPad::default()
    }

    /// `pad` (the first controller, or an idle one) with the keyboard's
    /// controls held on it too.
    pub fn merged(&self, pad: &GamepadState) -> GamepadState {
        let mut merged = *pad;
        for (i, &(_, control)) in PAD_KEYS.iter().enumerate() {
            if self.held & (1 << i) == 0 {
                continue;
            }
            match control {
                PadControl::Button(b) => merged.buttons |= b,
                PadControl::LeftTrigger => merged.left_trigger = u8::MAX,
                PadControl::RightTrigger => merged.right_trigger = u8::MAX,
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_holds_controller_buttons_on_top_of_the_first_pad() {
        let mut kb = KeyboardPad::default();
        assert!(kb.key(Key::Enter, true));
        assert!(kb.key(Key::Equal, true));
        assert!(!kb.key(Key::KeyQ, true), "Q is a key, not a control");
        let pad = GamepadState {
            buttons: button::X,
            left_trigger: 40,
            left_x: 9000,
            ..Default::default()
        };
        let m = kb.merged(&pad);
        assert_eq!(m.buttons, button::X | button::A);
        assert_eq!((m.left_trigger, m.right_trigger), (40, 255));
        assert_eq!(m.left_x, 9000);
        kb.key(Key::Enter, false);
        kb.key(Key::Equal, false);
        assert!(kb.is_idle());
        assert_eq!(kb.merged(&pad), pad);
    }

    #[test]
    fn a_button_held_by_two_keys_stays_down_until_both_are_up() {
        let mut kb = KeyboardPad::default();
        kb.key(Key::Enter, true);
        kb.key(Key::Space, true);
        kb.key(Key::Space, false);
        assert_eq!(kb.merged(&GamepadState::default()).buttons, button::A);
        kb.key(Key::Enter, false);
        assert_eq!(kb.merged(&GamepadState::default()).buttons, 0);
    }
}
