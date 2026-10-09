//! The keyboard and mouse as a controller: for a console or game that takes
//! neither, and for aiming with the mouse in any game.
//!
//! Two layouts, by key position rather than by character, so they stay put
//! on any keyboard layout:
//!
//! - [`KeyboardMouse::Controller`], Greenlight's default (its README's
//!   "Keyboard controls"): the keys alone, enough for the console's menus;
//!   Escape and Space join Backspace and Enter as B and A.
//! - [`KeyboardMouse::Shooter`], Better xCloud's "Shooter" virtual
//!   controller (`src/utils/local-db/mkb-mapping-presets-table.ts`): WASD
//!   the left stick, the mouse the right one, its buttons the triggers.
//!
//! The mouse as a stick is Better xCloud's (`src/modules/mkb/mkb-handler.ts`
//! `handleMouseMove`): the stick leans as far as the mouse moves fast, at
//! least a fifth of the way so that slow aim gets past a game's dead zone,
//! and goes back to the centre once the mouse stops. Two things differ, for
//! reasons the tests below measure. Better xCloud reads the speed off each
//! browser frame's motion, which ties it to the display's rate; here it is
//! each mouse report's own speed, the same for a 125 Hz mouse or a
//! 1000 Hz one on any display. And it waits a fixed 50 ms to call the
//! mouse stopped; here it is three missed reports (12 to 50 ms), so the
//! aim stops sooner after the hand does.

use std::time::{Duration, Instant};

use pingpong_proto::gamepad::{button, GamepadState};
use pingpong_proto::input::Key;

use crate::input::MouseFrame;

/// What the keyboard and mouse are to the console.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyboardMouse {
    /// What Microsoft's own client does ([`resolve`](Self::resolve)): a
    /// keyboard and a mouse for a console, the controller layout for a
    /// cloud game.
    #[default]
    Auto,
    /// The keys are the first controller (Greenlight's layout); the mouse is
    /// a mouse.
    Controller,
    /// The keys and the mouse are the first controller, as a shooter is
    /// played: WASD moves, the mouse aims, its buttons fire.
    Shooter,
    /// A keyboard and a mouse, for games that take them (on Xbox Cloud
    /// Gaming, those marked "mouse and keyboard").
    Native,
}

impl KeyboardMouse {
    /// What [`Auto`](Self::Auto) is for a console (`console`) or a cloud
    /// game; the other modes are themselves.
    ///
    /// A console takes a keyboard and a mouse: its dashboard is driven by
    /// the keys, and games made for them (Battlefield, Call of Duty) play
    /// with them. Microsoft's web client sends a console both with its
    /// mouse and keyboard setting on (the keys once it holds the keyboard,
    /// in full screen; Better xCloud has to patch `homeConsoleConnect` to
    /// turn them off), and Battlefield 6 on a console aims with the mouse
    /// sent as a mouse. A
    /// game in the cloud mostly takes neither (the web client sends them
    /// only to titles marked "mouse and keyboard", which Ping's catalogue
    /// does not read), so there the keys are a controller.
    pub fn resolve(self, console: bool) -> KeyboardMouse {
        match self {
            KeyboardMouse::Auto if console => KeyboardMouse::Native,
            KeyboardMouse::Auto => KeyboardMouse::Controller,
            mode => mode,
        }
    }
}

/// What presses a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Key(Key),
    /// A mouse button, as the DOM's `buttons` bit (1 left, 2 right).
    Mouse(u8),
}

/// What a key or button is on the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PadControl {
    /// XInput button bits.
    Button(u32),
    LeftTrigger,
    RightTrigger,
    /// A stick axis ([`LEFT_X`] ...) pushed all the way, +1 or -1 (up and
    /// right are positive, as XInput's).
    Axis(usize, i8),
}

const LEFT_X: usize = 0;
const LEFT_Y: usize = 1;
const RIGHT_X: usize = 2;
const RIGHT_Y: usize = 3;

const fn key(k: Key) -> Source {
    Source::Key(k)
}

