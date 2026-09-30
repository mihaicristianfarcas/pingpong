//! Game controllers, the platform-neutral part: each controller's state goes
//! to the host when it changes and again every 100 ms (a lost packet is then
//! corrected without waiting for the next change), and holding Start alone
//! for a second turns the controller into a mouse (Moonlight's gamepad mouse
//! emulation). The platforms read their controllers and play the host's
//! rumble (`mac::gamepad`, `win::gamepad`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_proto::gamepad::{button, GamepadState};
use pingpong_proto::input::{Button, InputEvent};

use crate::input::InputSender;
use crate::pointer::{CursorDraw, PointerState};

pub const REPEAT: Duration = Duration::from_millis(100);
/// The host repeats a running motor's state every 200 ms; silence this long
/// means the stream is gone, so stop.
pub const RUMBLE_TIMEOUT: Duration = Duration::from_secs(1);
/// Hold Start this long to switch between pad and mouse (Moonlight's).
const MOUSE_TOGGLE_HOLD: Duration = Duration::from_millis(1000);
/// Full stick deflection moves the pointer this fast, in stream pixels.
const MOUSE_SPEED: f64 = 1800.0;
/// Full deflection of the right stick scrolls this many notches a second.
const SCROLL_NOTCHES: f64 = 20.0;
const STICK_DEADZONE: f64 = 0.2;

/// The host's rumble for controller `index`: (index, low, high).
pub type Rumble = (u8, u16, u16);

pub fn rumble_channel() -> (
    crossbeam_channel::Sender<Rumble>,
    crossbeam_channel::Receiver<Rumble>,
) {
    crossbeam_channel::bounded(64)
}

/// What a controller in mouse mode drives: the pointer the mouse drives.
pub struct PadMouse {
    pub pointer: Arc<parking_lot::Mutex<PointerState>>,
    /// Puts the pointer on the screen where it moved to.
    pub show: Arc<dyn Fn(CursorDraw) + Send + Sync>,
    pub input: InputSender,
}

/// A stick axis (-32768..32767) as -1..1, with a dead zone and a curve that
/// keeps small movements fine.
pub fn stick(v: i16) -> f64 {
    let v = v as f64 / 32767.0;
    let mag = ((v.abs() - STICK_DEADZONE) / (1.0 - STICK_DEADZONE)).clamp(0.0, 1.0);
    mag * mag * v.signum()
}

/// One connected controller's sending state.
pub struct PadSlot {
    state: GamepadState,
    sent: Instant,
    /// Driving the pointer instead of a pad.
    mouse_mode: bool,
    /// Since when Start has been held, and whether that hold already toggled
    /// mouse mode (its Start then never reaches the host).
    start_since: Option<Instant>,
    start_toggled: bool,
    /// Buttons as last seen in mouse mode, for clicks on the edges.
    mouse_buttons: u32,
    scroll_residue: f64,
}

impl PadSlot {
    pub fn new(now: Instant) -> PadSlot {
        PadSlot {
            state: GamepadState::default(),
            sent: now - REPEAT,
            mouse_mode: false,
            start_since: None,
            start_toggled: false,
            mouse_buttons: 0,
            scroll_residue: 0.0,
        }
    }

    /// The controller reads `state` now (`dt` since the last poll): what to
    /// send the host, if anything. `seq` is the slot's sequence number, which
    /// outlives a controller.
    pub fn update(
        &mut self,
        index: u8,
        mut state: GamepadState,
        seq: &mut u16,
        now: Instant,
        dt: f64,
        mouse: Option<&PadMouse>,
    ) -> Option<GamepadState> {
        state.index = index;
        state.connected = true;
        if let Some(mouse) = mouse {
            // Start held on its own for a second switches pad <-> mouse.
            let start_alone = state.buttons == button::START;
            match self.start_since {
                _ if !start_alone && state.buttons & button::START == 0 => {
                    self.start_since = None;
                    self.start_toggled = false;
                }
                None if start_alone => self.start_since = Some(now),
                Some(t)
                    if start_alone
                        && !self.start_toggled
                        && now.duration_since(t) >= MOUSE_TOGGLE_HOLD =>
                {
                    self.start_toggled = true;
                    self.mouse_mode = !self.mouse_mode;
                    if !self.mouse_mode {
                        self.release_mouse(mouse);
                    }
                    tracing::info!(
                        slot = index,
                        mouse = self.mouse_mode,
                        "controller mouse mode"
                    );
                }
                _ => {}
            }
            if self.start_toggled {
                // This hold was a toggle: its Start never reaches a game.
                state.buttons &= !button::START;
            }
            if self.mouse_mode {
                self.drive_mouse(mouse, &state, dt);
                // The host's pad rests while the controller is a mouse.
                state = GamepadState {
                    index,
                    connected: true,
                    ..Default::default()
                };
            }
        }
        if state.same_input(&self.state) && now.duration_since(self.sent) < REPEAT {
            return None;
        }
        *seq = seq.wrapping_add(1);
        state.seq = *seq;
        self.state = state;
        self.sent = now;
        Some(state)
    }

