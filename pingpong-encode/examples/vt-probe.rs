//! Capture the main display, encode it with VideoToolbox and write the
//! Annex-B stream to a file, reporting encode times.
//!   vt-probe OUT.h265|OUT.h264 [WIDTHxHEIGHT] [FPS] [MBPS]

#[cfg(target_os = "macos")]
fn main() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use pingpong_capture::sck::SckCapture;
    use pingpong_encode::videotoolbox::VtEncoder;
    use pingpong_encode::{Codec, EncoderConfig, FrameKind};

    let out = std::env::args()
        .nth(1)
        .expect("usage: vt-probe OUT.h265 [WxH] [FPS] [MBPS]");
    let size = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "1920x1080".into());
    let (w, h): (u32, u32) = size
        .split_once('x')
        .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
        .unwrap();
    let fps: u32 = std::env::args()
        .nth(3)
        .and_then(|f| f.parse().ok())
        .unwrap_or(60);
    let mbps: u32 = std::env::args()
        .nth(4)
        .and_then(|f| f.parse().ok())
        .unwrap_or(20);
    let codec = if out.ends_with(".h264") {
        Codec::H264
    } else {
        Codec::Hevc
    };

    let mut cap = SckCapture::new(SckCapture::main_display(), w, h, fps, true).expect("capture");
    let config = EncoderConfig {
        codec,
        width: w,
        height: h,
        fps,
        bitrate_bps: mbps * 1_000_000,
        preset: 1,
        two_pass: false,
        slices: 1,
    };
    let started = Instant::now();
    let mut enc = VtEncoder::new(config).expect("encoder");
    println!(
        "encoder ready in {} ms, low latency: {}",
        started.elapsed().as_millis(),
        enc.low_latency
    );

    // THROUGHPUT=N: encode the same image continuously, N frames in flight,
    // and report the rate the encoder sustains.
    if let Some(depth) = std::env::var("THROUGHPUT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        let _ = cap.grab(500);
        let img = cap.image().expect("an image");
        let run = Instant::now();
        let (mut index, mut done) = (0u64, 0u64);
        while run.elapsed() < Duration::from_secs(3) {
            while enc.in_flight() < depth {
                enc.submit(&img, index, index == 0).expect("submit");
                index += 1;
            }
            if let Some(f) = enc.next(Duration::from_millis(100)) {
                f.expect("encode");
                done += 1;
            }
        }
        println!(
            "{depth} in flight: {:.1} frames/s",
            done as f64 / run.elapsed().as_secs_f64()
        );
        return;
    }
    let mut file = std::fs::File::create(&out).unwrap();
    let mut times = Vec::new();
    let (mut bytes, mut keyframes, mut dropped) = (0usize, 0, 0);
    let run = Instant::now();
    let mut index = 0u64;
    while run.elapsed() < Duration::from_secs(3) {
        let _ = cap.grab(1000 / fps);
        let Some(img) = cap.image() else { continue };
        // A keyframe at the start and one midway, as the client would ask.
        let force = index == 0 || index == 90;
        let t = Instant::now();
        let frame = enc.encode(&img, index, force).expect("encode");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if frame.data.is_empty() {
            dropped += 1;
        }
        if frame.kind == FrameKind::Idr {
            keyframes += 1;
        }
        bytes += frame.data.len();
        file.write_all(&frame.data).unwrap();
        index += 1;
    }
    let (ltr, waiting, _) = enc.ltr_state();
    println!("long-term references: enabled {ltr}, {waiting} made (unconfirmed)");
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    println!(
        "{index} frames ({dropped} dropped), {keyframes} keyframes, {:.1} Mbit/s; encode \
            ms p50 {:.2} p95 {:.2} max {:.2}",
        bytes as f64 * 8.0 / 3.0 / 1e6,
        p(0.5),
        p(0.95),
        p(1.0)
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}
