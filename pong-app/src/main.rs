//! Pong's window: the host on this computer at a glance -- its session, the
//! devices that may connect (and the ones asking to), its settings and log.
//! The host itself runs on without it (a service on Windows, Pong.app or a
//! user unit elsewhere); this talks to it over its web API on localhost.

// No console window behind the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod app;
mod worker;

use gpui::{actions, App, AppContext, KeyBinding, Menu, MenuItem};

actions!(pong, [Quit, Dismiss, HideApp, HideOthers, ShowAll]);

/// Where this window logs (the host logs where it keeps its state):
///
///   macOS    ~/Library/Logs/Pong/pong-app.log
///   Windows  %LOCALAPPDATA%\Pong\Logs\pong-app.log
///   Linux    ~/.local/state/pong/pong-app.log
fn logs_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        return env("LOCALAPPDATA")
            .unwrap_or_else(std::env::temp_dir)
            .join("Pong")
            .join("Logs");
    }
    let home = env("HOME").unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/Pong")
    } else {
        env("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("pong")
    }
}

/// To stderr and a file (the previous two runs' kept beside it).
fn init_logging() {
    use tracing_subscriber::prelude::*;
    let logs = logs_dir();
    let _ = std::fs::create_dir_all(&logs);
    let _ = std::fs::rename(logs.join("pong-app.1.log"), logs.join("pong-app.2.log"));
    let _ = std::fs::rename(logs.join("pong-app.log"), logs.join("pong-app.1.log"));
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,gpui=warn,gpui_macos=error".into())
    };
    let registry = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(filter()),
    );
    match std::fs::File::create(logs.join("pong-app.log")) {
        Ok(f) => {
            let _ = registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Mutex::new(f))
                        .with_filter(filter()),
                )
                .try_init();
        }
        Err(_) => {
            let _ = registry.try_init();
        }
    }
}

fn main() {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, data = %api::data_dir().display(), "Pong app");
    gpui_platform::application()
        .with_assets(pingpong_ui::Assets)
        .run(|cx: &mut App| {
            pingpong_ui::init(cx);
            let cmd = if cfg!(target_os = "macos") {
                "cmd"
            } else {
                "ctrl"
            };
            cx.bind_keys([
                KeyBinding::new(&format!("{cmd}-q"), Quit, None),
                KeyBinding::new("escape", Dismiss, Some("PongApp")),
            ]);
            if cfg!(target_os = "macos") {
                cx.bind_keys([
                    KeyBinding::new("cmd-h", HideApp, None),
                    KeyBinding::new("alt-cmd-h", HideOthers, None),
                ]);
            }
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &HideApp, cx| cx.hide());
            cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
            cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
            cx.set_menus([Menu::new("Pong").items([
                MenuItem::action("Hide Pong", HideApp),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Pong", Quit),
            ])]);
            let options = pingpong_ui::window_options(
                "Pong",
                "dev.pingpong.PongApp",
                940.0,
                640.0,
                (700.0, 460.0),
                cx,
            );
            let window = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| app::PongApp::new(window, cx))
                })
                .expect("opening Pong's window");
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let _ = window.update(cx, |_, window, cx| {
                window.activate_window();
                cx.activate(true);
            });
        });
}
