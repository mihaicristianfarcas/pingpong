//! Screen text for an agent's session (`pingpong_proto::screen`): the
//! agent's questions about its screen go to a thread of their own, which
//! reads the platform's accessibility tree (`a11y::ScreenReader`) and sends
//! the answer back in parts.
//!
//! Off the receive thread: a read is many calls into other processes
//! (0.1-0.6 s on a busy window), and the receive loop injects input. One
//! read at a time: the queue holds two, and a question asked while it is
//! full is dropped -- the agent asks again when no answer comes.
//!
//! Only the running session's agent is answered (`host`): what its
//! screenshots show, as text. A person's client, or a watcher, has the
//! picture and nothing to ask.

use std::sync::Arc;

use crossbeam_channel::{Sender, TrySendError};
use pingpong_proto::screen::{self, Query};
use pingpong_transport::{Endpoint, Peer};

/// Questions waiting for the reader at most. A model asks one at a time; a
/// click's check and a `read_screen` can overlap.
const QUEUE: usize = 2;

pub struct ScreenText {
    questions: Sender<(u32, Query)>,
}

impl ScreenText {
    /// Answer `peer`'s questions with `reader`, until dropped.
    pub fn start(
        mut reader: crate::a11y::ScreenReader,
        endpoint: Arc<Endpoint>,
        peer: Arc<Peer>,
    ) -> ScreenText {
        let (tx, rx) = crossbeam_channel::bounded::<(u32, Query)>(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("screen-text".into())
            .spawn(move || {
                // Ends when the session drops its sender.
                for (id, query) in rx {
                    let started = std::time::Instant::now();
                    let text = reader.read(query);
                    let parts = screen::reply_packets(id, &text);
                    tracing::debug!(
                        ?query,
                        elements = text.elements.len(),
                        parts = parts.len(),
                        ms = started.elapsed().as_millis() as u64,
                        note = text.note,
                        "screen text"
                    );
                    for p in &parts {
                        let _ = endpoint.send(&peer, p);
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the screen-text thread");
        }
        ScreenText { questions: tx }
    }

    pub fn ask(&self, id: u32, query: Query) {
        match self.questions.try_send((id, query)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::debug!(id, "screen text busy: question dropped")
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Where the agent's display sits in the host's own coordinates (points on
/// a Mac, pixels elsewhere), and where its picture sits in the stream (all
/// of it, but on a Linux host that scales its screen to fit): the
/// accessibility tree's places are the host's, the agent's are the stream's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
    /// x, y, width, height in stream pixels.
    pub picture: (u32, u32, u32, u32),
}

impl Geometry {
    /// A box in host coordinates -> the part of it on the display, in
    /// stream pixels; None when none of it is on the display (or the box
    /// is empty).
    pub fn stream_box(&self, x: f64, y: f64, w: f64, h: f64) -> Option<(u16, u16, u16, u16)> {
        if !(w > 0.0 && h > 0.0 && self.width > 0.0 && self.height > 0.0) {
            return None;
        }
        let (px, py, pw, ph) = (
            self.picture.0 as f64,
            self.picture.1 as f64,
            self.picture.2 as f64,
            self.picture.3 as f64,
        );
        let sx = |v: f64| px + ((v - self.left) * pw / self.width).clamp(0.0, pw);
        let sy = |v: f64| py + ((v - self.top) * ph / self.height).clamp(0.0, ph);
        let (x0, x1) = (sx(x), sx(x + w));
        let (y0, y1) = (sy(y), sy(y + h));
        if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
            return None;
        }
        Some((
            x0.round() as u16,
            y0.round() as u16,
            (x1 - x0).round() as u16,
            (y1 - y0).round() as u16,
        ))
    }

    /// A stream pixel -> the host's coordinates (its center).
    pub fn host_point(&self, x: u16, y: u16) -> (f64, f64) {
        let (px, py) = (self.picture.0 as f64, self.picture.1 as f64);
        let (pw, ph) = (self.picture.2.max(1) as f64, self.picture.3.max(1) as f64);
        (
            self.left + (x as f64 - px + 0.5) * self.width / pw,
            self.top + (y as f64 - py + 0.5) * self.height / ph,
        )
    }
}

/// Elements too small to be a control a person could click (a scrolled-out
/// row Chromium clamps to a sliver, an empty spacer): left out of a window's
/// list, though what they hold is still walked.
pub const MIN_SIDE: f64 = 4.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn retina() -> Geometry {
        // A 1280 x 800 stream of a display whose top-left is at (1512, 0)
        // in points, half the stream's size (a 2x virtual display).
        Geometry {
            left: 1512.0,
            top: 0.0,
            width: 640.0,
            height: 400.0,
            picture: (0, 0, 1280, 800),
        }
    }

    #[test]
    fn a_box_on_a_scaled_display_lands_in_stream_pixels() {
        let g = retina();
        assert_eq!(
            g.stream_box(1512.0 + 10.0, 20.0, 40.0, 12.0),
            Some((20, 40, 80, 24))
        );
    }

    #[test]
    fn a_box_half_off_the_display_is_cut_to_it_and_one_off_it_is_gone() {
        let g = retina();
        assert_eq!(
            g.stream_box(1500.0, 390.0, 24.0, 20.0),
            Some((0, 780, 24, 20))
        );
        assert_eq!(
            g.stream_box(0.0, 0.0, 200.0, 200.0),
            None,
            "another display"
        );
        assert_eq!(
            g.stream_box(1600.0, -500.0, 50.0, 20.0),
            None,
            "scrolled away"
        );
        assert_eq!(g.stream_box(1600.0, 10.0, 0.0, 20.0), None, "empty");
    }

    #[test]
    fn a_stream_pixel_goes_back_to_the_middle_of_its_points() {
        let g = retina();
        let (x, y) = g.host_point(20, 40);
        assert!((x - 1522.25).abs() < 1e-9 && (y - 20.25).abs() < 1e-9);
        let (x, y) = g.host_point(1279, 799);
        assert!(x < 1512.0 + 640.0 && y < 400.0);
    }

    #[test]
    fn a_letterboxed_picture_keeps_its_bars_out_of_the_desktop() {
        // A 1920 x 1200 X screen in a 1280 x 720 stream: 1152 x 720,
        // centred, 64 pixels of bar each side.
        let g = Geometry {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1200.0,
            picture: (64, 0, 1152, 720),
        };
        assert_eq!(
            g.stream_box(0.0, 0.0, 1920.0, 1200.0),
            Some((64, 0, 1152, 720))
        );
        let (x, y) = g.host_point(64 + 576, 360);
        assert!((x - 960.8333).abs() < 0.01 && (y - 600.8333).abs() < 0.01);
    }
}
