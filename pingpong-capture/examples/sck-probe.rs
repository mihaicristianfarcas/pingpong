//! Capture the main display for a few seconds and report what arrived.
//!   sck-probe [WIDTHxHEIGHT] [FPS]

#[cfg(target_os = "macos")]
fn main() {
    use std::time::{Duration, Instant};

    use objc2_core_video::{
        CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    };
    use pingpong_capture::sck::SckCapture;
    use pingpong_capture::Grab;

    let size = std::env::args().nth(1).unwrap_or_else(|| "1280x800".into());
    let (w, h) = size
        .split_once('x')
        .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
        .unwrap();
    let fps: u32 = std::env::args()
        .nth(2)
        .and_then(|f| f.parse().ok())
        .unwrap_or(60);
    let started = Instant::now();
    let mut cap = SckCapture::new(SckCapture::main_display(), w, h, fps * 1000, true, false)
        .expect("capture");
    println!("started in {} ms", started.elapsed().as_millis());
    let (mut frames, mut timeouts) = (0, 0);
    let run = Instant::now();
    while run.elapsed() < Duration::from_secs(3) {
        match cap.grab(100).expect("grab") {
            Grab::Frame => frames += 1,
            Grab::Timeout => timeouts += 1,
        }
    }
    let img = cap.image().expect("an image");
    let fourcc = CVPixelBufferGetPixelFormatType(&img).to_be_bytes();
    println!(
        "{frames} frames, {timeouts} timeouts in 3 s; last {}x{} {}",
        CVPixelBufferGetWidth(&img),
        CVPixelBufferGetHeight(&img),
        String::from_utf8_lossy(&fourcc)
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}
