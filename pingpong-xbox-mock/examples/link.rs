//! How Ping's Xbox client holds up on a poor link: it streams from the mock
//! console over a link with the delay, loss and rate given, and says how
//! often and for how long the picture stopped, how late frames were handed
//! to the decoder, and how many keyframes were asked for.
//!
//!   cargo run --release -p pingpong-xbox-mock --example link -- \
//!       [--secs 20] [--delay MS] [--loss PCT] [--rate MBPS] [--queue MS] \\
//!       [--outage-every MS --outage MS]
//!
//! `--delay` is one way, each way (a round trip is twice it). Lateness is
//! the time from when the console sent a frame to when the client handed
//! it on; with no loss it is the delay and the frame's time on the wire.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pingpong_xbox::auth;
use pingpong_xbox::connection::{AudioFrame, Event, Input, Sink, Socket, VideoFrame};
use pingpong_xbox::http::Http;
use pingpong_xbox::stream::{self, StreamOptions, Target};
use pingpong_xbox_mock::{Config, Link, MockConsole, CONSOLE_ID, CONSOLE_NAME};

/// Frames handed to the decoder: when, and which (by RTP time).
#[derive(Default)]
struct Seen {
    frames: Vec<(Instant, u32, bool)>,
}

struct Recorder(Arc<Mutex<Seen>>);

impl Sink for Recorder {
    fn video(&mut self, f: &VideoFrame<'_>) -> bool {
        self.0
            .lock()
            .frames
            .push((Instant::now(), f.rtp_time, f.keyframe));
        true
    }
    fn audio(&mut self, _: &AudioFrame<'_>) {}
    fn event(&mut self, _: Event) {}
    fn input(&mut self, _: &mut dyn FnMut(Input)) {}
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let mut secs = 20u64;
    let mut link = Link::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v: f64 = args
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{a} takes a number"));
        match a.as_str() {
            "--secs" => secs = v as u64,
            "--delay" => link.delay = Duration::from_secs_f64(v / 1000.0),
            "--loss" => link.video_loss = v as u8,
            "--rate" => link.rate_bps = (v * 1e6) as u64,
            "--queue" => link.queue = Duration::from_secs_f64(v / 1000.0),
            "--outage-every" => {
                let length = link.outages.map_or(Duration::ZERO, |o| o.1);
                link.outages = Some((Duration::from_secs_f64(v / 1000.0), length));
            }
            "--outage" => {
                let every = link.outages.map_or(Duration::from_secs(2), |o| o.0);
                link.outages = Some((every, Duration::from_secs_f64(v / 1000.0)));
            }
            _ => panic!("unknown {a}"),
        }
    }
    let mock = MockConsole::start(Config {
        pending_polls: 0,
        console_asleep: false,
        link,
        ..Config::default()
    })
    .expect("the mock");
    let http = Http::with_mock(Some(mock.url()));
    let tmp = tempfile::tempdir().expect("a temporary folder");
    let dir = tmp.path().to_path_buf();
    let code = auth::start_sign_in(&http).expect("sign-in");
    auth::finish_sign_in(&http, &code, &dir, &AtomicBool::new(false)).expect("signed in");

    let seen = Arc::new(Mutex::new(Seen::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let socket = Socket::bind(stream::route_towards(&http)).unwrap();
    let thread = {
        let (http, dir, seen, stop) = (http.clone(), dir.clone(), seen.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut sink = Recorder(seen);
            let options = StreamOptions {
                width: 1280,
                height: 720,
                ..StreamOptions::default()
            };
            let target = Target::Console {
                id: CONSOLE_ID.into(),
                name: CONSOLE_NAME.into(),
            };
            stream::run(&dir, http, &target, &options, socket, &mut sink, &stop)
        })
    };
    let started = Instant::now();
    while seen.lock().frames.is_empty() && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
    }
    // Counted from the first frame on: the connection's set-up is not the
    // link's doing.
    let from = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = thread.join();
    let record = mock.record();
    let epoch = record.video_started.expect("the console sent video");
    let seen = seen.lock();
    let frames: Vec<_> = seen.frames.iter().filter(|f| f.0 >= from).collect();
    let until = frames.last().map_or(from, |f| f.0);

    let mut lateness: Vec<f64> = frames
        .iter()
        .map(|(at, rtp, _)| {
            let due = epoch + Duration::from_nanos(*rtp as u64 * 1_000_000_000 / 90_000);
            at.saturating_duration_since(due).as_secs_f64() * 1000.0
        })
        .collect();
    lateness.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| {
        lateness
            .get(((lateness.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    // Freezes: the picture stood still for longer than three frames.
    let mut gaps: Vec<f64> = frames
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).as_secs_f64() * 1000.0)
        .collect();
    gaps.sort_by(|a, b| b.total_cmp(a));
    let freezes: Vec<f64> = gaps.iter().copied().filter(|g| *g > 50.0).collect();
    let frozen: f64 = freezes.iter().sum();
    let span = (until - from).as_secs_f64();
    println!(
        "link: delay {:?} each way, {}% video loss, {} Mb/s, queue {:?}, outages {:?}",
        link.delay,
        link.video_loss,
        link.rate_bps as f64 / 1e6,
        link.queue,
        link.outages
    );
    println!(
        "frames handed on: {} in {:.1} s ({:.1}/s), {} keyframes",
        frames.len(),
        span,
        frames.len() as f64 / span.max(0.001),
        frames.iter().filter(|f| f.2).count()
    );
    println!(
        "lateness ms: median {:.1}, p95 {:.1}, p99 {:.1}, max {:.1}",
        pct(0.5),
        pct(0.95),
        pct(0.99),
        pct(1.0)
    );
    println!(
        "freezes over 50 ms: {}, {:.0} ms in all ({:.1}% of the time); longest {:.0} ms",
        freezes.len(),
        frozen,
        frozen / 10.0 / span.max(0.001),
        gaps.first().copied().unwrap_or(0.0)
    );
    println!(
        "keyframes asked for: {} by RTCP, {} on the control channel; the console sent {} frames, {} keyframes",
        record.plis, record.keyframe_requests, record.frames, record.keyframes
    );
}
