//! The keyboard, as the stream sees it, in protocol scancodes (set 1): which
//! keys the host holds down, and Moonlight's Ctrl+Alt+Shift chords. The
//! platforms turn their key events into scancodes and act on what this says.
//! (The Mac's AppKit handler predates it and keeps its own.)

use std::collections::HashSet;

use pingpong_proto::input::InputEvent;

use crate::input::InputSender;

/// This computer's keyboard repeat (delay, then interval), sent with the
/// session: the host repeats a held key with it, so a key held in the
/// stream repeats as it does here (`pingpong_input::repeat`). `None` where
/// the system does not say (Linux: the host's own settings apply).
pub fn repeat_rate() -> Option<pingpong_proto::repeat::RepeatRate> {
    #[cfg(target_os = "macos")]
    {
        let delay = objc2_app_kit::NSEvent::keyRepeatDelay();
        let interval = objc2_app_kit::NSEvent::keyRepeatInterval();
        pingpong_proto::repeat::RepeatRate::from_millis(
            (delay * 1000.0).round().clamp(0.0, 65535.0) as u16,
            (interval * 1000.0).round().clamp(0.0, 65535.0) as u16,
        )
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };
        let (mut delay, mut speed) = (0u32, 0u32);
        // SAFETY: each call writes one u32 through a pointer to a live local.
        let read = unsafe {
            let none = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
            SystemParametersInfoW(
                SPI_GETKEYBOARDDELAY,
                0,
                Some(&mut delay as *mut u32 as *mut _),
                none,
            )
            .is_ok()
                && SystemParametersInfoW(
                    SPI_GETKEYBOARDSPEED,
                    0,
                    Some(&mut speed as *mut u32 as *mut _),
                    none,
                )
                .is_ok()
        };
        read.then(|| pingpong_proto::repeat::RepeatRate::from_windows(delay, speed))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

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

/// Modifiers held on this computer, either side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The Windows key.
    pub win: bool,
}

/// Whether the key with virtual-key code `vk`, pressed with `held`, is one
/// of Windows' screenshot shortcuts: Print Screen with any modifiers
/// (Windows 11 opens the Snipping Tool on it; Alt: the window; Win: saved
/// to a file; Win+Alt: the Game Bar; ShareX and Greenshot take it too), and
/// Win+Shift+S and Win+Shift+R (the Snipping Tool's picture and
/// recording). These stay this computer's while streaming, as ⌘⇧3/4/5 do
/// on a Mac, which never sends them to the host: a screenshot of the
/// stream is a screenshot of this screen.
pub fn windows_screenshot(vk: u32, held: Modifiers) -> bool {
    const VK_SNAPSHOT: u32 = 0x2C;
    const VK_R: u32 = 0x52;
    const VK_S: u32 = 0x53;
    match vk {
        VK_SNAPSHOT => true,
        VK_S | VK_R => held.win && held.shift && !held.ctrl && !held.alt,
        _ => false,
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

    #[test]
    fn print_screen_is_a_screenshot_with_any_modifiers() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        let win_alt = Modifiers {
            win: true,
            alt: true,
            ..Default::default()
        };
        for held in [Modifiers::default(), alt, win_alt] {
            assert!(windows_screenshot(0x2C, held), "{held:?}");
        }
    }

    #[test]
    fn win_shift_s_is_the_snipping_tool_and_shift_s_is_a_key() {
        let win_shift = Modifiers {
            win: true,
            shift: true,
            ..Default::default()
        };
        assert!(windows_screenshot(0x53, win_shift));
        assert!(windows_screenshot(0x52, win_shift), "its recording");
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert!(!windows_screenshot(0x53, shift));
        let chord = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            win: false,
        };
        assert!(
            !windows_screenshot(0x53, chord),
            "Ctrl+Alt+Shift+S: statistics"
        );
    }

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
