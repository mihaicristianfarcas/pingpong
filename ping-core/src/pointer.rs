//! The pointer, as the stream sees it: relative motion while the host's
//! application has taken the mouse (a game), else a pointer at a position in
//! stream pixels (the desktop). Shared with the network thread, which learns
//! from the host what its foreground application is doing with the mouse.

use pingpong_proto::control::{CursorShape, CursorState};
use pingpong_proto::input::InputEvent;

use crate::input::InputSender;

/// Where the client draws the host's pointer, if it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorDraw {
    pub visible: bool,
    /// Stream pixels.
    pub x: f32,
    pub y: f32,
    pub shape: CursorShape,
}

#[derive(Debug, Clone, Copy)]
pub struct PointerState {
    /// The host app has taken the mouse (hidden or clipped the cursor): send
    /// relative motion and draw nothing.
    pub relative: bool,
    /// Ctrl+Alt+Shift+M: the other mode than the host's, until pressed again
    /// (Moonlight's mouse-mode toggle, for a game that fools the automatic
    /// choice).
    pub mode_override: Option<bool>,
    pub shape: CursorShape,
    /// Pointer position, stream pixels.
    pub x: f32,
    pub y: f32,
    pub captured: bool,
    stream: (f32, f32),
}

impl PointerState {
    pub fn new(stream_w: u32, stream_h: u32) -> PointerState {
        PointerState {
            relative: false,
            mode_override: None,
            shape: CursorShape::Arrow,
            x: stream_w as f32 / 2.0,
            y: stream_h as f32 / 2.0,
            captured: false,
            stream: (stream_w as f32, stream_h as f32),
        }
    }

    /// The stream's size in pixels.
    pub fn stream_size(&self) -> (f32, f32) {
        self.stream
    }

    /// Move the pointer by (`dx`, `dy`) stream pixels: in a game that has the
    /// mouse, relative motion for the host; on the desktop, our pointer moves
    /// (`show` puts it on the screen) and the host's follows to the same place.
    pub fn nudge(&mut self, dx: f64, dy: f64, input: &InputSender, show: &dyn Fn(CursorDraw)) {
        if self.is_relative() {
            input.motion(dx, dy);
            return;
        }
        self.x = (self.x + dx as f32).clamp(0.0, self.stream.0 - 1.0);
        self.y = (self.y + dy as f32).clamp(0.0, self.stream.1 - 1.0);
        show(self.draw());
        input.send(InputEvent::MouseMoveAbs {
            x: self.x as u16,
            y: self.y as u16,
        });
    }

    /// Put the pointer at (`x`, `y`) stream pixels (the local pointer is
    /// there) and the host's with it. False when that is where it was.
    pub fn place(&mut self, x: f32, y: f32, input: &InputSender) -> bool {
        let (x, y) = (
            x.clamp(0.0, self.stream.0 - 1.0),
            y.clamp(0.0, self.stream.1 - 1.0),
        );
        if (x as u16, y as u16) == (self.x as u16, self.y as u16) {
            return false;
        }
        self.x = x;
        self.y = y;
        input.send(InputEvent::MouseMoveAbs {
            x: x as u16,
            y: y as u16,
        });
        true
    }

    /// Relative motion (a game has the mouse) or a pointer (desktop).
    pub fn is_relative(&self) -> bool {
        self.mode_override.unwrap_or(self.relative)
    }

    /// Switch to the other mode than the host's, or back to following it.
    /// Returns whether motion is now relative.
    pub fn toggle_mode(&mut self) -> bool {
        self.mode_override = match self.mode_override {
            None => Some(!self.relative),
            Some(_) => None,
        };
        self.is_relative()
    }

    /// The session's mode is known (or changed): re-centre on the new size.
    pub fn resize(&mut self, stream_w: u32, stream_h: u32) {
        let (captured, mode_override) = (self.captured, self.mode_override);
        *self = PointerState {
            captured,
            mode_override,
            ..PointerState::new(stream_w, stream_h)
        };
    }

    /// Follow the host: `CursorState` says whether its foreground application
    /// has the mouse, and where its pointer is.
    pub fn apply_host(&mut self, c: &CursorState) {
        let relative = !c.visible || c.clipped;
        if self.relative && !relative {
            // Leaving a game: put our pointer where the host's is.
            self.x = c.x as f32;
            self.y = c.y as f32;
        }
        self.relative = relative;
        self.shape = c.shape;
    }

    pub fn draw(&self) -> CursorDraw {
        CursorDraw {
            visible: self.captured && !self.is_relative(),
            x: self.x,
            y: self.y,
            shape: self.shape,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PointerState;

    #[test]
    fn mouse_mode_flips_then_follows_the_host_again() {
        let mut p = PointerState::new(1920, 1080);
        assert!(!p.is_relative(), "the desktop to begin with");
        assert!(p.toggle_mode(), "M: the other mode");
        p.relative = true; // the host's game takes the mouse meanwhile
        assert!(p.is_relative());
        assert!(
            p.toggle_mode(),
            "M again: back to the host's choice, relative now"
        );
        assert_eq!(p.mode_override, None);
        assert!(
            !p.toggle_mode(),
            "and from there, M gives the desktop pointer"
        );
        p.resize(3024, 1890);
        assert_eq!(
            p.mode_override,
            Some(false),
            "a new mode keeps the override"
        );
    }
}
