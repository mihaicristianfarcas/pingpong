//! The pointer, as the stream sees it: relative motion, or a pointer at a
//! position in stream pixels. Shared with the network thread, which hears
//! from the host.
//!
//! **Moonlight's way, against a host that draws the pointer into the
//! picture** (`features::POINTER_IN_PICTURE`, every current host, as
//! Apollo does): the client draws no pointer, and motion is relative
//! unless the user switches to positions with Ctrl+Alt+Shift+M (Moonlight's
//! "remote desktop" mouse). Relative motion is what games read (raw input),
//! and the pointer seen is the host's own, so it cannot disagree with where
//! a click lands.
//!
//! **Against an older host** the client draws the pointer itself, in the
//! shape the host reports, and switches between positions (desktop) and
//! relative motion (a game took the mouse) as the host's `CursorState`
//! says.

use pingpong_proto::control::{self, CursorShape, CursorState, SessionAck};
use pingpong_proto::input::InputEvent;

use crate::input::InputSender;

/// Where the client draws the host's pointer, if it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorDraw {
    pub visible: bool,
    /// Motion goes as positions: a platform pointer standing in for the
    /// stream's belongs at (`x`, `y`), drawn or not.
    pub absolute: bool,
    /// Stream pixels.
    pub x: f32,
    pub y: f32,
    pub shape: CursorShape,
}

#[derive(Debug, Clone, Copy)]
pub struct PointerState {
    /// Send relative motion: the user's choice against a host that draws the
    /// pointer, else the host's (its app hid or clipped the pointer).
    pub relative: bool,
    /// Ctrl+Alt+Shift+M: the other mode, until pressed again (Moonlight's
    /// mouse-mode toggle).
    pub mode_override: Option<bool>,
    /// The host draws the pointer into the picture: the client draws none,
    /// and the host's `CursorState` is not followed.
    pub host_draws: bool,
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
            host_draws: false,
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

    /// Move the pointer by (`dx`, `dy`) stream pixels: relative motion for
    /// the host; or, sending positions, our pointer moves (`show` puts it
    /// where it belongs) and the host's follows to the same place.
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

    /// Relative motion, or positions.
    pub fn is_relative(&self) -> bool {
        self.mode_override.unwrap_or(self.relative)
    }

    /// Switch to the other mode, or back to the usual one (relative against
    /// a host that draws the pointer, else the host's choice). Returns
    /// whether motion is now relative.
    pub fn toggle_mode(&mut self) -> bool {
        self.mode_override = match self.mode_override {
            None => Some(!self.relative),
            Some(_) => None,
        };
        self.is_relative()
    }

    /// The session started (or was renegotiated): its size, and whether the
    /// host draws the pointer. Re-centred on the new size; the user's mode
    /// switch is kept.
    pub fn started(&mut self, ack: &SessionAck) {
        let (captured, mode_override) = (self.captured, self.mode_override);
        let host_draws = ack.features & control::features::POINTER_IN_PICTURE != 0;
        *self = PointerState {
            captured,
            mode_override,
            host_draws,
            relative: host_draws,
            ..PointerState::new(ack.width as u32, ack.height as u32)
        };
    }

    /// Follow a host that leaves the pointer to the client: `CursorState`
    /// says whether its foreground application has the mouse, and where its
    /// pointer is.
    pub fn apply_host(&mut self, c: &CursorState) {
        if self.host_draws {
            return;
        }
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
        let absolute = self.captured && !self.is_relative();
        CursorDraw {
            visible: absolute && !self.host_draws,
            absolute,
            x: self.x,
            y: self.y,
            shape: self.shape,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PointerState;
    use pingpong_proto::control::{features, AckStatus, CursorState, SessionAck};

    fn ack(width: u16, height: u16, features: u8) -> SessionAck {
        SessionAck {
            status: AckStatus::Ok,
            codec: 0,
            width,
            height,
            refresh_mhz: 60_000,
            bitrate_kbps: 20_000,
            audio_channels: 2,
            nonce: 0,
            host: 0,
            features,
            video: 0,
        }
    }

    #[test]
    fn a_host_that_draws_the_pointer_gets_relative_motion_and_no_drawn_pointer() {
        let mut p = PointerState::new(1920, 1080);
        p.captured = true;
        p.started(&ack(1920, 1080, features::POINTER_IN_PICTURE));
        assert!(p.is_relative(), "Moonlight's default");
        assert!(!p.draw().visible);
        // An older client's cue, which this one does not follow.
        let desktop = CursorState {
            visible: true,
            ..CursorState::IN_PICTURE
        };
        p.apply_host(&desktop);
        assert!(p.is_relative());
        assert!(!p.toggle_mode(), "M: positions, the remote desktop mouse");
        let d = p.draw();
        assert!(d.absolute && !d.visible, "positions, but the host draws");
    }

    #[test]
    fn an_older_host_steers_the_mode_and_the_client_draws() {
        let mut p = PointerState::new(1920, 1080);
        p.captured = true;
        p.started(&ack(1920, 1080, 0));
        assert!(!p.is_relative(), "the desktop to begin with");
        assert!(p.draw().visible);
        p.apply_host(&CursorState::IN_PICTURE);
        assert!(p.is_relative(), "a hidden pointer: a game took the mouse");
        assert!(!p.draw().visible);
    }

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
        p.started(&ack(3024, 1890, 0));
        assert_eq!(
            p.mode_override,
            Some(false),
            "a new mode keeps the override"
        );
    }
}
