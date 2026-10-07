//! Input on its way to the console: what the platform layer hands over
//! (controller states, keys, mouse motion) turned into the input channel's
//! reports and the control channel's "a controller was plugged in".
//!
//! Pure, so the rules are tested without a connection:
//!
//! - **Controllers** are whole states; the newest per controller wins, and
//!   each is sent again every [`PAD_HEARTBEAT`] while plugged in, as the web
//!   client does (Greenlight's `input/queue.ts`: "send at least every
//!   33 ms"), though the channel is reliable.
//! - **The keyboard and mouse** are part of the first controller
//!   ([`KeyboardMouse`]: the keys alone, Greenlight's default, or the keys
//!   and the mouse as a shooter plays) or a keyboard and a mouse (Windows
//!   key codes, for games that take them).
//! - **Mouse** motion between button changes is summed into one frame, so
//!   a burst of motion is one frame and a click is never merged away.
//! - Controller 0 is announced when the stream starts (the keyboard is a
//!   controller then, and games wait for one); others when they appear.

use std::time::{Duration, Instant};

use pingpong_proto::gamepad::GamepadState;
use pingpong_proto::input::Key;

use crate::input::{
    FrameTimes, KeyFrame, MouseFrame, PadFrame, Report, ReportWriter, MAX_PADS, MAX_REPORT_LEN,
};
use crate::keymap::vk;
use crate::messages::control;
use crate::virtual_pad::{KeyboardMouse, KeyboardPad};

/// The web client sends the browser's mouse movement doubled
/// (Greenlight's `input/mousekeyboard.ts`); a console has only been seen
/// with that, so Ping's is doubled too.
const MOUSE_SCALE: i32 = 2;

/// A controller's state is sent at least this often (the web client's 33 ms).
pub const PAD_HEARTBEAT: Duration = Duration::from_millis(33);

/// Frame timings are sent once this many have gathered, unless input goes
/// out sooner (Greenlight batches more than five).
const METADATA_BATCH: usize = 6;

const MAX_KEYS: usize = 16;
const MAX_MOUSE: usize = 4;
const MAX_METADATA: usize = 8;

/// Input from the platform layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A controller's whole state; `connected: false` once it is unplugged.
    Pad(GamepadState),
    /// A key went down or up (positionally, as Ping's keys are).
    Key { key: Key, down: bool },
    /// Mouse motion (in pixels) and wheel since the last, and the buttons
    /// now (the DOM's bits: 1 left, 2 right, 4 middle).
    Mouse(MouseFrame),
}

/// Everything input-side the connection keeps.
pub struct InputState {
    keyboard_pad: Option<KeyboardPad>,
    pads: [GamepadState; MAX_PADS as usize],
    announced: [bool; MAX_PADS as usize],
    /// Controllers whose state changed since it was last sent.
    dirty: u8,
    last_sent: [Option<Instant>; MAX_PADS as usize],
    keys: Vec<KeyFrame>,
    mouse: Vec<MouseFrame>,
    metadata: Vec<FrameTimes>,
    control: Vec<String>,
    writer: ReportWriter,
}

impl InputState {
    /// `keyboard_mouse`: what the keyboard and mouse are to the console.
    pub fn new(keyboard_mouse: KeyboardMouse) -> InputState {
        InputState {
            keyboard_pad: KeyboardPad::new(keyboard_mouse),
            pads: std::array::from_fn(|i| GamepadState {
                index: i as u8,
                ..GamepadState::default()
            }),
            announced: [false; MAX_PADS as usize],
            dirty: 0,
            last_sent: [None; MAX_PADS as usize],
            keys: Vec::with_capacity(MAX_KEYS),
            mouse: Vec::with_capacity(MAX_MOUSE),
            metadata: Vec::with_capacity(MAX_METADATA),
            control: Vec::new(),
            writer: ReportWriter::new(),
        }
    }

    /// The stream is ready for input: announce the first controller.
    pub fn start(&mut self) {
        self.announce(0, true);
        self.dirty |= 1;
    }

    fn announce(&mut self, index: u8, added: bool) {
        let slot = &mut self.announced[index as usize];
        if *slot != added {
            *slot = added;
            self.control.push(control::gamepad_changed(index, added));
        }
    }

