//! The stream window on Windows: its own thread and message loop, a
//! borderless window covering the monitor (or a normal one), and all
//! keyboard and mouse input.
//!
//! Feels like Moonlight's stream:
//! - while captured, the keyboard is the host's: a low-level hook takes every
//!   key, the Windows key and Alt+Tab included (in a window, those stay
//!   Windows'), as SDL's keyboard grab does;
//! - the pointer is confined to the window and hidden: the host draws its
//!   own into the picture, and raw relative motion goes to it (Moonlight's
//!   default), or, after +M, the pointer's position. Against an older host
//!   that leaves the pointer to the client, it is Windows' own pointer in
//!   the host application's shape on the desktop, and hidden only while a
//!   game has taken the mouse;
//! - Ctrl+Alt+Shift+Q quits, +S toggles statistics, +Z releases/recaptures
//!   the mouse and keyboard, +M switches mouse mode, +X full screen, +V types
//!   the clipboard, +D minimises (Moonlight's chords).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use pingpong_proto::control::CursorShape;
use pingpong_proto::input::{Button, InputEvent};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, GetStockObject, MonitorFromPoint, MonitorFromWindow,
    ValidateRect, BLACK_BRUSH, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_ESCAPE, VK_F4, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN, VK_TAB,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_REMOVE, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::render::{Layout, RenderShared};
use crate::input::InputSender;
use crate::keymap;
use crate::pointer::PointerState;

const WM_APP_QUIT: u32 = WM_APP + 1;
const WM_APP_FULLSCREEN: u32 = WM_APP + 2;
const WM_APP_CURSOR: u32 = WM_APP + 3;
const WM_APP_MINIMIZE: u32 = WM_APP + 4;
const WM_APP_CAPTURE: u32 = WM_APP + 5;

/// The stream window, for the CLI's test hooks.
static CURRENT: AtomicIsize = AtomicIsize::new(0);

/// What the window needs from the session, owned by its thread.
pub struct Handler {
    pub input: InputSender,
    pub render: Arc<RenderShared>,
    pub pointer: Arc<Mutex<PointerState>>,
    pub on_quit: Box<dyn Fn() + Send>,
    /// The stream window gained (true) or lost (false) focus.
    pub on_focus: Option<Box<dyn Fn(bool) + Send>>,
    /// Ctrl+Alt+Shift+T, watching an AI agent: take over or hand back.
    pub on_take_over: Option<Box<dyn Fn() + Send>>,
    held_keys: HashSet<u16>,
    held_buttons: HashSet<u8>,
    ctrl: [bool; 2],
    alt: [bool; 2],
    shift: [bool; 2],
    /// Keys swallowed as the last key of a hotkey chord: their key-up must
    /// not be forwarded either.
    swallowed: HashSet<u16>,
}

impl Handler {
    pub fn new(
        input: InputSender,
        render: Arc<RenderShared>,
        pointer: Arc<Mutex<PointerState>>,
        on_quit: Box<dyn Fn() + Send>,
    ) -> Handler {
        Handler {
            input,
            render,
            pointer,
            on_quit,
            on_focus: None,
            on_take_over: None,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            ctrl: [false; 2],
            alt: [false; 2],
            shift: [false; 2],
            swallowed: HashSet::new(),
        }
    }

    fn captured(&self) -> bool {
        self.pointer.lock().captured
    }

    fn chord(&self) -> bool {
        self.ctrl.iter().any(|&d| d) && self.alt.iter().any(|&d| d) && self.shift.iter().any(|&d| d)
    }

    /// Release everything held on the host: on focus loss, capture release and
    /// quit, so nothing stays stuck down.
    fn release_all(&mut self) {
        for sc in self.held_keys.drain() {
            self.input.send(InputEvent::KeyUp(sc));
        }
        for b in self.held_buttons.drain() {
            if let Some(b) = button(b) {
                self.input.send(InputEvent::ButtonUp(b));
            }
        }
    }

