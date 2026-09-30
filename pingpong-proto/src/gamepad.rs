//! Game controllers. The client sends each controller's whole state whenever
//! it changes (and again now and then), as Moonlight does; the host mirrors it
//! onto a virtual Xbox 360 pad. Whole states make loss and reordering
//! harmless: a per-controller sequence number lets the newest win.

/// Controllers per session (XInput's limit, and Moonlight's).
pub const MAX_PADS: usize = 4;

/// Button bits: XInput's `XINPUT_GAMEPAD_*`, which Moonlight uses too.
pub mod button {
    pub const DPAD_UP: u32 = 0x0001;
    pub const DPAD_DOWN: u32 = 0x0002;
    pub const DPAD_LEFT: u32 = 0x0004;
    pub const DPAD_RIGHT: u32 = 0x0008;
    pub const START: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LEFT_THUMB: u32 = 0x0040;
    pub const RIGHT_THUMB: u32 = 0x0080;
    pub const LEFT_SHOULDER: u32 = 0x0100;
    pub const RIGHT_SHOULDER: u32 = 0x0200;
    pub const GUIDE: u32 = 0x0400;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GamepadState {
    /// Controller slot, 0..MAX_PADS.
    pub index: u8,
    /// Per controller; wrap-aware, newer wins.
    pub seq: u16,
    /// False once the controller has gone: the host unplugs its pad.
    pub connected: bool,
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Sticks, XInput convention: up and right are positive.
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

impl GamepadState {
    /// Same input, ignoring the sequence number.
    pub fn same_input(&self, other: &GamepadState) -> bool {
        GamepadState { seq: 0, ..*self } == GamepadState { seq: 0, ..*other }
    }
}

/// Host side: drops states older than one already applied.
#[derive(Debug, Default)]
pub struct GamepadGate {
    last: [Option<u16>; MAX_PADS],
}

impl GamepadGate {
    pub fn new() -> GamepadGate {
        GamepadGate::default()
    }

    /// Whether `state` should be applied.
    pub fn admit(&mut self, state: &GamepadState) -> bool {
        let Some(slot) = self.last.get_mut(state.index as usize) else {
            return false;
        };
        let newer = match *slot {
            None => true,
            // A controller that reconnects starts its count again; a gap this
            // large is a restart, not an old packet.
            Some(last) => {
                let d = state.seq.wrapping_sub(last) as i16;
                !(-1000..=0).contains(&d)
            }
        };
        if newer {
            *slot = Some(state.seq);
        }
        newer
    }

    pub fn reset(&mut self) {
        self.last = [None; MAX_PADS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(index: u8, seq: u16) -> GamepadState {
        GamepadState {
            index,
            seq,
            connected: true,
            ..Default::default()
        }
    }

    #[test]
    fn newest_wins_per_controller() {
        let mut g = GamepadGate::new();
        assert!(g.admit(&st(0, 5)));
        assert!(!g.admit(&st(0, 5)), "a repeat changes nothing");
        assert!(!g.admit(&st(0, 4)), "an older state is dropped");
        assert!(g.admit(&st(1, 1)), "controllers are independent");
        assert!(g.admit(&st(0, 6)));
        assert!(
            g.admit(&st(2, 65534)) && g.admit(&st(2, 0)),
            "the count wraps"
        );
        assert!(
            g.admit(&st(3, 3000)) && g.admit(&st(3, 1)),
            "a reconnected controller restarts its count"
        );
        assert!(!g.admit(&st(9, 1)), "no such slot");
    }
}
