//! `xbox-mock`: a mock Xbox on this computer, for trying Ping's Xbox
//! streaming without a console (see the crate's documentation).
//!
//!   xbox-mock [--listen ADDR:PORT] [--awake] [--cloud gamepass|free|none]
//!
//! It prints the `PING_XBOX_MOCK=…` line to give Ping; then sign in, list
//! the consoles and stream as with a real account:
//!
//!   PING_XBOX_MOCK=http://127.0.0.1:47920 ping xbox sign-in
//!   PING_XBOX_MOCK=http://127.0.0.1:47920 ping xbox stream "Mock Xbox"
//!
//! Listening on a LAN address lets a Ping on another computer use it.

use std::net::SocketAddr;

use pingpong_xbox_mock::{Cloud, Config, MockConsole};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut config = Config {
        bind: SocketAddr::from(([127, 0, 0, 1], 47920)),
        // Signing in completes at the first poll: there is nobody to type
        // the code.
        pending_polls: 0,
        ..Config::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => match args.next().and_then(|v| v.parse().ok()) {
                Some(addr) => config.bind = addr,
                None => return usage(),
            },
            "--awake" => config.console_asleep = false,
            "--cloud" => {
                config.cloud = match args.next().as_deref() {
                    Some("gamepass") => Cloud::GamePass,
                    Some("free") => Cloud::FreeToPlay,
                    Some("none") => Cloud::None,
                    _ => return usage(),
                }
            }
            _ => return usage(),
        }
    }
    let mock = match MockConsole::start(config) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot listen: {e}");
            std::process::exit(1);
        }
    };
    println!("PING_XBOX_MOCK={}", mock.url());
    eprintln!("A mock Xbox is running. Ctrl-C stops it.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let r = mock.record();
        if r.connected {
            tracing::info!(
                frames = r.frames,
                keyframes = r.keyframes,
                audio = r.audio_packets,
                reports = r.reports.len(),
                keyframe_requests = r.keyframe_requests + r.plis,
                "mock console"
            );
        }
    }
}

fn usage() {
    eprintln!("usage: xbox-mock [--listen ADDR:PORT] [--awake] [--cloud gamepass|free|none]");
    std::process::exit(2);
}
