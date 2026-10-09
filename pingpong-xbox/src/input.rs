//! The input channel's reports: what the client sends the console
//! (controllers, keyboard, mouse, frame timings) and what the console sends
//! back (rumble, the size of its picture).
//!
//! The layout is the one the Xbox web client speaks, as Greenlight writes it
//! (`packages/player/src/client/lib/channel/input/packet.ts`) and reads it
//! (`.../channel/input.ts`): a 14-byte header -- the report kinds present as
//! flags, a sequence number, a timestamp -- then one section per kind, in
//! flag order, each a count and fixed-size frames. Little-endian throughout,
//! but for one field (see [`write_gamepad`]).
//!
//! Encoding writes into a caller's buffer and never allocates: the input
//! thread sends one report per controller change. Decoding reads untrusted
//! bytes and returns `None` on anything short or unknown.

use pingpong_proto::gamepad::{button, GamepadState};

/// Report kinds: bit flags in the header's first field.
pub mod kind {
    pub const METADATA: u16 = 1;
    pub const GAMEPAD: u16 = 2;
    pub const POINTER: u16 = 4;
    pub const CLIENT_METADATA: u16 = 8;
    pub const SERVER_METADATA: u16 = 16;
    pub const MOUSE: u16 = 32;
    pub const KEYBOARD: u16 = 64;
    pub const VIBRATION: u16 = 128;
}

/// Kind flags (2), sequence (4), the client's clock in milliseconds (8).
pub const HEADER_LEN: usize = 14;
/// Index (1), buttons (2), four stick axes (8), two triggers (4), two
/// "physicality" words (8).
pub const GAMEPAD_FRAME_LEN: usize = 23;
/// Seven u32 timings.
pub const METADATA_FRAME_LEN: usize = 28;
/// X, Y, wheel X, wheel Y (u32 each), buttons, relative (u8 each).
pub const MOUSE_FRAME_LEN: usize = 18;
/// Kind of key code, pressed, the code.
pub const KEYBOARD_FRAME_LEN: usize = 3;

/// The largest report the client sends: header, a section of each kind it
/// uses, with room for four controllers and a burst of keys.
pub const MAX_REPORT_LEN: usize = HEADER_LEN
    + 1
    + 4 * GAMEPAD_FRAME_LEN
    + 1
    + 8 * METADATA_FRAME_LEN
    + 1
    + 4 * MOUSE_FRAME_LEN
    + 1
    + 16 * KEYBOARD_FRAME_LEN;

/// Controllers the console takes (as Greenlight's control channel tracks).
pub const MAX_PADS: u8 = 4;

/// Xbox button bits on the input channel. Not XInput's: Nexus (the Xbox
/// button) has a bit of its own, and the rest follow it in this order.
pub mod xbutton {
    pub const NEXUS: u16 = 1 << 1;
    pub const MENU: u16 = 1 << 2;
    pub const VIEW: u16 = 1 << 3;
    pub const A: u16 = 1 << 4;
    pub const B: u16 = 1 << 5;
    pub const X: u16 = 1 << 6;
    pub const Y: u16 = 1 << 7;
    pub const DPAD_UP: u16 = 1 << 8;
    pub const DPAD_DOWN: u16 = 1 << 9;
    pub const DPAD_LEFT: u16 = 1 << 10;
    pub const DPAD_RIGHT: u16 = 1 << 11;
    pub const LEFT_SHOULDER: u16 = 1 << 12;
    pub const RIGHT_SHOULDER: u16 = 1 << 13;
    pub const LEFT_THUMB: u16 = 1 << 14;
    pub const RIGHT_THUMB: u16 = 1 << 15;
}