/// Greenlight's layout, and the Windows key (Command on a Mac) the Xbox
/// button, as it is on a keyboard plugged into a console.
const CONTROLLER: [(Source, PadControl); 22] = {
    use PadControl::*;
    [
        (key(Key::Enter), Button(button::A)),
        (key(Key::NumpadEnter), Button(button::A)),
        (key(Key::Space), Button(button::A)),
        (key(Key::Backspace), Button(button::B)),
        (key(Key::Escape), Button(button::B)),
        (key(Key::KeyX), Button(button::X)),
        (key(Key::KeyY), Button(button::Y)),
        (key(Key::ArrowUp), Button(button::DPAD_UP)),
        (key(Key::ArrowDown), Button(button::DPAD_DOWN)),
        (key(Key::ArrowLeft), Button(button::DPAD_LEFT)),
        (key(Key::ArrowRight), Button(button::DPAD_RIGHT)),
        (key(Key::BracketLeft), Button(button::LEFT_SHOULDER)),
        (key(Key::BracketRight), Button(button::RIGHT_SHOULDER)),
        (key(Key::KeyL), Button(button::LEFT_THUMB)),
        (key(Key::KeyR), Button(button::RIGHT_THUMB)),
        (key(Key::KeyM), Button(button::START)),
        (key(Key::KeyV), Button(button::BACK)),
        (key(Key::KeyN), Button(button::GUIDE)),
        (key(Key::SuperLeft), Button(button::GUIDE)),
        (key(Key::SuperRight), Button(button::GUIDE)),
        (key(Key::Minus), LeftTrigger),
        (key(Key::Equal), RightTrigger),
    ]
};

/// Better xCloud's "Shooter" layout, and the Windows key the Xbox button.
const SHOOTER: [(Source, PadControl); 30] = {
    use PadControl::*;
    [
        (key(Key::Backquote), Button(button::GUIDE)),
        (key(Key::SuperLeft), Button(button::GUIDE)),
        (key(Key::SuperRight), Button(button::GUIDE)),
        (key(Key::ArrowUp), Button(button::DPAD_UP)),
        (key(Key::ArrowDown), Button(button::DPAD_DOWN)),
        (key(Key::ArrowLeft), Button(button::DPAD_LEFT)),
        (key(Key::ArrowRight), Button(button::DPAD_RIGHT)),
        (key(Key::KeyW), Axis(LEFT_Y, 1)),
        (key(Key::KeyS), Axis(LEFT_Y, -1)),
        (key(Key::KeyA), Axis(LEFT_X, -1)),
        (key(Key::KeyD), Axis(LEFT_X, 1)),
        (key(Key::KeyI), Axis(RIGHT_Y, 1)),
        (key(Key::KeyK), Axis(RIGHT_Y, -1)),
        (key(Key::KeyJ), Axis(RIGHT_X, -1)),
        (key(Key::KeyL), Axis(RIGHT_X, 1)),
        (key(Key::Space), Button(button::A)),
        (key(Key::KeyE), Button(button::A)),
        (key(Key::KeyR), Button(button::X)),
        (key(Key::ControlLeft), Button(button::B)),
        (key(Key::Backspace), Button(button::B)),
        (key(Key::KeyV), Button(button::Y)),
        (key(Key::Enter), Button(button::START)),
        (key(Key::Tab), Button(button::BACK)),
        (key(Key::KeyC), Button(button::LEFT_SHOULDER)),
        (key(Key::KeyG), Button(button::LEFT_SHOULDER)),
        (key(Key::KeyQ), Button(button::RIGHT_SHOULDER)),
        (Source::Mouse(1), RightTrigger),
        (Source::Mouse(2), LeftTrigger),
        (key(Key::ShiftLeft), Button(button::LEFT_THUMB)),
        (key(Key::KeyF), Button(button::RIGHT_THUMB)),
    ]
};

