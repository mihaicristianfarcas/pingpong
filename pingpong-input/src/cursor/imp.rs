//! The Win32 half of the cursor watcher. See the parent module for why.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use pingpong_proto::control::{CursorShape, CursorState};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClipCursor, GetCursor, GetCursorInfo, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetWindowThreadProcessId, LoadCursorW, CURSORINFO, CURSOR_SHOWING, HCURSOR, IDC_APPSTARTING,
    IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

use super::classify;

/// How often the pointer state is read.
///
/// 20 Hz, not per frame. A mode switch landing within 50 ms is not
/// perceptible; the measured transitions were seconds apart.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(50);

/// How often the foreground thread is joined, when it has to be (see
/// `foreground_cursor`): its side effects are real, so not at 20 Hz.
pub const ATTACH_EVERY: Duration = Duration::from_millis(250);

/// Resend the current state even when nothing changed, this often.
///
/// The state is sent on change, which makes it a handful of packets per
/// session -- but control messages carry no FEC and no retransmit of their own,
/// so a single lost datagram would strand the client in the wrong mode
/// indefinitely. A heartbeat bounds that to half a second.
pub const HEARTBEAT: Duration = Duration::from_millis(500);

/// Samples the foreground app's pointer state, and says when it is worth
/// telling the client.
pub struct CursorWatcher {
    standard: HashMap<isize, CursorShape>,
    last_sent: Option<CursorState>,
    last_sample: Instant,
    last_send: Instant,
    stream: (u32, u32),
    /// The captured display's size in desktop coordinates, so the reported
    /// position can be scaled into stream pixels the client understands.
    display: (u32, u32),
    /// What the last join of the foreground thread said, and when.
    attached: std::cell::Cell<(Option<CursorShape>, Option<Instant>)>,
}

impl CursorWatcher {
    /// `stream` is the negotiated mode's pixel size; `display` is the captured
    /// display's size in Windows' own coordinates. They agree while the process
    /// is DPI-aware, but the scaling is kept for the same reason
    /// `AbsoluteTransform` keeps it -- the agreement is a consequence of that
    /// declaration, not a property of Windows.
    pub fn new(stream: (u32, u32), display: (u32, u32)) -> CursorWatcher {
        let mut standard = HashMap::new();
        for (id, shape) in [
            (IDC_ARROW, CursorShape::Arrow),
            (IDC_IBEAM, CursorShape::IBeam),
            (IDC_WAIT, CursorShape::Wait),
            (IDC_CROSS, CursorShape::Cross),
            (IDC_SIZEALL, CursorShape::SizeAll),
            (IDC_SIZENS, CursorShape::SizeNS),
            (IDC_SIZEWE, CursorShape::SizeWE),
            (IDC_SIZENWSE, CursorShape::SizeNWSE),
            (IDC_SIZENESW, CursorShape::SizeNESW),
            (IDC_HAND, CursorShape::Hand),
            (IDC_NO, CursorShape::No),
            (IDC_HELP, CursorShape::Help),
            (IDC_APPSTARTING, CursorShape::AppStarting),
        ] {
            // SAFETY: a null instance means "a system cursor", which is what
            // these identifiers are. The handles are process-wide and are not
            // ours to free.
            if let Ok(h) = unsafe { LoadCursorW(None, id) } {
                standard.insert(h.0 as isize, shape);
            }
        }
        // Far enough in the past that the first poll always samples and sends.
        let long_ago = Instant::now() - HEARTBEAT - SAMPLE_EVERY;
        CursorWatcher {
            standard,
            last_sent: None,
            last_sample: long_ago,
            last_send: long_ago,
            stream,
            display,
            attached: std::cell::Cell::new((Some(CursorShape::Arrow), None)),
        }
    }

    /// Sample if due, and return the state when the client should be told.
    ///
    /// `None` means "nothing to say": either it is not time to look yet, or the
    /// state is unchanged and the heartbeat is not due.
    pub fn poll(&mut self, now: Instant) -> Option<CursorState> {
        if now.duration_since(self.last_sample) < SAMPLE_EVERY {
            return None;
        }
        self.last_sample = now;

        let state = self.read();
        let changed = self.last_sent != Some(state);
        // Position alone changing is not worth a packet: the client draws the
        // pointer at its OWN position, which is the whole reason it has no lag.
        // Only a change of MODE or SHAPE needs to travel promptly.
        let material = match (self.last_sent, changed) {
            (Some(prev), true) => {
                prev.visible != state.visible
                    || prev.clipped != state.clipped
                    || prev.shape != state.shape
            }
            (None, _) => true,
            (_, false) => false,
        };
        if material || now.duration_since(self.last_send) >= HEARTBEAT {
            self.last_sent = Some(state);
            self.last_send = now;
            return Some(state);
        }
        None
    }