/// XInput's button bits (what [`GamepadState`] carries) to the console's.
pub fn xbox_buttons(xinput: u32) -> u16 {
    const MAP: [(u32, u16); 15] = [
        (button::GUIDE, xbutton::NEXUS),
        (button::START, xbutton::MENU),
        (button::BACK, xbutton::VIEW),
        (button::A, xbutton::A),
        (button::B, xbutton::B),
        (button::X, xbutton::X),
        (button::Y, xbutton::Y),
        (button::DPAD_UP, xbutton::DPAD_UP),
        (button::DPAD_DOWN, xbutton::DPAD_DOWN),
        (button::DPAD_LEFT, xbutton::DPAD_LEFT),
        (button::DPAD_RIGHT, xbutton::DPAD_RIGHT),
        (button::LEFT_SHOULDER, xbutton::LEFT_SHOULDER),
        (button::RIGHT_SHOULDER, xbutton::RIGHT_SHOULDER),
        (button::LEFT_THUMB, xbutton::LEFT_THUMB),
        (button::RIGHT_THUMB, xbutton::RIGHT_THUMB),
    ];
    MAP.iter()
        .filter(|(x, _)| xinput & x != 0)
        .fold(0, |acc, (_, b)| acc | b)
}

/// The console's button bits back to XInput's (the mock console reads
/// reports with it).
pub fn xinput_buttons(xbox: u16) -> u32 {
    (0..16)
        .map(|bit| 1u16 << bit)
        .filter(|b| xbox & b != 0)
        .map(|b| match b {
            xbutton::NEXUS => button::GUIDE,
            xbutton::MENU => button::START,
            xbutton::VIEW => button::BACK,
            xbutton::A => button::A,
            xbutton::B => button::B,
            xbutton::X => button::X,
            xbutton::Y => button::Y,
            xbutton::DPAD_UP => button::DPAD_UP,
            xbutton::DPAD_DOWN => button::DPAD_DOWN,
            xbutton::DPAD_LEFT => button::DPAD_LEFT,
            xbutton::DPAD_RIGHT => button::DPAD_RIGHT,
            xbutton::LEFT_SHOULDER => button::LEFT_SHOULDER,
            xbutton::RIGHT_SHOULDER => button::RIGHT_SHOULDER,
            xbutton::LEFT_THUMB => button::LEFT_THUMB,
            xbutton::RIGHT_THUMB => button::RIGHT_THUMB,
            _ => 0,
        })
        .fold(0, |acc, b| acc | b)
}

/// One controller's state, as the console takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PadFrame {
    pub index: u8,
    pub buttons: u16,
    /// Up and right are positive; -32767..=32767.
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    /// 0..=65535.
    pub left_trigger: u16,
    pub right_trigger: u16,
}

impl PadFrame {
    /// A controller's state from the platform layer (XInput's conventions:
    /// the same sticks, 8-bit triggers).
    pub fn from_state(s: &GamepadState) -> PadFrame {
        // The console's sticks stop at -32767, as the web client clamps.
        let axis = |v: i16| v.max(-i16::MAX);
        // 0..255 to 0..65535, so a full pull is a full pull.
        let trigger = |v: u8| v as u16 * 257;
        PadFrame {
            index: s.index,
            buttons: xbox_buttons(s.buttons),
            left_x: axis(s.left_x),
            left_y: axis(s.left_y),
            right_x: axis(s.right_x),
            right_y: axis(s.right_y),
            left_trigger: trigger(s.left_trigger),
            right_trigger: trigger(s.right_trigger),
        }
    }
}

/// When a frame was received, handed to the decoder, decoded and shown, in
/// the client's milliseconds: the console's own latency measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTimes {
    /// The frame's RTP timestamp: the console's key for it.
    pub server_key: u32,
    pub first_packet_ms: u32,
    pub submitted_ms: u32,
    pub decoded_ms: u32,
    pub rendered_ms: u32,
}

/// A mouse report. Relative motion only, as Greenlight sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseFrame {
    pub dx: i32,
    pub dy: i32,
    pub wheel_x: i32,
    pub wheel_y: i32,
    /// The DOM's `buttons` bits: 1 left, 2 right, 4 middle, 8 back, 16
    /// forward.
    pub buttons: u8,
}

