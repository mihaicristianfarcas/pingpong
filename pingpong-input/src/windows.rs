//! `SendInput` injection (v2 design §5.1, §5.2).
//!
//! Keys inject with `KEYEVENTF_SCANCODE`, so Windows derives the virtual key
//! from the scancode using the HOST's active layout. For positional input --
//! WASD, hotkeys, modifiers -- that is exactly right and is why games work.
//! For text entry the character follows the host's layout, not the client's.
//! v2 design §5.1 states that tradeoff and accepts it deliberately; it is not
//! a bug.

use pingpong_proto::input::{Button, InputEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
};
// XBUTTON1/XBUTTON2 live under WindowsAndMessaging in `windows` 0.61, not
// beside the MOUSEEVENTF_* flags they are used with. Taken from the SDK rather
// than hand-written as 1 and 2: a magic number here injects the wrong side
// button and nothing about that is visible until a player uses one.
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{AbsoluteTransform, DisplayRect, HeldSet, InputError, InputSink, VirtualDesktop};

impl DisplayRect {
    /// The primary display's rectangle, in virtual-desktop coordinates.
    ///
    /// The primary display *defines* the virtual desktop's origin, so its
    /// top-left is (0, 0) by construction. `activate` makes the virtual display
    /// primary, which is why this is the right rectangle for the session.
    ///
    /// `SM_CXSCREEN` is in the desktop's coordinate space, not in scanned-out
    /// pixels: a display at 125% reports 2419 for a 3024-pixel-wide panel. That
    /// difference is exactly why the transform scales rather than assuming the
    /// stream and the desktop share units.
    pub fn primary() -> DisplayRect {
        // SAFETY: GetSystemMetrics reads global state and cannot fail.
        unsafe {
            DisplayRect {
                left: 0,
                top: 0,
                width: GetSystemMetrics(SM_CXSCREEN).max(0) as u32,
                height: GetSystemMetrics(SM_CYSCREEN).max(0) as u32,
            }
        }
    }
}

impl VirtualDesktop {
    /// The bounding rectangle of every attached display, as Windows reports it.
    ///
    /// This is the space `MOUSEEVENTF_VIRTUALDESK` normalises against, so it is
    /// what the absolute transform must be built from. It is larger than the
    /// captured display whenever the host's physical monitor is still attached,
    /// which is the normal case.
    pub fn current() -> VirtualDesktop {
        // SAFETY: GetSystemMetrics reads global state and cannot fail; an
        // unknown index simply returns 0.
        unsafe {
            VirtualDesktop {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32,
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32,
            }
        }
    }
}

/// The E0 extended prefix rides in the scancode's high bit (v2 design §4.3).
const EXTENDED_BIT: u16 = 0x8000;

pub struct SendInputSink {
    held: HeldSet,
    /// Stream pixels -> SendInput's absolute space. Built once at open: a
    /// session's display geometry does not change under it, and if it does the
    /// session is torn down anyway (v2 design §6.4).
    absolute: AbsoluteTransform,
    typist: Typist,
}

impl SendInputSink {
    pub fn new(absolute: AbsoluteTransform) -> SendInputSink {
        SendInputSink {
            held: HeldSet::new(),
            absolute,
            typist: Typist::spawn(),
        }
    }

    /// Now, or behind the text still being typed, so order holds.
    fn send(&self, inputs: Vec<INPUT>) -> Result<(), InputError> {
        if inputs.is_empty() {
            return Ok(());
        }
        if !self.typist.busy() {
            return submit(&inputs);
        }
        match self.typist.queue(inputs, false) {
            None => Ok(()),
            Some(inputs) => submit(&inputs),
        }
    }

