//! Injecting input on a Linux host running X11: XTest, which X treats as a
//! keyboard and mouse of its own, so every client sees the events as typed
//! and clicked.
//!
//! Keys go by position: the scancode's Linux key code plus 8 is the X key
//! code (evdev and libinput servers), and the host's layout decides the
//! character. Text goes by the key that types each character on the
//! host's layout (with Shift when that is how), and a character the layout
//! lacks as a keysym put on a spare key code for the moment it is typed
//! (xdotool's way). The spare codes are the fallback only: a client reading
//! a key after its code was remapped for the next character types the wrong
//! one, which fast typing into GTK apps did.
//! Absolute positions are the stream's pixels, mapped through where the
//! picture sits in the stream (letterboxed) onto the X screen.

use std::collections::HashMap;
use std::time::Duration;

use pingpong_proto::input::{Button, InputEvent};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::{HeldSet, InputError, InputSink};

/// A notch of the wheel, in the protocol's units (Windows' WHEEL_DELTA).
const NOTCH: i32 = 120;
/// Key codes borrowed for text, taken in turn so a client still reading the
/// last one's mapping does not see it change under it.
const SPARE_KEYS: usize = 4;

pub struct XTestSink {
    conn: RustConnection,
    root: u32,
    screen: (u32, u32),
    /// The picture's rectangle in the stream (x, y, w, h).
    picture: (u32, u32, u32, u32),
    held: HeldSet,
    /// Wheel movement short of a notch, (vertical, horizontal).
    wheel: (i32, i32),
    spare: Vec<u8>,
    next_spare: usize,
    keysyms_per_keycode: u8,
    /// The keys that type each keysym on the layout: code, and whether
    /// Shift is needed.
    typing: HashMap<u32, (u8, bool)>,
    shift: Option<u8>,
}

/// Shift_L.
const SHIFT_L: u32 = 0xFFE1;

/// Keysym -> the key that types it (the first key found wins: the main
/// block comes first) and whether it takes Shift. Only the first two
/// levels: the others need modifiers a client may interpret differently.
fn typing_map(min: u8, per: u8, keysyms: &[u32], spare: &[u8]) -> HashMap<u32, (u8, bool)> {
    let mut map = HashMap::new();
    for (i, syms) in keysyms.chunks(per.max(1) as usize).enumerate() {
        let kc = min as usize + i;
        let Ok(kc) = u8::try_from(kc) else { break };
        if spare.contains(&kc) {
            continue;
        }
        for (level, &sym) in syms.iter().take(2).enumerate() {
            if sym != 0 {
                map.entry(sym).or_insert((kc, level == 1));
            }
        }
        // A key with only a lowercase letter types its capital with Shift.
        if let (Some(&lower), true) = (syms.first(), syms.get(1).is_none_or(|&s| s == 0)) {
            if (0x61..=0x7A).contains(&lower) {
                map.entry(lower - 0x20).or_insert((kc, true));
            }
        }
    }
    map
}

fn lost(e: impl std::fmt::Display) -> InputError {
    tracing::warn!(error = %e, "X input");
    InputError::Unavailable
}

