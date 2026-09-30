//! The menus a Mac app is expected to have, and the actions behind them, for
//! Ping and Pong alike: the app's own menu (About, Check for Updates,
//! Services, Hide, Quit), Edit (the text fields' cut, copy and paste, which
//! also makes them findable), Window and Help. Each app adds its own File
//! and View between them.
//!
//! GPUI draws a menu bar only on a Mac. On Windows and Linux the actions
//! and their keys work all the same; what the menus would offer is on the
//! window itself there.

use std::path::PathBuf;

use gpui::{actions, App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

use crate::text_field;

actions!(
    pingpong,
    [
        About,
        /// Handled by each app: they own their update check.
        CheckForUpdates,
        Quit,
        HideApp,
        HideOthers,
        ShowAll,
        CloseWindow,
        Minimize,
        Zoom,
        BringAllToFront,
        Help,
        ReportIssue,
        ShowLogs,
    ]
);

/// What the shared items say and open for one app.
#[derive(Debug, Clone)]
pub struct AppMenus {
    /// "Ping" or "Pong".
    pub name: &'static str,
    /// The version the About panel shows, and the build after it in
    /// brackets (a checkout's commit; empty for a release).
    pub version: String,
    pub build: String,
    /// The page Help opens, as a path in the repository ("docs/usage.md").
    pub help: &'static str,
    /// The app's log, for Show Logs.
    pub log: PathBuf,
}

/// The project's pages on GitHub (the workspace's `repository`).
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Once, when the app starts: the shared actions' keys and what they do.
pub fn init(app: AppMenus, cx: &mut App) {
    let cmd = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([KeyBinding::new(&format!("{cmd}-q"), Quit, None)]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-h", HideApp, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-m", Minimize, None),
        ]);
    }
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &BringAllToFront, cx| cx.activate(true));
    // The window in front, as the title bar's own buttons would.
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    cx.on_action(|_: &Minimize, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.minimize_window());
        }
    });
    cx.on_action(|_: &Zoom, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.zoom_window());
        }
    });
    let about = app.clone();
    cx.on_action(move |_: &About, _| {
        crate::desktop::show_about(about.name, &about.version, &about.build)
    });
    let help = format!("{REPOSITORY}/blob/main/{}", app.help);
    cx.on_action(move |_: &Help, cx| cx.open_url(&help));
    cx.on_action(|_: &ReportIssue, cx| cx.open_url(&format!("{REPOSITORY}/issues")));
    cx.on_action(move |_: &ShowLogs, cx| cx.reveal_path(&app.log));
}

/// The app's own menu. `settings` is the app's Settings item, if it has a
/// settings page to open.
pub fn app_menu(name: &str, settings: Option<MenuItem>) -> Menu {
    let mut items = vec![
        MenuItem::action(format!("About {name}"), About),
        MenuItem::action("Check for Updates…", CheckForUpdates),
        MenuItem::separator(),
    ];
    if let Some(settings) = settings {
        items.push(settings);
        items.push(MenuItem::separator());
    }
    items.extend([
        MenuItem::os_submenu("Services", SystemMenuType::Services),
        MenuItem::separator(),
        MenuItem::action(format!("Hide {name}"), HideApp),
        MenuItem::action("Hide Others", HideOthers),
        MenuItem::action("Show All", ShowAll),
        MenuItem::separator(),
        MenuItem::action(format!("Quit {name}"), Quit),
    ]);
    Menu::new(name.to_string()).items(items)
}

/// Edit: what the text fields do. No Undo: the fields keep no history to
/// undo into, and an item that never does anything is worse than none.
pub fn edit_menu() -> Menu {
    Menu::new("Edit").items([
        MenuItem::os_action("Cut", text_field::Cut, OsAction::Cut),
        MenuItem::os_action("Copy", text_field::Copy, OsAction::Copy),
        MenuItem::os_action("Paste", text_field::Paste, OsAction::Paste),
        MenuItem::separator(),
        MenuItem::os_action("Select All", text_field::SelectAll, OsAction::SelectAll),
    ])
}

/// Window: the system lists the app's windows under these (it does so for
/// the menu named "Window").
pub fn window_menu() -> Menu {
    Menu::new("Window").items([
        MenuItem::action("Minimize", Minimize),
        MenuItem::action("Zoom", Zoom),
        MenuItem::separator(),
        MenuItem::action("Bring All to Front", BringAllToFront),
    ])
}

/// Help: the system puts its search field in the menu named "Help".
pub fn help_menu(name: &str) -> Menu {
    Menu::new("Help").items([
        MenuItem::action(format!("{name} Help"), Help),
        MenuItem::separator(),
        MenuItem::action("Show Logs in Finder", ShowLogs),
        MenuItem::action("Report an Issue…", ReportIssue),
    ])
}