    fn send_key(&mut self, sc: u16, down: bool) {
        if down {
            if self.held_keys.insert(sc) {
                self.input.send(InputEvent::KeyDown(sc));
            }
        } else if self.held_keys.remove(&sc) {
            self.input.send(InputEvent::KeyUp(sc));
        }
    }

    /// Moonlight's chords (Ctrl+Alt+Shift+key). True when it was one.
    fn hotkey(&mut self, hwnd: HWND, sc: u16) -> bool {
        if !self.chord() {
            return false;
        }
        let post = |msg: u32| unsafe {
            let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
        };
        match sc {
            0x10 => {
                // Q
                self.release_all();
                (self.on_quit)();
            }
            0x1F => {
                // S
                let on = self.render.toggle_overlay();
                tracing::info!(on, "statistics overlay");
            }
            // Z: after the hook returns (capture changes the pointer).
            0x2C => post(WM_APP_CAPTURE),
            0x2F => {
                // V: type the clipboard on the host (Moonlight's). Ctrl, Alt
                // and Shift are down on the host: lift them, or the text would
                // type as shortcuts.
                if let Some(text) = clipboard_text() {
                    for sc in self.held_keys.drain() {
                        self.input.send(InputEvent::KeyUp(sc));
                    }
                    let chars = self.input.type_text(&text);
                    tracing::info!(chars, "clipboard typed on the host");
                }
            }
            0x32 => {
                // M: mouse mode, relative <-> desktop pointer (Moonlight's).
                let mut p = self.pointer.lock();
                let relative = p.toggle_mode();
                tracing::info!(
                    relative,
                    automatic = p.mode_override.is_none(),
                    "mouse mode"
                );
                drop(p);
                post(WM_APP_CURSOR);
            }
            0x2D => post(WM_APP_FULLSCREEN),
            0x14 => {
                // T: watching an agent, take over or hand back.
                if self.on_take_over.is_none() {
                    return false;
                }
                self.release_all();
                if let Some(f) = &self.on_take_over {
                    f();
                }
            }
            0x20 => {
                self.release_all();
                post(WM_APP_MINIMIZE);
            }
            _ => return false,
        }
        true
    }

    /// A key from the low-level hook, while the stream window is in front.
    /// True: it is the stream's, and Windows must not see it.
    fn key(&mut self, hwnd: HWND, kb: &KBDLLHOOKSTRUCT, down: bool, fullscreen: bool) -> bool {
        let vk = kb.vkCode;
        let fake = kb.scanCode & 0x200 != 0;
        let side = |l: u16, r: u16| -> Option<usize> {
            if vk == l as u32 {
                Some(0)
            } else if vk == r as u32 {
                Some(1)
            } else {
                None
            }
        };
        if !fake {
            if let Some(i) = side(VK_LCONTROL.0, VK_RCONTROL.0) {
                self.ctrl[i] = down;
            } else if let Some(i) = side(VK_LMENU.0, VK_RMENU.0) {
                self.alt[i] = down;
            } else if let Some(i) = side(VK_LSHIFT.0, VK_RSHIFT.0) {
                self.shift[i] = down;
            }
        }
        let extended = kb.flags.0 & LLKHF_EXTENDED.0 != 0;
        let Some(sc) = keymap::windows_scancode(kb.scanCode, extended, vk) else {
            // Pause, media and volume keys: the protocol has none; they stay
            // this computer's.
            return false;
        };
        if down && self.hotkey(hwnd, sc) {
            self.swallowed.insert(sc);
            return true;
        }
        if !down && self.swallowed.remove(&sc) {
            return true;
        }
        if !self.captured() {
            return false;
        }
        // In a window, the system's own shortcuts stay the system's.
        if !fullscreen {
            let alt = self.alt.iter().any(|&d| d);
            let ctrl = self.ctrl.iter().any(|&d| d);
            let system = vk == VK_LWIN.0 as u32
                || vk == VK_RWIN.0 as u32
                || (alt
                    && (vk == VK_TAB.0 as u32 || vk == VK_ESCAPE.0 as u32 || vk == VK_F4.0 as u32))
                || (ctrl && vk == VK_ESCAPE.0 as u32);
            if system {
                return false;
            }
        }
        self.send_key(sc, down);
        true
    }

