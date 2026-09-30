//! Ping: stream a PC or Mac running Pong -- the host list, pairing, settings
//! and AI agents, one app for macOS, Windows and Linux, drawn with GPUI in
//! the look Pong shares (pingpong-ui). Streams open in their own window,
//! from ping-core.

// No console window behind the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod agents;
mod app;
mod chat;
mod hosts;
mod model;
mod notify;
mod platform;
mod prefs;
mod settings;

use gpui::{actions, App, AppContext, KeyBinding, Menu, MenuItem};

actions!(
    ping,
    [
        Quit,
        OpenSettings,
        ShowHosts,
        ShowAgents,
        Refresh,
        AddHost,
        Dismiss,
        HideApp,
        HideOthers,
        ShowAll
    ]
);

fn main() {
    // `Ping mcp`: this device's agent as an MCP server (stdin/stdout), for
    // Claude Code, Codex, Claude Desktop and the rest.
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        let code = ping_agent::mcp::main(&args);
        std::process::exit(if code == std::process::ExitCode::SUCCESS {
            0
        } else {
            1
        });
    }
    // Linux: the stream's own process (see ping_core::linux).
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some("--ping-stream") {
        ping_core::linux::child_main();
    }
    ping_core::logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Ping app");
    // Join the DHT now, and keep hosts' internet addresses fresh and a path
    // warm to each while the app is open: connecting from outside the host's
    // network then takes a click, not seconds.
    ping_core::wan::warm_up();
    ping_core::wan::presence(ping_core::store::data_dir());

    let application = gpui_platform::application().with_assets(pingpong_ui::Assets);
    // The Dock icon brings the window back (it hides while a stream has
    // the screen).
    application.on_reopen(|cx| {
        for w in cx.windows() {
            let _ = w.update(cx, |_, window, _| {
                pingpong_ui::set_window_visible(window, true)
            });
        }
        cx.activate(true);
    });
    application.run(|cx: &mut App| {
        pingpong_ui::init(cx);
        bind_keys(cx);
        let options = pingpong_ui::window_options(
            "Ping",
            "dev.pingpong.Ping",
            940.0,
            620.0,
            (680.0, 440.0),
            cx,
        );
        let window = cx
            .open_window(options, |window, cx| {
                cx.new(|cx| app::PingApp::new(window, cx))
            })
            .expect("opening Ping's window");
        // Closing the window quits, as it did.
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

fn bind_keys(cx: &mut App) {
    let cmd = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([
        KeyBinding::new(&format!("{cmd}-q"), Quit, None),
        KeyBinding::new(&format!("{cmd}-,"), OpenSettings, None),
        KeyBinding::new(&format!("{cmd}-r"), Refresh, None),
        KeyBinding::new(&format!("{cmd}-n"), AddHost, None),
        KeyBinding::new(&format!("{cmd}-1"), ShowHosts, None),
        KeyBinding::new(&format!("{cmd}-2"), ShowAgents, None),
        KeyBinding::new("escape", Dismiss, Some("PingApp")),
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
    cx.set_menus([
        Menu::new("Ping").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Hide Ping", HideApp),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Ping", Quit),
        ]),
        Menu::new("View").items([
            MenuItem::action("Hosts", ShowHosts),
            MenuItem::action("Agents", ShowAgents),
            MenuItem::separator(),
            MenuItem::action("Look for Hosts Again", Refresh),
            MenuItem::action("Add Host…", AddHost),
        ]),
    ]);
}
