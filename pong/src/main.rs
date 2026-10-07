//! Pong: the pingpong streaming host.
//!
//!   pong [host]                      run the host in this session
//!   pong identity                    print this host's public keys
//!   pong clients                     list paired clients and what each may do
//!   pong add-client NAME X25519 MLKEM [--agent] [--permissions LIST]
//!                                    pair a client by hand (headless setups)
//!   pong remove-client X25519        unpair a client
//!   pong permissions X25519 [LIST]   what a paired client may do: show, or set
//!
//! On Windows, also:
//!
//!   pong install | uninstall         the PongService Windows service
//!   pong service                     the service itself (started by Windows)
//!   pong clipboard-agent             clipboard sharing as the signed-in user
//!                                    (started by the host for a session)

#[cfg(windows)]
mod apps;
#[cfg(windows)]
mod audio;
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "unix/audio.rs"]
mod audio;
mod bitrate;
#[cfg(windows)]
mod clipagent;
#[cfg(windows)]
mod endsession;
#[cfg(windows)]
mod gamepad;
mod host;
mod negotiate;
mod netif;
#[cfg(windows)]
mod nvprefs;
mod pairing;
#[cfg(target_os = "macos")]
#[path = "mac/permissions.rs"]
mod permissions;
mod pipeline;
#[cfg(windows)]
mod platform;
#[cfg(target_os = "macos")]
#[path = "mac/platform.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "linux/platform.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "linux/portal.rs"]
mod portal;
#[cfg(target_os = "macos")]
#[path = "mac/power.rs"]
mod power;
#[cfg(target_os = "linux")]
#[path = "linux/power.rs"]
mod power;
mod presence;
mod priority;
mod sender;
#[cfg(windows)]
mod service;
mod session;
#[cfg(target_os = "macos")]
#[path = "mac/sound.rs"]
mod sound;
#[cfg(windows)]
mod tuning;
#[cfg(windows)]
mod video;
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "unix/video.rs"]
mod video;
mod web;

use std::process::ExitCode;

use pingpong_proto::permission::Permissions;
use pingpong_transport::PublicIdentity;
// The data folder's modules live in the library, for `pongctl` too; here
// they keep their `crate::` paths.
use pong_data::{clients, config, private};

fn init_logging(dir: &std::path::Path) -> tracing_appender::non_blocking::WorkerGuard {
    init_logging_named(dir, "pong.log")
}

fn init_logging_named(
    dir: &std::path::Path,
    name: &str,
) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::prelude::*;
    let logs = dir.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let file = tracing_appender::rolling::daily(&logs, name);
    let (file, guard) = tracing_appender::non_blocking(file);
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "info,mainline=error,zbus=error,pong=info,pingpong_transport=info,pingpong_display=info,pingpong_input=info,\
                pingpong_capture=info,pingpong_encode=info"
                .into()
        })
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file)
                .with_filter(filter()),
        )
        .init();
    guard
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = config::data_dir();
    let command = args.first().map(String::as_str).unwrap_or("host");
    // On Windows the data folder is the host's alone (see `private`). Not
    // from the clipboard helper, which runs as the signed-in user.
    if command != "clipboard-agent" {
        private::secure_data_dir(&dir);
    }
    match command {
        // Clipboard sharing as the signed-in user, for the host (Windows).
        #[cfg(windows)]
        "clipboard-agent" => clipagent::run(&args),
        "host" => run_host(dir),
        #[cfg(windows)]
        "service" => run_service(&dir),
        #[cfg(windows)]
        "install" => match service::install() {
            Ok(()) => {
                println!(
                    "PongService installed and started. Pong now runs at boot, before sign-in."
                );
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        #[cfg(windows)]
        "uninstall" => match service::uninstall() {
            Ok(()) => {
                println!("PongService removed.");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        "identity" => print_identity(&dir),
        "clients" => list_clients(&dir),
        "permissions" => permissions(&dir, &args),
        "add-client" => add_client(&dir, &args),
        "remove-client" => remove_client(&dir, &args),
        other => {
            eprintln!(
                "unknown command {other}\n\nusage: pong \
                    [host|identity|clients|add-client|remove-client|permissions{}]",
                if cfg!(windows) {
                    "|install|uninstall"
                } else {
                    ""
                }
            );
            ExitCode::FAILURE
        }
    }
}

/// Print `e` and fail.
fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("{e}");
    ExitCode::FAILURE
}

/// `pong host`: run the host in this session until it is asked to stop.
fn run_host(dir: std::path::PathBuf) -> ExitCode {
    let _guard = init_logging(&dir);
    #[cfg(windows)]
    pingpong_capture::gpu::declare_dpi_aware();
    let host = match host::Host::open(dir) {
        Ok(host) => host,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(windows)]
    {
        let h = host.clone();
        std::thread::spawn(move || {
            service::wait_for_stop_request();
            tracing::info!("the service asked the host to stop");
            h.shutdown();
        });
        endsession::watch(host.clone());
    }
    // Ctrl-C or a kill: end the session properly, so the client hears the
    // host is going.
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        permissions::check();
        let h = host.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            tracing::info!("asked to stop");
            h.shutdown();
        }) {
            tracing::warn!(
                error = %e,
                "cannot catch Ctrl-C; a stop will not end the session cleanly"
            );
        }
    }
    host.start_services();
    host.serve();
    #[cfg(windows)]
    endsession::stopped();
    ExitCode::SUCCESS
}

