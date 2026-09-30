//! Look at and poke an X screen, for tests (standing in for someone at a
//! Linux host: answering the desktop's dialogs, checking what is shown).
//!
//!   DISPLAY=:1 x11-poke shot OUT.png        the screen, pointer included
//!   DISPLAY=:1 x11-poke click X Y [BUTTON]  move there and click
//!   DISPLAY=:1 x11-poke key KEYCODE...      press and release X key codes
//!   DISPLAY=:1 x11-poke move X Y

#[cfg(target_os = "linux")]
fn main() {
    use std::time::Duration;

    use pingpong_capture::x11::X11Capture;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::protocol::xtest::ConnectionExt as _;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize| args.get(i).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    match args.first().map(String::as_str) {
        Some("shot") => {
            let mut cap = X11Capture::new(None, true).expect("an X display");
            let _ = cap.grab(100);
            let f = cap.image().expect("an image");
            let mut rgba = Vec::with_capacity(f.width as usize * f.height as usize * 4);
            for row in f.bytes().chunks(f.stride).take(f.height as usize) {
                for px in row[..f.width as usize * 4].chunks(4) {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
            }
            let file = std::fs::File::create(&args[1]).expect("the output file");
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), f.width, f.height);
            enc.set_color(png::ColorType::Rgba);
            enc.write_header()
                .and_then(|mut w| w.write_image_data(&rgba))
                .expect("writing the PNG");
        }
        Some(cmd @ ("click" | "move" | "key")) => {
            let (conn, screen) = x11rb::connect(None).expect("an X display");
            let root = conn.setup().roots[screen].root;
            let fake = |kind: u8, detail: u8, x: i16, y: i16| {
                conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, root, x, y, 0)
                    .unwrap();
                conn.flush().unwrap();
                std::thread::sleep(Duration::from_millis(60));
            };
            match cmd {
                "move" | "click" => {
                    fake(xproto::MOTION_NOTIFY_EVENT, 0, num(1) as i16, num(2) as i16);
                    if cmd == "click" {
                        let b = if args.len() > 3 { num(3) as u8 } else { 1 };
                        fake(xproto::BUTTON_PRESS_EVENT, b, 0, 0);
                        fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
                    }
                }
                _ => {
                    for code in args[1..].iter().filter_map(|c| c.parse::<u8>().ok()) {
                        fake(xproto::KEY_PRESS_EVENT, code, 0, 0);
                        fake(xproto::KEY_RELEASE_EVENT, code, 0, 0);
                    }
                }
            }
            let _ = conn.get_input_focus().unwrap().reply();
        }
        _ => eprintln!(
            "usage: x11-poke shot OUT.png | click X Y [BUTTON] | move X Y | key KEYCODE..."
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux only");
}