    fn mouse_button(&mut self, n: u8, down: bool) -> bool {
        if !self.captured() {
            // The first click into an uncaptured window recaptures (Moonlight).
            return down;
        }
        let Some(b) = button(n) else { return false };
        if down {
            if self.held_buttons.insert(n) {
                self.input.send(InputEvent::ButtonDown(b));
            }
        } else if self.held_buttons.remove(&n) {
            self.input.send(InputEvent::ButtonUp(b));
        }
        false
    }
}

fn button(n: u8) -> Option<Button> {
    Some(match n {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::X1,
        4 => Button::X2,
        _ => return None,
    })
}

/// The window thread's state.
struct State {
    hwnd: HWND,
    handler: RefCell<Handler>,
    fullscreen: Cell<bool>,
}

thread_local! {
    static STATE: Cell<*const State> = const { Cell::new(std::ptr::null()) };
}

fn state() -> Option<&'static State> {
    let p = STATE.with(|s| s.get());
    // SAFETY: set to a State that outlives the message loop, cleared after.
    unsafe { p.as_ref() }
}

pub struct Window {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl Window {
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Open the window on its own thread; returns once it is up.
    pub fn open(
        handler: Handler,
        fullscreen: bool,
        stream: (u32, u32),
        title: &str,
    ) -> Result<Window, String> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<isize, String>>();
        let title = title.to_string();
        let thread = std::thread::Builder::new()
            .name("ping-window".into())
            .spawn(move || window_thread(handler, fullscreen, stream, &title, tx))
            .map_err(|e| e.to_string())?;
        match rx.recv() {
            Ok(Ok(hwnd)) => {
                CURRENT.store(hwnd, Ordering::Release);
                Ok(Window {
                    hwnd,
                    thread: Some(thread),
                })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("the stream window's thread ended".into()),
        }
    }

    pub fn close(&mut self) {
        let _ = CURRENT.compare_exchange(self.hwnd, 0, Ordering::AcqRel, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_APP_QUIT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            let _ = t.join();
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.close();
    }
}

/// Moonlight's Ctrl+Alt+Shift+X for the current stream window: full screen
/// <-> a window.
pub fn toggle_fullscreen() {
    let hwnd = CURRENT.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_APP_FULLSCREEN,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// The text on the clipboard, if any.
pub fn clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT).ok().and_then(|h| {
            let mem = HGLOBAL(h.0);
            let p = GlobalLock(mem) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            let _ = GlobalUnlock(mem);
            Some(s)
        });
        let _ = CloseClipboard();
        text
    }
}

fn client_screen_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
        let mut tl = POINT {
            x: r.left,
            y: r.top,
        };
        let mut br = POINT {
            x: r.right,
            y: r.bottom,
        };
        let _ = ClientToScreen(hwnd, &mut tl);
        let _ = ClientToScreen(hwnd, &mut br);
        RECT {
            left: tl.x,
            top: tl.y,
            right: br.x,
            bottom: br.y,
        }
    }
}

fn monitor_rects(hwnd: Option<HWND>) -> (RECT, RECT) {
    unsafe {
        let monitor = match hwnd {
            Some(h) => MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST),
            None => {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST)
            }
        };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        (info.rcMonitor, info.rcWork)
    }
}

const WINDOWED: WINDOW_STYLE = WS_OVERLAPPEDWINDOW;
const FULLSCREEN: WINDOW_STYLE = WS_POPUP;

/// The outer rectangle of a window whose client area is `w`x`h`, centred in
/// `work`.
fn windowed_rect(work: RECT, w: i32, h: i32, dpi: u32) -> RECT {
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    let mut r = RECT {
        left: 0,
        top: 0,
        right: w.min(ww * 9 / 10),
        bottom: h.min(wh * 9 / 10),
    };
    unsafe {
        let _ = AdjustWindowRectExForDpi(&mut r, WINDOWED, false, WINDOW_EX_STYLE(0), dpi);
    }
    let (rw, rh) = (r.right - r.left, r.bottom - r.top);
    let x = work.left + (ww - rw) / 2;
    let y = work.top + (wh - rh) / 2;
    RECT {
        left: x,
        top: y,
        right: x + rw,
        bottom: y + rh,
    }
}

