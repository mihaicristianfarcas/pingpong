//! The taskbar's notification area (`Shell_NotifyIconW`): the app's own icon
//! from the executable's resources, owned by a hidden window on the main
//! thread that takes the icon's messages. A left click (or Enter on the
//! icon) is `Activate`; the right button, or the menu key, shows the menu.
//!
//! The window is a real top-level one, never shown, and not a message-only
//! window: only top-level windows hear Explorer say `TaskbarCreated`, which
//! is when an icon has to be put back (Explorer restarted, or had not yet
//! started when the app did at sign-in).

use std::cell::RefCell;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetSystemMetrics, GetWindowLongPtrW, LoadIconW, LoadImageW, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu, GWLP_USERDATA,
    HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, SM_CYSMICON, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU,
    WM_NULL, WNDCLASSW, WS_OVERLAPPED,
};

use super::{Events, TrayEvent, TrayItem};

/// The message the icon's events arrive in.
const ICON_MESSAGE: u32 = WM_APP + 1;
/// The icon was chosen with the keyboard (Enter or Space on it).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
/// This window has one icon.
const ICON_ID: u32 = 1;

/// What the window procedure needs: kept in the window's user data.
struct State {
    events: Events,
    items: RefCell<Vec<TrayItem>>,
    /// The icon as it was last described to the shell.
    icon: RefCell<NOTIFYICONDATAW>,
    /// The number Explorer broadcasts under when the taskbar is (re)made.
    taskbar_created: u32,
}

impl State {
    /// Put the icon up. Fails quietly when the taskbar is not there yet:
    /// Explorer says `TaskbarCreated` when it is.
    fn add(&self) {
        let mut icon = self.icon.borrow_mut();
        // SAFETY: `icon` is a fully initialized NOTIFYICONDATAW with its own
        // size in `cbSize`, alive for both calls.
        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &*icon).as_bool() {
                tracing::debug!("the notification area did not take the icon (yet)");
                return;
            }
            // Version 4: clicks arrive as NIN_SELECT with the place in
            // wparam, and the menu as WM_CONTEXTMENU.
            icon.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &*icon);
        }
    }

    fn send(&self, event: TrayEvent) {
        let _ = self.events.unbounded_send(event);
    }

    /// The menu, at the icon, until something is chosen or it is dismissed.
    fn show_menu(&self, hwnd: HWND, x: i32, y: i32) {
        // A copy: the menu's own message loop runs the app, which may set
        // another menu meanwhile.
        let items = self.items.borrow().clone();
        if items.is_empty() {
            return;
        }
        // SAFETY: the menu is created, filled, shown and destroyed here; the
        // strings outlive the calls that copy them; `hwnd` is this thread's
        // window.
        let chosen = unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            for item in &items {
                let _ = match item {
                    TrayItem::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
                    TrayItem::Label(text) => AppendMenuW(
                        menu,
                        MF_STRING | MF_GRAYED,
                        0,
                        &HSTRING::from(text.as_str()),
                    ),
                    // Command 0 is "nothing chosen": ids are offset by one.
                    TrayItem::Action { id, title } => AppendMenuW(
                        menu,
                        MF_STRING,
                        *id as usize + 1,
                        &HSTRING::from(title.as_str()),
                    ),
                };
            }
            // Looks removable, is not: a popup menu whose owner is not the
            // foreground window stays up when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                x,
                y,
                None,
                hwnd,
                None,
            );
            // The other half of the same rule (Microsoft's KB135788): a
            // message after the menu, so the next click on the icon works.
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            chosen.0
        };
        if chosen > 0 {
            self.send(TrayEvent::Item(chosen as u32 - 1));
        }
    }
}

/// The signed 16-bit halves of a message parameter holding a point.
fn point_of(packed: usize) -> (i32, i32) {
    (
        (packed & 0xffff) as u16 as i16 as i32,
        ((packed >> 16) & 0xffff) as u16 as i16 as i32,
    )
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the user data is null (before `Tray::new` sets it, and after
    // `Tray::drop` clears it) or the `State` that `Tray` owns, which is freed
    // only after the pointer is cleared; all of it on this one thread.
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() };
    if let Some(state) = state {
        if message == ICON_MESSAGE {
            // Version 4 of the icon's messages: the event in lparam's low
            // word, where on screen in wparam.
            match (lparam.0 as u32) & 0xffff {
                NIN_SELECT | NIN_KEYSELECT => state.send(TrayEvent::Activate),
                WM_CONTEXTMENU => {
                    let (x, y) = point_of(wparam.0);
                    state.show_menu(hwnd, x, y);
                }
                _ => {}
            }
            return LRESULT(0);
        }
        if message == state.taskbar_created {
            state.add();
            return LRESULT(0);
        }
    }
    // SAFETY: the parameters are the ones this procedure was called with.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// `text` as the shell's fixed, NUL-terminated field takes it.
fn tip(text: &str) -> [u16; 128] {
    let mut out = [0u16; 128];
    for (slot, unit) in out.iter_mut().take(127).zip(text.encode_utf16()) {
        *slot = unit;
    }
    out
}

/// The app's icon at the notification area's size: the executable's first
/// icon resource (the one the window and the taskbar show), or the system's
/// plain application icon if it has none.
fn app_icon(instance: HINSTANCE) -> HICON {
    // SAFETY: resource 1 is named by its ordinal, as `MAKEINTRESOURCE` does;
    // a failure is handled.
    unsafe {
        LoadImageW(
            Some(instance),
            PCWSTR(1 as _),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
        .map(|handle| HICON(handle.0))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default()
    }
}

pub(super) struct Tray {
    hwnd: HWND,
    /// Owned here; the window's user data points at it.
    state: *mut State,
}

impl Tray {
    pub(super) fn new(app_id: &str, tooltip: &str, events: Events) -> Option<Tray> {
        let class = HSTRING::from(format!("{app_id}.Tray"));
        // SAFETY: plain Win32 window creation on the calling (main) thread;
        // every pointer passed outlives its call, and the class's procedure
        // is `window_proc` above.
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            // Zero when the class is there already (a second tray): the
            // window is made from it all the same.
            let _ = RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                Default::default(),
                &class,
                &HSTRING::from(tooltip),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
            .inspect_err(|e| tracing::warn!(error = %e, "no window for the tray icon"))
            .ok()?;
            let icon = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: ICON_ID,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
                uCallbackMessage: ICON_MESSAGE,
                hIcon: app_icon(instance),
                szTip: tip(tooltip),
                ..Default::default()
            };
            let state = Box::into_raw(Box::new(State {
                events,
                items: RefCell::new(Vec::new()),
                icon: RefCell::new(icon),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            (*state).add();
            Some(Tray { hwnd, state })
        }
    }

    fn state(&self) -> &State {
        // SAFETY: `state` is the box made in `new`, freed only in `drop`.
        unsafe { &*self.state }
    }

    pub(super) fn set_menu(&self, items: &[TrayItem]) {
        *self.state().items.borrow_mut() = items.to_vec();
    }

    pub(super) fn set_tooltip(&self, tooltip: &str) {
        let mut icon = self.state().icon.borrow_mut();
        icon.szTip = tip(tooltip);
        let mut change = *icon;
        change.uFlags = NIF_TIP | NIF_SHOWTIP;
        // SAFETY: `change` is a copy of the icon's description, alive for
        // the call.
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &change) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: the icon goes, then the window's pointer to the state,
        // then the window, and only then the state itself: the window
        // procedure never sees a freed `State`.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &*self.state().icon.borrow());
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            drop(Box::from_raw(self.state));
        }
    }
}