/// The keyboard (and, in the shooter's layout, the mouse) as a controller:
/// what is held. Merged with the first controller, so both work at once
/// (as Greenlight merges them, `Channel/Input.ts` `mergeState`).
#[derive(Debug, Clone)]
pub struct KeyboardPad {
    layout: &'static [(Source, PadControl)],
    /// Bit `i`: `layout[i]` is held.
    held: u64,
    /// Each stick axis as the keys lean it: the way of the key pressed last
    /// of those held (Better xCloud's), or 0.
    axes: [i8; 4],
    /// The mouse's buttons as last seen.
    mouse_buttons: u8,
    /// The mouse as the right stick, in the shooter's layout.
    stick: Option<MouseStick>,
}

impl KeyboardPad {
    /// The controller for `mode`; none for a keyboard and mouse. `Auto` is
    /// resolved before a stream starts; unresolved, it is the cloud's.
    pub fn new(mode: KeyboardMouse) -> Option<KeyboardPad> {
        let (layout, stick): (&'static [_], _) = match mode {
            KeyboardMouse::Auto | KeyboardMouse::Controller => (&CONTROLLER, None),
            KeyboardMouse::Shooter => (&SHOOTER, Some(MouseStick::default())),
            KeyboardMouse::Native => return None,
        };
        Some(KeyboardPad {
            layout,
            held: 0,
            axes: [0; 4],
            mouse_buttons: 0,
            stick,
        })
    }

    /// A key went down or up. Whether it is one of the controller's (and so
    /// not a key the console should see).
    pub fn key(&mut self, key: Key, down: bool) -> bool {
        self.press(Source::Key(key), down)
    }

    /// Whether the mouse is part of the controller, rather than the
    /// console's mouse.
    pub fn takes_mouse(&self) -> bool {
        self.stick.is_some()
    }

    /// The mouse moved, or its buttons changed. Whether the controller did.
    pub fn mouse(&mut self, m: &MouseFrame, now: Instant) -> bool {
        let mut changed = false;
        let flipped = self.mouse_buttons ^ m.buttons;
        for bit in (0..8).map(|i| 1u8 << i).filter(|b| flipped & b != 0) {
            changed |= self.press(Source::Mouse(bit), m.buttons & bit != 0);
        }
        self.mouse_buttons = m.buttons;
        if let Some(stick) = &mut self.stick {
            if m.dx != 0 || m.dy != 0 {
                changed |= stick.motion(m.dx, m.dy, now);
            }
        }
        changed
    }

    /// Time passed: whether the controller changed (the mouse's stick
    /// eased, or went back to the centre).
    pub fn tick(&mut self, now: Instant) -> bool {
        self.stick.as_mut().is_some_and(|s| s.tick(now))
    }

    /// When [`tick`](Self::tick) is next due, while the mouse's stick leans.
    pub fn next_tick(&self) -> Option<Instant> {
        self.stick.as_ref().and_then(|s| s.next_sample)
    }

    /// Nothing held, the mouse's stick in the centre.
    pub fn is_idle(&self) -> bool {
        self.held == 0 && self.stick.as_ref().is_none_or(|s| s.axes == (0, 0))
    }

    fn press(&mut self, source: Source, down: bool) -> bool {
        let mut found = false;
        for (i, &(s, control)) in self.layout.iter().enumerate() {
            if s != source {
                continue;
            }
            found = true;
            if down {
                self.held |= 1 << i;
            } else {
                self.held &= !(1 << i);
            }
            if let PadControl::Axis(axis, way) = control {
                if down {
                    self.axes[axis] = way;
                } else if self.axes[axis] == way && !self.holds(control) {
                    let other = PadControl::Axis(axis, -way);
                    self.axes[axis] = if self.holds(other) { -way } else { 0 };
                }
            }
        }
        found
    }

    fn holds(&self, control: PadControl) -> bool {
        self.layout
            .iter()
            .enumerate()
            .any(|(i, &(_, c))| c == control && self.held & (1 << i) != 0)
    }