    fn read(&self) -> CursorState {
        let cursor = self.foreground_cursor();
        let clip = self.clip_fraction();
        let (x, y) = self.position();
        classify(cursor, clip, x, y)
    }

    /// The foreground thread's cursor, or `None` when it has hidden the pointer.
    ///
    /// `GetCursor` reports the CALLING thread's cursor, so the input queues have
    /// to be joined first -- that is what makes this the foreground app's answer
    /// rather than our own.
    ///
    /// **Joining the queues has side effects**, as measured: every
    /// join and detach makes Windows send the window under the pointer a
    /// WM_MOUSEMOVE and resets the queue's double-click state, so at 20 Hz a
    /// double-click streamed from Ping arrived as two single clicks (and hover
    /// timers restarted all the time). So the global cursor is read first,
    /// which has none: it answers whenever Windows shows a pointer. Only when
    /// it does not (the app hid it; or no mouse is attached, when it says
    /// nothing at all) is the foreground thread joined, at most every 250 ms,
    /// and never within a double-click's time of a click.
    fn foreground_cursor(&self) -> Option<CursorShape> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: writes one CURSORINFO we own, its size set.
        if unsafe { GetCursorInfo(&mut info) }.is_ok()
            && info.flags.0 & CURSOR_SHOWING.0 != 0
            && !info.hCursor.0.is_null()
        {
            return Some(self.name(info.hCursor));
        }
        let (last, at) = self.attached.get();
        let now = Instant::now();
        // SAFETY: reads a global setting.
        let double_click = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64 + 100);
        let clicked_lately = crate::since_last_button().is_some_and(|d| d < double_click);
        if clicked_lately || at.is_some_and(|t| now.duration_since(t) < ATTACH_EVERY) {
            return last;
        }
        let shape = self.attached_cursor();
        self.attached.set((shape, Some(now)));
        shape
    }

    fn attached_cursor(&self) -> Option<CursorShape> {
        // SAFETY: the attach is undone on every path out. Leaving two input
        // queues joined would make this process interfere with the foreground
        // app's input handling for as long as the session lasts.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return Some(CursorShape::Arrow);
            }
            let fg = GetWindowThreadProcessId(hwnd, None);
            let us = GetCurrentThreadId();
            let need_attach = fg != 0 && fg != us;
            if need_attach && !AttachThreadInput(us, fg, true).as_bool() {
                // Cannot see the app's cursor -- an elevated foreground window
                // blocks the attach under UIPI. Reporting "visible" is the safe
                // answer: the user keeps a working pointer, where guessing
                // "hidden" would lock theirs against a window they cannot even
                // click on.
                return Some(CursorShape::Arrow);
            }

            let handle = GetCursor();
            let shape = if handle.0.is_null() {
                None
            } else {
                Some(self.name(handle))
            };

            if need_attach {
                let _ = AttachThreadInput(us, fg, false);
            }
            shape
        }
    }

    /// A custom cursor is `Arrow`: wrong in detail, never absent. Losing the
    /// pointer entirely over an unrecognised handle would be far worse.
    fn name(&self, handle: HCURSOR) -> CursorShape {
        self.standard
            .get(&(handle.0 as isize))
            .copied()
            .unwrap_or(CursorShape::Arrow)
    }

    /// The clip rectangle as a fraction of the virtual desktop's area, or
    /// `None` when the pointer is not confined at all.
    fn clip_fraction(&self) -> Option<f32> {
        let mut clip = RECT::default();
        // SAFETY: writes one RECT we own.
        if unsafe { GetClipCursor(&mut clip) }.is_err() {
            return None;
        }
        // SAFETY: GetSystemMetrics reads global state and cannot fail.
        let (dw, dh) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as f32,
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as f32,
            )
        };
        let (cw, ch) = (
            (clip.right - clip.left).max(0) as f32,
            (clip.bottom - clip.top).max(0) as f32,
        );
        if cw >= dw && ch >= dh {
            return None;
        }
        Some((cw * ch) / (dw * dh))
    }

    /// The host pointer's position, in the stream's pixels.
    fn position(&self) -> (u16, u16) {
        let mut p = POINT::default();
        // SAFETY: writes one POINT we own.
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            return (0, 0);
        }
        // Screen coordinates are the captured display's own: it sits at the
        // desktop origin, since `activate` makes it primary. Clamped rather
        // than wrapped: a pointer parked on another monitor has no stream
        // pixel, and pinning it to the edge is the honest answer.
        let scale = |v: i32, display: u32, stream: u32| -> u16 {
            if display == 0 {
                return 0;
            }
            let local = v.max(0) as i64;
            ((local * stream as i64) / display as i64).clamp(0, stream.saturating_sub(1) as i64)
                as u16
        };
        (
            scale(p.x, self.display.0, self.stream.0),
            scale(p.y, self.display.1, self.stream.1),
        )
    }
}
