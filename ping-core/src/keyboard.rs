//! The keyboard, as the stream sees it, in protocol scancodes (set 1): which
//! keys the host holds down, and Moonlight's Ctrl+Alt+Shift chords. The
//! platforms turn their key events into scancodes and act on what this says.
//! (The Mac's AppKit handler predates it and keeps its own.)

use std::collections::HashSet;

use pingpong_proto::input::InputEvent;

use crate::input::InputSender;

/// Moonlight's chords, Ctrl+Alt+Shift and a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Q: stop streaming.
    Quit,
    /// S: statistics on or off.
    Stats,
    /// Z: release or capture the mouse and keyboard.
    Capture,
    /// V: type the clipboard on the host.
    Paste,
    /// M: mouse mode, relative or desktop pointer.
    MouseMode,
    /// X: full screen or a window.
    Fullscreen,
    /// D: minimise.
    Minimize,
    /// T: watching an AI agent, take its keyboard and mouse, or give them
    /// back.
    TakeOver,
}

/// What to do with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// A chord: do this; the key is the stream's (not the system's).
    Hotkey(Hotkey),
    /// Sent to the host (or a repeat of a key held there): the stream's.
    Forwarded,
    /// Not the stream's: the system may have it.
    Passed,
}

const CTRL_L: u16 = 0x1D;
const CTRL_R: u16 = 0x8000 | 0x1D;
const ALT_L: u16 = 0x38;
const ALT_R: u16 = 0x8000 | 0x38;
const SHIFT_L: u16 = 0x2A;
const SHIFT_R: u16 = 0x36;

#[derive(Default)]
pub struct Keyboard {
    held: HashSet<u16>,
    /// Modifiers down locally (whether or not the host has them).
    modifiers: HashSet<u16>,
    /// Keys swallowed as the last key of a chord: their release is not
    /// forwarded either.
    swallowed: HashSet<u16>,
}

impl Keyboard {
    fn chord(&self) -> bool {
        let any = |a: u16, b: u16| self.modifiers.contains(&a) || self.modifiers.contains(&b);
        any(CTRL_L, CTRL_R) && any(ALT_L, ALT_R) && any(SHIFT_L, SHIFT_R)
    }

    pub fn alt(&self) -> bool {
        self.modifiers.contains(&ALT_L) || self.modifiers.contains(&ALT_R)
    }

    pub fn ctrl(&self) -> bool {
        self.modifiers.contains(&CTRL_L) || self.modifiers.contains(&CTRL_R)
    }

    /// A key went down or up locally (`sc`, a protocol scancode). While
    /// `captured` it goes to the host; a chord is acted on either way.
    pub fn key(&mut self, sc: u16, down: bool, captured: bool, input: &InputSender) -> KeyAction {
        if matches!(sc, CTRL_L | CTRL_R | ALT_L | ALT_R | SHIFT_L | SHIFT_R) {
            if down {
                self.modifiers.insert(sc);
            } else {
                self.modifiers.remove(&sc);
            }
        }
        if down && self.chord() {
            if let Some(h) = hotkey(sc) {
                self.swallowed.insert(sc);
                return KeyAction::Hotkey(h);
            }
        }
        if !down && self.swallowed.remove(&sc) {
            return KeyAction::Forwarded;
        }
        if !captured {
            return KeyAction::Passed;
        }
        if down {
            // Held already: auto-repeat is the host's job.
            if self.held.insert(sc) {
                input.send(InputEvent::KeyDown(sc));
            }
        } else if self.held.remove(&sc) {
            input.send(InputEvent::KeyUp(sc));
        }
        KeyAction::Forwarded
    }

    /// Lift every key the host holds (focus lost, capture released, quit;
    /// before typing the clipboard, so it does not type as shortcuts).
    pub fn release_all(&mut self, input: &InputSender) {
        for sc in self.held.drain() {
            input.send(InputEvent::KeyUp(sc));
        }
    }

    /// Focus left the stream: local modifiers are no longer known.
    pub fn forget_modifiers(&mut self) {
        self.modifiers.clear();
    }
}

fn hotkey(sc: u16) -> Option<Hotkey> {
    Some(match sc {
        0x10 => Hotkey::Quit,
        0x1F => Hotkey::Stats,
        0x2C => Hotkey::Capture,
        0x2F => Hotkey::Paste,
        0x32 => Hotkey::MouseMode,
        0x2D => Hotkey::Fullscreen,
        0x20 => Hotkey::Minimize,
        0x14 => Hotkey::TakeOver,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> (InputSender, crossbeam_channel::Receiver<InputEvent>) {
        InputSender::for_test()
    }

    /// Everything sent so far (the test sender forwards on a thread).
    fn drain(rx: &crossbeam_channel::Receiver<InputEvent>) -> Vec<InputEvent> {
        std::thread::sleep(std::time::Duration::from_millis(50));
        rx.try_iter().collect()
    }

    #[test]
    fn keys_go_to_the_host_once_and_chords_do_not() {
        let (input, rx) = sender();
        let mut k = Keyboard::default();
        assert_eq!(k.key(0x11, true, true, &input), KeyAction::Forwarded);
        assert_eq!(
            k.key(0x11, true, true, &input),
            KeyAction::Forwarded,
            "a repeat"
        );
        assert_eq!(drain(&rx).len(), 1, "one KeyDown for W");
        for m in [CTRL_L, ALT_R, SHIFT_L] {
            k.key(m, true, true, &input);
        }
        assert_eq!(
            k.key(0x10, true, true, &input),
            KeyAction::Hotkey(Hotkey::Quit)
        );
        assert_eq!(
            k.key(0x10, false, true, &input),
            KeyAction::Forwarded,
            "its release is swallowed too"
        );
        let sent = drain(&rx);
        assert!(
            !sent.contains(&InputEvent::KeyDown(0x10)) && !sent.contains(&InputEvent::KeyUp(0x10))
        );
        k.release_all(&input);
        assert_eq!(drain(&rx).len(), 4, "W and the three modifiers lifted");
    }

    #[test]
    fn uncaptured_keys_are_the_systems_but_chords_still_work() {
        let (input, rx) = sender();
        let mut k = Keyboard::default();
        assert_eq!(k.key(0x11, true, false, &input), KeyAction::Passed);
        for m in [CTRL_R, ALT_L, SHIFT_R] {
            k.key(m, true, false, &input);
        }
        assert_eq!(
            k.key(0x2C, true, false, &input),
            KeyAction::Hotkey(Hotkey::Capture)
        );
        assert_eq!(drain(&rx).len(), 0);
    }
}