impl XTestSink {
    /// Input for the screen of `display` (None: `$DISPLAY`); `picture` is
    /// where it is drawn in the stream.
    pub fn new(
        display: Option<&str>,
        picture: (u32, u32, u32, u32),
    ) -> Result<XTestSink, InputError> {
        let (conn, screen) = x11rb::connect(display).map_err(lost)?;
        if conn.extension_information("XTEST").ok().flatten().is_none() {
            tracing::warn!("the X server has no XTEST: input from the client goes nowhere");
            return Err(InputError::Unavailable);
        }
        let s = &conn.setup().roots[screen];
        let (root, sw, sh) = (s.root, s.width_in_pixels as u32, s.height_in_pixels as u32);
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let map = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(lost)?
            .reply()
            .map_err(lost)?;
        let per = map.keysyms_per_keycode;
        let spare: Vec<u8> = (min..=max)
            .rev()
            .filter(|&kc| {
                let at = (kc - min) as usize * per as usize;
                map.keysyms[at..at + per as usize].iter().all(|&k| k == 0)
            })
            .take(SPARE_KEYS)
            .collect();
        if spare.is_empty() {
            tracing::info!("no spare key code: text the layout cannot type is dropped");
        }
        let typing = typing_map(min, per, &map.keysyms, &spare);
        let shift = typing.get(&SHIFT_L).map(|&(kc, _)| kc);
        Ok(XTestSink {
            conn,
            root,
            screen: (sw, sh),
            picture,
            held: HeldSet::new(),
            wheel: (0, 0),
            spare,
            next_spare: 0,
            keysyms_per_keycode: per,
            typing,
            shift,
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), InputError> {
        let root = if kind == xproto::MOTION_NOTIFY_EVENT && detail == 0 {
            self.root
        } else {
            x11rb::NONE
        };
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, root, x, y, 0)
            .map_err(lost)?;
        Ok(())
    }

    fn key(&self, sc: u16, down: bool) -> Result<(), InputError> {
        let Some(code) = crate::evdev::key_code(sc) else {
            tracing::debug!(sc, "no key for this scancode");
            return Ok(());
        };
        let Ok(keycode) = u8::try_from(code + 8) else {
            return Ok(());
        };
        self.fake(
            if down {
                xproto::KEY_PRESS_EVENT
            } else {
                xproto::KEY_RELEASE_EVENT
            },
            keycode,
            0,
            0,
        )
    }

    fn button(&self, b: Button, down: bool) -> Result<(), InputError> {
        let n = match b {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
            Button::X1 => 8,
            Button::X2 => 9,
        };
        self.fake(
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            n,
            0,
            0,
        )
    }

    fn click(&self, n: u8) -> Result<(), InputError> {
        self.fake(xproto::BUTTON_PRESS_EVENT, n, 0, 0)?;
        self.fake(xproto::BUTTON_RELEASE_EVENT, n, 0, 0)
    }

    /// Stream pixels → the screen's.
    fn to_screen(&self, x: u16, y: u16) -> (i16, i16) {
        let (px, py, pw, ph) = self.picture;
        let (sw, sh) = self.screen;
        let map = |v: u16, p: u32, pw: u32, s: u32| {
            let v = (v as i64 - p as i64).clamp(0, pw.max(1) as i64 - 1);
            (v * s as i64 / pw.max(1) as i64).clamp(0, s as i64 - 1) as i16
        };
        (map(x, px, pw, sw), map(y, py, ph, sh))
    }

    fn text(&mut self, c: char) -> Result<(), InputError> {
        let cp = c as u32;
        let keysym = match c {
            '\n' | '\r' => 0xFF0D,
            '\t' => 0xFF09,
            _ if cp < 0x20 || cp == 0x7F => return Ok(()),
            _ if cp <= 0x7E || (0xA0..=0xFF).contains(&cp) => cp,
            _ => 0x0100_0000 | cp,
        };
        // On the layout: its own key, no remapping.
        if let Some(&(kc, shifted)) = self.typing.get(&keysym) {
            let shift = if shifted { self.shift } else { None };
            if let Some(s) = shift {
                self.fake(xproto::KEY_PRESS_EVENT, s, 0, 0)?;
            }
            self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0)?;
            self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0)?;
            if let Some(s) = shift {
                self.fake(xproto::KEY_RELEASE_EVENT, s, 0, 0)?;
            }
            return Ok(());
        }
        if self.spare.is_empty() {
            return Ok(());
        }
        let kc = self.spare[self.next_spare % self.spare.len()];
        self.next_spare += 1;
        let syms = vec![keysym; self.keysyms_per_keycode as usize];
        self.conn
            .change_keyboard_mapping(1, kc, self.keysyms_per_keycode, &syms)
            .map_err(lost)?;
        // The mapping reaches clients before the key does.
        self.conn
            .get_input_focus()
            .map_err(lost)?
            .reply()
            .map_err(lost)?;
        self.fake(xproto::KEY_PRESS_EVENT, kc, 0, 0)?;
        self.fake(xproto::KEY_RELEASE_EVENT, kc, 0, 0)?;
        // Give the client time to read the key before its code is reused.
        self.conn.flush().map_err(lost)?;
        std::thread::sleep(Duration::from_millis(4));
        Ok(())
    }
}

impl InputSink for XTestSink {
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError> {
        for &ev in events {
            self.held.observe(ev);
            match ev {
                InputEvent::KeyDown(sc) => self.key(sc, true)?,
                InputEvent::KeyUp(sc) => self.key(sc, false)?,
                InputEvent::MouseMoveRel { dx, dy } => {
                    self.fake(xproto::MOTION_NOTIFY_EVENT, 1, dx, dy)?
                }
                InputEvent::MouseMoveAbs { x, y } => {
                    let (x, y) = self.to_screen(x, y);
                    self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
                }
                InputEvent::ButtonDown(b) => self.button(b, true)?,
                InputEvent::ButtonUp(b) => self.button(b, false)?,
                InputEvent::Wheel { dv, dh } => {
                    // X's wheel is buttons: 4 up, 5 down, 6 left, 7 right.
                    self.wheel.0 += dv as i32;
                    self.wheel.1 += dh as i32;
                    while self.wheel.0 >= NOTCH {
                        self.click(4)?;
                        self.wheel.0 -= NOTCH;
                    }
                    while self.wheel.0 <= -NOTCH {
                        self.click(5)?;
                        self.wheel.0 += NOTCH;
                    }
                    while self.wheel.1 >= NOTCH {
                        self.click(7)?;
                        self.wheel.1 -= NOTCH;
                    }
                    while self.wheel.1 <= -NOTCH {
                        self.click(6)?;
                        self.wheel.1 += NOTCH;
                    }
                }
                InputEvent::Text(c) => self.text(c)?,
            }
        }
        self.conn.flush().map_err(lost)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let events = self.held.drain_release_events();
        self.wheel = (0, 0);
        self.inject(&events)
    }
}

impl Drop for XTestSink {
    fn drop(&mut self) {
        // The borrowed key codes back to nothing.
        let none = vec![0u32; self.keysyms_per_keycode as usize];
        for &kc in self.spare.iter().take(self.next_spare) {
            let _ = self
                .conn
                .change_keyboard_mapping(1, kc, self.keysyms_per_keycode, &none);
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_types_what_it_has() {
        // min key code 10; two levels each: (1, !), (a, A), (b, nothing), Shift_L.
        let keysyms = [0x31, 0x21, 0x61, 0x41, 0x62, 0, SHIFT_L, 0, 0, 0];
        let map = typing_map(10, 2, &keysyms, &[14]);
        assert_eq!(map.get(&0x31), Some(&(10, false)));
        assert_eq!(map.get(&0x21), Some(&(10, true)));
        assert_eq!(map.get(&0x41), Some(&(11, true)));
        assert_eq!(
            map.get(&0x42),
            Some(&(12, true)),
            "B from the b key with Shift"
        );
        assert_eq!(map.get(&SHIFT_L), Some(&(13, false)));
        assert_eq!(map.get(&0xE9), None, "é is not on this layout");
    }
}
