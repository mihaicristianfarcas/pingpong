//! Windows ends the host process when it shuts down, restarts or signs the
//! console out. Before it does, end the stream properly: tell the client (so
//! Ping says the host is shutting down instead of waiting for it) and give
//! the host its monitors back.
//!
//! The host loads user32, so Windows treats it as a GUI program and never
//! sends it the console's shutdown events (see `SetConsoleCtrlHandler`); a
//! program without a window is simply terminated. A hidden top-level window
//! gets `WM_QUERYENDSESSION` / `WM_ENDSESSION` instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, MSG,
    WINDOW_EX_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};

use crate::host::Host;

static HOST: OnceLock<Arc<Host>> = OnceLock::new();
static STOPPED: AtomicBool = AtomicBool::new(false);

/// Windows allows about 5 s for `WM_ENDSESSION` before it ends the process.
const ALLOWED: Duration = Duration::from_secs(4);

/// Watch for the end of the Windows session on a thread of its own.
pub fn watch(host: Arc<Host>) {
    let _ = HOST.set(host);
    let spawned = std::thread::Builder::new()
        .name("end-session".into())
        .spawn(|| unsafe { run() });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch for Windows shutting down");
    }
}

/// The host has wound down: a pending `WM_ENDSESSION` may return.
pub fn stopped() {
    STOPPED.store(true, Ordering::Release);
}

unsafe fn run() {
    let instance = match GetModuleHandleW(None) {
        Ok(m) => m.into(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot watch for Windows shutting down");
            return;
        }
    };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: w!("PongEndSession"),
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "cannot watch for Windows \
            shutting down");
        return;
    }
    // Top-level (a message-only window gets no session messages), never shown.
    let created = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("PongEndSession"),
        w!("Pong"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance),
        None,
    );
    if let Err(e) = created {
        tracing::warn!(error = %e, "cannot watch for Windows shutting down");
        return;
    }
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                end_stream();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn end_stream() {
    let Some(host) = HOST.get() else { return };
    tracing::info!("Windows is shutting down or signing out; ending the stream first");
    host.shutdown();
    let started = Instant::now();
    while !STOPPED.load(Ordering::Acquire) && started.elapsed() < ALLOWED {
        std::thread::sleep(Duration::from_millis(20));
    }
}
