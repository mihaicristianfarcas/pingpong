//! `xbox-mock`: a mock Xbox on this computer, for trying Ping's Xbox
//! streaming without a console (see the crate's documentation).
//!
//!   xbox-mock [--listen ADDR:PORT] [--awake] [--cloud gamepass|free|none]
//!             [--sign-in-polls N] [--delay MS] [--loss PCT] [--rate MBPS]
//!             [--queue MS] [--outage-every MS --outage MS]
//!
//! `--sign-in-polls N` answers the first N polls of a sign-in "not yet"
//! (the sign-in sheet stays up that long: one poll a second). The rest are
//! the link to the client (`Link`): a delay each way, video packets lost,
//! a rate with its queue, and outages when nothing gets through, as a
//! slow or Wi-Fi link has them.
//!
//! It prints the `PING_XBOX_MOCK=…` line to give Ping; then sign in, list
//! the consoles and stream as with a real account:
//!
//!   PING_XBOX_MOCK=http://127.0.0.1:47920 pingctl xbox sign-in
//!   PING_XBOX_MOCK=http://127.0.0.1:47920 pingctl xbox stream "Mock Xbox"
//!
//! Listening on a LAN address lets a Ping on another computer use it.

use std::net::SocketAddr;

use std::time::Duration;

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
            "--sign-in-polls" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => config.pending_polls = n,
                None => return usage(),
            },
            "--cloud" => {
                config.cloud = match args.next().as_deref() {
                    Some("gamepass") => Cloud::GamePass,
                    Some("free") => Cloud::FreeToPlay,
                    Some("none") => Cloud::None,
                    _ => return usage(),
                }
            }
            link
            @ ("--delay" | "--loss" | "--rate" | "--queue" | "--outage-every" | "--outage") => {
                let Some(v) = args.next().and_then(|v| v.parse::<f64>().ok()) else {
                    return usage();
                };
                let ms = Duration::from_secs_f64(v / 1000.0);
                let l = &mut config.link;
                match link {
                    "--delay" => l.delay = ms,
                    "--loss" => l.video_loss = v.clamp(0.0, 100.0) as u8,
                    "--rate" => l.rate_bps = (v * 1e6) as u64,
                    "--queue" => l.queue = ms,
                    "--outage-every" => {
                        l.outages = Some((ms, l.outages.map_or(Duration::ZERO, |o| o.1)))
                    }
                    _ => l.outages = Some((l.outages.map_or(Duration::from_secs(2), |o| o.0), ms)),
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
                keys = r.reports.iter().map(|r| r.keys.len()).sum::<usize>(),
                keyframe_requests = r.keyframe_requests + r.plis,
                "mock console"
            );
        }
    }
}

fn usage() {
    eprintln!(
        "usage: xbox-mock [--listen ADDR:PORT] [--awake] [--cloud gamepass|free|none] \
         [--sign-in-polls N] [--delay MS] [--loss PCT] [--rate MBPS] [--queue MS] \
         [--outage-every MS --outage MS]"
    );
    std::process::exit(2);
}
