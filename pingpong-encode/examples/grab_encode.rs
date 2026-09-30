//! Capture a display with Desktop Duplication, convert, encode, write Annex-B.
//!
//!   grab_encode <\\.\DISPLAYn|primary> <codec h264|hevc|av1> <seconds> <fps> <mbps> <out-file>
//!
//! Must run in the interactive session (not over SSH): see tools/host-run.ps1.
//! Frame 30 is encoded as a forced IDR and frames 60..=62 are invalidated, so
//! the output exercises IDR, RFI and recovery-frame marking.

#[cfg(windows)]
fn main() {
    use pingpong_capture::{dda::DdaCapture, gpu::Gpu, Grab};
    use pingpong_encode::{
        convert::Converter, nvenc::NvencEncoder, Codec, EncoderConfig, FrameKind,
    };
    use std::io::Write;
    use std::time::{Duration, Instant};

    tracing_subscriber::fmt().with_env_filter("info").init();
    let args: Vec<String> = std::env::args().collect();
    let target = args.get(1).cloned().unwrap_or_else(|| "primary".into());
    let codec = match args.get(2).map(String::as_str) {
        Some("hevc") => Codec::Hevc,
        Some("av1") => Codec::Av1,
        _ => Codec::H264,
    };
    let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let fps: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mbps: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(50);
    let out = args.get(6).cloned().unwrap_or_else(|| "grab.h264".into());

    for (name, adapter) in Gpu::list_outputs() {
        println!("output {name} on {adapter}");
    }
    let name = if target == "primary" {
        Gpu::list_outputs()[0].0.clone()
    } else {
        target
    };
    let gpu = Gpu::for_output(&name).expect("open output");
    let (device, context) = (gpu.device.clone(), gpu.context.clone());
    let mut cap = DdaCapture::new(gpu);

    // First frame fixes the size.
    let start = Instant::now();
    while cap.texture().is_none() {
        match cap.grab(100) {
            Ok(_) => {}
            Err(e) => println!("grab: {e}"),
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no first frame");
    }
    let (w, h) = cap.size();
    let (w, h) = (w & !1, h & !1);
    let mut conv = Converter::new(&device, &context, w, h).expect("converter");
    let settings = EncoderConfig {
        codec,
        width: w,
        height: h,
        fps,
        bitrate_bps: mbps * 1_000_000,
        preset: 1,
        two_pass: true,
        slices: 1,
    };
    let mut enc = NvencEncoder::new(&device, conv.output(), settings).expect("encoder");

    let mut file = std::fs::File::create(&out).expect("create output");
    let interval = Duration::from_nanos(1_000_000_000 / fps as u64);
    let mut next = Instant::now();
    let (mut new_frames, mut kinds) = (0u32, [0u32; 3]);
    let mut times = Vec::new();
    for index in 0..(seconds * fps as u64) {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += interval;
        if let Ok(Grab::Frame) = cap.grab(0) {
            new_frames += 1;
        }
        let t = Instant::now();
        conv.convert(cap.texture().unwrap()).expect("convert");
        if index == 60 {
            println!("invalidate 58..=59 -> {}", enc.invalidate(58, 59));
        }
        let frame = enc.encode(index, index == 30).expect("encode");
        times.push(t.elapsed().as_micros() as u32);
        kinds[match frame.kind {
            FrameKind::Idr => 0,
            FrameKind::P => 1,
            FrameKind::Recovery => 2,
        }] += 1;
        if frame.kind != FrameKind::P {
            println!(
                "frame {} is {:?} ({} bytes)",
                frame.index,
                frame.kind,
                frame.data.len()
            );
        }
        file.write_all(&frame.data).unwrap();
    }
    times.sort();
    let p = |q: f64| times[((times.len() as f64 - 1.0) * q) as usize] as f64 / 1000.0;
    println!(
        "{w}x{h} {} frames, {new_frames} new captures, idr={} p={} recovery={} \
            convert+encode p50={:.2} p95={:.2} p99={:.2} ms",
        times.len(),
        kinds[0],
        kinds[1],
        kinds[2],
        p(0.5),
        p(0.95),
        p(0.99)
    );
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
