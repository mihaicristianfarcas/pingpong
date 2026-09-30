//! Whether this process may post input, and a pointer move read back (no
//! keys are sent). Moves the pointer to the display's centre and back.

#[cfg(target_os = "macos")]
fn main() {
    use objc2_core_graphics::{CGDisplayBounds, CGEvent, CGMainDisplayID};
    use pingpong_input::macos::{trusted, CgEventSink};
    use pingpong_input::InputSink;
    use pingpong_proto::input::InputEvent;

    let display = CGMainDisplayID();
    let b = CGDisplayBounds(display);
    let here = || CGEvent::location(CGEvent::new(None).as_deref());
    println!(
        "trusted: {}; display {display} {:?}; pointer at {:?}",
        trusted(),
        b.size,
        here()
    );
    let (w, h) = (1000u32, 1000u32);
    let mut sink = CgEventSink::new(display, w, h);
    let before = here();
    sink.inject(&[InputEvent::MouseMoveAbs { x: 500, y: 500 }])
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("after move to the centre: {:?}", here());
    sink.inject(&[InputEvent::MouseMoveRel { dx: 50, dy: 0 }])
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("after +50 px right: {:?}", here());
    let back = (
        (before.x - b.origin.x) * w as f64 / b.size.width,
        (before.y - b.origin.y) * h as f64 / b.size.height,
    );
    sink.inject(&[InputEvent::MouseMoveAbs {
        x: back.0 as u16,
        y: back.1 as u16,
    }])
    .unwrap();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
