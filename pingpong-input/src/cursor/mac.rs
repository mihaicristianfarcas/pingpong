//! The Mac half of the cursor watcher: the pointer's shape and position on
//! the streamed display, for the client to draw its own pointer (no round
//! trip) instead of one in the picture.
//!
//! macOS has no public "which standard cursor is this": the system cursor
//! (`+[NSCursor currentSystemCursor]`) is told apart from the standard ones
//! by its hot spot, its size and how many images it carries, which differ
//! between every pair that matters (measured on macOS 26; the table is
//! built from the running system's own cursors, so a new look follows).
//! What matches none (an app's own cursor) is drawn as the arrow.
//!
//! The pointer is always reported visible: a Mac app hides it while you
//! type, and that must not switch the client to game mode. A game that
//! takes the mouse is the user's to switch (Ctrl+Alt+Shift+M).

use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSCursor, NSCursorFrameResizeDirections, NSCursorFrameResizePosition};
use objc2_core_graphics::{CGDisplayBounds, CGEvent};
use pingpong_proto::control::{CursorShape, CursorState};

/// How often the pointer is read.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(50);
/// Resend the state this often even when nothing changed (control messages
/// are not retransmitted).
pub const HEARTBEAT: Duration = Duration::from_millis(500);

/// Hot spot and size (points, rounded), and the number of images.
type Key = (i32, i32, i32, i32, usize);

fn key(c: &NSCursor) -> Key {
    let hot = c.hotSpot();
    let image = c.image();
    let size = image.size();
    (
        hot.x.round() as i32,
        hot.y.round() as i32,
        size.width.round() as i32,
        size.height.round() as i32,
        image.representations().len(),
    )
}

/// Get AppKit's cursors working in a process that runs no application
/// (Pong's host): on the main thread, before the first watcher.
pub fn prepare() {
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        // A host in the background: never in the Dock.
        app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    }
}

#[allow(deprecated)]
fn standard() -> Vec<(Key, CursorShape)> {
    use CursorShape::*;
    autoreleasepool(|_| {
        let mut table = vec![
            (key(&NSCursor::arrowCursor()), Arrow),
            (key(&NSCursor::IBeamCursor()), IBeam),
            (key(&NSCursor::IBeamCursorForVerticalLayout()), IBeam),
            (key(&NSCursor::pointingHandCursor()), Hand),
            (key(&NSCursor::crosshairCursor()), Cross),
            (key(&NSCursor::operationNotAllowedCursor()), No),
            (key(&NSCursor::resizeLeftRightCursor()), SizeWE),
            (key(&NSCursor::resizeLeftCursor()), SizeWE),
            (key(&NSCursor::resizeRightCursor()), SizeWE),
            (key(&NSCursor::resizeUpDownCursor()), SizeNS),
            (key(&NSCursor::resizeUpCursor()), SizeNS),
            (key(&NSCursor::resizeDownCursor()), SizeNS),
            (key(&NSCursor::openHandCursor()), SizeAll),
            (key(&NSCursor::closedHandCursor()), SizeAll),
        ];
        // Window edges and corners since macOS 15.
        if objc2::available!(macos = 15.0) {
            let all = NSCursorFrameResizeDirections::All;
            for (pos, shape) in [
                (NSCursorFrameResizePosition::Left, SizeWE),
                (NSCursorFrameResizePosition::Right, SizeWE),
                (NSCursorFrameResizePosition::Top, SizeNS),
                (NSCursorFrameResizePosition::Bottom, SizeNS),
                (NSCursorFrameResizePosition::TopLeft, SizeNWSE),
                (NSCursorFrameResizePosition::BottomRight, SizeNWSE),
                (NSCursorFrameResizePosition::TopRight, SizeNESW),
                (NSCursorFrameResizePosition::BottomLeft, SizeNESW),
            ] {
                table.push((
                    key(&NSCursor::frameResizeCursorFromPosition_inDirections(
                        pos, all,
                    )),
                    shape,
                ));
            }
            table.push((key(&NSCursor::columnResizeCursor()), SizeWE));
            table.push((key(&NSCursor::rowResizeCursor()), SizeNS));
        }
        // Where two shapes look alike, neither is trusted: the arrow.
        let mut out: Vec<(Key, CursorShape)> = Vec::new();
        for (k, shape) in table {
            match out.iter_mut().find(|(o, _)| *o == k) {
                Some((_, s)) if *s != shape => *s = Arrow,
                Some(_) => {}
                None => out.push((k, shape)),
            }
        }
        out
    })
}

