//! Input injection on the host (v2 design §5).
//!
//! A platform-free trait, [`InputSink`], with one implementation per host
//! platform: `SendInput` on Windows, CoreGraphics events on macOS, XTest on
//! Linux under X11 (Wayland injects through the desktop portal, in `pong`).
//! Display control (`pingpong-display`) has the same shape: both are seams
//! where host platforms differ, so the session code above them is portable.
//!
//! Section references in this crate (`v2 design §5.4`) are to the original
//! design documents in `docs/design/`.

use pingpong_proto::input::{Button, InputEvent};

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::SendInputSink;

#[cfg(target_os = "macos")]
pub mod macos;

/// Scancodes → Linux key codes (pure; used by the X11 sink).
pub mod evdev;
pub mod repeat;
pub use repeat::{KeyRepeat, RepeatRate};
#[cfg(target_os = "linux")]
pub mod x11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputError {
    /// `SendInput` returned a count below what was submitted, which is how it
    /// reports being blocked -- typically by UIPI against an elevated window.
    SendInput {
        sent: u32,
        expected: u32,
    },
    Os(u32),
    /// The input system cannot be reached (Linux: the X server).
    Unavailable,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::SendInput { sent, expected } => write!(
                f,
                "SendInput injected {sent} of {expected} events; input is being \
                    blocked, most likely by UIPI against an elevated foreground window"
            ),
            InputError::Os(code) => write!(f, "input injection failed: os error {code}"),
            InputError::Unavailable => {
                write!(f, "input injection failed: the input system is unreachable")
            }
        }
    }
}

impl std::error::Error for InputError {}

pub trait InputSink {
    /// Inject events in order. Already filtered by the sequence gate, so every
    /// event here is new.
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError>;

    /// Release everything currently held. The backstop for input redundancy
    /// (v2 design §5.4) and the answer to "the client dies holding a key"
    /// (v2 design §9) -- called on every teardown.
    fn release_all(&mut self) -> Result<(), InputError>;

    /// When input last went out on the sink's own time, after `inject`
    /// returned: text a Windows host types paced. What the host tells its
    /// own user's input from the injected by.
    fn last_sent(&self) -> Option<std::time::Instant> {
        None
    }

    /// How a held key repeats here when the system does not repeat
    /// injected keys itself (Windows, macOS): the host's own keyboard
    /// settings. `None` where it does (X11, Wayland). See [`repeat`].
    fn key_repeat(&self) -> Option<RepeatRate> {
        None
    }

    /// Whether `scancode`, held, repeats (as on this system's keyboards).
    fn repeats(&self, scancode: u16) -> bool {
        !repeat::is_lock_key(scancode)
    }

    /// Press the held key `scancode` again, as a keyboard's auto-repeat.
    fn repeat_key(&mut self, scancode: u16) -> Result<(), InputError> {
        self.inject(&[InputEvent::KeyDown(scancode)])
    }
}

/// What is currently pressed, so `release_all` is real rather than hopeful.
///
/// Pure, so the bookkeeping is testable on any machine. The Windows sink
/// owns one of these and feeds every injected event through it.
#[derive(Debug, Default)]
pub struct HeldSet {
    keys: Vec<u16>,
    buttons: Vec<Button>,
}

impl HeldSet {
    pub fn new() -> HeldSet {
        HeldSet::default()
    }

    pub fn observe(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::KeyDown(sc) => {
                if !self.keys.contains(&sc) {
                    self.keys.push(sc);
                }
            }
            InputEvent::KeyUp(sc) => self.keys.retain(|&k| k != sc),
            InputEvent::ButtonDown(b) => {
                if !self.buttons.contains(&b) {
                    self.buttons.push(b);
                }
            }
            InputEvent::ButtonUp(b) => self.buttons.retain(|&x| x != b),
            // Motion and wheel carry no held state.
            InputEvent::MouseMoveRel { .. }
            | InputEvent::MouseMoveAbs { .. }
            | InputEvent::Wheel { .. }
            | InputEvent::Text(_) => {}
        }
    }

    /// The events that would release everything held, emptying the set.
    pub fn drain_release_events(&mut self) -> Vec<InputEvent> {
        let mut out: Vec<InputEvent> = self.keys.drain(..).map(InputEvent::KeyUp).collect();
        out.extend(self.buttons.drain(..).map(InputEvent::ButtonUp));
        out
    }
}

/// The absolute coordinate space `MOUSEEVENTF_ABSOLUTE` expects: 0..=65535
/// across the **whole virtual desktop**, regardless of pixel dimensions.
pub const ABSOLUTE_RANGE: u32 = 65_535;

