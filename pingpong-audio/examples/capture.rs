//! Capture host audio for a few seconds and report levels (Windows).
//!
//!   cargo run -p pingpong-audio --example capture [seconds] [--sink]

#[cfg(windows)]
fn main() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    tracing_subscriber::fmt().init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let sink = args.iter().any(|a| a == "--sink");
    println!("outputs: {:?}", pingpong_audio::wasapi::outputs());
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            stop.store(true, Ordering::Relaxed);
        });
    }
    let mut enc = pingpong_audio::opus::Encoder::new(2, 96_000).unwrap();
    let mut buf = [0u8; 1500];
    let (mut frames, mut bytes, mut peak) = (0u64, 0u64, 0f32);
    let start = Instant::now();
    let mut last = Instant::now();
    let cfg = pingpong_audio::wasapi::CaptureConfig {
        channels: 2,
        virtual_sink: sink,
        state_path: None,
    };
    pingpong_audio::wasapi::run(&cfg, &stop, |pcm| {
        frames += 1;
        peak = pcm.iter().fold(peak, |p, v| p.max(v.abs()));
        bytes += enc.encode(pcm, &mut buf).unwrap() as u64;
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            println!(
                "{:.1}s frames={frames} ({:.0}/s) peak={peak:.3} opus={} B",
                start.elapsed().as_secs_f32(),
                frames as f32 / start.elapsed().as_secs_f32(),
                bytes
            );
            peak = 0.0;
        }
    })
    .unwrap();
}

#[cfg(not(windows))]
fn main() {}
