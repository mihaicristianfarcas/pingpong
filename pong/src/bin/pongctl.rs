//! `pongctl`: the host's command line, for the person at it. `pong` is the
//! host itself (what the service, the login item and the systemd unit
//! run); this reads and changes what it keeps, as `pingctl` does for Ping.
//!
//!   pongctl identity                    this host's public keys
//!   pongctl clients                     paired clients and what each may do
//!   pongctl add-client NAME X25519 MLKEM [--agent] [--permissions LIST]
//!                                       pair a client by hand (headless setups)
//!   pongctl remove-client X25519        unpair a client
//!   pongctl permissions X25519 [LIST]   what a paired client may do: show, or set
//!
//! Changes apply to a running host when it restarts. On Windows the
//! installed host's data folder is SYSTEM's and Administrators', so these
//! need an administrator's terminal there.

use std::path::Path;
use std::process::ExitCode;

use pingpong_proto::permission::Permissions;
use pingpong_transport::PublicIdentity;
use pong_data::{clients, config, private};

fn usage() -> ExitCode {
    eprintln!(
        "usage: pongctl identity | clients | remove-client X25519\n\
         \x20      pongctl add-client NAME X25519 MLKEM [--agent] [--permissions LIST]\n\
         \x20      pongctl permissions X25519 [LIST]"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = config::data_dir();
    // The host's data folder is the host's alone on Windows; anything that
    // writes to it keeps it so (`private`).
    private::secure_data_dir(&dir);
    match args.first().map(String::as_str) {
        Some("identity") => print_identity(&dir),
        Some("clients") => list_clients(&dir),
        Some("add-client") => add_client(&dir, &args),
        Some("remove-client") => remove_client(&dir, &args),
        Some("permissions") => permissions(&dir, &args),
        _ => usage(),
    }
}

/// Print `e` and fail.
fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("{e}");
    ExitCode::FAILURE
}

fn print_identity(dir: &Path) -> ExitCode {
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

fn list_clients(dir: &Path) -> ExitCode {
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
fn save_permissions(dir: &Path, x: &str, p: Permissions) -> ExitCode {
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

fn permissions(dir: &Path, args: &[String]) -> ExitCode {
    const USAGE: &str = "usage: pongctl permissions X25519 [all|none|see-only|NAME,NAME…]\n\
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

fn add_client(dir: &Path, args: &[String]) -> ExitCode {
    let (Some(name), Some(x), Some(m)) = (args.get(1), args.get(2), args.get(3)) else {
        eprintln!("usage: pongctl add-client NAME X25519 MLKEM [--agent] [--permissions LIST]");
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

fn remove_client(dir: &Path, args: &[String]) -> ExitCode {
    let Some(x) = args.get(1) else {
        eprintln!("usage: pongctl remove-client X25519");
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