/// A key, as a Windows virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFrame {
    pub vk: u8,
    pub down: bool,
}

/// The key code is a Windows virtual-key code (the web client's "VKey";
/// 1 is a "known" key and 3 an app command, neither of which is sent).
const KEY_CODE_VK: u8 = 2;

/// What one report carries. Sections left empty are left out.
#[derive(Debug, Default)]
pub struct Report<'a> {
    pub metadata: &'a [FrameTimes],
    pub pads: &'a [PadFrame],
    pub mouse: &'a [MouseFrame],
    pub keys: &'a [KeyFrame],
}

/// Writes reports, numbering them.
#[derive(Debug, Default)]
pub struct ReportWriter {
    seq: u32,
}

impl ReportWriter {
    pub fn new() -> ReportWriter {
        ReportWriter::default()
    }

    /// The report the client opens the channel with: it says how many touch
    /// points it has (one: none to speak of). Returns its length.
    pub fn client_metadata(&mut self, now_ms: f64, out: &mut [u8; MAX_REPORT_LEN]) -> usize {
        let at = self.header(kind::CLIENT_METADATA, now_ms, out);
        out[at] = 1;
        at + 1
    }

    /// One report; `None` when there is nothing in it, or more than fits.
    pub fn write(
        &mut self,
        report: &Report<'_>,
        now_ms: f64,
        out: &mut [u8; MAX_REPORT_LEN],
    ) -> Option<usize> {
        let sections = [
            (kind::METADATA, report.metadata.len(), 8),
            (kind::GAMEPAD, report.pads.len(), MAX_PADS as usize),
            (kind::MOUSE, report.mouse.len(), 4),
            (kind::KEYBOARD, report.keys.len(), 16),
        ];
        if sections.iter().all(|&(_, n, _)| n == 0) || sections.iter().any(|&(_, n, max)| n > max) {
            return None;
        }
        let kinds = sections
            .iter()
            .filter(|&&(_, n, _)| n > 0)
            .fold(0, |acc, &(k, _, _)| acc | k);
        let mut at = self.header(kinds, now_ms, out);
        if !report.metadata.is_empty() {
            out[at] = report.metadata.len() as u8;
            at += 1;
            for m in report.metadata {
                write_metadata(m, now_ms, &mut out[at..at + METADATA_FRAME_LEN]);
                at += METADATA_FRAME_LEN;
            }
        }
        if !report.pads.is_empty() {
            out[at] = report.pads.len() as u8;
            at += 1;
            for p in report.pads {
                write_gamepad(p, &mut out[at..at + GAMEPAD_FRAME_LEN]);
                at += GAMEPAD_FRAME_LEN;
            }
        }
        if !report.mouse.is_empty() {
            out[at] = report.mouse.len() as u8;
            at += 1;
            for m in report.mouse {
                write_mouse(m, &mut out[at..at + MOUSE_FRAME_LEN]);
                at += MOUSE_FRAME_LEN;
            }
        }
        if !report.keys.is_empty() {
            out[at] = report.keys.len() as u8;
            at += 1;
            for k in report.keys {
                out[at] = KEY_CODE_VK;
                out[at + 1] = k.down as u8;
                out[at + 2] = k.vk;
                at += KEYBOARD_FRAME_LEN;
            }
        }
        Some(at)
    }

    fn header(&mut self, kinds: u16, now_ms: f64, out: &mut [u8]) -> usize {
        self.seq = self.seq.wrapping_add(1);
        out[0..2].copy_from_slice(&kinds.to_le_bytes());
        out[2..6].copy_from_slice(&self.seq.to_le_bytes());
        out[6..14].copy_from_slice(&now_ms.to_le_bytes());
        HEADER_LEN
    }
}

