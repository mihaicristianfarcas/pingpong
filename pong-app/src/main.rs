//! Pong's window: the host on this computer at a glance -- its session, the
//! devices that may connect (and the ones asking to), its settings and log.
//! The host itself runs on without it (a service on Windows, Pong.app or a
//! user unit elsewhere); this talks to it over its web API on localhost.

// No console window behind the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod app;
mod background;
mod host;
mod link;
mod prefs;
mod worker;

use std::rc::Rc;

use gpui::{actions, App, KeyBinding, Menu, MenuItem};
use pingpong_ui::menus;
use pingpong_update::{Build, Channel, Checker};

actions!(
    pong,
    [Dismiss, GoOverview, GoDevices, GoGeneral, GoVideo, GoNetwork, GoAgents, GoLogs,]
);

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

/// The keys and the menu bar. The menus show while the app is a regular
/// one (a system without a tray); as a menu bar item on a Mac it has no
/// menu bar of its own, and the keys are what is left of them.
fn bind_keys_and_menus(cx: &mut App) {
    let cmd = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    menus::init(
        menus::AppMenus {
            name: "Pong",
            version: Build::this().version.to_string(),
            build: if Build::this().release {
                String::new()
            } else {
                Build::this().short_commit().to_string()
            },
            help: "docs/usage.md",
            log: logs_dir().join("pong-app.log"),
        },
        cx,
    );
    cx.bind_keys([
        KeyBinding::new("escape", Dismiss, Some("PongApp")),
        KeyBinding::new(&format!("{cmd}-1"), GoOverview, None),
        KeyBinding::new(&format!("{cmd}-2"), GoDevices, None),
        KeyBinding::new(&format!("{cmd}-,"), GoGeneral, None),
    ]);
    cx.on_action(|_: &menus::CheckForUpdates, cx| background::open(background::Show::Updates, cx));
    cx.set_menus([
        menus::app_menu("Pong", Some(MenuItem::action("Settings…", GoGeneral))),
        Menu::new("File").items([MenuItem::action("Close Window", menus::CloseWindow)]),
        menus::edit_menu(),
        Menu::new("View").items([
            MenuItem::action("Overview", GoOverview),
            MenuItem::action("Devices", GoDevices),
            MenuItem::separator(),
            MenuItem::action("General", GoGeneral),
            MenuItem::action("Video", GoVideo),
            MenuItem::action("Network", GoNetwork),
            MenuItem::action("AI Agents", GoAgents),
            MenuItem::separator(),
            MenuItem::action("Logs", GoLogs),
        ]),
        menus::window_menu(),
        menus::help_menu("Pong"),
    ]);
}

fn main() {
    // One copy of the app: a second start shows the first one's window. A
    // check (PONG_UI_DEMO) is not the app started again, and runs beside it.
    // Before the log is opened: opening it moves the last runs' logs aside,
    // the running copy's among them.
    let (show, shows) = futures::channel::mpsc::unbounded::<()>();
    let _instance = if std::env::var_os("PONG_UI_DEMO").is_some() {
        None
    } else {
        match pingpong_ui::instance::claim(&api::app_dir(), "pong-window", move || {
            let _ = show.unbounded_send(());
        }) {
            Some(instance) => Some(instance),
            None => {
                eprintln!("Pong's window is running already: it was asked to show itself");
                return;
            }
        }
    };
    init_logging();
    // `--background`: started at login, to sit in the tray without a window.
    let background = std::env::args().skip(1).any(|a| a == "--background");
    let build = Build::this();
    tracing::info!(version = build.version, commit = build.short_commit(), os = std::env::consts::OS, data = %api::data_dir().display(), background, "Pong app");
    let application = gpui_platform::application().with_assets(pingpong_ui::Assets);
    // Opened again from the Finder or the launcher while it runs.
    application.on_reopen(|cx| background::open(background::Show::Window, cx));
    application.run(move |cx: &mut App| {
        pingpong_ui::init(cx);
        // Notifications are from "Pong" (Windows names their sender so).
        cx.set_app_identity(background::APP_ID, "Pong");
        bind_keys_and_menus(cx);
        let dir = api::app_dir();
        let prefs = prefs::Prefs::load(&dir);
        let (news, update_news) = futures::channel::mpsc::unbounded::<()>();
        // A check asks GitHub nothing on its own: what it shows is made
        // up (PONG_UI_DEMO's `update`), or asked for by hand.
        let channel = if std::env::var_os("PONG_UI_DEMO").is_some() {
            Channel::Off
        } else {
            prefs.update_channel()
        };
        let updates = Rc::new(Checker::start(build, dir, channel, move || {
            let _ = news.unbounded_send(());
        }));
        let link = link::Link::start(cx);
        background::start(link, updates, update_news, shows, background, cx);
    });
}
