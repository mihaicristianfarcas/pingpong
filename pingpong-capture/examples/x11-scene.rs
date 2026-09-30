//! A test scene for a Linux host: a window over the whole X screen with
//! boxes moving at 60 fps and the frame number in binary (a row of squares,
//! lowest bit first), printing the input it receives -- keys with their
//! keysyms, buttons, pointer positions -- one line each on stdout.
//!
//!   DISPLAY=:1 cargo run -p pingpong-capture --example x11-scene [SECONDS]
//!
//! Over everything, unmanaged, unless `SCENE_MANAGED` is set: then an
//! ordinary window, which a window manager lets take the keyboard (under
//! GNOME, a click on it does).

#[cfg(target_os = "linux")]
fn main() {
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::protocol::Event;
    use x11rb::wrapper::ConnectionExt as _;

    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (conn, screen) = x11rb::connect(None).expect("an X display");
    let s = conn.setup().roots[screen].clone();
    let (w, h) = (s.width_in_pixels, s.height_in_pixels);
    let win = conn.generate_id().unwrap();
    let events = EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION;
    conn.create_window(
        s.root_depth,
        win,
        s.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        s.root_visual,
        &CreateWindowAux::new()
            .background_pixel(0x202020)
            .override_redirect(u32::from(std::env::var_os("SCENE_MANAGED").is_none()))
            .event_mask(events),
    )
    .unwrap();
    conn.map_window(win).unwrap();
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(0xffffff))
        .unwrap();
    conn.flush().unwrap();

    let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
    let keymap = |conn: &x11rb::rust_connection::RustConnection| {
        conn.get_keyboard_mapping(min, max - min + 1)
            .unwrap()
            .reply()
            .unwrap()
    };
    let mut map = keymap(&conn);

    let start = Instant::now();
    let mut frame: u32 = 0;
    let colours = [0xe04040u32, 0x40c040, 0x4080f0, 0xe0c040, 0xc040e0];
    println!("scene {w}x{h}");
    while start.elapsed() < Duration::from_secs(secs) {
        let t = frame as f32 / 60.0;
        conn.clear_area(false, win, 0, 0, w, h).unwrap();
        for (i, &c) in colours.iter().enumerate() {
            let x = ((t * (120.0 + 40.0 * i as f32)).sin() * 0.4 + 0.5) * (w as f32 - 140.0);
            let y = (h as f32 / 6.0) * (i as f32 + 1.0) - 70.0;
            conn.change_gc(gc, &ChangeGCAux::new().foreground(c))
                .unwrap();
            conn.poly_fill_rectangle(
                win,
                gc,
                &[Rectangle {
                    x: x as i16,
                    y: y as i16,
                    width: 140,
                    height: 140,
                }],
            )
            .unwrap();
        }
        // The frame number, lowest bit first: white squares are ones.
        for bit in 0..16 {
            let on = frame >> bit & 1 == 1;
            conn.change_gc(
                gc,
                &ChangeGCAux::new().foreground(if on { 0xffffff } else { 0x505050 }),
            )
            .unwrap();
            conn.poly_fill_rectangle(
                win,
                gc,
                &[Rectangle {
                    x: 20 + bit * 40,
                    y: 20,
                    width: 32,
                    height: 32,
                }],
            )
            .unwrap();
        }
        conn.flush().unwrap();
        while let Some(ev) = conn.poll_for_event().unwrap() {
            match ev {
                Event::KeyPress(e) | Event::KeyRelease(e) => {
                    let at = (e.detail - min) as usize * map.keysyms_per_keycode as usize;
                    let sym = map.keysyms.get(at).copied().unwrap_or(0);
                    let what = if e.response_type & 0x7f == KEY_PRESS_EVENT {
                        "press"
                    } else {
                        "release"
                    };
                    println!("key {} {what} sym {sym:#x}", e.detail);
                }
                Event::ButtonPress(e) => {
                    println!("button {} press at {} {}", e.detail, e.root_x, e.root_y)
                }
                Event::ButtonRelease(e) => println!("button {} release", e.detail),
                Event::MotionNotify(e) => println!("pointer {} {}", e.root_x, e.root_y),
                Event::MappingNotify(_) => map = keymap(&conn),
                _ => {}
            }
        }
        frame += 1;
        let next = start + Duration::from_micros(frame as u64 * 16_667);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    conn.sync().unwrap();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux only");
}
