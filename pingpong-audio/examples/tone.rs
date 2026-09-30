//! Play a tone through the whole client path -- Opus, packetizer with FEC,
//! simulated loss and bursty arrival, depacketizer, player -- on this machine.
//!
//!   cargo run -p pingpong-audio --example tone [seconds] [loss%] [stall_ms] [channels]
//!
//! `channels`: 2 (default), 6 or 8; with surround the tone is in the centre
//! channel only, which stereo speakers play only if the downmix works.
//!
//! `stall_ms`: once a second, hold delivery back that long and then release
//! it in a burst (what a Wi-Fi scan does).

#[cfg(target_os = "macos")]
fn main() {
    use pingpong_audio::{opus::Encoder, player::Player};
    use pingpong_proto::audio::{AudioDepacketizer, AudioPacketizer, FRAME_SAMPLES, SAMPLE_RATE};
    use std::time::{Duration, Instant};

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(3);
    let loss: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stall: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut held: Vec<pingpong_proto::audio::AudioPacket> = Vec::new();
    let channels: u8 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
    let player = Player::start(channels, pingpong_audio::coreaudio::open).expect("audio output");
    let mut enc = Encoder::new(channels, pingpong_audio::bitrate_bps(channels, 20_000)).unwrap();
    let mut pack = AudioPacketizer::new();
    let mut depack = AudioDepacketizer::new();
    let mut out = Vec::new();
    let mut buf = [0u8; 1500];
    let mut seed = 0x1234_5678u32;
    let start = Instant::now();
    let mut frame = 0usize;
    let mut last_report = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        // WASAPI hands over 10 ms at a time: two packets per burst.
        for _ in 0..2 {
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .flat_map(|i| {
                    let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.15;
                    // Stereo: both sides. Surround: centre (index 2) only.
                    (0..channels as usize)
                        .map(move |c| if channels == 2 || c == 2 { v } else { 0.0 })
                })
                .collect();
            frame += 1;
            let n = enc.encode(&pcm, &mut buf).unwrap();
            for d in pack.push(&buf[..n], 0) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if seed % 100 < loss {
                    continue;
                }
                depack.push(&d, &mut out);
            }
            let ms = start.elapsed().as_millis() as u64;
            let stalled = stall > 0 && ms % 1000 < stall && ms > 1000;
            held.append(&mut out);
            if !stalled {
                for p in held.drain(..) {
                    player.push(p);
                }
            }
        }
        let due = start + Duration::from_micros(frame as u64 * 5000);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!("{:?} recovered={}", player.stats(), depack.recovered);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