    fn to_inputs(&self, ev: InputEvent, out: &mut Vec<INPUT>) {
        match ev {
            InputEvent::KeyDown(sc) => out.push(key_input(sc, false)),
            InputEvent::KeyUp(sc) => out.push(key_input(sc, true)),
            InputEvent::MouseMoveRel { dx, dy } => {
                // Relative: no ABSOLUTE flag, so the delta is applied to the
                // current position. This is the path games read as raw motion
                // (v2 design §5.2) and it must never carry MOUSEEVENTF_ABSOLUTE.
                out.push(mouse_input(dx as i32, dy as i32, MOUSEEVENTF_MOVE.0, 0));
            }
            InputEvent::MouseMoveAbs { x, y } => {
                // Normalise stream pixels into the 0..=65535 space. The client
                // has already done window -> stream (v2 design §5.2), so this is the
                // only transform the host performs.
                //
                // VIRTUALDESK below means Windows reads this pair across every
                // attached display, so the transform must be built from the
                // virtual desktop's bounds and the captured display's origin
                // within it -- not from the captured display's size alone.
                let (nx, ny) = self.absolute.normalize(x, y);
                out.push(mouse_input(
                    nx,
                    ny,
                    MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0,
                    0,
                ));
            }
            InputEvent::ButtonDown(b) => out.push(button_input(b, false)),
            InputEvent::ButtonUp(b) => out.push(button_input(b, true)),
            InputEvent::Wheel { dv, dh } => {
                if dv != 0 {
                    out.push(mouse_input(0, 0, MOUSEEVENTF_WHEEL.0, dv as i32));
                }
                if dh != 0 {
                    out.push(mouse_input(0, 0, MOUSEEVENTF_HWHEEL.0, dh as i32));
                }
            }
            // Text: each UTF-16 unit as a Unicode keystroke (Sunshine's way),
            // so it arrives as typed whatever the host's layout.
            InputEvent::Text(c) => {
                let mut units = [0u16; 2];
                for &unit in c.encode_utf16(&mut units).iter() {
                    for up in [false, true] {
                        let flags = if up {
                            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                        } else {
                            KEYEVENTF_UNICODE
                        };
                        out.push(INPUT {
                            r#type: INPUT_KEYBOARD,
                            Anonymous: INPUT_0 {
                                ki: KEYBDINPUT {
                                    wVk: Default::default(),
                                    wScan: unit,
                                    dwFlags: flags,
                                    time: 0,
                                    dwExtraInfo: 0,
                                },
                            },
                        });
                    }
                }
            }
        }
    }
}

fn submit(inputs: &[INPUT]) -> Result<(), InputError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let mut sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() && sync_thread_desktop() {
        // The input desktop changed under us (a UAC prompt or the lock
        // screen is up): SendInput only reaches the desktop the calling
        // thread is attached to. Re-attach and retry once, as Apollo does.
        // Works for the secure desktop only when running as SYSTEM.
        sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    }
    if sent as usize != inputs.len() {
        return Err(InputError::SendInput {
            sent,
            expected: inputs.len() as u32,
        });
    }
    Ok(())
}

/// How long after one typed character the next may go. Measured on
/// Windows 11's Notepad: 8 ms garbles, 12 ms and up does not.
const TEXT_GAP: Duration = Duration::from_millis(15);

/// Text, one character at a time. Windows 11's WinUI text fields (the new
/// Notepad) stall for a moment after each word, and read the Unicode
/// keystrokes that queue meanwhile as whichever came last -- "pingpong
/// rrrrrrrrrrrrown" for a sentence an agent typed. Layout keys fare no better
/// (they are dropped), and the stall is not on the thread a window message
/// would wait for, so the only cure is time: each character alone,
/// `TEXT_GAP` after the one before. On this thread, not the one packets
/// arrive on; whatever follows text while it is still being typed (an Enter,
/// a click) waits behind it.
struct Typist {
    jobs: Option<Sender<Job>>,
    /// Queued and not yet sent.
    pending: Arc<AtomicUsize>,
    /// Jobs queued before a bump are dropped (`release_all`).
    generation: Arc<AtomicU64>,
    /// When this thread last sent something.
    sent: Arc<Mutex<Option<Instant>>>,
}

struct Job {
    inputs: Vec<INPUT>,
    /// A character, `TEXT_GAP` after the last.
    paced: bool,
    generation: u64,
}

impl Typist {
    fn spawn() -> Typist {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let generation = Arc::new(AtomicU64::new(0));
        let sent = Arc::new(Mutex::new(None));
        let (p, g, s) = (pending.clone(), generation.clone(), sent.clone());
        let spawned = std::thread::Builder::new()
            .name("typist".into())
            .spawn(move || {
                let mut last: Option<Instant> = None;
                for job in rx {
                    if job.generation == g.load(Ordering::Acquire) {
                        if let (true, Some(wait)) = (
                            job.paced,
                            last.and_then(|t| TEXT_GAP.checked_sub(t.elapsed())),
                        ) {
                            std::thread::sleep(wait);
                        }
                        if let Err(e) = submit(&job.inputs) {
                            tracing::debug!(error = %e, "typing");
                        }
                        let now = Instant::now();
                        *s.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
                        if job.paced {
                            last = Some(now);
                        }
                    }
                    p.fetch_sub(1, Ordering::AcqRel);
                }
            });
        if let Err(e) = &spawned {
            tracing::warn!(error = %e, "no typing thread; text goes out at once");
        }
        Typist {
            jobs: spawned.ok().map(|_| tx),
            pending,
            generation,
            sent,
        }
    }