fn write_metadata(m: &FrameTimes, now_ms: f64, out: &mut [u8]) {
    let now = now_ms as u32;
    // The last two are when the report was made, twice over: the web client
    // fills both with its clock.
    let words = [
        m.server_key,
        m.first_packet_ms,
        m.submitted_ms,
        m.decoded_ms,
        m.rendered_ms,
        now,
        now,
    ];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
}

/// One controller frame. The last word ("virtual physicality") is written
/// big-endian, as the web client writes it: the console sees the bytes
/// 00 00 00 01 there, and a client that "fixed" the byte order would not
/// send what the console has always been sent.
fn write_gamepad(p: &PadFrame, out: &mut [u8]) {
    out[0] = p.index;
    out[1..3].copy_from_slice(&p.buttons.to_le_bytes());
    out[3..5].copy_from_slice(&p.left_x.to_le_bytes());
    out[5..7].copy_from_slice(&p.left_y.to_le_bytes());
    out[7..9].copy_from_slice(&p.right_x.to_le_bytes());
    out[9..11].copy_from_slice(&p.right_y.to_le_bytes());
    out[11..13].copy_from_slice(&p.left_trigger.to_le_bytes());
    out[13..15].copy_from_slice(&p.right_trigger.to_le_bytes());
    out[15..19].copy_from_slice(&1u32.to_le_bytes());
    out[19..23].copy_from_slice(&1u32.to_be_bytes());
}

fn write_mouse(m: &MouseFrame, out: &mut [u8]) {
    out[0..4].copy_from_slice(&m.dx.to_le_bytes());
    out[4..8].copy_from_slice(&m.dy.to_le_bytes());
    out[8..12].copy_from_slice(&m.wheel_x.to_le_bytes());
    out[12..16].copy_from_slice(&m.wheel_y.to_le_bytes());
    out[16] = m.buttons;
    // 0: relative motion.
    out[17] = 0;
}