/// The bounding rectangle of every attached display, in Windows' virtual-screen
/// coordinates (`SM_XVIRTUALSCREEN` and friends).
///
/// `left` and `top` are negative when a monitor sits left of or above the
/// primary, which is why they are signed and why the transform subtracts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Stream pixels -> the 0..=65535 space `MOUSEEVENTF_VIRTUALDESK` reads.
///
/// Pure, so the arithmetic is testable on any machine, including those where
/// the Windows sink cannot even be compiled.
///
/// **Why the origin matters.** The captured display is rarely the whole desktop:
/// with the host's physical monitor still attached, the virtual desktop spans
/// both it and the VDD. Normalising a stream pixel against the *captured*
/// display's size while asking Windows to interpret it across the *virtual*
/// desktop puts the cursor on the wrong monitor entirely -- the captured
/// display then never changes, and change-driven capture correctly sends
/// nothing, which presents as a frozen stream rather than as a mouse bug.
/// The captured display's rectangle in virtual-desktop coordinates.
///
/// **Not the same units as the stream.** A display running at 125% reports a
/// rectangle a fifth smaller than the pixels it actually scans out, and Windows
/// positions the cursor in *these* coordinates -- so stream pixels have to be
/// scaled onto this rectangle rather than used directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AbsoluteTransform {
    desktop: VirtualDesktop,
    /// Where the captured display sits inside the virtual desktop, and how big
    /// it is in the desktop's own coordinate space.
    display: DisplayRect,
    /// The stream's pixel dimensions, which the client's coordinates are in.
    stream: (u32, u32),
}

impl AbsoluteTransform {
    pub fn new(
        desktop: VirtualDesktop,
        display: DisplayRect,
        stream: (u32, u32),
    ) -> AbsoluteTransform {
        AbsoluteTransform {
            desktop,
            display,
            stream,
        }
    }

    /// A pixel on the captured display -> the normalised pair to hand `SendInput`.
    pub fn normalize(&self, x: u16, y: u16) -> (i32, i32) {
        let desktop_x = self.display.left + Self::onto(x, self.display.width, self.stream.0);
        let desktop_y = self.display.top + Self::onto(y, self.display.height, self.stream.1);
        (
            Self::axis(desktop_x, self.desktop.left, self.desktop.width),
            Self::axis(desktop_y, self.desktop.top, self.desktop.height),
        )
    }

    /// Stream pixels -> the display's own coordinate space, which is what makes
    /// this independent of the display's DPI scaling.
    fn onto(value: u16, display_span: u32, stream_span: u32) -> i32 {
        if stream_span <= 1 || display_span == 0 {
            return 0;
        }
        ((value as i64 * (display_span as i64 - 1)) / (stream_span as i64 - 1)) as i32
    }

