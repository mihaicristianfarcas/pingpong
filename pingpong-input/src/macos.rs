//! Injecting input on a Mac host: CoreGraphics events, posted where the
//! hardware's go (the HID event tap), so every app sees them as typed and
//! clicked.
//!
//! Keys arrive by position (PC scancodes) and go out by position (Mac key
//! codes, `pingpong_proto::mackeys`), so the Mac's own layout decides the
//! character, as it would for its own keyboard. Modifier keys go out as flag
//! changes, and every key carries the modifiers held, as macOS expects.
//! Absolute positions are the stream's pixels on the display being streamed.
//!
//! macOS drops posted events silently unless the posting process (for a
//! command-line Pong, the terminal it runs in) has the Accessibility
//! permission; `trusted` says whether it does, without asking.

use std::time::{Duration, Instant};

use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};
use pingpong_proto::input::{Button, InputEvent};

use crate::{HeldSet, InputError, InputSink};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Whether macOS lets this process post input (the Accessibility permission).
pub fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Two clicks this close in time and space are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_POINTS: f64 = 4.0;

pub struct CgEventSink {
    source: Option<CFRetained<CGEventSource>>,
    /// The streamed display, in global points.
    bounds: CGRect,
    /// The stream's pixels across that display.
    stream: (f64, f64),
    held: HeldSet,
    flags: CGEventFlags,
    buttons_down: u8,
    last_click: Option<(Instant, Button, CGPoint, i64)>,
    /// Scroll left over below a whole pixel.
    wheel_rest: (f64, f64),
}

// SAFETY: the event source is used only through &mut self.
unsafe impl Send for CgEventSink {}

impl CgEventSink {
    /// Inject onto `display_id`, streamed at `stream_w`x`stream_h` pixels.
    pub fn new(display_id: u32, stream_w: u32, stream_h: u32) -> CgEventSink {
        CgEventSink {
            source: CGEventSource::new(CGEventSourceStateID::HIDSystemState),
            bounds: CGDisplayBounds(display_id),
            stream: (stream_w.max(1) as f64, stream_h.max(1) as f64),
            held: HeldSet::new(),
            flags: CGEventFlags::empty(),
            buttons_down: 0,
            last_click: None,
            wheel_rest: (0.0, 0.0),
        }
    }

    fn cursor(&self) -> CGPoint {
        CGEvent::location(CGEvent::new(self.source.as_deref()).as_deref())
    }

    fn clamp(&self, p: CGPoint) -> CGPoint {
        let b = self.bounds;
        CGPoint::new(
            p.x.clamp(b.origin.x, b.origin.x + b.size.width - 1.0),
            p.y.clamp(b.origin.y, b.origin.y + b.size.height - 1.0),
        )
    }

    fn post(&self, event: Option<CFRetained<CGEvent>>) {
        if let Some(e) = event {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
        }
    }

    /// The move event for the buttons held: a drag while one is.
    fn move_type(&self) -> (CGEventType, CGMouseButton) {
        if self.buttons_down & 1 != 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons_down & 2 != 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.buttons_down != 0 {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        }
    }

