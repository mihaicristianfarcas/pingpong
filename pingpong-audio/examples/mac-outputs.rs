//! The Mac's output devices as a Mac host sees them for surround: each
//! one's channels, speakers, and the most a stream can carry from it.
//!
//!   cargo run -p pingpong-audio --example mac-outputs
//!   cargo run -p pingpong-audio --example mac-outputs -- --tap SECONDS [--mute] [DEVICE]
//!   cargo run -p pingpong-audio --example mac-outputs -- --route DEVICE CHANNELS SECONDS
//!
//! `--route` does what a session does for a loopback device: makes DEVICE
//! the default output with CHANNELS (6 or 8) speakers, for SECONDS, then
//! puts both back (a surround test's setup).
//! `--tap` also captures the default output (or the device named) for that
//! long and prints each channel's loudest sample, in the stream's order
//! (FL FR FC LFE BL BR SL SR); `--mute` keeps what it hears off the device,
//! as a session does. It needs the System Audio Recording
//! permission, which macOS asks for the first time: for the terminal, when
//! run from one.

#[cfg(target_os = "macos")]
fn main() {
    use pingpong_audio::channels;
    use pingpong_audio::tap::{default_output, output_devices, Tap};
    use std::sync::{Arc, Mutex};

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let default = default_output().map(|d| d.id);
    for d in output_devices() {
        println!(
            "{}{} ({}): {} channels at {} Hz{}, speakers {:?} -> {} on the stream",
            if Some(d.id) == default { "* " } else { "  " },
            d.name,
            d.uid,
            d.channels,
            d.rate,
            if d.is_virtual { ", virtual" } else { "" },
            d.labels(),
            d.surround_channels()
        );
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--route") {
        let to = args
            .get(1)
            .and_then(|n| output_devices().into_iter().find(|d| &d.name == n));
        let channels: u8 = args.get(2).and_then(|c| c.parse().ok()).unwrap_or(6);
        let secs: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30.0);
        let Some(to) = to else {
            eprintln!("usage: --route DEVICE CHANNELS SECONDS (DEVICE as listed above)");
            std::process::exit(1);
        };
        let route = pingpong_audio::tap::Route::to(&to, channels);
        if let Some(d) = output_devices().into_iter().find(|d| d.id == to.id) {
            println!("routed: {} speakers {:?}", d.name, d.labels());
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
        drop(route);
        println!("put back");
        return;
    }
    if args.first().map(String::as_str) != Some("--tap") {
        return;
    }
    let secs: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let mute = args.iter().any(|a| a == "--mute");
    let named = args.iter().skip(2).find(|a| *a != "--mute");
    let device = match named {
        Some(name) => output_devices().into_iter().find(|d| &d.name == name),
        None => default_output(),
    };
    let Some(device) = device else {
        eprintln!("no such output device");
        std::process::exit(1);
    };
    let wire = device.surround_channels();
    let plan = channels::plan(&device.labels(), wire);
    let peaks = Arc::new(Mutex::new((vec![0f32; wire as usize], 0usize)));
    let seen = peaks.clone();
    let channels_in = device.channels;
    let mut wired = Vec::new();
    let tap = Tap::start(
        &device,
        mute,
        &[],
        Box::new(move |pcm| {
            wired.clear();
            channels::remap(pcm, channels_in, &plan, wire as usize, &mut wired);
            let mut p = seen.lock().unwrap();
            p.1 += pcm.len() / channels_in.max(1);
            for (i, s) in wired.iter().enumerate() {
                let c = i % wire as usize;
                p.0[c] = p.0[c].max(s.abs());
            }
        }),
    );
    let tap = match tap {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!("tap: {} channels at {} Hz", tap.channels, tap.rate);
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    drop(tap);
    let p = peaks.lock().unwrap();
    println!(
        "{} frames in {secs} s; loudest per channel: {:?}",
        p.1,
        p.0.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>()
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac-outputs is for macOS");
}