    /// `pad` (the first controller, or an idle one) with the keyboard's and
    /// the mouse's controls on it too. A stick they lean wins over the
    /// controller's.
    pub fn merged(&self, pad: &GamepadState) -> GamepadState {
        let mut merged = *pad;
        for (i, &(_, control)) in self.layout.iter().enumerate() {
            if self.held & (1 << i) == 0 {
                continue;
            }
            match control {
                PadControl::Button(b) => merged.buttons |= b,
                PadControl::LeftTrigger => merged.left_trigger = u8::MAX,
                PadControl::RightTrigger => merged.right_trigger = u8::MAX,
                PadControl::Axis(..) => {}
            }
        }
        let leaned = |axis: usize, v: &mut i16| {
            if self.axes[axis] != 0 {
                *v = i16::from(self.axes[axis]) * i16::MAX;
            }
        };
        leaned(LEFT_X, &mut merged.left_x);
        leaned(LEFT_Y, &mut merged.left_y);
        leaned(RIGHT_X, &mut merged.right_x);
        leaned(RIGHT_Y, &mut merged.right_y);
        if let Some(s) = self.stick.as_ref().filter(|s| s.axes != (0, 0)) {
            (merged.right_x, merged.right_y) = s.axes;
        }
        merged
    }
}

/// How fast the mouse moves for the stick to lean all the way, in pixels a
/// second. Better xCloud's sensitivity 100 leans the stick a tenth of the
/// way per CSS pixel of a frame's motion: all the way at 600 CSS pixels a
/// second on a 60 Hz display, which is 1200 of a Retina Mac's pixels (two
/// to the point), the pixels Ping's motion is in.
const FULL_SPEED: f64 = 1200.0;

/// The least the stick leans while the mouse moves: Better xCloud's
/// "deadzone counterweight" (20, a fifth), so that slow aim is not lost in
/// a game's dead zone.
const LEAST_LEAN: f64 = 0.2;

/// The most, before each axis stops at its end: Better xCloud's 1.1, so a
/// diagonal reaches the corners.
const MOST_LEAN: f64 = 1.1;

/// The mouse's speed, each report's (its motion over the time since the
/// one before, exact for a mouse moving steadily at any report rate), is
/// smoothed over this long: enough to even out reports that arrive a
/// millisecond early or late, little enough to add only a few
/// milliseconds.
const SMOOTHING: Duration = Duration::from_millis(8);

/// A mouse that has missed this many of its reports has stopped, and the
/// stick goes back to the centre.
const MISSED_REPORTS: u32 = 3;

/// The stopping time's bounds. Better xCloud waits a fixed 50 ms
/// (`onMouseStopped`), leaving the aim moving for 50 ms after the hand has
/// stopped; three reports of a 1000 Hz mouse are 3 ms, too few to ride out
/// a late one.
const STOPPED_AFTER: (Duration, Duration) = (Duration::from_millis(12), Duration::from_millis(50));

/// A mouse's report interval until one is measured: USB's default for a
/// mouse, 125 Hz.
const USB_INTERVAL: Duration = Duration::from_millis(8);

/// How often the stick is sampled while it leans. A console reads input
/// at 60 or 120 Hz; at 250 Hz a change waits 2 ms on average, and a
/// 1000 Hz mouse sends the console a quarter of the reports it would.
const SAMPLE_EVERY: Duration = Duration::from_millis(4);

/// The mouse as a stick.
#[derive(Debug, Clone)]
struct MouseStick {
    /// Pixels a second, y down as on the screen, smoothed.
    speed: (f64, f64),
    /// The time between the mouse's reports while it moves, smoothed.
    interval: Duration,
    last_motion: Option<Instant>,
    next_sample: Option<Instant>,
    /// As last sampled: XInput's axes, up positive.
    axes: (i16, i16),
}

impl Default for MouseStick {
    fn default() -> Self {
        MouseStick {
            speed: (0.0, 0.0),
            interval: USB_INTERVAL,
            last_motion: None,
            next_sample: None,
            axes: (0, 0),
        }
    }
}