    fn move_to(&self, to: CGPoint, delta: (i64, i64)) {
        let (kind, button) = self.move_type();
        let e = CGEvent::new_mouse_event(self.source.as_deref(), kind, to, button);
        if let Some(e) = &e {
            // Games read the deltas, not the position.
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaX, delta.0);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaY, delta.1);
            CGEvent::set_flags(Some(e), self.flags);
        }
        self.post(e);
    }

    fn button(&mut self, b: Button, down: bool) {
        let (kind, cg, number, bit) = match (b, down) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0, 1),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0, 1),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1, 2),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1, 2),
            (Button::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 2, 4),
            (Button::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 2, 4),
            (Button::X1, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 3, 8),
            (Button::X1, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 3, 8),
            (Button::X2, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 4, 16),
            (Button::X2, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 4, 16),
        };
        let at = self.cursor();
        let clicks = if down {
            let n = match self.last_click {
                Some((t, lb, lp, n))
                    if lb == b
                        && t.elapsed() < DOUBLE_CLICK
                        && (lp.x - at.x).abs() <= DOUBLE_CLICK_POINTS
                        && (lp.y - at.y).abs() <= DOUBLE_CLICK_POINTS =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((Instant::now(), b, at, n));
            self.buttons_down |= bit;
            n
        } else {
            self.buttons_down &= !bit;
            self.last_click.map_or(1, |c| c.3)
        };
        let e = CGEvent::new_mouse_event(self.source.as_deref(), kind, at, cg);
        if let Some(e) = &e {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventButtonNumber, number);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventClickState, clicks);
            CGEvent::set_flags(Some(e), self.flags);
        }
        self.post(e);
    }

    fn key(&mut self, scancode: u16, down: bool) {
        let Some(code) = pingpong_proto::mackeys::keycode_for_scancode(scancode) else {
            return;
        };
        let modifier = match code {
            0x38 | 0x3C => Some(CGEventFlags::MaskShift),
            0x3B | 0x3E => Some(CGEventFlags::MaskControl),
            0x3A | 0x3D => Some(CGEventFlags::MaskAlternate),
            0x37 | 0x36 => Some(CGEventFlags::MaskCommand),
            0x39 => Some(CGEventFlags::MaskAlphaShift),
            _ => None,
        };
        if let Some(mask) = modifier {
            if code == 0x39 {
                // Caps Lock latches: each press toggles it.
                if down {
                    self.flags.toggle(mask);
                }
            } else if down {
                self.flags.insert(mask);
            } else {
                self.flags.remove(mask);
            }
        }
        let e = CGEvent::new_keyboard_event(self.source.as_deref(), code, down);
        if let Some(e) = &e {
            if modifier.is_some() {
                CGEvent::set_type(Some(e), CGEventType::FlagsChanged);
            }
            CGEvent::set_flags(Some(e), self.flags);
        }
        self.post(e);
    }

    fn text(&self, c: char) {
        let mut units = [0u16; 2];
        let units = c.encode_utf16(&mut units);
        for down in [true, false] {
            let e = CGEvent::new_keyboard_event(self.source.as_deref(), 0, down);
            if let Some(e) = &e {
                unsafe {
                    CGEvent::keyboard_set_unicode_string(Some(e), units.len() as _, units.as_ptr())
                };
            }
            self.post(e);
        }
    }

    fn wheel(&mut self, dv: i16, dh: i16) {
        // Windows counts 120 per notch; a notch here scrolls 30 points.
        const POINTS_PER_UNIT: f64 = 30.0 / 120.0;
        self.wheel_rest.0 += dv as f64 * POINTS_PER_UNIT;
        // A positive horizontal wheel is "right" on Windows, "left" here.
        self.wheel_rest.1 -= dh as f64 * POINTS_PER_UNIT;
        let (v, h) = (self.wheel_rest.0.trunc(), self.wheel_rest.1.trunc());
        if v == 0.0 && h == 0.0 {
            return;
        }
        self.wheel_rest.0 -= v;
        self.wheel_rest.1 -= h;
        let e = CGEvent::new_scroll_wheel_event2(
            self.source.as_deref(),
            CGScrollEventUnit::Pixel,
            2,
            v as i32,
            h as i32,
            0,
        );
        self.post(e);
    }
}

impl InputSink for CgEventSink {
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError> {
        for &ev in events {
            self.held.observe(ev);
            match ev {
                InputEvent::KeyDown(sc) => self.key(sc, true),
                InputEvent::KeyUp(sc) => self.key(sc, false),
                InputEvent::MouseMoveAbs { x, y } => {
                    let b = self.bounds;
                    let to = self.clamp(CGPoint::new(
                        b.origin.x + x as f64 * b.size.width / self.stream.0,
                        b.origin.y + y as f64 * b.size.height / self.stream.1,
                    ));
                    self.move_to(to, (0, 0));
                }
                InputEvent::MouseMoveRel { dx, dy } => {
                    let at = self.cursor();
                    let scale = (
                        self.bounds.size.width / self.stream.0,
                        self.bounds.size.height / self.stream.1,
                    );
                    let to = self.clamp(CGPoint::new(
                        at.x + dx as f64 * scale.0,
                        at.y + dy as f64 * scale.1,
                    ));
                    self.move_to(to, (dx as i64, dy as i64));
                }
                InputEvent::ButtonDown(b) => self.button(b, true),
                InputEvent::ButtonUp(b) => self.button(b, false),
                InputEvent::Wheel { dv, dh } => self.wheel(dv, dh),
                InputEvent::Text(c) => self.text(c),
            }
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let events = self.held.drain_release_events();
        for ev in events {
            match ev {
                InputEvent::KeyUp(sc) => self.key(sc, false),
                InputEvent::ButtonUp(b) => self.button(b, false),
                _ => {}
            }
        }
        Ok(())
    }
}