/// What the console sent on the input channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerReport {
    /// Run controller `index`'s motors (percent, 0..=100) for `duration_ms`,
    /// then rest `delay_ms`, `repeat` more times.
    Vibration(Vibration),
    /// The size of the picture the console sends.
    VideoSize { width: u32, height: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vibration {
    pub index: u8,
    pub left: u8,
    pub right: u8,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub duration_ms: u16,
    pub delay_ms: u16,
    pub repeat: u8,
}

/// Read a report from the console. Its kind is the first byte only (the
/// kinds it sends fit in one).
pub fn parse_server_report(d: &[u8]) -> Option<ServerReport> {
    let u16_at = |i: usize| Some(u16::from_le_bytes(d.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(d.get(i..i + 4)?.try_into().ok()?));
    match *d.first()? as u16 {
        kind::VIBRATION => {
            // Byte 2 is the kind of rumble (0: four motors), the only one
            // the console is known to send.
            let pct = |i: usize| d.get(i).map(|&v| v.min(100));
            Some(ServerReport::Vibration(Vibration {
                index: *d.get(3)?,
                left: pct(4)?,
                right: pct(5)?,
                left_trigger: pct(6)?,
                right_trigger: pct(7)?,
                duration_ms: u16_at(8)?,
                delay_ms: u16_at(10)?,
                repeat: *d.get(12)?,
            }))
        }
        kind::SERVER_METADATA => Some(ServerReport::VideoSize {
            height: u32_at(2)?,
            width: u32_at(6)?,
        }),
        _ => None,
    }
}

/// What a report from the client says (the mock console reads them).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientReport {
    pub seq: u32,
    pub kinds: u16,
    pub touch_points: Option<u8>,
    pub metadata: Vec<FrameTimes>,
    pub pads: Vec<PadFrame>,
    pub mouse: Vec<MouseFrame>,
    pub keys: Vec<KeyFrame>,
}

/// Read a report the client sent; `None` if it is malformed or has kinds
/// the client does not send (touch, sensors).
pub fn parse_client_report(d: &[u8]) -> Option<ClientReport> {
    let kinds = u16::from_le_bytes(d.get(0..2)?.try_into().ok()?);
    let seq = u32::from_le_bytes(d.get(2..6)?.try_into().ok()?);
    let known =
        kind::METADATA | kind::GAMEPAD | kind::CLIENT_METADATA | kind::MOUSE | kind::KEYBOARD;
    if kinds & !known != 0 {
        return None;
    }
    let mut r = ClientReport {
        seq,
        kinds,
        ..ClientReport::default()
    };
    let mut at = HEADER_LEN;
    let frames = |at: &mut usize, len: usize| -> Option<Vec<&[u8]>> {
        let n = *d.get(*at)? as usize;
        *at += 1;
        let body = d.get(*at..*at + n * len)?;
        *at += n * len;
        Some(body.chunks_exact(len).collect())
    };
    let le32 = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let le16 = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    if kinds & kind::METADATA != 0 {
        for f in frames(&mut at, METADATA_FRAME_LEN)? {
            r.metadata.push(FrameTimes {
                server_key: le32(f, 0),
                first_packet_ms: le32(f, 4),
                submitted_ms: le32(f, 8),
                decoded_ms: le32(f, 12),
                rendered_ms: le32(f, 16),
            });
        }
    }
    if kinds & kind::GAMEPAD != 0 {
        for f in frames(&mut at, GAMEPAD_FRAME_LEN)? {
            r.pads.push(PadFrame {
                index: f[0],
                buttons: le16(f, 1),
                left_x: le16(f, 3) as i16,
                left_y: le16(f, 5) as i16,
                right_x: le16(f, 7) as i16,
                right_y: le16(f, 9) as i16,
                left_trigger: le16(f, 11),
                right_trigger: le16(f, 13),
            });
        }
    }
    if kinds & kind::MOUSE != 0 {
        for f in frames(&mut at, MOUSE_FRAME_LEN)? {
            r.mouse.push(MouseFrame {
                dx: le32(f, 0) as i32,
                dy: le32(f, 4) as i32,
                wheel_x: le32(f, 8) as i32,
                wheel_y: le32(f, 12) as i32,
                buttons: f[16],
            });
        }
    }
    if kinds & kind::KEYBOARD != 0 {
        for f in frames(&mut at, KEYBOARD_FRAME_LEN)? {
            r.keys.push(KeyFrame {
                vk: f[2],
                down: f[1] != 0,
            });
        }
    }
    if kinds & kind::CLIENT_METADATA != 0 {
        r.touch_points = Some(*d.get(at)?);
        at += 1;
    }
    (at == d.len()).then_some(r)
}

/// The console's vibration report, as the mock console writes it.
pub fn write_vibration(v: &Vibration) -> [u8; 13] {
    let mut out = [0u8; 13];
    out[0] = kind::VIBRATION as u8;
    out[3] = v.index;
    out[4] = v.left;
    out[5] = v.right;
    out[6] = v.left_trigger;
    out[7] = v.right_trigger;
    out[8..10].copy_from_slice(&v.duration_ms.to_le_bytes());
    out[10..12].copy_from_slice(&v.delay_ms.to_le_bytes());
    out[12] = v.repeat;
    out
}

/// The console's video size report, as the mock console writes it.
pub fn write_video_size(width: u32, height: u32) -> [u8; 10] {
    let mut out = [0u8; 10];
    out[0] = kind::SERVER_METADATA as u8;
    out[2..6].copy_from_slice(&height.to_le_bytes());
    out[6..10].copy_from_slice(&width.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_xinput_button_has_an_xbox_bit_and_back() {
        let all = button::DPAD_UP
            | button::DPAD_DOWN
            | button::DPAD_LEFT
            | button::DPAD_RIGHT
            | button::START
            | button::BACK
            | button::LEFT_THUMB
            | button::RIGHT_THUMB
            | button::LEFT_SHOULDER
            | button::RIGHT_SHOULDER
            | button::GUIDE
            | button::A
            | button::B
            | button::X
            | button::Y;
        let x = xbox_buttons(all);
        assert_eq!(x.count_ones(), 15);
        // Bit 0 is not a button.
        assert_eq!(x & 1, 0);
        assert_eq!(xinput_buttons(x), all);
        assert_eq!(xbox_buttons(button::GUIDE), xbutton::NEXUS);
        assert_eq!(xbox_buttons(button::START), xbutton::MENU);
    }

    #[test]
    fn a_gamepad_report_has_the_web_clients_layout() {
        let pad = PadFrame::from_state(&GamepadState {
            index: 1,
            buttons: button::A | button::DPAD_LEFT,
            left_trigger: 255,
            right_trigger: 0,
            left_x: i16::MIN,
            left_y: 1000,
            right_x: -2,
            right_y: i16::MAX,
            ..Default::default()
        });
        let mut w = ReportWriter::new();
        let mut out = [0u8; MAX_REPORT_LEN];
        let n = w
            .write(
                &Report {
                    pads: &[pad],
                    ..Default::default()
                },
                1234.5,
                &mut out,
            )
            .unwrap();
        assert_eq!(n, HEADER_LEN + 1 + GAMEPAD_FRAME_LEN);
        assert_eq!(&out[0..2], &kind::GAMEPAD.to_le_bytes());
        assert_eq!(&out[2..6], &1u32.to_le_bytes());
        assert_eq!(f64::from_le_bytes(out[6..14].try_into().unwrap()), 1234.5);
        let f = &out[15..n];
        assert_eq!(out[14], 1, "one frame");
        assert_eq!(f[0], 1, "controller index");
        assert_eq!(
            u16::from_le_bytes([f[1], f[2]]),
            xbutton::A | xbutton::DPAD_LEFT
        );
        // The stick's far left is clamped to -32767.
        assert_eq!(i16::from_le_bytes([f[3], f[4]]), -32767);
        assert_eq!(u16::from_le_bytes([f[11], f[12]]), 65535);
        assert_eq!(&f[15..19], &[1, 0, 0, 0]);
        assert_eq!(&f[19..23], &[0, 0, 0, 1]);
        let parsed = parse_client_report(&out[..n]).unwrap();
        assert_eq!(parsed.pads, vec![pad]);
        assert_eq!(parsed.seq, 1);
    }

    #[test]
    fn reports_are_numbered_from_one() {
        let mut w = ReportWriter::new();
        let mut out = [0u8; MAX_REPORT_LEN];
        let n = w.client_metadata(0.0, &mut out);
        assert_eq!(n, 15);
        let first = parse_client_report(&out[..n]).unwrap();
        assert_eq!((first.seq, first.touch_points), (1, Some(1)));
        let n = w
            .write(
                &Report {
                    keys: &[KeyFrame {
                        vk: 0x41,
                        down: true,
                    }],
                    ..Default::default()
                },
                0.0,
                &mut out,
            )
            .unwrap();
        let second = parse_client_report(&out[..n]).unwrap();
        assert_eq!(second.seq, 2);
        assert_eq!(
            second.keys,
            vec![KeyFrame {
                vk: 0x41,
                down: true
            }]
        );
        assert_eq!(&out[15..18], &[2, 1, 0x41]);
    }

    #[test]
    fn several_kinds_go_in_flag_order() {
        let mut w = ReportWriter::new();
        let mut out = [0u8; MAX_REPORT_LEN];
        let times = FrameTimes {
            server_key: 90_000,
            first_packet_ms: 5,
            submitted_ms: 6,
            decoded_ms: 9,
            rendered_ms: 11,
        };
        let mouse = MouseFrame {
            dx: -4,
            dy: 6,
            wheel_y: -120,
            buttons: 1,
            ..Default::default()
        };
        let key = KeyFrame {
            vk: 0x20,
            down: false,
        };
        let n = w
            .write(
                &Report {
                    metadata: &[times],
                    mouse: &[mouse],
                    keys: &[key],
                    ..Default::default()
                },
                50.0,
                &mut out,
            )
            .unwrap();
        assert_eq!(
            n,
            HEADER_LEN + 1 + METADATA_FRAME_LEN + 1 + MOUSE_FRAME_LEN + 1 + KEYBOARD_FRAME_LEN
        );
        let r = parse_client_report(&out[..n]).unwrap();
        assert_eq!(r.kinds, kind::METADATA | kind::MOUSE | kind::KEYBOARD);
        assert_eq!(r.metadata, vec![times]);
        assert_eq!(r.mouse, vec![mouse]);
        assert_eq!(r.keys, vec![key]);
    }

    #[test]
    fn an_empty_or_oversized_report_is_not_written() {
        let mut w = ReportWriter::new();
        let mut out = [0u8; MAX_REPORT_LEN];
        assert_eq!(w.write(&Report::default(), 0.0, &mut out), None);
        let pads = [PadFrame::default(); 5];
        assert_eq!(
            w.write(
                &Report {
                    pads: &pads,
                    ..Default::default()
                },
                0.0,
                &mut out
            ),
            None
        );
    }

    #[test]
    fn the_largest_report_fits_its_buffer() {
        let mut w = ReportWriter::new();
        let mut out = [0u8; MAX_REPORT_LEN];
        let n = w.write(
            &Report {
                metadata: &[FrameTimes::default(); 8],
                pads: &[PadFrame::default(); 4],
                mouse: &[MouseFrame::default(); 4],
                keys: &[KeyFrame { vk: 1, down: true }; 16],
            },
            0.0,
            &mut out,
        );
        assert_eq!(n, Some(MAX_REPORT_LEN));
    }

    #[test]
    fn vibration_and_video_size_round_trip() {
        let v = Vibration {
            index: 2,
            left: 40,
            right: 100,
            left_trigger: 0,
            right_trigger: 7,
            duration_ms: 250,
            delay_ms: 50,
            repeat: 3,
        };
        assert_eq!(
            parse_server_report(&write_vibration(&v)),
            Some(ServerReport::Vibration(v))
        );
        assert_eq!(
            parse_server_report(&write_video_size(1920, 1080)),
            Some(ServerReport::VideoSize {
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn a_motor_above_a_hundred_percent_is_full() {
        let mut d = write_vibration(&Vibration::default());
        d[4] = 250;
        let Some(ServerReport::Vibration(v)) = parse_server_report(&d) else {
            panic!("a vibration report");
        };
        assert_eq!(v.left, 100);
    }

    #[test]
    fn truncated_or_unknown_reports_are_refused() {
        let v = write_vibration(&Vibration::default());
        for n in 0..v.len() {
            assert_eq!(parse_server_report(&v[..n]), None, "{n} bytes");
        }
        assert_eq!(parse_server_report(&[0x40, 0, 0, 0]), None);
        let mut w = ReportWriter::new();
        let mut out = [0u8; MAX_REPORT_LEN];
        let n = w
            .write(
                &Report {
                    pads: &[PadFrame::default()],
                    ..Default::default()
                },
                0.0,
                &mut out,
            )
            .unwrap();
        for cut in 0..n {
            assert_eq!(parse_client_report(&out[..cut]), None, "{cut} bytes");
        }
        // A touch report, which the client never sends.
        let mut touch = out;
        touch[0] = kind::POINTER as u8;
        assert_eq!(parse_client_report(&touch[..n]), None);
        // Trailing bytes are not ignored.
        assert_eq!(parse_client_report(&out[..n + 1]), None);
    }
}
