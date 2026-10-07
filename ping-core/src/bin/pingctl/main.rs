//! `pingctl`: the command-line client. The same streaming code as the app.
//!
//!   pingctl identity                            this client's public keys
//!   pingctl discover                            hosts on the local network
//!   pingctl pair HOST[:PORT]                    pair (shows a PIN to type on the host)
//!   pingctl pair-agent HOST[:PORT]              pair this device's AI agent (its own key)
//!   pingctl watch NAME [stream flags]           watch the AI agent working on NAME
//!   pingctl hosts                               paired hosts
//!   pingctl add-host NAME ADDR X25519 MLKEM     pair by hand (see `pong identity`)
//!   pingctl remove-host NAME
//!   pingctl wake NAME                           Wake-on-LAN
//!   pingctl stream NAME [--size WxH] [--fps N] [--mbps N] [--codec h264|hevc|av1]
//!                       [--windowed] [--no-vsync] [--frame-pacing] [--stats] [--cmd-is-win]
//!                       [--mute-in-background] [--audio-channels 2|6|8] [--steam]
//!                       [--no-audio] [--host-audio] [--wan-only] [--keep-host-displays]
//!                       [--via ADDR] [--no-clipboard] [--hdr] [--yuv444]
//!   pingctl xbox …                              an Xbox console or cloud game (`xbox`)
//!
//! `PING_TEST_INPUT` scripts input into a stream (see `script`).

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
mod script;
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
mod stream;
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
mod xbox;

use std::path::Path;
use std::process::ExitCode;

pub(crate) fn usage() -> ExitCode {
    eprintln!(
        "usage: pingctl identity | discover | pair HOST[:PORT] | pair-agent HOST[:PORT] | hosts | remove-host NAME | wake NAME\n\
            \x20      pingctl watch NAME [stream flags]\n\
            \x20      pingctl add-host NAME ADDR X25519 MLKEM\n\
            \x20      pingctl stream NAME [--size WxH] [--fps N] [--mbps N] [--codec h264|hevc|av1]\n\
            \x20                          [--windowed] [--no-vsync] [--frame-pacing] [--stats] [--cmd-is-win]\n\
            \x20                          [--mute-in-background] [--audio-channels 2|6|8] [--steam]\n\
            \x20                          [--no-audio] [--host-audio] [--wan-only] [--keep-host-displays] [--via ADDR]\n\
            \x20                          [--no-clipboard] [--hdr] [--yuv444]\n\
            \x20      pingctl xbox sign-in | sign-out | consoles | wake NAME | off NAME | games | friends\n\
            \x20      pingctl xbox stream CONSOLE [--keyboard] [stream flags]\n\
            \x20      pingctl xbox play GAME [--keyboard] [--region NAME] [stream flags]"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,mainline=error,ping_core=info,pingpong_transport=info,pingpong_decode=info"
                    .into()
            }),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = ping_core::store::data_dir();
    match args.first().map(String::as_str) {
        Some("identity") => identity(&dir),
        Some("discover") => discover(&dir),
        Some("pair") => match args.get(1) {
            Some(spec) => pair(&dir, spec),
            None => usage(),
        },
        Some("pair-agent") => match args.get(1) {
            Some(spec) => pair_agent(&dir, spec),
            None => usage(),
        },
        Some("hosts") => {
            for h in ping_core::store::Hosts::load(&dir).list() {
                println!("{}\t{}", h.name, h.address);
            }
            ExitCode::SUCCESS
        }
        Some("add-host") => add_host(&dir, &args),
        Some("wake") => match args.get(1) {
            Some(name) => wake(&dir, name),
            None => usage(),
        },
        Some("remove-host") => match args
            .get(1)
            .map(|n| ping_core::store::Hosts::load(&dir).remove(n))
        {
            Some(Ok(true)) => ExitCode::SUCCESS,
            Some(Ok(false)) => fail("no such host"),
            Some(Err(e)) => fail(e),
            None => usage(),
        },
        #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
        Some("stream") => match args.get(1) {
            Some(name) => stream::run(
                stream::What::Host(name.clone()),
                stream::request(&args[2..]),
            ),
            None => usage(),
        },
        #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
        Some("xbox") => xbox::run(&args[1..]),
        #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
        Some("watch") => match args.get(1) {
            Some(name) => {
                let mut flags = args[2..].to_vec();
                flags.push("--watch".into());
                stream::run(stream::What::Host(name.clone()), stream::request(&flags))
            }
            None => usage(),
        },
        _ => usage(),
    }
}