    pub fn apply(&mut self, input: Input, now: Instant) {
        match input {
            Input::Pad(state) => {
                let i = state.index;
                if i >= MAX_PADS {
                    return;
                }
                if state.connected {
                    self.announce(i, true);
                    self.pads[i as usize] = state;
                } else {
                    // The first controller stays: the keyboard and the
                    // console's idea of player one live on it.
                    if i != 0 {
                        self.announce(i, false);
                    }
                    self.pads[i as usize] = GamepadState {
                        index: i,
                        ..GamepadState::default()
                    };
                }
                self.dirty |= 1 << i;
            }
            Input::Key { key, down } => {
                if let Some(kb) = &mut self.keyboard_pad {
                    // Keys that are not the controller's are not sent: the
                    // console would see a keyboard only some keys of.
                    if kb.key(key, down) {
                        self.dirty |= 1;
                    }
                    return;
                }
                if self.keys.len() < MAX_KEYS * 4 {
                    self.keys.push(KeyFrame { vk: vk(key), down });
                }
            }
            Input::Mouse(m) => {
                if let Some(kb) = self.keyboard_pad.as_mut().filter(|kb| kb.takes_mouse()) {
                    if kb.mouse(&m, now) {
                        self.dirty |= 1;
                    }
                    return;
                }
                let m = MouseFrame {
                    dx: m.dx.saturating_mul(MOUSE_SCALE),
                    dy: m.dy.saturating_mul(MOUSE_SCALE),
                    ..m
                };
                let room = self.mouse.len() < MAX_MOUSE * 4;
                match self.mouse.last_mut() {
                    Some(last) if last.buttons == m.buttons => {
                        last.dx = last.dx.saturating_add(m.dx);
                        last.dy = last.dy.saturating_add(m.dy);
                        last.wheel_x = last.wheel_x.saturating_add(m.wheel_x);
                        last.wheel_y = last.wheel_y.saturating_add(m.wheel_y);
                    }
                    _ if room => self.mouse.push(m),
                    _ => {}
                }
            }
        }
    }

    /// A frame was handed to the decoder: its timings go to the console.
    pub fn frame_shown(&mut self, times: FrameTimes) {
        if self.metadata.len() < MAX_METADATA * 4 {
            self.metadata.push(times);
        }
    }

    /// Time passed: controllers due for their heartbeat are marked to be
    /// sent again, and the mouse's stick eases.
    pub fn tick(&mut self, now: Instant) {
        if self.keyboard_pad.as_mut().is_some_and(|kb| kb.tick(now)) {
            self.dirty |= 1;
        }
        for i in 0..MAX_PADS as usize {
            let due = self.last_sent[i].is_some_and(|t| now.duration_since(t) >= PAD_HEARTBEAT);
            if self.announced[i] && due {
                self.dirty |= 1 << i;
            }
        }
    }

    /// When [`tick`](Self::tick) is next due.
    pub fn next_tick(&self) -> Option<Instant> {
        let heartbeat = (0..MAX_PADS as usize)
            .filter(|&i| self.announced[i])
            .filter_map(|i| self.last_sent[i])
            .map(|t| t + PAD_HEARTBEAT)
            .min();
        let stick = self.keyboard_pad.as_ref().and_then(KeyboardPad::next_tick);
        heartbeat.into_iter().chain(stick).min()
    }

    /// The next control channel message to send.
    pub fn take_control(&mut self) -> Option<String> {
        (!self.control.is_empty()).then(|| self.control.remove(0))
    }

    /// The next report to send, written into `out`; its length. Frame
    /// timings wait for company or a batch.
    pub fn take_report(
        &mut self,
        now: Instant,
        now_ms: f64,
        out: &mut [u8; MAX_REPORT_LEN],
    ) -> Option<usize> {
        let mut pads = [PadFrame::default(); MAX_PADS as usize];
        let mut n_pads = 0;
        for i in 0..MAX_PADS as usize {
            if self.dirty & (1 << i) == 0 || !self.announced[i] {
                continue;
            }
            let state = match (&self.keyboard_pad, i) {
                (Some(kb), 0) => kb.merged(&self.pads[0]),
                _ => self.pads[i],
            };
            pads[n_pads] = PadFrame::from_state(&state);
            n_pads += 1;
            self.last_sent[i] = Some(now);
        }
        self.dirty = 0;
        let keys = self.keys.len().min(MAX_KEYS);
        let mouse = self.mouse.len().min(MAX_MOUSE);
        let input = n_pads + keys + mouse > 0;
        let metadata = if input || self.metadata.len() >= METADATA_BATCH {
            self.metadata.len().min(MAX_METADATA)
        } else {
            0
        };
        let n = self.writer.write(
            &Report {
                metadata: &self.metadata[..metadata],
                pads: &pads[..n_pads],
                mouse: &self.mouse[..mouse],
                keys: &self.keys[..keys],
            },
            now_ms,
            out,
        )?;
        self.metadata.drain(..metadata);
        self.mouse.drain(..mouse);
        self.keys.drain(..keys);
        Some(n)
    }