    fn busy(&self) -> bool {
        self.pending.load(Ordering::Acquire) > 0
    }

    /// Returns the inputs when there is no thread to take them.
    fn queue(&self, inputs: Vec<INPUT>, paced: bool) -> Option<Vec<INPUT>> {
        let Some(jobs) = &self.jobs else {
            return Some(inputs);
        };
        self.pending.fetch_add(1, Ordering::AcqRel);
        let job = Job {
            inputs,
            paced,
            generation: self.generation.load(Ordering::Acquire),
        };
        if let Err(mpsc::SendError(job)) = jobs.send(job) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Some(job.inputs);
        }
        None
    }

    /// Drop what is queued, and wait (briefly) for what is being sent.
    fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let until = Instant::now() + Duration::from_millis(250);
        while self.busy() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Typist {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

fn key_input(scancode: u16, up: bool) -> INPUT {
    let extended = scancode & EXTENDED_BIT != 0;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: Default::default(),
                wScan: scancode & !EXTENDED_BIT,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, flags: u32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn button_input(b: Button, up: bool) -> INPUT {
    let (flags, data) = match (b, up) {
        (Button::Left, false) => (MOUSEEVENTF_LEFTDOWN.0, 0),
        (Button::Left, true) => (MOUSEEVENTF_LEFTUP.0, 0),
        (Button::Right, false) => (MOUSEEVENTF_RIGHTDOWN.0, 0),
        (Button::Right, true) => (MOUSEEVENTF_RIGHTUP.0, 0),
        (Button::Middle, false) => (MOUSEEVENTF_MIDDLEDOWN.0, 0),
        (Button::Middle, true) => (MOUSEEVENTF_MIDDLEUP.0, 0),
        (Button::X1, false) => (MOUSEEVENTF_XDOWN.0, XBUTTON1 as i32),
        (Button::X1, true) => (MOUSEEVENTF_XUP.0, XBUTTON1 as i32),
        (Button::X2, false) => (MOUSEEVENTF_XDOWN.0, XBUTTON2 as i32),
        (Button::X2, true) => (MOUSEEVENTF_XUP.0, XBUTTON2 as i32),
    };
    mouse_input(0, 0, flags, data)
}

impl InputSink for SendInputSink {
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError> {
        let mut inputs = Vec::with_capacity(events.len() + 1);
        let mut result = Ok(());
        for &ev in events {
            self.held.observe(ev);
            if let InputEvent::Text(_) = ev {
                // What came before it first, then the character, paced.
                result = result.and(self.send(std::mem::take(&mut inputs)));
                let mut ch = Vec::with_capacity(4);
                self.to_inputs(ev, &mut ch);
                if let Some(ch) = self.typist.queue(ch, true) {
                    result = result.and(submit(&ch));
                }
            } else {
                self.to_inputs(ev, &mut inputs);
            }
        }
        result.and(self.send(inputs))
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        // Typing that has not gone out yet stops here.
        self.typist.cancel();
        let releases = self.held.drain_release_events();
        if releases.is_empty() {
            return Ok(());
        }
        tracing::info!(count = releases.len(), "releasing held input");
        let mut inputs = Vec::with_capacity(releases.len());
        for ev in releases {
            self.to_inputs(ev, &mut inputs);
        }
        submit(&inputs)
    }

    fn last_sent(&self) -> Option<Instant> {
        *self.typist.sent.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Attach this thread to whichever desktop currently receives input. Returns
/// whether it succeeded.
pub fn sync_thread_desktop() -> bool {
    use windows::Win32::Foundation::GENERIC_ALL;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
        DF_ALLOWOTHERACCOUNTHOOK,
    };
    unsafe {
        match OpenInputDesktop(
            DF_ALLOWOTHERACCOUNTHOOK,
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        ) {
            Ok(desk) => {
                let ok = SetThreadDesktop(desk).is_ok();
                let _ = CloseDesktop(desk);
                ok
            }
            Err(_) => false,
        }
    }
}
