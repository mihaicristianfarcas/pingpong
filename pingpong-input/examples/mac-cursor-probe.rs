//! The cursor on screen now, against the standard ones (macOS).
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn main() {
    use objc2_app_kit::{NSApplication, NSCursor};
    // AppKit's cursors need the application object.
    let _app =
        NSApplication::sharedApplication(objc2::MainThreadMarker::new().expect("main thread"));
    let describe = |c: &NSCursor| {
        let img = c.image();
        let size = img.size();
        let tiff = img.TIFFRepresentation().map(|d| d.len()).unwrap_or(0);
        let reps = img.representations().len();
        format!(
            "hot ({:.1}, {:.1}) size {:.0}x{:.0} tiff {} reps {}",
            c.hotSpot().x,
            c.hotSpot().y,
            size.width,
            size.height,
            tiff,
            reps
        )
    };
    let known = [
        ("arrow", NSCursor::arrowCursor()),
        ("ibeam", NSCursor::IBeamCursor()),
        ("hand", NSCursor::pointingHandCursor()),
        ("cross", NSCursor::crosshairCursor()),
        ("no", NSCursor::operationNotAllowedCursor()),
        ("lr", NSCursor::resizeLeftRightCursor()),
        ("ud", NSCursor::resizeUpDownCursor()),
        ("l", NSCursor::resizeLeftCursor()),
        ("r", NSCursor::resizeRightCursor()),
        ("u", NSCursor::resizeUpCursor()),
        ("d", NSCursor::resizeDownCursor()),
        ("open", NSCursor::openHandCursor()),
        ("closed", NSCursor::closedHandCursor()),
        ("vibeam", NSCursor::IBeamCursorForVerticalLayout()),
        ("copy", NSCursor::dragCopyCursor()),
        ("link", NSCursor::dragLinkCursor()),
        ("menu", NSCursor::contextualMenuCursor()),
        ("poof", NSCursor::disappearingItemCursor()),
    ];
    for (name, c) in &known {
        println!("{name:8} {}", describe(c));
    }
    for _ in 0..3 {
        match NSCursor::currentSystemCursor() {
            Some(c) => println!("current  {}", describe(&c)),
            None => println!("current  none"),
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // As the host reads it: on a thread of its own.
    pingpong_input::cursor::prepare();
    let display = unsafe { CGMainDisplayID() };
    std::thread::spawn(move || {
        let mut w = pingpong_input::cursor::CursorWatcher::new(display, (1280, 800));
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(600));
            println!("watcher  {:?}", w.poll(std::time::Instant::now()));
        }
    })
    .join()
    .unwrap();
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
}

#[cfg(not(target_os = "macos"))]
fn main() {}