fn publish_layout(s: &State) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(s.hwnd, &mut r);
    }
    let dpi = unsafe { GetDpiForWindow(s.hwnd) }.max(96);
    let h = s.handler.borrow();
    h.render.set_layout(Layout {
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
        scale: dpi as f64 / 96.0,
    });
}

/// Capture or release the pointer and keyboard.
fn set_capture(s: &State, captured: bool) {
    let mut h = s.handler.borrow_mut();
    {
        let mut p = h.pointer.lock();
        if p.captured == captured {
            return;
        }
        p.captured = captured;
    }
    if !captured {
        h.release_all();
    }
    drop(h);
    apply_pointer(s);
    tracing::info!(captured, "pointer capture");
}

/// Confine the pointer to the window while captured, and hide it -- or,
/// for an older host on its desktop, show it in the host's shape.
fn apply_pointer(s: &State) {
    let h = s.handler.borrow();
    let p = *h.pointer.lock();
    unsafe {
        if p.captured {
            let r = client_screen_rect(s.hwnd);
            let _ = ClipCursor(Some(&r));
        } else {
            let _ = ClipCursor(None);
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let r = client_screen_rect(s.hwnd);
        let inside = pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
        if inside && GetForegroundWindow() == s.hwnd {
            SetCursor(cursor_for(&p));
        }
    }
}

fn cursor_for(p: &PointerState) -> Option<HCURSOR> {
    if p.captured && (p.is_relative() || p.host_draws) {
        return None;
    }
    let id = match p.shape {
        CursorShape::Arrow => IDC_ARROW,
        CursorShape::IBeam => IDC_IBEAM,
        CursorShape::Wait => IDC_WAIT,
        CursorShape::Cross => IDC_CROSS,
        CursorShape::SizeAll => IDC_SIZEALL,
        CursorShape::SizeNS => IDC_SIZENS,
        CursorShape::SizeWE => IDC_SIZEWE,
        CursorShape::SizeNWSE => IDC_SIZENWSE,
        CursorShape::SizeNESW => IDC_SIZENESW,
        CursorShape::Hand => IDC_HAND,
        CursorShape::No => IDC_NO,
        CursorShape::Help => IDC_HELP,
        CursorShape::AppStarting => IDC_APPSTARTING,
    };
    unsafe { LoadCursorW(None, id).ok() }
}

/// Map a client-area point to stream pixels, through where the picture is.
fn to_stream(s: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let h = s.handler.borrow();
    let ((vx, vy, vw, vh), (sw, sh)) = h.render.video_rect()?;
    if vw <= 0.0 || vh <= 0.0 {
        return None;
    }
    Some((
        ((x as f64 - vx) * sw / vw) as f32,
        ((y as f64 - vy) * sh / vh) as f32,
    ))
}

/// Ask the window `hwnd` to reconsider the pointer (the host's changed).
pub fn post_cursor_changed(hwnd: isize) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            WM_APP_CURSOR,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Put Windows' pointer on stream pixel (`x`, `y`) of the picture in window
/// `hwnd` (a controller driving it, or leaving a game).
pub fn place_pointer(hwnd: isize, render: &RenderShared, x: f32, y: f32) {
    if hwnd == 0 {
        return;
    }
    let Some(((vx, vy, vw, vh), (sw, sh))) = render.video_rect() else {
        return;
    };
    if sw <= 0.0 || sh <= 0.0 {
        return;
    }
    let mut pt = POINT {
        x: (vx + x as f64 * vw / sw).round() as i32,
        y: (vy + y as f64 * vh / sh).round() as i32,
    };
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        let _ = ClientToScreen(hwnd, &mut pt);
        let _ = SetCursorPos(pt.x, pt.y);
    }
}

