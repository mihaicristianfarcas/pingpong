//! Plug in a virtual display, arrange it (MODE=mirror|extend), and report
//! what CoreGraphics and ScreenCaptureKit then make of the displays.

#[cfg(target_os = "macos")]
fn main() {
    use objc2_core_graphics::{
        CGDisplayBounds, CGDisplayIsActive, CGDisplayIsAsleep, CGDisplayIsOnline,
        CGDisplayMirrorsDisplay, CGDisplayPrimaryDisplay, CGMainDisplayID,
    };
    use pingpong_capture::sck::SckCapture;
    use pingpong_display::macos::VirtualDisplay;
    use pingpong_display::DisplayMode;

    let mirror = std::env::var("MODE").as_deref() != Ok("extend");
    let mut vd = VirtualDisplay::new(
        DisplayMode {
            width: 1280,
            height: 800,
            refresh_mhz: 60_000,
        },
        "Pong",
        false,
    )
    .expect("virtual display");
    vd.make_main(mirror).expect("arrange");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    println!(
        "mirror={mirror}: virtual {} main {} primary-of-set {} ; display 1 mirrors {}",
        vd.id,
        CGMainDisplayID(),
        CGDisplayPrimaryDisplay(vd.id),
        CGDisplayMirrorsDisplay(1)
    );
    for d in [vd.id, 1] {
        println!(
            "  display {d}: online {} active {} asleep {} bounds {:?}",
            CGDisplayIsOnline(d),
            CGDisplayIsActive(d),
            CGDisplayIsAsleep(d),
            CGDisplayBounds(d)
        );
    }
    if let Err(e) = SckCapture::new(0xFFFF, 64, 64, 1000, false, false) {
        println!("capturable: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