    /// The controller went away: let go of what mouse mode holds, and unplug
    /// the host's pad.
    pub fn disconnect(
        &mut self,
        index: u8,
        seq: &mut u16,
        mouse: Option<&PadMouse>,
    ) -> GamepadState {
        if let Some(mouse) = mouse {
            self.release_mouse(mouse);
        }
        *seq = seq.wrapping_add(1);
        GamepadState {
            index,
            seq: *seq,
            connected: false,
            ..Default::default()
        }
    }

    /// A controller in mouse mode, one poll: move, scroll, click.
    fn drive_mouse(&mut self, mouse: &PadMouse, state: &GamepadState, dt: f64) {
        let (dx, dy) = (
            stick(state.left_x) * MOUSE_SPEED * dt,
            -stick(state.left_y) * MOUSE_SPEED * dt,
        );
        if dx != 0.0 || dy != 0.0 {
            mouse
                .pointer
                .lock()
                .nudge(dx, dy, &mouse.input, &*mouse.show);
        }
        self.scroll_residue += stick(state.right_y) * SCROLL_NOTCHES * 120.0 * dt;
        let whole = self.scroll_residue.trunc();
        if whole.abs() >= 1.0 {
            self.scroll_residue -= whole;
            mouse.input.send(InputEvent::Wheel {
                dv: whole as i16,
                dh: 0,
            });
        }
        for (bit, b) in [
            (button::A, Button::Left),
            (button::B, Button::Right),
            (button::X, Button::Middle),
        ] {
            let (was, is) = (self.mouse_buttons & bit != 0, state.buttons & bit != 0);
            if is != was {
                mouse.input.send(if is {
                    InputEvent::ButtonDown(b)
                } else {
                    InputEvent::ButtonUp(b)
                });
            }
        }
        self.mouse_buttons = state.buttons;
    }

    /// Let go of any button mouse mode is holding.
    fn release_mouse(&mut self, mouse: &PadMouse) {
        for (bit, b) in [
            (button::A, Button::Left),
            (button::B, Button::Right),
            (button::X, Button::Middle),
        ] {
            if self.mouse_buttons & bit != 0 {
                mouse.input.send(InputEvent::ButtonUp(b));
            }
        }
        self.mouse_buttons = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stick_curve_rests_in_its_dead_zone_and_reaches_full_speed() {
        assert_eq!(stick(0), 0.0);
        assert_eq!(stick(6000), 0.0, "inside the dead zone");
        assert!((stick(i16::MAX) - 1.0).abs() < 1e-9);
        assert!((stick(i16::MIN) + 1.0).abs() < 1e-3);
        // Half way out moves at well under half speed: fine control.
        let half = stick(16384);
        assert!(half > 0.05 && half < 0.2, "{half}");
    }

    #[test]
    fn changes_go_at_once_and_repeats_every_100_ms() {
        let t0 = Instant::now();
        let mut slot = PadSlot::new(t0);
        let mut seq = 0;
        let a = GamepadState {
            buttons: button::A,
            ..Default::default()
        };
        let sent = slot
            .update(1, a, &mut seq, t0, 0.004, None)
            .expect("new state goes out");
        assert_eq!((sent.index, sent.seq, sent.connected), (1, 1, true));
        assert!(slot
            .update(1, a, &mut seq, t0 + Duration::from_millis(50), 0.004, None)
            .is_none());
        assert!(
            slot.update(1, a, &mut seq, t0 + Duration::from_millis(101), 0.004, None)
                .is_some(),
            "the repeat"
        );
        let gone = slot.disconnect(1, &mut seq, None);
        assert_eq!((gone.connected, gone.seq), (false, 3));
    }
}