fn toggle_fullscreen_now(s: &State) {
    let to_window = s.fullscreen.get();
    let (monitor, work) = monitor_rects(Some(s.hwnd));
    unsafe {
        if to_window {
            let dpi = GetDpiForWindow(s.hwnd).max(96);
            let (mw, mh) = (monitor.right - monitor.left, monitor.bottom - monitor.top);
            let r = windowed_rect(work, mw * 8 / 10, mh * 8 / 10, dpi);
            SetWindowLongPtrW(s.hwnd, GWL_STYLE, (WINDOWED | WS_VISIBLE).0 as isize);
            let _ = SetWindowPos(
                s.hwnd,
                Some(HWND_NOTOPMOST),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_SHOWWINDOW,
            );
        } else {
            SetWindowLongPtrW(s.hwnd, GWL_STYLE, (FULLSCREEN | WS_VISIBLE).0 as isize);
            let _ = SetWindowPos(
                s.hwnd,
                Some(HWND_TOP),
                monitor.left,
                monitor.top,
                monitor.right - monitor.left,
                monitor.bottom - monitor.top,
                SWP_FRAMECHANGED | SWP_SHOWWINDOW,
            );
        }
    }
    s.fullscreen.set(!to_window);
    publish_layout(s);
    apply_pointer(s);
    tracing::info!(fullscreen = !to_window, "window mode");
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}

fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}