    /// The report the input channel opens with.
    pub fn opening_report(&mut self, now_ms: f64, out: &mut [u8; MAX_REPORT_LEN]) -> usize {
        self.writer.client_metadata(now_ms, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{parse_client_report, xbutton};
    use crate::messages::{parse_control, ControlMessage};
    use pingpong_proto::gamepad::button;

    fn reports(s: &mut InputState, now: Instant) -> Vec<crate::input::ClientReport> {
        let mut out = [0u8; MAX_REPORT_LEN];
        let mut all = Vec::new();
        while let Some(n) = s.take_report(now, 0.0, &mut out) {
            all.push(parse_client_report(&out[..n]).unwrap());
        }
        all
    }

    fn controls(s: &mut InputState) -> Vec<ControlMessage> {
        std::iter::from_fn(|| s.take_control())
            .map(|m| parse_control(m.as_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn the_first_controller_is_announced_at_the_start() {
        let mut s = InputState::new(KeyboardMouse::Controller);
        s.start();
        assert_eq!(
            controls(&mut s),
            vec![ControlMessage::GamepadChanged {
                index: 0,
                added: true
            }]
        );
        let r = reports(&mut s, Instant::now());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].pads, vec![PadFrame::default()]);
    }

    #[test]
    fn keys_drive_the_first_controller_beside_a_real_one() {
        let mut s = InputState::new(KeyboardMouse::Controller);
        s.start();
        controls(&mut s);
        reports(&mut s, Instant::now());
        s.apply(
            Input::Pad(GamepadState {
                index: 0,
                connected: true,
                buttons: button::X,
                ..Default::default()
            }),
            Instant::now(),
        );
        s.apply(
            Input::Key {
                key: Key::Enter,
                down: true,
            },
            Instant::now(),
        );
        s.apply(
            Input::Key {
                key: Key::KeyQ,
                down: true,
            },
            Instant::now(),
        );
        let r = reports(&mut s, Instant::now());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].pads[0].buttons, xbutton::X | xbutton::A);
        assert!(r[0].keys.is_empty(), "Q is not sent as a key either");
    }

    #[test]
    fn keys_reach_the_console_as_keys_when_asked() {
        let mut s = InputState::new(KeyboardMouse::Native);
        s.start();
        reports(&mut s, Instant::now());
        s.apply(
            Input::Key {
                key: Key::KeyA,
                down: true,
            },
            Instant::now(),
        );
        s.apply(
            Input::Key {
                key: Key::KeyA,
                down: false,
            },
            Instant::now(),
        );
        let r = reports(&mut s, Instant::now());
        assert_eq!(
            r[0].keys,
            vec![
                KeyFrame {
                    vk: b'A',
                    down: true
                },
                KeyFrame {
                    vk: b'A',
                    down: false
                }
            ]
        );
        assert!(r[0].pads.is_empty());
    }

    #[test]
    fn a_second_controller_comes_and_goes() {
        let mut s = InputState::new(KeyboardMouse::Controller);
        s.start();
        controls(&mut s);
        let pad = GamepadState {
            index: 1,
            connected: true,
            ..Default::default()
        };
        s.apply(Input::Pad(pad), Instant::now());
        s.apply(Input::Pad(pad), Instant::now());
        assert_eq!(
            controls(&mut s),
            vec![ControlMessage::GamepadChanged {
                index: 1,
                added: true
            }]
        );
        s.apply(
            Input::Pad(GamepadState {
                connected: false,
                ..pad
            }),
            Instant::now(),
        );
        assert_eq!(
            controls(&mut s),
            vec![ControlMessage::GamepadChanged {
                index: 1,
                added: false
            }]
        );
        // Unplugging the first controller keeps it announced.
        s.apply(Input::Pad(GamepadState::default()), Instant::now());
        assert!(controls(&mut s).is_empty());
    }

    #[test]
    fn mouse_motion_is_summed_until_a_button_changes() {
        let mut s = InputState::new(KeyboardMouse::Native);
        let m = |dx, buttons| {
            Input::Mouse(MouseFrame {
                dx,
                buttons,
                ..Default::default()
            })
        };
        s.apply(m(3, 0), Instant::now());
        s.apply(m(4, 0), Instant::now());
        s.apply(m(0, 1), Instant::now());
        s.apply(m(0, 0), Instant::now());
        s.apply(m(2, 0), Instant::now());
        let r = reports(&mut s, Instant::now());
        let frames: Vec<(i32, u8)> = r[0].mouse.iter().map(|f| (f.dx, f.buttons)).collect();
        // Doubled, as the web client's.
        assert_eq!(frames, vec![(14, 0), (0, 1), (4, 0)]);
    }

    #[test]
    fn a_burst_too_big_for_one_report_takes_several() {
        let mut s = InputState::new(KeyboardMouse::Native);
        for _ in 0..20 {
            s.apply(
                Input::Key {
                    key: Key::KeyB,
                    down: true,
                },
                Instant::now(),
            );
        }
        let r = reports(&mut s, Instant::now());
        assert_eq!(
            r.iter().map(|r| r.keys.len()).collect::<Vec<_>>(),
            vec![16, 4]
        );
        assert_eq!(r[1].seq, r[0].seq + 1);
    }

    #[test]
    fn controllers_are_sent_again_on_their_heartbeat() {
        let t0 = Instant::now();
        let mut s = InputState::new(KeyboardMouse::Controller);
        s.start();
        reports(&mut s, t0);
        assert_eq!(s.next_tick(), Some(t0 + PAD_HEARTBEAT));
        s.tick(t0 + PAD_HEARTBEAT / 2);
        assert!(reports(&mut s, t0 + PAD_HEARTBEAT / 2).is_empty());
        s.tick(t0 + PAD_HEARTBEAT);
        assert_eq!(reports(&mut s, t0 + PAD_HEARTBEAT).len(), 1);
    }

    #[test]
    fn frame_timings_wait_for_input_or_a_batch() {
        let mut s = InputState::new(KeyboardMouse::Native);
        for k in 0..METADATA_BATCH as u32 - 1 {
            s.frame_shown(FrameTimes {
                server_key: k,
                ..Default::default()
            });
        }
        assert!(reports(&mut s, Instant::now()).is_empty());
        s.frame_shown(FrameTimes::default());
        let r = reports(&mut s, Instant::now());
        assert_eq!(r[0].metadata.len(), METADATA_BATCH);
        s.frame_shown(FrameTimes::default());
        s.apply(
            Input::Key {
                key: Key::KeyC,
                down: true,
            },
            Instant::now(),
        );
        let r = reports(&mut s, Instant::now());
        assert_eq!((r[0].metadata.len(), r[0].keys.len()), (1, 1));
    }

    #[test]
    fn the_mouse_aims_with_the_first_controllers_right_stick_in_the_shooters_layout() {
        let t0 = Instant::now();
        let mut s = InputState::new(KeyboardMouse::Shooter);
        s.start();
        controls(&mut s);
        reports(&mut s, t0);
        s.apply(
            Input::Mouse(MouseFrame {
                dx: 20,
                buttons: 1,
                ..Default::default()
            }),
            t0,
        );
        let r = reports(&mut s, t0);
        assert_eq!(r.len(), 1);
        assert!(r[0].mouse.is_empty(), "the console sees no mouse");
        assert!(r[0].pads[0].right_x > 0);
        assert_eq!(r[0].pads[0].right_trigger, u16::MAX);
        // Still, the stick goes back to the centre, on a tick.
        let mut t = t0;
        let mut last = None;
        while t < t0 + Duration::from_millis(100) {
            t = s.next_tick().unwrap();
            s.tick(t);
            if let Some(r) = reports(&mut s, t).last() {
                last = Some(r.pads[0].right_x);
            }
        }
        assert_eq!(last, Some(0));
    }
}
