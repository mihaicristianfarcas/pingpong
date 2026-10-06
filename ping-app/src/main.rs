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
mod xbox;

use gpui::{actions, App, AppContext, KeyBinding, Menu, MenuItem};
use pingpong_ui::menus;

actions!(
    ping,
    [
        OpenSettings,
        ShowHosts,
        ShowAgents,
        ShowGeneral,
        ShowVideo,
        ShowAudio,
        ShowInput,
        ShowAgentSetup,
        Refresh,
        AddHost,
        Dismiss,
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
    if let Some(ping_core::linux::STREAM_FLAG | ping_core::linux::XBOX_STREAM_FLAG) =
        std::env::args().nth(1).as_deref()
    {
        ping_core::linux::child_main();
    }
    // Started by an update: the old copy quits first.
    pingpong_update::install::after_update();
    // One copy of the app: a second start shows the first one's window. A
    // check (PING_UI_DEMO) is not the app started again, and runs beside it.
    // Before the log is opened: opening it moves the last runs' logs aside,
    // the running copy's among them.
    let (show, mut shows) = futures::channel::mpsc::unbounded::<()>();
    let _instance = if std::env::var_os("PING_UI_DEMO").is_some() {
        None
    } else {
        match pingpong_ui::instance::claim(&ping_core::store::data_dir(), "ping", move || {
            let _ = show.unbounded_send(());
        }) {
            Some(instance) => Some(instance),
            None => {
                eprintln!("Ping is running already: it was asked to show itself");
                return;
            }
        }
    };
    ping_core::logging::init();
    let build = pingpong_update::Build::this();
    tracing::info!(
        version = build.version,
        commit = build.short_commit(),
        "Ping app"
    );
    // Join the DHT now, and keep hosts' internet addresses fresh and a path
    // warm to each while the app is open: connecting from outside the host's
    // network then takes a click, not seconds.
    ping_core::wan::warm_up();
    ping_core::wan::presence(ping_core::store::data_dir());

    let application = gpui_platform::application().with_assets(pingpong_ui::Assets);
    // The Dock icon brings the window back (it hides while a stream has
    // the screen).
    application.on_reopen(show_windows);
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
        // Started again while it runs: the window comes forward.
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while shows.next().await.is_some() {
                cx.update(show_windows);
            }
        })
        .detach();
        let _ = window.update(cx, |_, window, cx| {
            window.activate_window();
            cx.activate(true);
        });
    });
}

/// Bring the app's window back and forward.
fn show_windows(cx: &mut App) {
    for w in cx.windows() {
        let _ = w.update(cx, |_, window, _| {
            pingpong_ui::set_window_visible(window, true)
        });
    }
    cx.activate(true);
}

fn bind_keys(cx: &mut App) {
    let cmd = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let build = pingpong_update::Build::this();
    menus::init(
        menus::AppMenus {
            name: "Ping",
            version: build.version.to_string(),
            build: if build.release {
                String::new()
            } else {
                build.short_commit().to_string()
            },
            help: "docs/usage.md",
            log: ping_core::logging::logs_dir().join("ping.log"),
        },
        cx,
    );
    cx.bind_keys([
        KeyBinding::new(&format!("{cmd}-,"), OpenSettings, None),
        KeyBinding::new(&format!("{cmd}-r"), Refresh, None),
        KeyBinding::new(&format!("{cmd}-n"), AddHost, None),
        KeyBinding::new(&format!("{cmd}-1"), ShowHosts, None),
        KeyBinding::new(&format!("{cmd}-2"), ShowAgents, None),
        KeyBinding::new("escape", Dismiss, Some("PingApp")),
    ]);
    // The menu bar, as a Mac app has it: the app's own menu, then File,
    // Edit, View, Window and Help.
    cx.set_menus([
        menus::app_menu("Ping", Some(MenuItem::action("Settings…", OpenSettings))),
        Menu::new("File").items([
            MenuItem::action("Add Host…", AddHost),
            MenuItem::action("Look for Hosts Again", Refresh),
            MenuItem::separator(),
            MenuItem::action("Close Window", menus::CloseWindow),
        ]),
        menus::edit_menu(),
        Menu::new("View").items([
            MenuItem::action("Hosts", ShowHosts),
            MenuItem::action("Agents", ShowAgents),
            MenuItem::separator(),
            MenuItem::action("General", ShowGeneral),
            MenuItem::action("Video", ShowVideo),
            MenuItem::action("Audio", ShowAudio),
            MenuItem::action("Input", ShowInput),
            MenuItem::action("Agent Setup", ShowAgentSetup),
        ]),
        menus::window_menu(),
        menus::help_menu("Ping"),
    ]);
}
