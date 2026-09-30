//! Replay what Pong does at each session, for each WIDTHxHEIGHT given:
//! plug in a virtual display, make it main (mirrored), unplug it. Prints the
//! display's mode and mirror state at each step.

#[cfg(target_os = "macos")]
fn main() {
    use objc2_core_graphics::{
        CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsInMirrorSet, CGDisplayMirrorsDisplay,
        CGDisplayMode, CGDisplayPrimaryDisplay, CGMainDisplayID,
    };
    use pingpong_display::macos::VirtualDisplay;
    use pingpong_display::DisplayMode;

    let state = |id: u32| {
        let m = CGDisplayCopyDisplayMode(id);
        format!(
            "mode {}x{} px, {}x{} pt; in mirror set {}, mirrors {}, set primary {}; main {}",
            CGDisplayMode::pixel_width(m.as_deref()),
            CGDisplayMode::pixel_height(m.as_deref()),
            CGDisplayBounds(id).size.width,
            CGDisplayBounds(id).size.height,
            CGDisplayIsInMirrorSet(id),
            CGDisplayMirrorsDisplay(id),
            CGDisplayPrimaryDisplay(id),
            CGMainDisplayID()
        )
    };
    for size in std::env::args().skip(1) {
        let (w, h): (u16, u16) = size
            .split_once('x')
            .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
            .unwrap();
        println!("== {size}");
        let mut vd = match VirtualDisplay::new(
            DisplayMode {
                width: w,
                height: h,
                refresh_mhz: 60_000,
            },
            "Pong",
        ) {
            Ok(vd) => vd,
            Err(e) => {
                println!("  not plugged in: {e}");
                continue;
            }
        };
        println!("  plugged in {}: {}", vd.id, state(vd.id));
        match vd.make_main(true) {
            Ok(()) => println!("  arranged: {}", state(vd.id)),
            Err(e) => println!("  arranging failed ({e}): {}", state(vd.id)),
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        drop(vd);
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
