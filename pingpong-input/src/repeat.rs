//! Auto-repeat for a key held on the client, made on the host.
//!
//! A held key repeats because the keyboard (or its driver) repeats it:
//! keys injected with `SendInput` or posted as CoreGraphics events are
//! pressed once and never repeat, so holding Backspace from a client
//! would delete one character. The client sends a key down once (repeats
//! would arrive bunched by the network), so the host makes the repeats,
//! as Sunshine does (`input.cpp`, `repeat_key`): the last key pressed
//! repeats after a delay, at an interval, until it is released or another
//! key takes over. Sunshine uses fixed timings (500 ms, 24.9 a second);
//! here they are the client's own keyboard settings, sent with the session,
//! so a held key repeats on the host as it would on the keyboard in front
//! of you. X11 and Wayland repeat injected keys themselves.
//!
//! Pure: the platform says which keys repeat and when; the session drives
//! the clock.

use std::time::Instant;

pub use pingpong_proto::repeat::RepeatRate;

/// The key repeating, if one is.
#[derive(Debug)]
pub struct KeyRepeat {
    rate: RepeatRate,
    held: Option<(u16, Instant)>,
}

impl KeyRepeat {
    pub fn new(rate: RepeatRate) -> KeyRepeat {
        KeyRepeat { rate, held: None }
    }

    pub fn rate(&self) -> RepeatRate {
        self.rate
    }

    /// `scancode` went down: it repeats from `delay` on, and the key that
    /// was repeating stops, as on a keyboard.
    pub fn press(&mut self, scancode: u16, now: Instant) {
        self.held = Some((scancode, now + self.rate.delay));
    }

    /// `scancode` came up: if it was repeating, nothing is now.
    pub fn release(&mut self, scancode: u16) {
        if self.held.is_some_and(|(sc, _)| sc == scancode) {
            self.held = None;
        }
    }

    /// Nothing repeats (everything was released).
    pub fn clear(&mut self) {
        self.held = None;
    }

    /// When the next repeat is due.
    pub fn due(&self) -> Option<Instant> {
        self.held.map(|(_, at)| at)
    }

    /// The key to press again, if its repeat is due at `now`. One at a time:
    /// a late call does not catch up (a burst of repeats is not what a
    /// keyboard does), the next is an interval on.
    pub fn poll(&mut self, now: Instant) -> Option<u16> {
        let (scancode, at) = self.held?;
        if now < at {
            return None;
        }
        let mut next = at + self.rate.interval;
        if next <= now {
            next = now + self.rate.interval;
        }
        self.held = Some((scancode, next));
        Some(scancode)
    }
}

/// The lock keys (Caps, Num, Scroll), which toggle on a press and never
/// repeat.
pub fn is_lock_key(scancode: u16) -> bool {
    matches!(scancode & 0x7FFF, 0x3A | 0x45 | 0x46)
}

/// Shift, Ctrl, Alt and the Windows key, either side (the E0 prefix in
/// the high bit, as on the wire).
pub fn is_modifier(scancode: u16) -> bool {
    matches!(
        scancode,
        0x2A | 0x36 | 0x1D | 0x801D | 0x38 | 0x8038 | 0x805B | 0x805C
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const A: u16 = 0x1E;
    const B: u16 = 0x30;

    fn rate() -> RepeatRate {
        RepeatRate {
            delay: Duration::from_millis(500),
            interval: Duration::from_millis(40),
        }
    }

    #[test]
    fn a_held_key_repeats_after_the_delay_then_every_interval() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::new(rate());
        r.press(A, t0);
        assert_eq!(r.poll(t0 + Duration::from_millis(499)), None);
        assert_eq!(r.poll(t0 + Duration::from_millis(500)), Some(A));
        assert_eq!(r.poll(t0 + Duration::from_millis(520)), None);
        assert_eq!(r.poll(t0 + Duration::from_millis(540)), Some(A));
        assert_eq!(r.due(), Some(t0 + Duration::from_millis(580)));
    }

    #[test]
    fn releasing_the_key_stops_it() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::new(rate());
        r.press(A, t0);
        r.release(A);
        assert_eq!(r.poll(t0 + Duration::from_secs(1)), None);
        assert_eq!(r.due(), None);
    }

    #[test]
    fn the_last_key_pressed_is_the_one_that_repeats() {
        // Holding A, then pressing B: B repeats; letting go of A changes
        // nothing; letting go of B leaves nothing repeating, A included.
        let t0 = Instant::now();
        let mut r = KeyRepeat::new(rate());
        r.press(A, t0);
        r.press(B, t0 + Duration::from_millis(300));
        r.release(A);
        assert_eq!(r.poll(t0 + Duration::from_millis(800)), Some(B));
        r.release(B);
        assert_eq!(r.poll(t0 + Duration::from_secs(2)), None);
    }

    #[test]
    fn a_late_poll_repeats_once_not_in_a_burst() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::new(rate());
        r.press(A, t0);
        let late = t0 + Duration::from_millis(900);
        assert_eq!(r.poll(late), Some(A));
        assert_eq!(r.poll(late), None);
        assert_eq!(r.due(), Some(late + Duration::from_millis(40)));
    }

    #[test]
    fn lock_keys_and_modifiers_are_known() {
        assert!(is_lock_key(0x3A) && is_lock_key(0x45) && is_lock_key(0x46));
        assert!(!is_lock_key(A));
        assert!(is_modifier(0x2A) && is_modifier(0x805B) && is_modifier(0x8038));
        assert!(!is_modifier(A) && !is_modifier(0x3A));
    }
}