impl MouseStick {
    /// The mouse moved (y down, as on the screen). Whether the stick did.
    fn motion(&mut self, dx: i32, dy: i32, now: Instant) -> bool {
        let (dx, dy) = (f64::from(dx), f64::from(dy));
        match self.last_motion.filter(|_| self.is_moving(now)) {
            Some(last) => {
                let gap = now
                    .saturating_duration_since(last)
                    .max(Duration::from_millis(1));
                self.interval = (self.interval * 3 + gap) / 4;
                let k = 1.0 - (-gap.as_secs_f64() / SMOOTHING.as_secs_f64()).exp();
                let secs = gap.as_secs_f64();
                self.speed.0 += (dx / secs - self.speed.0) * k;
                self.speed.1 += (dy / secs - self.speed.1) * k;
            }
            // The first report after the mouse was still has no report
            // before it to time it from: it took one report interval.
            None => {
                let secs = self.interval.as_secs_f64();
                self.speed = (dx / secs, dy / secs);
            }
        }
        self.last_motion = Some(now);
        match self.next_sample {
            Some(t) if now < t => false,
            _ => self.sample(now),
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        match self.next_sample {
            Some(t) if now >= t => self.sample(now),
            _ => false,
        }
    }

    /// Whether the mouse has reported recently enough to still be moving.
    fn is_moving(&self, now: Instant) -> bool {
        let stopped_after =
            (self.interval * MISSED_REPORTS).clamp(STOPPED_AFTER.0, STOPPED_AFTER.1);
        self.last_motion
            .is_some_and(|t| now.saturating_duration_since(t) < stopped_after)
    }

    fn sample(&mut self, now: Instant) -> bool {
        let axes = if self.is_moving(now) {
            self.next_sample = Some(now + SAMPLE_EVERY);
            lean(self.speed.0, self.speed.1)
        } else {
            self.speed = (0.0, 0.0);
            self.last_motion = None;
            self.next_sample = None;
            (0, 0)
        };
        let changed = axes != self.axes;
        self.axes = axes;
        changed
    }
}

/// The stick's axes for the mouse moving at (`vx`, `vy`) pixels a second,
/// y down as on the screen.
fn lean(vx: f64, vy: f64) -> (i16, i16) {
    let (x, y) = (vx / FULL_SPEED, -vy / FULL_SPEED);
    let len = x.hypot(y);
    if len == 0.0 {
        return (0, 0);
    }
    let scale = if len < LEAST_LEAN {
        LEAST_LEAN / len
    } else if len > MOST_LEAN {
        MOST_LEAN / len
    } else {
        1.0
    };
    let axis = |a: f64| ((a * scale).clamp(-1.0, 1.0) * f64::from(i16::MAX)).round() as i16;
    (axis(x), axis(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shooter() -> KeyboardPad {
        KeyboardPad::new(KeyboardMouse::Shooter).unwrap()
    }

    fn moved(dx: i32, dy: i32) -> MouseFrame {
        MouseFrame {
            dx,
            dy,
            ..Default::default()
        }
    }

    /// Moves the mouse `dx` pixels every `every` for `over`, ticking as the
    /// connection would; the right stick's x at the end, and when the
    /// mouse last reported.
    fn steady(
        pad: &mut KeyboardPad,
        t0: Instant,
        dx: i32,
        every: Duration,
        over: Duration,
    ) -> (i16, Instant) {
        let mut t = t0;
        let mut last = t0;
        while t < t0 + over {
            pad.mouse(&moved(dx, 0), t);
            last = t;
            t += every;
            while let Some(due) = pad.next_tick().filter(|&d| d < t) {
                pad.tick(due);
            }
        }
        (pad.merged(&GamepadState::default()).right_x, last)
    }

    fn lean_of(axis: i16) -> f64 {
        f64::from(axis) / f64::from(i16::MAX)
    }

    #[test]
    fn the_windows_key_is_the_xbox_button_in_both_layouts() {
        for mode in [KeyboardMouse::Controller, KeyboardMouse::Shooter] {
            for key in [Key::SuperLeft, Key::SuperRight] {
                let mut kb = KeyboardPad::new(mode).unwrap();
                assert!(kb.key(key, true), "{mode:?} {key:?}");
                let m = kb.merged(&GamepadState::default());
                assert_eq!(m.buttons, button::GUIDE, "{mode:?} {key:?}");
                kb.key(key, false);
                assert_eq!(kb.merged(&GamepadState::default()).buttons, 0);
            }
        }
    }

    #[test]
    fn the_keyboard_holds_controller_buttons_on_top_of_the_first_pad() {
        let mut kb = KeyboardPad::new(KeyboardMouse::Controller).unwrap();
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
        let mut kb = KeyboardPad::new(KeyboardMouse::Controller).unwrap();
        kb.key(Key::Enter, true);
        kb.key(Key::Space, true);
        kb.key(Key::Space, false);
        assert_eq!(kb.merged(&GamepadState::default()).buttons, button::A);
        kb.key(Key::Enter, false);
        assert_eq!(kb.merged(&GamepadState::default()).buttons, 0);
    }

    #[test]
    fn automatic_is_a_keyboard_and_mouse_for_a_console_and_a_controller_in_the_cloud() {
        assert_eq!(KeyboardMouse::default(), KeyboardMouse::Auto);
        assert_eq!(KeyboardMouse::Auto.resolve(true), KeyboardMouse::Native);
        assert_eq!(
            KeyboardMouse::Auto.resolve(false),
            KeyboardMouse::Controller
        );
        for mode in [
            KeyboardMouse::Controller,
            KeyboardMouse::Shooter,
            KeyboardMouse::Native,
        ] {
            assert_eq!(mode.resolve(true), mode);
            assert_eq!(mode.resolve(false), mode);
        }
    }

    #[test]
    fn a_keyboard_and_mouse_have_no_controller() {
        assert!(KeyboardPad::new(KeyboardMouse::Native).is_none());
        assert!(!KeyboardPad::new(KeyboardMouse::Controller)
            .unwrap()
            .takes_mouse());
        assert!(shooter().takes_mouse());
    }

    #[test]
    fn every_layout_fits_the_held_bits() {
        assert!(CONTROLLER.len() <= 64 && SHOOTER.len() <= 64);
    }

    #[test]
    fn wasd_lean_the_left_stick_and_the_key_pressed_last_wins() {
        let mut kb = shooter();
        let left = |kb: &KeyboardPad| {
            let m = kb.merged(&GamepadState::default());
            (m.left_x, m.left_y)
        };
        kb.key(Key::KeyW, true);
        kb.key(Key::KeyD, true);
        assert_eq!(left(&kb), (i16::MAX, i16::MAX));
        kb.key(Key::KeyA, true);
        assert_eq!(left(&kb), (-i16::MAX, i16::MAX));
        kb.key(Key::KeyA, false);
        assert_eq!(left(&kb), (i16::MAX, i16::MAX), "D is still held");
        kb.key(Key::KeyD, false);
        kb.key(Key::KeyW, false);
        assert_eq!(left(&kb), (0, 0));
        assert!(kb.is_idle());
    }

    #[test]
    fn the_mouse_buttons_are_the_triggers() {
        let mut kb = shooter();
        let t = Instant::now();
        let buttons = |b| MouseFrame {
            buttons: b,
            ..Default::default()
        };
        assert!(kb.mouse(&buttons(1), t));
        let m = kb.merged(&GamepadState::default());
        assert_eq!((m.left_trigger, m.right_trigger), (0, 255));
        kb.mouse(&buttons(3), t);
        let m = kb.merged(&GamepadState::default());
        assert_eq!((m.left_trigger, m.right_trigger), (255, 255));
        assert!(!kb.mouse(&buttons(7), t), "the middle button is nothing");
        kb.mouse(&buttons(0), t);
        assert!(kb.is_idle());
    }

    #[test]
    fn the_mouse_leans_the_right_stick_as_far_as_it_moves_fast() {
        // 600 pixels a second: half way.
        let mut kb = shooter();
        let (x, _) = steady(
            &mut kb,
            Instant::now(),
            3,
            Duration::from_millis(5),
            Duration::from_millis(200),
        );
        assert!((lean_of(x) - 0.5).abs() < 0.01, "{}", lean_of(x));
        // Moving down leans it down.
        let mut kb = shooter();
        kb.mouse(&moved(0, 10), Instant::now());
        assert!(kb.merged(&GamepadState::default()).right_y < 0);
    }

    #[test]
    fn slow_aim_still_leans_a_fifth_and_fast_aim_stops_at_the_end() {
        let t0 = Instant::now();
        let mut kb = shooter();
        kb.mouse(&moved(1, 0), t0);
        let x = kb.merged(&GamepadState::default()).right_x;
        assert!((lean_of(x) - LEAST_LEAN).abs() < 0.01, "{}", lean_of(x));
        let mut kb = shooter();
        let (x, _) = steady(
            &mut kb,
            t0,
            50,
            Duration::from_millis(1),
            Duration::from_millis(100),
        );
        assert_eq!(x, i16::MAX);
    }

    #[test]
    fn a_mouse_of_any_rate_leans_the_stick_the_same() {
        // 1000 pixels a second from a 1000 Hz mouse and a 125 Hz one. Better
        // xCloud's would lean twice as far at a 120 Hz display as at 60.
        let t0 = Instant::now();
        let (fast, _) = steady(
            &mut shooter(),
            t0,
            1,
            Duration::from_millis(1),
            Duration::from_millis(200),
        );
        let (slow, _) = steady(
            &mut shooter(),
            t0,
            8,
            Duration::from_millis(8),
            Duration::from_millis(200),
        );
        let want = 1000.0 / FULL_SPEED;
        for x in [fast, slow] {
            assert!((lean_of(x) - want).abs() < 0.01, "{} vs {want}", lean_of(x));
        }
    }

    /// Moves a mouse that reports `every` so long at 600 pixels a second
    /// for 100 ms, then stops it: how long after its last report the stick
    /// is back in the centre.
    fn time_to_centre(every: Duration) -> Duration {
        let mut kb = shooter();
        let dx = (600.0 * every.as_secs_f64()).round() as i32;
        let (_, last) = steady(
            &mut kb,
            Instant::now(),
            dx,
            every,
            Duration::from_millis(100),
        );
        while let Some(due) = kb.next_tick() {
            kb.tick(due);
            if kb.merged(&GamepadState::default()).right_x == 0 {
                return due.saturating_duration_since(last);
            }
        }
        unreachable!("the stick stays out")
    }

    #[test]
    fn the_stick_goes_back_to_the_centre_three_reports_after_the_mouse_stops() {
        // Better xCloud's is 50 ms for any mouse.
        let fast = time_to_centre(Duration::from_millis(1));
        let usb = time_to_centre(Duration::from_millis(8));
        assert!(fast <= STOPPED_AFTER.0 + SAMPLE_EVERY, "{fast:?}");
        assert!(
            usb <= 3 * Duration::from_millis(8) + SAMPLE_EVERY,
            "{usb:?}"
        );
        assert!(fast >= STOPPED_AFTER.0 && usb >= 3 * Duration::from_millis(8));
    }

    #[test]
    fn the_stick_holds_steady_between_a_slow_mouses_reports() {
        // A 125 Hz mouse at 625 pixels a second (5 a report): every sample,
        // between reports too, leans the stick 625/1200 of the way.
        let t0 = Instant::now();
        let every = Duration::from_millis(8);
        let mut kb = shooter();
        let mut samples = 0;
        for i in 0..40 {
            let t = t0 + every * i;
            kb.mouse(&moved(5, 0), t);
            while let Some(due) = kb.next_tick().filter(|&d| d < t + every) {
                kb.tick(due);
                if i >= 10 {
                    let x = lean_of(kb.merged(&GamepadState::default()).right_x);
                    assert!((x - 625.0 / FULL_SPEED).abs() < 0.01, "{x}");
                    samples += 1;
                }
            }
        }
        assert!(samples >= 30, "{samples}");
    }

    #[test]
    fn the_mouse_wins_over_the_controllers_right_stick_while_it_moves() {
        let mut kb = shooter();
        let pad = GamepadState {
            right_x: -5000,
            right_y: 7000,
            ..Default::default()
        };
        assert_eq!(kb.merged(&pad), pad);
        kb.mouse(&moved(30, 0), Instant::now());
        let m = kb.merged(&pad);
        assert!(m.right_x > 0);
        assert_eq!(m.right_y, 0);
    }
}
