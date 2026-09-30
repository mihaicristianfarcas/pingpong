//! Scripted input for hands-free end-to-end tests: `PING_TEST_INPUT` holds
//! steps separated by `;`, run one after another once the stream is up.
//!
//! | Step | What it does |
//! |---|---|
//! | `wait MS` | pause |
//! | `key SC` | press and release a key (PS/2 set-1 scancode, hex; `e05b` is the left Windows key) |
//! | `keydown SC`, `keyup SC` | hold a key across later steps |
//! | `move DX DY` | relative pointer motion |
//! | `abs X Y` | absolute pointer position, in stream pixels |
//! | `click` | left click |
//! | `wheel N` | wheel, vertical |
//! | `text WORDS` | type text, as a paste would |
//! | `paste` | type this computer's clipboard (Ctrl+Alt+Shift+V) |
//! | `fullscreen` | switch full screen and window (not on Linux) |
//! | `pad BUTTONS LX LY [LT RT]`, `pad-off` | controller 0's state (XInput bits), or unplug it |
//! | `takeover` | watching an agent: take over, or hand back |
//! | `quit` | end the stream as the user would |
//!
//! For example `PING_TEST_INPUT="wait 3000; key 01; move 100 0; click"`.

use ping_core::session::EndCallback;

pub fn run_script(
    script: &str,
    input: &ping_core::input::InputSender,
    controls: Option<ping_core::stream::ControlSender>,
    quit: EndCallback,
) {
    use pingpong_proto::input::{Button, InputEvent};
    let mut pad_seq = 0u16;
    for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = step.split_whitespace().collect();
        let num = |i: usize| {
            parts
                .get(i)
                .and_then(|v| {
                    i64::from_str_radix(
                        v.trim_start_matches("0x"),
                        if v.starts_with("0x") { 16 } else { 10 },
                    )
                    .ok()
                })
                .unwrap_or(0)
        };
        match parts[0] {
            "wait" => std::thread::sleep(std::time::Duration::from_millis(num(1) as u64)),
            // quit: end the stream as the user would.
            "quit" => quit(None),
            "key" => {
                let sc = u16::from_str_radix(parts.get(1).unwrap_or(&"0"), 16).unwrap_or(0);
                input.send(InputEvent::KeyDown(sc));
                std::thread::sleep(std::time::Duration::from_millis(40));
                input.send(InputEvent::KeyUp(sc));
            }
            // keydown / keyup SC: hold a key across later steps.
            "keydown" | "keyup" => {
                let sc = u16::from_str_radix(parts.get(1).unwrap_or(&"0"), 16).unwrap_or(0);
                input.send(if parts[0] == "keydown" {
                    InputEvent::KeyDown(sc)
                } else {
                    InputEvent::KeyUp(sc)
                });
            }
            "move" => input.motion(num(1) as f64, num(2) as f64),
            "abs" => input.send(InputEvent::MouseMoveAbs {
                x: num(1) as u16,
                y: num(2) as u16,
            }),
            "click" => {
                input.send(InputEvent::ButtonDown(Button::Left));
                std::thread::sleep(std::time::Duration::from_millis(40));
                input.send(InputEvent::ButtonUp(Button::Left));
            }
            "wheel" => input.send(InputEvent::Wheel {
                dv: num(1) as i16,
                dh: 0,
            }),
            // fullscreen: switch full screen <-> window, as Ctrl-Option-Shift-X.
            "fullscreen" => {
                #[cfg(target_os = "macos")]
                dispatch2::DispatchQueue::main().exec_async(ping_core::mac::toggle_fullscreen);
                #[cfg(windows)]
                ping_core::win::toggle_fullscreen();
                #[cfg(target_os = "linux")]
                eprintln!("test input: fullscreen is not scripted on Linux");
            }
            // text WORDS: type them as the clipboard would be.
            "text" => {
                let chars = input.type_text(step["text".len()..].trim_start());
                eprintln!("test input: typed {chars} characters");
            }
            // paste: type this computer's clipboard, as Ctrl-Option-Shift-V does.
            "paste" => {
                #[cfg(target_os = "macos")]
                let text = ping_core::mac::clipboard_text();
                #[cfg(windows)]
                let text = ping_core::win::clipboard_text();
                #[cfg(target_os = "linux")]
                let text = arboard::Clipboard::new()
                    .ok()
                    .and_then(|mut c| c.get_text().ok());
                let chars = text.map_or(0, |t| input.type_text(&t));
                eprintln!("test input: typed {chars} characters");
            }
            // pad BUTTONS LX LY [LT RT]: controller 0's whole state
            // (XInput button bits); pad-off unplugs it.
            "pad" | "pad-off" => {
                pad_seq = pad_seq.wrapping_add(1);
                let state = pingpong_proto::gamepad::GamepadState {
                    index: 0,
                    seq: pad_seq,
                    connected: parts[0] == "pad",
                    buttons: num(1) as u32,
                    left_x: num(2) as i16,
                    left_y: num(3) as i16,
                    left_trigger: num(4) as u8,
                    right_trigger: num(5) as u8,
                    ..Default::default()
                };
                if let Some(c) = &controls {
                    c.send(pingpong_proto::control::Control::Gamepad(state));
                }
            }
            // takeover: watching an agent, take over or hand back (Ctrl-Alt-Shift-T).
            "takeover" => {
                if let Some(c) = &controls {
                    c.toggle_take_over();
                }
            }
            other => eprintln!("test input: unknown step {other}"),
        }
    }
    eprintln!("test input: done");
}