/// Print `e` and fail.
pub(crate) fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("{e}");
    ExitCode::FAILURE
}

fn identity(dir: &Path) -> ExitCode {
    match ping_core::store::identity(dir) {
        Ok(id) => {
            let (x, m) = id.public().to_b64();
            println!("{x} {m}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn discover(dir: &Path) -> ExitCode {
    match ping_core::pair::discover(dir, std::time::Duration::from_secs(3)) {
        Ok(found) if found.is_empty() => {
            eprintln!(
                "no hosts found on the local network (add one by address with `pingctl pair \
                    HOST`)"
            );
            ExitCode::SUCCESS
        }
        Ok(found) => {
            for f in found {
                let state = if f.paired { "paired" } else { "not paired" };
                println!(
                    "{}\t{}\t{}\t{}\thttps://{}:{}",
                    f.name,
                    f.os,
                    f.pairing_addr(),
                    state,
                    f.address,
                    f.web_port
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// Where to type the PIN, while pairing waits for it.
fn show_pin(pin: &str) {
    eprintln!("\nOn the host, open its web UI (Pair a device) and enter this PIN:\n\n    {pin}\n");
}

fn pair(dir: &Path, spec: &str) -> ExitCode {
    let addr = match ping_core::pair::resolve(spec) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    let pin = pingpong_pairing::pair::new_pin();
    let name = ping_core::pair::client_name();
    eprintln!("Pairing with {addr} as \"{name}\"...");
    match ping_core::pair::pair(dir, addr, &name, &pin, || show_pin(&pin)) {
        Ok(host) => {
            println!(
                "Paired with {} ({}). Stream with: pingctl stream {}",
                host.name, host.address, host.name
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("Pairing failed: {e}")),
    }
}

fn pair_agent(dir: &Path, spec: &str) -> ExitCode {
    let addr = match ping_core::pair::resolve(spec) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    let pin = pingpong_pairing::pair::new_pin();
    eprintln!("Pairing this device's AI agent with {addr}...");
    match ping_core::pair::pair_agent(dir, addr, &pin, &Default::default(), || show_pin(&pin)) {
        Ok(host) => {
            println!(
                "The agent is paired with {}. Agents reach it through `Ping mcp` or \
                    `ping-agent`.",
                host.name
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("Pairing failed: {e}")),
    }
}

fn add_host(dir: &Path, args: &[String]) -> ExitCode {
    let (Some(name), Some(addr), Some(x), Some(m)) =
        (args.get(1), args.get(2), args.get(3), args.get(4))
    else {
        return usage();
    };
    if pingpong_transport::PublicIdentity::from_b64(x, m).is_err() {
        return fail("malformed host keys");
    }
    let host = ping_core::store::KnownHost::by_hand(name, addr, x, m);
    match ping_core::store::Hosts::load(dir).upsert(host) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

fn wake(dir: &Path, name: &str) -> ExitCode {
    let hosts = ping_core::store::Hosts::load(dir);
    let Some(host) = hosts.find(name) else {
        return fail(format!("no paired host named {name}; see `pingctl hosts`"));
    };
    match ping_core::wake::wake(host) {
        Ok(n) => {
            println!("sent {n} wake packets to {}", host.name);
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}