fn point(l: LPARAM) -> (i32, i32) {
    (
        (l.0 & 0xFFFF) as i16 as i32,
        ((l.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(s) = state().filter(|s| s.hwnd == hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_ACTIVATE => {
            let how = loword(wparam.0) as u32;
            let active = how != WA_INACTIVE && hiword(wparam.0) == 0;
            // A click activating a window may be on its title bar, about to
            // move it: capture then waits for a click in the picture.
            if !active || how != WA_CLICKACTIVE || s.fullscreen.get() {
                set_capture(s, active);
            }
            if let Some(f) = &s.handler.borrow().on_focus {
                f(active);
            }
            LRESULT(0)
        }
        WM_SIZE | WM_MOVE => {
            publish_layout(s);
            if s.handler.borrow().captured() {
                apply_pointer(s);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DPICHANGED => {
            let r = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            publish_layout(s);
            LRESULT(0)
        }
        WM_INPUT => {
            let mut raw = RAWINPUT::default();
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let got = unsafe {
                GetRawInputData(
                    HRAWINPUT(lparam.0 as *mut _),
                    RID_INPUT,
                    Some(&mut raw as *mut _ as *mut _),
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
                let m = unsafe { raw.data.mouse };
                // Relative devices only (a tablet or remote desktop reports
                // positions, which the window messages carry anyway).
                if m.usFlags.0 & 1 == 0 && (m.lLastX != 0 || m.lLastY != 0) {
                    let h = s.handler.borrow();
                    let p = h.pointer.lock();
                    if p.captured && p.is_relative() {
                        h.input.motion(m.lLastX as f64, m.lLastY as f64);
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEMOVE => {
            let (x, y) = point(lparam);
            let relative = {
                let h = s.handler.borrow();
                let p = h.pointer.lock();
                !p.captured || p.is_relative()
            };
            if !relative {
                if let Some((sx, sy)) = to_stream(s, x, y) {
                    let h = s.handler.borrow();
                    h.pointer.lock().place(sx, sy, &h.input);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (n, down) = match msg {
                WM_LBUTTONDOWN => (0, true),
                WM_LBUTTONUP => (0, false),
                WM_RBUTTONDOWN => (1, true),
                WM_RBUTTONUP => (1, false),
                WM_MBUTTONDOWN => (2, true),
                WM_MBUTTONUP => (2, false),
                _ => (
                    if hiword(wparam.0) == 1 { 3 } else { 4 },
                    msg == WM_XBUTTONDOWN,
                ),
            };
            let recapture = s.handler.borrow_mut().mouse_button(n, down);
            if recapture {
                set_capture(s, true);
            }
            if msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP {
                LRESULT(1)
            } else {
                LRESULT(0)
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let h = s.handler.borrow();
            if h.captured() {
                let delta = hiword(wparam.0) as i16;
                h.input.send(if msg == WM_MOUSEWHEEL {
                    InputEvent::Wheel { dv: delta, dh: 0 }
                } else {
                    InputEvent::Wheel { dv: 0, dh: delta }
                });
            }
            LRESULT(0)
        }
        WM_SETCURSOR if loword(lparam.0 as usize) as u32 == HTCLIENT => {
            let p = *s.handler.borrow().pointer.lock();
            unsafe {
                SetCursor(cursor_for(&p));
            }
            LRESULT(1)
        }
        WM_SYSCOMMAND if (wparam.0 & 0xFFF0) as u32 == SC_KEYMENU => LRESULT(0),
        WM_CLOSE => {
            let mut h = s.handler.borrow_mut();
            h.release_all();
            (h.on_quit)();
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        WM_APP_CURSOR => {
            apply_pointer(s);
            LRESULT(0)
        }
        WM_APP_CAPTURE => {
            let captured = s.handler.borrow().captured();
            set_capture(s, !captured);
            LRESULT(0)
        }
        WM_APP_FULLSCREEN => {
            toggle_fullscreen_now(s);
            LRESULT(0)
        }
        WM_APP_MINIMIZE => {
            set_capture(s, false);
            unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            LRESULT(0)
        }
        WM_APP_QUIT => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        if let Some(s) = state() {
            if unsafe { GetForegroundWindow() } == s.hwnd {
                let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
                let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                // Busy (a modal loop inside the window procedure): let it be.
                if let Ok(mut h) = s.handler.try_borrow_mut() {
                    if h.key(s.hwnd, kb, down, s.fullscreen.get()) {
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn register_raw_mouse(hwnd: Option<HWND>) {
    let device = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: if hwnd.is_some() {
            Default::default()
        } else {
            RIDEV_REMOVE
        },
        hwndTarget: hwnd.unwrap_or_default(),
    };
    unsafe {
        if let Err(e) =
            RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        {
            tracing::warn!(error = %e, "raw mouse input unavailable");
        }
    }
}

fn window_thread(
    handler: Handler,
    fullscreen: bool,
    stream: (u32, u32),
    title: &str,
    ready: std::sync::mpsc::Sender<Result<isize, String>>,
) {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        let class = w!("PingStream");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            // MAKEINTRESOURCE(1): the icon build.rs embeds in Ping.exe.
            hIcon: LoadIconW(Some(instance), PCWSTR(std::ptr::without_provenance(1)))
                .unwrap_or_default(),
            hCursor: HCURSOR::default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        // Already registered by an earlier stream: fine.
        RegisterClassExW(&wc);

        let (monitor, work) = monitor_rects(None);
        let (style, rect) = if fullscreen {
            (FULLSCREEN, monitor)
        } else {
            let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem().max(96);
            (
                WINDOWED,
                windowed_rect(work, stream.0 as i32, stream.1 as i32, dpi),
            )
        };
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let hwnd = match CreateWindowExW(
            WS_EX_APPWINDOW,
            class,
            PCWSTR(title.as_ptr()),
            style,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("no stream window: {e}")));
                return;
            }
        };
        let state = State {
            hwnd,
            handler: RefCell::new(handler),
            fullscreen: Cell::new(fullscreen),
        };
        STATE.with(|s| s.set(&state));
        register_raw_mouse(Some(hwnd));
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(instance), 0);
        if let Err(e) = &hook {
            tracing::warn!(error = %e, "no keyboard hook: system shortcuts stay Windows'");
        }
        // Someone playing with a controller touches neither keyboard nor
        // mouse: keep the display on while streaming.
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        publish_layout(&state);
        let _ = ready.send(Ok(hwnd.0 as isize));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }

        let _ = ClipCursor(None);
        if let Ok(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        register_raw_mouse(None);
        SetThreadExecutionState(ES_CONTINUOUS);
        STATE.with(|s| s.set(std::ptr::null()));
        drop(state);
    }
}