/// `pong service`: the Windows service, which keeps `pong host` running.
#[cfg(windows)]
fn run_service(dir: &std::path::Path) -> ExitCode {
    let _guard = init_logging_named(dir, "pong-service.log");
    match service::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}\n(`pong service` is started by Windows; use `pong install`)");
            ExitCode::FAILURE
        }
    }
}

fn print_identity(dir: &std::path::Path) -> ExitCode {
    match pingpong_transport::Identity::load_or_create(&config::identity_path(dir)) {
        Ok(id) => {
            let (x, m) = id.public().to_b64();
            println!("x25519 = \"{x}\"\nmlkem = \"{m}\"");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// A set of permissions as the CLI prints it.
fn spoken(p: Permissions) -> String {
    if p == Permissions::NONE {
        "none".into()
    } else {
        p.names().join(",")
    }
}

fn list_clients(dir: &std::path::Path) -> ExitCode {
    for c in clients::Clients::load(dir).list() {
        let role = if c.agent { "agent" } else { "person" };
        println!(
            "{}\t{}\t{role}\t{}",
            c.name,
            c.x25519,
            spoken(c.permissions)
        );
    }
    ExitCode::SUCCESS
}

/// Set a client's permissions in the file; a running host reads them at
/// its next start.
fn save_permissions(dir: &std::path::Path, x: &str, p: Permissions) -> ExitCode {
    match clients::Clients::load(dir).set_permissions(x, p) {
        Ok(Some(now)) => {
            println!(
                "{}; restart the host for it to apply to a running host (Pong's window \
                    and web UI change it at once)",
                spoken(now)
            );
            ExitCode::SUCCESS
        }
        Ok(None) => fail("no such client"),
        Err(e) => fail(e),
    }
}

fn permissions(dir: &std::path::Path, args: &[String]) -> ExitCode {
    const USAGE: &str = "usage: pong permissions X25519 [all|none|see-only|NAME,NAME…]\n\
        names: view, keyboard, mouse, controller, clipboard_read, clipboard_write, launch, \
        take_over, watch (people); view, keyboard, mouse, unwatched (agents)";
    let Some(x) = args.get(1) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let Some(list) = args.get(2) else {
        return match clients::Clients::load(dir)
            .list()
            .iter()
            .find(|c| &c.x25519 == x)
        {
            Some(c) => {
                println!("{}", spoken(c.permissions));
                ExitCode::SUCCESS
            }
            None => fail("no such client"),
        };
    };
    match Permissions::parse(list) {
        Ok(p) => save_permissions(dir, x, p),
        Err(e) => fail(format!("{e}\n{USAGE}")),
    }
}

fn add_client(dir: &std::path::Path, args: &[String]) -> ExitCode {
    let (Some(name), Some(x), Some(m)) = (args.get(1), args.get(2), args.get(3)) else {
        eprintln!("usage: pong add-client NAME X25519 MLKEM [--agent] [--permissions LIST]");
        return ExitCode::FAILURE;
    };
    let agent = args.iter().any(|a| a == "--agent");
    let chosen = match args.iter().position(|a| a == "--permissions") {
        Some(i) => match args.get(i + 1).map(|l| Permissions::parse(l)) {
            Some(Ok(p)) => Some(p),
            Some(Err(e)) => return fail(e),
            None => return fail("--permissions needs a list (all, see-only, view,keyboard,…)"),
        },
        None => None,
    };
    let public = match PublicIdentity::from_b64(x, m) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    match clients::Clients::load(dir).add(name, &public, None, agent, chosen) {
        Ok(c) => {
            println!(
                "paired {name} ({}); restart the host to admit it",
                spoken(c.permissions)
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn remove_client(dir: &std::path::Path, args: &[String]) -> ExitCode {
    let Some(x) = args.get(1) else {
        eprintln!("usage: pong remove-client X25519");
        return ExitCode::FAILURE;
    };
    match clients::Clients::load(dir).remove(x) {
        Ok(Some(c)) => {
            println!("removed {}", c.name);
            ExitCode::SUCCESS
        }
        Ok(None) => fail("no such client"),
        Err(e) => fail(e),
    }
}
