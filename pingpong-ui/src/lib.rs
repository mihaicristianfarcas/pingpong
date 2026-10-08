//! The look Ping and Pong share, in GPUI: quiet neutral surfaces, hairlines,
//! a sidebar with glass-pill selection, settings as cards of labelled rows,
//! colour kept for status, and Ping's orange as the one accent. See
//! docs/ui.md.

pub mod controls;
pub mod desktop;
pub mod icon;
pub mod instance;
pub mod login;
pub mod markdown;
pub mod menus;
pub mod text_field;
pub mod theme;
pub mod tray;
pub mod updates;

use std::sync::OnceLock;

use gpui::{
    point, px, size, App, Bounds, SharedString, TitlebarOptions, Window, WindowBounds,
    WindowOptions,
};

pub use controls::*;
pub use icon::{icon, Assets, Icon, IconName, IconSize};
pub use markdown::markdown;
pub use text_field::{field, Field, FieldEvent, TextField};
pub use theme::{rgba, Ink, Layer, Metrics, Radius, Theme, Type};

static UI_FONT: OnceLock<SharedString> = OnceLock::new();
static MONO_FONT: OnceLock<SharedString> = OnceLock::new();

/// Once, when the app starts: fonts and the text field's keys.
pub fn init(cx: &mut App) {
    let names: std::collections::HashSet<String> =
        cx.text_system().all_font_names().into_iter().collect();
    let pick = |candidates: &[&'static str], fallback: &'static str| -> SharedString {
        candidates
            .iter()
            .find(|c| names.contains(**c))
            .copied()
            .unwrap_or(fallback)
            .into()
    };
    let ui = if cfg!(target_os = "macos") {
        ".SystemUIFont".into()
    } else if cfg!(windows) {
        pick(&["Segoe UI Variable Text", "Segoe UI"], "Segoe UI")
    } else {
        pick(
            &[
                "Inter",
                "Noto Sans",
                "Cantarell",
                "Ubuntu",
                "DejaVu Sans",
                "Liberation Sans",
            ],
            "sans-serif",
        )
    };
    let mono = if cfg!(target_os = "macos") {
        pick(&["SF Mono"], "Menlo")
    } else if cfg!(windows) {
        pick(&["Cascadia Mono", "Consolas"], "Consolas")
    } else {
        pick(
            &[
                "JetBrains Mono",
                "Noto Sans Mono",
                "DejaVu Sans Mono",
                "Liberation Mono",
            ],
            "monospace",
        )
    };
    let _ = UI_FONT.set(ui);
    let _ = MONO_FONT.set(mono);
    text_field::bind_keys(cx);
}

pub fn ui_font() -> SharedString {
    UI_FONT.get().cloned().unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            ".SystemUIFont".into()
        } else {
            "sans-serif".into()
        }
    })
}

pub fn mono_font() -> SharedString {
    MONO_FONT
        .get()
        .cloned()
        .unwrap_or_else(|| "monospace".into())
}

/// A main window: on a Mac the toolbar shares the title bar (traffic lights
/// over the sidebar); elsewhere the system's own title bar.
pub fn window_options(
    title: &str,
    app_id: &str,
    width: f32,
    height: f32,
    min: (f32, f32),
    cx: &App,
) -> WindowOptions {
    // On a small screen (1280 × 800 at 125 %, an agent's display) the window
    // fits in what the taskbar or Dock leaves, a little inset.
    let wanted = size(px(width), px(height));
    let bounds = match cx.primary_display().map(|d| d.visible_bounds()) {
        Some(area) => {
            let fit = size(
                wanted.width.min(area.size.width - px(24.0)).max(px(min.0)),
                wanted
                    .height
                    .min(area.size.height - px(48.0))
                    .max(px(min.1)),
            );
            Bounds::centered_at(area.center(), fit)
        }
        None => Bounds::centered(None, wanted, cx),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_string().into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: cfg!(target_os = "macos").then_some(point(px(16.0), px(16.0))),
        }),
        window_min_size: Some(size(px(min.0), px(min.1))),
        app_id: Some(app_id.to_string()),
        ..Default::default()
    }
}

/// Save the window as a PNG (automated checks). On a Mac, through
/// `screencapture` of this window alone; elsewhere unsupported.
pub fn snapshot(window: &Window, path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).map_err(|e| e.to_string())?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return Err("not an AppKit window".into());
        };
        let number: isize = unsafe {
            let view = h.ns_view.as_ptr() as *mut objc2::runtime::AnyObject;
            let win: *mut objc2::runtime::AnyObject = objc2::msg_send![view, window];
            if win.is_null() {
                return Err("the view has no window".into());
            }
            objc2::msg_send![win, windowNumber]
        };
        let out = std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-l", &number.to_string()])
            .arg(path)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() && path.exists() {
            Ok(())
        } else {
            Err(format!(
                "screencapture failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, path);
        Err("snapshots are taken on a Mac only".into())
    }
}

/// "⌘," on a Mac, "Ctrl+," elsewhere.
pub fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

/// Show or hide one window (the app keeps running): Ping's goes away while a
/// stream has the screen, as Moonlight's does.
pub fn set_window_visible(window: &Window, visible: bool) {
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(unused_imports))]
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    match handle.as_raw() {
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(h) => unsafe {
            let view = h.ns_view.as_ptr() as *mut objc2::runtime::AnyObject;
            let win: *mut objc2::runtime::AnyObject = objc2::msg_send![view, window];
            if win.is_null() {
                return;
            }
            // Queued on the main run loop, not done now: AppKit calls back
            // into GPUI as the window comes and goes, and GPUI is busy
            // drawing whoever asked.
            let nil: *mut objc2::runtime::AnyObject = std::ptr::null_mut();
            let sel = if visible {
                objc2::sel!(makeKeyAndOrderFront:)
            } else {
                objc2::sel!(orderOut:)
            };
            let _: () = objc2::msg_send![win, performSelectorOnMainThread: sel, withObject: nil, waitUntilDone: false];
        },
        #[cfg(windows)]
        RawWindowHandle::Win32(h) => unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOW,
            };
            let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
            let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
            if visible {
                let _ = SetForegroundWindow(hwnd);
            }
        },
        _ => {
            if visible {
                window.activate_window();
            } else {
                window.minimize_window();
            }
        }
    }
}