    /// Divides by `span - 1` so the last pixel reaches 65535 exactly rather than
    /// falling a pixel short of the edge.
    fn axis(desktop_pixel: i32, virtual_origin: i32, span: u32) -> i32 {
        if span <= 1 {
            return 0;
        }
        let offset = (desktop_pixel - virtual_origin).max(0) as i64;
        ((offset * ABSOLUTE_RANGE as i64) / (span as i64 - 1)).min(ABSOLUTE_RANGE as i64) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::input::{Button, InputEvent};

    /// The inverse Windows applies, so these tests can assert in desktop pixels
    /// rather than in normalised magic numbers.
    fn lands_on(norm: i32, origin: i32, span: u32) -> i32 {
        origin + ((norm as i64 * (span as i64 - 1)) / ABSOLUTE_RANGE as i64) as i32
    }

    #[test]
    fn a_lone_display_at_the_origin_spans_the_whole_range() {
        let desktop = VirtualDesktop {
            left: 0,
            top: 0,
            width: 3024,
            height: 1964,
        };
        let t = AbsoluteTransform::new(
            desktop,
            DisplayRect {
                left: 0,
                top: 0,
                width: 3024,
                height: 1964,
            },
            (3024, 1964),
        );
        assert_eq!(t.normalize(0, 0), (0, 0));
        assert_eq!(
            t.normalize(3023, 1963),
            (ABSOLUTE_RANGE as i32, ABSOLUTE_RANGE as i32)
        );
    }

    #[test]
    fn a_second_display_lands_on_itself_not_on_the_primary() {
        // With the physical monitor still attached, the virtual desktop spans
        // both; normalising a VDD pixel against the VDD's own size sends the
        // cursor to the other monitor, so the captured display never changes
        // and the stream looks frozen.
        let span = 2560 + 3024;
        let desktop = VirtualDesktop {
            left: 0,
            top: 0,
            width: span,
            height: 1964,
        };
        let t = AbsoluteTransform::new(
            desktop,
            DisplayRect {
                left: 2560,
                top: 0,
                width: 3024,
                height: 1964,
            },
            (3024, 1964),
        );

        let landed = lands_on(t.normalize(0, 0).0, 0, span);
        assert!(
            (landed - 2560).abs() <= 2,
            "the VDD's top-left landed at x={landed}, not at its origin 2560"
        );
    }

    #[test]
    fn a_scaled_display_maps_the_whole_stream_onto_itself() {
        // A real host's geometry: the VDD is streamed at 3024x1964 but runs
        // at 125%, so it occupies only 2419x1571 of desktop coordinate space.
        // Feeding stream pixels straight in overshoots the display by a quarter
        // and the pointer runs off the picture -- "the mouse escapes".
        let desktop = VirtualDesktop {
            left: -2560,
            top: 0,
            width: 4979,
            height: 1571,
        };
        let vdd = DisplayRect {
            left: 0,
            top: 0,
            width: 2419,
            height: 1571,
        };
        let t = AbsoluteTransform::new(desktop, vdd, (3024, 1964));

        let (nx, ny) = t.normalize(3023, 1963);
        let (x, y) = (
            lands_on(nx, desktop.left, desktop.width),
            lands_on(ny, desktop.top, desktop.height),
        );
        assert!(
            (x - 2418).abs() <= 2 && (y - 1570).abs() <= 2,
            "the stream's bottom-right landed at ({x},{y}), not on the VDD's at (2418,1570)"
        );
        // And the origin still maps to the origin. Within a pixel: both the
        // transform and this inverse are integer, so each can round once.
        let origin_x = lands_on(t.normalize(0, 0).0, desktop.left, desktop.width);
        assert!(
            origin_x.abs() <= 2,
            "the stream's top-left landed at x={origin_x}, not on the VDD's at 0"
        );
    }

    #[test]
    fn a_display_left_of_the_primary_has_a_negative_virtual_origin() {
        // SM_XVIRTUALSCREEN is negative when a monitor sits left of the
        // primary. Subtracting it is what keeps the normalised range positive.
        let t = AbsoluteTransform::new(
            VirtualDesktop {
                left: -1920,
                top: 0,
                width: 1920 + 2560,
                height: 1440,
            },
            DisplayRect {
                left: -1920,
                top: 0,
                width: 1920,
                height: 1440,
            },
            (1920, 1440),
        );
        assert_eq!(t.normalize(0, 0), (0, 0));
    }

    #[test]
    fn a_degenerate_desktop_never_divides_by_zero() {
        // GetSystemMetrics can report 0 before a display is attached, and a
        // panic here would take down the receive thread mid-session.
        let t = AbsoluteTransform::new(
            VirtualDesktop {
                left: 0,
                top: 0,
                width: 0,
                height: 0,
            },
            DisplayRect {
                left: 0,
                top: 0,
                width: 0,
                height: 0,
            },
            (0, 0),
        );
        assert_eq!(t.normalize(10, 10), (0, 0));
    }

    #[test]
    fn the_held_set_releases_what_is_down() {
        // The backstop for input redundancy and the answer to "the client dies
        // holding a key". Without tracking, release_all() is a wish.
        let mut held = HeldSet::new();
        held.observe(InputEvent::KeyDown(0x11));
        held.observe(InputEvent::KeyDown(0x1e));
        held.observe(InputEvent::ButtonDown(Button::Left));

        let mut released = held.drain_release_events();
        released.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(
            released,
            vec![
                InputEvent::ButtonUp(Button::Left),
                InputEvent::KeyUp(0x11),
                InputEvent::KeyUp(0x1e),
            ]
        );
    }

    #[test]
    fn releasing_a_key_removes_it_from_the_held_set() {
        let mut held = HeldSet::new();
        held.observe(InputEvent::KeyDown(0x11));
        held.observe(InputEvent::KeyUp(0x11));
        assert!(held.drain_release_events().is_empty());
    }

    #[test]
    fn a_repeated_key_down_is_held_once() {
        let mut held = HeldSet::new();
        held.observe(InputEvent::KeyDown(0x11));
        held.observe(InputEvent::KeyDown(0x11));
        assert_eq!(held.drain_release_events(), vec![InputEvent::KeyUp(0x11)]);
    }

    #[test]
    fn motion_and_wheel_are_not_held_state() {
        let mut held = HeldSet::new();
        held.observe(InputEvent::MouseMoveRel { dx: 5, dy: 5 });
        held.observe(InputEvent::Wheel { dv: 120, dh: 0 });
        held.observe(InputEvent::MouseMoveAbs { x: 10, y: 10 });
        assert!(held.drain_release_events().is_empty());
    }

    #[test]
    fn draining_empties_the_set() {
        // Teardown may run twice (explicit SessionEnd then tunnel loss). The
        // second call must be a no-op, not a second round of phantom releases.
        let mut held = HeldSet::new();
        held.observe(InputEvent::KeyDown(0x11));
        assert_eq!(held.drain_release_events().len(), 1);
        assert!(held.drain_release_events().is_empty());
    }
}