/// Samples the pointer, and says when the client needs to hear.
pub struct CursorWatcher {
    display: u32,
    stream: (u32, u32),
    table: Vec<(Key, CursorShape)>,
    shape: CursorShape,
    last_sent: Option<CursorState>,
    last_sample: Instant,
    last_send: Instant,
    /// Cursors met that match no standard one (said once each).
    unknown: Vec<Key>,
}

impl CursorWatcher {
    /// `display`: the streamed display; `stream`: the session's pixel size.
    pub fn new(display: u32, stream: (u32, u32)) -> CursorWatcher {
        let now = Instant::now();
        CursorWatcher {
            display,
            stream,
            table: standard(),
            shape: CursorShape::Arrow,
            last_sent: None,
            last_sample: now - SAMPLE_EVERY,
            last_send: now - HEARTBEAT,
            unknown: Vec::new(),
        }
    }

    #[allow(deprecated)]
    fn sample(&mut self) -> CursorState {
        autoreleasepool(|_| {
            if let Some(c) = NSCursor::currentSystemCursor() {
                let k = key(&c);
                self.shape = match self.table.iter().find(|(t, _)| *t == k) {
                    Some((_, s)) => *s,
                    None => {
                        if !self.unknown.contains(&k) {
                            tracing::debug!(?k, "a cursor of an app's own: drawn as the arrow");
                            self.unknown.push(k);
                        }
                        CursorShape::Arrow
                    }
                };
            }
        });
        // Where the pointer is on the streamed display, in stream pixels.
        let at = CGEvent::new(None).map(|e| CGEvent::location(Some(&e)));
        let b = CGDisplayBounds(self.display);
        let (x, y) = match at {
            Some(p) if b.size.width > 0.0 && b.size.height > 0.0 => (
                ((p.x - b.origin.x) / b.size.width * self.stream.0 as f64)
                    .clamp(0.0, self.stream.0.saturating_sub(1) as f64),
                ((p.y - b.origin.y) / b.size.height * self.stream.1 as f64)
                    .clamp(0.0, self.stream.1.saturating_sub(1) as f64),
            ),
            _ => (0.0, 0.0),
        };
        CursorState {
            visible: true,
            clipped: false,
            shape: self.shape,
            x: x as u16,
            y: y as u16,
        }
    }

    /// The state to send now, if any: when it changed, or as a heartbeat.
    pub fn poll(&mut self, now: Instant) -> Option<CursorState> {
        if now.duration_since(self.last_sample) < SAMPLE_EVERY {
            return None;
        }
        self.last_sample = now;
        let state = self.sample();
        let changed = self
            .last_sent
            .is_none_or(|l| l.shape != state.shape || l.visible != state.visible);
        if changed || now.duration_since(self.last_send) >= HEARTBEAT {
            if changed {
                tracing::debug!(shape = ?state.shape, "cursor");
            }
            self.last_sent = Some(state);
            self.last_send = now;
            return Some(state);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    /// Needs AppKit's application object, made on the main thread: tests run
    /// on others (`examples/mac-cursor-probe.rs` shows the table instead).
    #[test]
    fn the_standard_cursors_are_told_apart() {
        if objc2::MainThreadMarker::new().is_none() {
            return;
        }
        super::prepare();
        let table = super::standard();
        let shapes: Vec<_> = table.iter().map(|(_, s)| *s).collect();
        use pingpong_proto::control::CursorShape::*;
        for s in [Arrow, IBeam, Hand, Cross, SizeWE, SizeNS] {
            assert!(shapes.contains(&s), "{s:?} in {table:?}");
        }
    }
}
