//! Plug in a virtual display, capture it for two seconds, unplug it.
//!   mac-virtual-display [WIDTHxHEIGHT] [HZ]

#[cfg(target_os = "macos")]
fn main() {
    use std::time::{Duration, Instant};

    use pingpong_capture::sck::SckCapture;
    use pingpong_capture::Grab;
    use pingpong_display::macos::VirtualDisplay;
    use pingpong_display::DisplayMode;

    let size = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "3024x1890".into());
    let (w, h): (u16, u16) = size
        .split_once('x')
        .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
        .unwrap();
    let hz: u32 = std::env::args()
        .nth(2)
        .and_then(|f| f.parse().ok())
        .unwrap_or(120);
    let started = Instant::now();
    let mut vd = VirtualDisplay::new(
        DisplayMode {
            width: w,
            height: h,
            refresh_mhz: hz * 1000,
        },
        "Pong",
    )
    .expect("virtual display");
    println!(
        "display {} ({}x{}) in {} ms",
        vd.id,
        vd.mode.width,
        vd.mode.height,
        started.elapsed().as_millis()
    );
    let main_before = SckCapture::main_display();
    if std::env::var("MAIN").is_ok() {
        let t = Instant::now();
        vd.make_main(true).expect("make main");
        println!(
            "made main in {} ms: main display {} -> {}",
            t.elapsed().as_millis(),
            main_before,
            SckCapture::main_display()
        );
    }
    let t = Instant::now();
    let mut cap = loop {
        match SckCapture::new(vd.id, w as u32, h as u32, hz * 1000, true) {
            Ok(c) => break c,
            Err(e) if t.elapsed() < Duration::from_secs(5) => {
                eprintln!("waiting: {e}");
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => panic!("{e}"),
        }
    };
    println!("capturing after {} ms", t.elapsed().as_millis());
    let run = Instant::now();
    let mut frames = 0;
    while run.elapsed() < Duration::from_secs(2) {
        if let Ok(Grab::Frame) = cap.grab(50) {
            frames += 1;
        }
    }
    println!("{frames} frames in 2 s; image: {}", cap.image().is_some());
    drop(cap);
    drop(vd);
    std::thread::sleep(Duration::from_millis(500));
    println!("unplugged; main display now {}", SckCapture::main_display());
}

#[cfg(not(target_os = "macos"))]
fn main() {}
