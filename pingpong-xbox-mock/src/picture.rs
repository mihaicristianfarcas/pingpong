//! What the mock console shows: a test picture that makes the stream's
//! behaviour visible in a screenshot. A bar sweeps across (motion, and
//! smearing after a loss would show on it), a counter counts frames, and a
//! panel draws the input the console last received -- each controller
//! button lit while held, the sticks, the triggers, the mouse, the last key
//! -- so input is seen to arrive without reading logs.

use crate::h264::Picture;
use pingpong_xbox::input::xbutton;

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 360;

type Colour = (u8, u8, u8);

/// BT.709, limited range.
const BACKGROUND: Colour = (40, 128, 128);
const PANEL: Colour = (56, 128, 128);
const WHITE: Colour = (235, 128, 128);
const XBOX_GREEN: Colour = (96, 91, 85);
const AMBER: Colour = (160, 60, 170);

/// The input the console last received, as drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputView {
    /// Xbox button bits ([`xbutton`]).
    pub buttons: u16,
    pub left: (i16, i16),
    pub right: (i16, i16),
    pub left_trigger: u16,
    pub right_trigger: u16,
    /// The last key's virtual-key code, and whether it is held.
    pub key: Option<(u8, bool)>,
    /// Where relative mouse motion has taken a dot, and its buttons.
    pub mouse: (i32, i32),
    pub mouse_buttons: u8,
}

/// The order buttons are drawn in, left to right.
const BUTTONS: [u16; 15] = [
    xbutton::A,
    xbutton::B,
    xbutton::X,
    xbutton::Y,
    xbutton::DPAD_UP,
    xbutton::DPAD_DOWN,
    xbutton::DPAD_LEFT,
    xbutton::DPAD_RIGHT,
    xbutton::LEFT_SHOULDER,
    xbutton::RIGHT_SHOULDER,
    xbutton::LEFT_THUMB,
    xbutton::RIGHT_THUMB,
    xbutton::VIEW,
    xbutton::MENU,
    xbutton::NEXUS,
];

/// Segments a..g of each digit (bit 0 is a).
const DIGITS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

fn digit(p: &mut Picture, x: usize, y: usize, d: u8, scale: usize, c: Colour) {
    let (w, h, t) = (6 * scale, 10 * scale, scale.max(2));
    let seg = DIGITS[d as usize % 10];
    let rects = [
        (x, y, w, t),                     // a
        (x + w - t, y, t, h / 2),         // b
        (x + w - t, y + h / 2, t, h / 2), // c
        (x, y + h - t, w, t),             // d
        (x, y + h / 2, t, h / 2),         // e
        (x, y, t, h / 2),                 // f
        (x, y + h / 2 - t / 2, w, t),     // g
    ];
    for (i, &(rx, ry, rw, rh)) in rects.iter().enumerate() {
        if seg & (1 << i) != 0 {
            p.fill(rx, ry, rw, rh, c);
        }
    }
}

fn number(p: &mut Picture, x: usize, y: usize, mut n: u64, digits: usize, scale: usize, c: Colour) {
    let step = 8 * scale;
    for i in (0..digits).rev() {
        digit(p, x + i * step, y, (n % 10) as u8, scale, c);
        n /= 10;
    }
}

/// Where a stick or the mouse puts a dot in a box of `size`.
fn dot_at(v: i32, max: i32, size: usize) -> usize {
    // The dot is 8 pixels: centred at rest, touching an edge at full tilt.
    let centre = (size / 2) as i64 - 4;
    let off = v.clamp(-max, max) as i64 * centre / max as i64;
    (centre + off).clamp(0, size as i64 - 8) as usize
}

/// Frame `frame` of the test picture.
pub fn draw(frame: u64, input: &InputView) -> Picture {
    let mut p = Picture::new(WIDTH, HEIGHT, BACKGROUND);
    p.fill(0, 0, WIDTH, 32, XBOX_GREEN);
    // The sweeping bar, 4 pixels a frame.
    let x = (frame as usize * 4) % WIDTH;
    p.fill(x, 48, 16, 128, WHITE);
    number(&mut p, 16, 192, frame, 6, 4, WHITE);

    // Buttons.
    for (i, &b) in BUTTONS.iter().enumerate() {
        let lit = input.buttons & b != 0;
        p.fill(
            16 + i * 24,
            272,
            16,
            16,
            if lit { XBOX_GREEN } else { PANEL },
        );
    }
    // Sticks (up is up on screen) and triggers.
    for (i, &(sx, sy)) in [input.left, input.right].iter().enumerate() {
        let (bx, by) = (400 + i * 80, 192);
        p.fill(bx, by, 64, 64, PANEL);
        p.fill(
            bx + dot_at(sx as i32, 32767, 64),
            by + dot_at(-(sy as i32), 32767, 64),
            8,
            8,
            WHITE,
        );
    }
    for (i, &t) in [input.left_trigger, input.right_trigger].iter().enumerate() {
        let (bx, by) = (560 + i * 32, 192);
        p.fill(bx, by, 16, 64, PANEL);
        let h = t as usize * 64 / 65535;
        p.fill(bx, by + 64 - h, 16, h, AMBER);
    }
    // Mouse: a dot moved by relative motion, amber while a button is held.
    p.fill(400, 272, 112, 64, PANEL);
    p.fill(
        400 + dot_at(input.mouse.0, 1000, 112),
        272 + dot_at(input.mouse.1, 1000, 64),
        8,
        8,
        if input.mouse_buttons != 0 {
            AMBER
        } else {
            WHITE
        },
    );
    // The last key's code, amber while held.
    if let Some((vk, down)) = input.key {
        number(
            &mut p,
            16,
            312,
            vk as u64,
            3,
            3,
            if down { AMBER } else { WHITE },
        );
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::Encoder;

    #[test]
    fn a_frame_changes_a_few_macroblocks() {
        let mut enc = Encoder::new(WIDTH, HEIGHT);
        let view = InputView::default();
        enc.encode(&draw(0, &view));
        let (p, key) = enc.encode(&draw(1, &view));
        assert!(!key);
        // The bar's old and new columns and the counter's last digit.
        assert!(p.len() < 40 * 400, "{} bytes", p.len());
    }

    #[test]
    fn a_held_button_lights_its_square() {
        let off = draw(0, &InputView::default());
        let on = draw(
            0,
            &InputView {
                buttons: xbutton::A,
                ..Default::default()
            },
        );
        let at = 272 * off.width + 16;
        assert_eq!(off.y[at], PANEL.0);
        assert_eq!(on.y[at], XBOX_GREEN.0);
    }

    #[test]
    fn dots_stay_in_their_boxes() {
        for v in [i32::MIN / 2, -32768, 0, 32767, i32::MAX / 2] {
            let x = dot_at(v, 32767, 64);
            assert!(x <= 56, "{v}: {x}");
        }
        assert_eq!(dot_at(0, 32767, 64), 28);
    }
}
