//! Input events, carried in datagrams with `Kind::Input` (v2 design §4.3).
//!
//! Pure: no winit, no Windows, no sockets. The client encodes the same bytes
//! the host decodes, and every rule that can be wrong -- the sequence span, the
//! i16 bounds, the hostile-input checks -- is testable here with no hardware.
//!
//! A packet carries the last up-to-8 events, NOT just the new ones. No
//! per-event sequence number is transmitted: the receiver derives the span from
//! `frame_id - count + 1 ..= frame_id`, which is what makes §5.4's redundancy
//! cost nothing.

use std::time::Duration;

use crate::header::{Header, Kind};
use crate::HEADER_LEN;

/// v2 design §4.3: `event_count` is 1..=8.
pub const MAX_EVENTS_PER_PACKET: usize = 8;

/// Header + count byte + 8 events at their largest encoding (5 bytes).
pub const MAX_INPUT_LEN: usize = HEADER_LEN + 1 + MAX_EVENTS_PER_PACKET * 5;

const TAG_KEY_DOWN: u8 = 0;
const TAG_KEY_UP: u8 = 1;
const TAG_MOUSE_MOVE_REL: u8 = 2;
const TAG_MOUSE_MOVE_ABS: u8 = 3;
const TAG_BUTTON_DOWN: u8 = 4;
const TAG_BUTTON_UP: u8 = 5;
const TAG_WHEEL: u8 = 6;
// Tag 7 is HeldState, reserved by v2 design §4.3 and deliberately not implemented.
const TAG_TEXT: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl Button {
    fn code(self) -> u8 {
        match self {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
            Button::X1 => 3,
            Button::X2 => 4,
        }
    }

    fn from_code(c: u8) -> Option<Button> {
        match c {
            0 => Some(Button::Left),
            1 => Some(Button::Right),
            2 => Some(Button::Middle),
            3 => Some(Button::X1),
            4 => Some(Button::X2),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// PS/2 set-1 scancode. The E0 extended prefix is encoded in the high bit,
    /// so right control is `0x801d`, not a two-byte sequence (v2 design §4.3).
    KeyDown(u16),
    KeyUp(u16),
    MouseMoveRel {
        dx: i16,
        dy: i16,
    },
    /// Stream pixels, already transformed by the client (v2 design §5.2).
    MouseMoveAbs {
        x: u16,
        y: u16,
    },
    ButtonDown(Button),
    ButtonUp(Button),
    Wheel {
        dv: i16,
        dh: i16,
    },
    /// A character typed as text, whatever the host's keyboard layout (the
    /// clipboard pasted as keystrokes). Holds no key down.
    Text(char),
}

impl InputEvent {
    /// The permission a device needs for the host to act on it
    /// (`permission::KEYBOARD` for keys and text, `MOUSE` for the rest), as
    /// Apollo's `passthrough` checks each input packet's kind (`input.cpp`).
    pub fn permission(&self) -> u16 {
        match self {
            InputEvent::KeyDown(_) | InputEvent::KeyUp(_) | InputEvent::Text(_) => {
                crate::permission::KEYBOARD
            }
            _ => crate::permission::MOUSE,
        }
    }
}

/// The events `allowed` lets through, in order: `events` itself when it
/// allows every kind, else the permitted ones, copied into `buf` (no
/// allocation: this runs for every input packet). A batch holds at most
/// [`MAX_EVENTS_PER_PACKET`]; events past that many are dropped.
pub fn permitted<'a>(
    events: &'a [InputEvent],
    allowed: crate::permission::Permissions,
    buf: &'a mut [InputEvent; MAX_EVENTS_PER_PACKET],
) -> &'a [InputEvent] {
    use crate::permission::{KEYBOARD, MOUSE};
    if allowed.allows(KEYBOARD | MOUSE) {
        return events;
    }
    let mut n = 0;
    for ev in events {
        if allowed.allows(ev.permission()) && n < buf.len() {
            buf[n] = *ev;
            n += 1;
        }
    }
    &buf[..n]
}

/// Write one event record. Returns bytes written.
fn encode_event(ev: InputEvent, out: &mut [u8]) -> usize {
    match ev {
        InputEvent::KeyDown(sc) => {
            out[0] = TAG_KEY_DOWN;
            out[1..3].copy_from_slice(&sc.to_le_bytes());
            3
        }
        InputEvent::KeyUp(sc) => {
            out[0] = TAG_KEY_UP;
            out[1..3].copy_from_slice(&sc.to_le_bytes());
            3
        }
        InputEvent::MouseMoveRel { dx, dy } => {
            out[0] = TAG_MOUSE_MOVE_REL;
            out[1..3].copy_from_slice(&dx.to_le_bytes());
            out[3..5].copy_from_slice(&dy.to_le_bytes());
            5
        }
        InputEvent::MouseMoveAbs { x, y } => {
            out[0] = TAG_MOUSE_MOVE_ABS;
            out[1..3].copy_from_slice(&x.to_le_bytes());
            out[3..5].copy_from_slice(&y.to_le_bytes());
            5
        }
        InputEvent::ButtonDown(b) => {
            out[0] = TAG_BUTTON_DOWN;
            out[1] = b.code();
            2
        }
        InputEvent::ButtonUp(b) => {
            out[0] = TAG_BUTTON_UP;
            out[1] = b.code();
            2
        }
        InputEvent::Wheel { dv, dh } => {
            out[0] = TAG_WHEEL;
            out[1..3].copy_from_slice(&dv.to_le_bytes());
            out[3..5].copy_from_slice(&dh.to_le_bytes());
            5
        }
        InputEvent::Text(c) => {
            out[0] = TAG_TEXT;
            out[1..5].copy_from_slice(&(c as u32).to_le_bytes());
            5
        }
    }
}

/// Read one event record. Returns the event and bytes consumed.
fn decode_event(body: &[u8]) -> Option<(InputEvent, usize)> {
    let i16_at =
        |o: usize| -> Option<i16> { Some(i16::from_le_bytes([*body.get(o)?, *body.get(o + 1)?])) };
    let u16_at =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes([*body.get(o)?, *body.get(o + 1)?])) };
    let ev = match *body.first()? {
        TAG_KEY_DOWN => (InputEvent::KeyDown(u16_at(1)?), 3),
        TAG_KEY_UP => (InputEvent::KeyUp(u16_at(1)?), 3),
        TAG_MOUSE_MOVE_REL => (
            InputEvent::MouseMoveRel {
                dx: i16_at(1)?,
                dy: i16_at(3)?,
            },
            5,
        ),
        TAG_MOUSE_MOVE_ABS => (
            InputEvent::MouseMoveAbs {
                x: u16_at(1)?,
                y: u16_at(3)?,
            },
            5,
        ),
        TAG_BUTTON_DOWN => (InputEvent::ButtonDown(Button::from_code(*body.get(1)?)?), 2),
        TAG_BUTTON_UP => (InputEvent::ButtonUp(Button::from_code(*body.get(1)?)?), 2),
        TAG_WHEEL => (
            InputEvent::Wheel {
                dv: i16_at(1)?,
                dh: i16_at(3)?,
            },
            5,
        ),
        TAG_TEXT => {
            let code =
                u32::from_le_bytes([*body.get(1)?, *body.get(2)?, *body.get(3)?, *body.get(4)?]);
            (InputEvent::Text(char::from_u32(code)?), 5)
        }
        _ => return None,
    };
    Some(ev)
}

/// Which client process a packet came from, carried in the header's
/// `fragment_idx` (v2 design §4.3).
///
/// **Why the sequence number alone is not enough.** `SequenceGate` keeps a
/// watermark and discards anything not newer, which is what makes the §5.4
/// retransmits idempotent. A client that restarts begins counting from 0 again,
/// so every packet it sends is "older" than the watermark left by the previous
/// one -- and the host discards ALL of it, silently, until the new client has
/// sent as many events as the old one did. Video keeps streaming throughout,
/// because a re-`SessionStart` for the mode already running is answered with a
/// re-ack that changes no state (`session::on_event`). The symptom is a session
/// that looks perfect and accepts no input at all.
///
/// A generation cannot be inferred from the sequence, either: a restart lands at
/// 0, which is only a few hundred behind a short-lived predecessor's watermark
/// and therefore indistinguishable from ordinary reordering. So the client says
/// which process it is, and the host resets when the answer changes.
///
/// Resetting the gate on the re-ack instead would double-inject: the client
/// retransmits `SessionStart` every 100 ms until acked, so several arrive after
/// activation -- eight of them in the 2026-08-04 14:47 run -- and each reset
/// would re-admit the whole ring, re-pressing held keys and re-clicking buttons
/// at the exact moment the user is first moving the mouse.
pub type Generation = u16;

/// A decoded packet: up to 8 events, contiguous in sequence, ending at
/// `newest_seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputBatch {
    pub newest_seq: u32,
    /// The sending client process; see [`Generation`].
    pub generation: Generation,
    events: [InputEvent; MAX_EVENTS_PER_PACKET],
    len: u8,
}

impl InputBatch {
    pub fn events(&self) -> &[InputEvent] {
        &self.events[..self.len as usize]
    }

    /// Sequence number of the oldest event in this packet. Wrap-aware by
    /// construction: subtraction on u32 wraps, which is what we want.
    pub fn oldest_seq(&self) -> u32 {
        self.newest_seq.wrapping_sub(self.len as u32 - 1)
    }
}

/// Encode up to 8 events into a complete `kind=2` datagram.
///
/// `newest_seq` is the sequence number of the LAST event in `events`; the rest
/// are derived by counting backwards. Panics if `events` is empty or longer
/// than [`MAX_EVENTS_PER_PACKET`] -- both are caller bugs, not network input.
pub fn encode(
    events: &[InputEvent],
    newest_seq: u32,
    generation: Generation,
    capture_ts_us: u32,
    out: &mut [u8; MAX_INPUT_LEN],
) -> usize {
    assert!(
        !events.is_empty() && events.len() <= MAX_EVENTS_PER_PACKET,
        "event_count must be 1..=8 (v2 design §4.3)"
    );

    let mut n = HEADER_LEN;
    out[n] = events.len() as u8;
    n += 1;
    for &ev in events {
        n += encode_event(ev, &mut out[n..]);
    }

    let header = Header {
        keyframe: false,
        recovery: false,
        lan_shards: false,
        frame_end: true,
        kind: Kind::Input,
        total_len: n as u16,
        // v2 design §4.3: for kind=2 this field is not a fragment index -- input is
        // never fragmented -- it is the sending client's [`Generation`].
        fragment_idx: generation,
        // v2 design §4: the remaining FEC fields are meaningless for input and are
        // zeroed. data_shards is 1 rather than 0 to match control.rs, where a
        // zero-shard header would be a malformed video header if misrouted.
        data_shards: 1,
        parity_shards: 0,
        fec_block_idx: 0,
        frame_len: (n - HEADER_LEN) as u32,
        capture_ts_us,
        frame_id: newest_seq,
    };
    let mut hdr = [0u8; HEADER_LEN];
    header
        .encode(&mut hdr)
        .expect("input header is a fixed, valid shape");
    out[..HEADER_LEN].copy_from_slice(&hdr);
    n
}

/// Decode the body of an already-parsed input datagram.
///
/// `body` is everything after the header; `newest_seq` is the header's
/// `frame_id` and `generation` its `fragment_idx`. Returns `None` for anything
/// malformed -- this parses hostile network input and must never panic.
pub fn decode(body: &[u8], newest_seq: u32, generation: Generation) -> Option<InputBatch> {
    let count = *body.first()? as usize;
    if count == 0 || count > MAX_EVENTS_PER_PACKET {
        return None;
    }
    let mut events = [InputEvent::Wheel { dv: 0, dh: 0 }; MAX_EVENTS_PER_PACKET];
    let mut off = 1;
    for slot in events.iter_mut().take(count) {
        let (ev, used) = decode_event(body.get(off..)?)?;
        *slot = ev;
        off += used;
    }
    Some(InputBatch {
        newest_seq,
        generation,
        events,
        len: count as u8,
    })
}

/// The sender's side: the last 8 events and the running sequence number.
///
/// Pure, so it lives here rather than in the client -- and so its behaviour is
/// tested with no winit and no socket. Every packet carries the WHOLE ring, not
/// just what is new, which is what makes v2 design §5.4's redundancy free.
pub struct InputRing {
    events: [InputEvent; MAX_EVENTS_PER_PACKET],
    len: usize,
    /// Sequence of the newest event. The first push is sequence 0.
    newest_seq: u32,
    pushed: bool,
    /// Stamped on every packet so the host can tell this process from the one
    /// that held the session before it. See [`Generation`].
    generation: Generation,
}

impl Default for InputRing {
    fn default() -> InputRing {
        InputRing::new()
    }
}

impl InputRing {
    /// A ring for this client process, with a generation taken from the clock.
    ///
    /// The clock rather than a random number because this crate is pure and has
    /// no RNG, and because the requirement is only that consecutive runs of the
    /// client differ -- this is a nonce for spotting a restart, not a secret.
    /// Two clients starting in the same microsecond would collide, which is not
    /// a way this is used.
    pub fn new() -> InputRing {
        InputRing::with_generation(crate::clock::now_us() as Generation)
    }

    pub fn with_generation(generation: Generation) -> InputRing {
        InputRing {
            events: [InputEvent::Wheel { dv: 0, dh: 0 }; MAX_EVENTS_PER_PACKET],
            len: 0,
            newest_seq: 0,
            pushed: false,
            generation,
        }
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn push(&mut self, ev: InputEvent) {
        // The first event is sequence 0, so the counter advances BEFORE every
        // push except the first. Advancing after would make the first packet
        // claim sequence 0 for an event the gate then sees again as 1.
        if self.pushed {
            self.newest_seq = self.newest_seq.wrapping_add(1);
        }
        self.pushed = true;

        if self.len < MAX_EVENTS_PER_PACKET {
            self.events[self.len] = ev;
            self.len += 1;
        } else {
            self.events.rotate_left(1);
            self.events[MAX_EVENTS_PER_PACKET - 1] = ev;
        }
    }

    /// Encode the whole ring. `None` when nothing has been pushed yet -- spec
    /// §4.3's `event_count` is 1..=8, so a zero-event packet is not valid, and
    /// the trailing repeat must not fire before any input exists.
    pub fn encode_into(&self, capture_ts_us: u32, out: &mut [u8; MAX_INPUT_LEN]) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        Some(encode(
            &self.events[..self.len],
            self.newest_seq,
            self.generation,
            capture_ts_us,
            out,
        ))
    }
}

/// Drops input the receiver has already applied.
///
/// This is what makes unreliable input safe (v2 design §5.4). Duplicates and
/// reorders are idempotent BY CONSTRUCTION -- including relative deltas, which
/// would otherwise be applied once per retransmitted copy and send the camera
/// spinning. It is also why the client's trailing repeat needs no host-side
/// code at all: the repeats simply do not get past here.
#[derive(Debug, Default)]
pub struct SequenceGate {
    /// `None` until the first packet, so sequence 0 is applicable.
    last_applied: Option<u32>,
    /// Which client the watermark belongs to. A watermark means nothing across
    /// a restart, because the new process counts from 0 again -- see
    /// [`Generation`].
    generation: Option<Generation>,
}

/// Wrap-aware "is `a` newer than `b`", per v1 design §5.1. u32 at ~1000 events/s takes
/// 49 days to wrap, but the rule is not conditional on it being likely.
fn newer_than(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl SequenceGate {
    pub fn new() -> SequenceGate {
        SequenceGate {
            last_applied: None,
            generation: None,
        }
    }

    pub fn last_applied_seq(&self) -> Option<u32> {
        self.last_applied
    }

    /// Return the events in `batch` that have not been applied yet, in order.
    pub fn admit<'a>(&mut self, batch: &'a InputBatch) -> &'a [InputEvent] {
        let events = batch.events();
        // A different client process: its sequence numbers have nothing to do
        // with the watermark, and comparing them discards every event it will
        // ever send. Start over from this packet.
        if self.generation != Some(batch.generation) {
            self.generation = Some(batch.generation);
            self.last_applied = Some(batch.newest_seq);
            return events;
        }
        let Some(last) = self.last_applied else {
            self.last_applied = Some(batch.newest_seq);
            return events;
        };
        if !newer_than(batch.newest_seq, last) {
            return &[];
        }
        // How many of this packet's events are newer than the watermark. The
        // packet spans oldest_seq..=newest_seq, so this saturates naturally
        // when the gap exceeds the ring.
        let new_count = batch.newest_seq.wrapping_sub(last).min(events.len() as u32) as usize;
        self.last_applied = Some(batch.newest_seq);
        &events[events.len() - new_count..]
    }
}

/// A physical key position, named by its **W3C UI Events `code` value**.
///
/// A published standard rather than a shadow of any one windowing library --
/// which is what lets this table stay in the pure core. winit's `KeyCode`
/// implements the same standard, so the client's mapping is 1:1 by name; a
/// future non-winit client reuses this table rather than reimplementing it.
///
/// Only keys with a real set-1 scancode appear here. A key winit can emit that
/// this enum has no variant for is one v2 design §5.1 deliberately drops, and that
/// decision belongs at the client boundary, not in this table.
#[rustfmt::skip]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Escape,
    Digit1, Digit2, Digit3, Digit4, Digit5,
    Digit6, Digit7, Digit8, Digit9, Digit0,
    Minus, Equal, Backspace, Tab,

    KeyQ, KeyW, KeyE, KeyR, KeyT, KeyY, KeyU, KeyI, KeyO, KeyP,
    BracketLeft, BracketRight, Enter, ControlLeft,

    KeyA, KeyS, KeyD, KeyF, KeyG, KeyH, KeyJ, KeyK, KeyL,
    Semicolon, Quote, Backquote, ShiftLeft, Backslash,

    KeyZ, KeyX, KeyC, KeyV, KeyB, KeyN, KeyM,
    Comma, Period, Slash, ShiftRight,
    NumpadMultiply, AltLeft, Space, CapsLock,

    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    NumLock, ScrollLock,

    Numpad0, Numpad1, Numpad2, Numpad3, Numpad4,
    Numpad5, Numpad6, Numpad7, Numpad8, Numpad9,
    NumpadSubtract, NumpadAdd, NumpadDecimal, NumpadDivide, NumpadEnter,

    /// The extra key on ISO layouts, between left shift and Z.
    IntlBackslash,

    ControlRight, PrintScreen, AltRight,
    Home, ArrowUp, PageUp, ArrowLeft, ArrowRight,
    End, ArrowDown, PageDown, Insert, Delete,
    SuperLeft, SuperRight, ContextMenu,
}

impl Key {
    /// Every variant, so the table can be checked exhaustively.
    #[rustfmt::skip]
    pub const ALL: &'static [Key] = &[
        Key::Escape,
        Key::Digit1, Key::Digit2, Key::Digit3, Key::Digit4, Key::Digit5,
        Key::Digit6, Key::Digit7, Key::Digit8, Key::Digit9, Key::Digit0,
        Key::Minus, Key::Equal, Key::Backspace, Key::Tab,
        Key::KeyQ, Key::KeyW, Key::KeyE, Key::KeyR, Key::KeyT,
        Key::KeyY, Key::KeyU, Key::KeyI, Key::KeyO, Key::KeyP,
        Key::BracketLeft, Key::BracketRight, Key::Enter, Key::ControlLeft,
        Key::KeyA, Key::KeyS, Key::KeyD, Key::KeyF, Key::KeyG,
        Key::KeyH, Key::KeyJ, Key::KeyK, Key::KeyL,
        Key::Semicolon, Key::Quote, Key::Backquote, Key::ShiftLeft, Key::Backslash,
        Key::KeyZ, Key::KeyX, Key::KeyC, Key::KeyV, Key::KeyB, Key::KeyN, Key::KeyM,
        Key::Comma, Key::Period, Key::Slash, Key::ShiftRight,
        Key::NumpadMultiply, Key::AltLeft, Key::Space, Key::CapsLock,
        Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6,
        Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12,
        Key::NumLock, Key::ScrollLock,
        Key::Numpad0, Key::Numpad1, Key::Numpad2, Key::Numpad3, Key::Numpad4,
        Key::Numpad5, Key::Numpad6, Key::Numpad7, Key::Numpad8, Key::Numpad9,
        Key::NumpadSubtract, Key::NumpadAdd, Key::NumpadDecimal,
        Key::NumpadDivide, Key::NumpadEnter,
        Key::IntlBackslash,
        Key::ControlRight, Key::PrintScreen, Key::AltRight,
        Key::Home, Key::ArrowUp, Key::PageUp, Key::ArrowLeft, Key::ArrowRight,
        Key::End, Key::ArrowDown, Key::PageDown, Key::Insert, Key::Delete,
        Key::SuperLeft, Key::SuperRight, Key::ContextMenu,
    ];
}

/// v2 design §4.3: the E0 extended prefix rides in the high bit.
const E0: u16 = 0x8000;

/// [`Key`] -> PS/2 set-1 make code, positionally (v2 design E4, §5.1).
///
/// **Total.** Every variant has a code; there is no `Option` and no fallback.
/// Keys with no set-1 representation simply have no `Key` variant, so the
/// question "what do we do about unmappable keys" is answered once, at the
/// client boundary, instead of being re-asked on every lookup here.
pub fn scancode(key: Key) -> u16 {
    match key {
        Key::Escape => 0x01,
        Key::Digit1 => 0x02,
        Key::Digit2 => 0x03,
        Key::Digit3 => 0x04,
        Key::Digit4 => 0x05,
        Key::Digit5 => 0x06,
        Key::Digit6 => 0x07,
        Key::Digit7 => 0x08,
        Key::Digit8 => 0x09,
        Key::Digit9 => 0x0a,
        Key::Digit0 => 0x0b,
        Key::Minus => 0x0c,
        Key::Equal => 0x0d,
        Key::Backspace => 0x0e,
        Key::Tab => 0x0f,

        Key::KeyQ => 0x10,
        Key::KeyW => 0x11,
        Key::KeyE => 0x12,
        Key::KeyR => 0x13,
        Key::KeyT => 0x14,
        Key::KeyY => 0x15,
        Key::KeyU => 0x16,
        Key::KeyI => 0x17,
        Key::KeyO => 0x18,
        Key::KeyP => 0x19,
        Key::BracketLeft => 0x1a,
        Key::BracketRight => 0x1b,
        Key::Enter => 0x1c,
        Key::ControlLeft => 0x1d,

        Key::KeyA => 0x1e,
        Key::KeyS => 0x1f,
        Key::KeyD => 0x20,
        Key::KeyF => 0x21,
        Key::KeyG => 0x22,
        Key::KeyH => 0x23,
        Key::KeyJ => 0x24,
        Key::KeyK => 0x25,
        Key::KeyL => 0x26,
        Key::Semicolon => 0x27,
        Key::Quote => 0x28,
        Key::Backquote => 0x29,
        Key::ShiftLeft => 0x2a,
        Key::Backslash => 0x2b,

        Key::KeyZ => 0x2c,
        Key::KeyX => 0x2d,
        Key::KeyC => 0x2e,
        Key::KeyV => 0x2f,
        Key::KeyB => 0x30,
        Key::KeyN => 0x31,
        Key::KeyM => 0x32,
        Key::Comma => 0x33,
        Key::Period => 0x34,
        Key::Slash => 0x35,
        Key::ShiftRight => 0x36,
        Key::NumpadMultiply => 0x37,
        Key::AltLeft => 0x38,
        Key::Space => 0x39,
        Key::CapsLock => 0x3a,

        Key::F1 => 0x3b,
        Key::F2 => 0x3c,
        Key::F3 => 0x3d,
        Key::F4 => 0x3e,
        Key::F5 => 0x3f,
        Key::F6 => 0x40,
        Key::F7 => 0x41,
        Key::F8 => 0x42,
        Key::F9 => 0x43,
        Key::F10 => 0x44,
        Key::F11 => 0x57,
        Key::F12 => 0x58,

        Key::NumLock => 0x45,
        Key::ScrollLock => 0x46,

        Key::Numpad7 => 0x47,
        Key::Numpad8 => 0x48,
        Key::Numpad9 => 0x49,
        Key::NumpadSubtract => 0x4a,
        Key::Numpad4 => 0x4b,
        Key::Numpad5 => 0x4c,
        Key::Numpad6 => 0x4d,
        Key::NumpadAdd => 0x4e,
        Key::Numpad1 => 0x4f,
        Key::Numpad2 => 0x50,
        Key::Numpad3 => 0x51,
        Key::Numpad0 => 0x52,
        Key::NumpadDecimal => 0x53,

        Key::IntlBackslash => 0x56,

        Key::NumpadEnter => E0 | 0x1c,
        Key::ControlRight => E0 | 0x1d,
        Key::NumpadDivide => E0 | 0x35,
        Key::PrintScreen => E0 | 0x37,
        Key::AltRight => E0 | 0x38,
        Key::Home => E0 | 0x47,
        Key::ArrowUp => E0 | 0x48,
        Key::PageUp => E0 | 0x49,
        Key::ArrowLeft => E0 | 0x4b,
        Key::ArrowRight => E0 | 0x4d,
        Key::End => E0 | 0x4f,
        Key::ArrowDown => E0 | 0x50,
        Key::PageDown => E0 | 0x51,
        Key::Insert => E0 | 0x52,
        Key::Delete => E0 | 0x53,
        Key::SuperLeft => E0 | 0x5b,
        Key::SuperRight => E0 | 0x5c,
        Key::ContextMenu => E0 | 0x5d,
    }
}

/// v2 design §5.3, matching Moonlight's `MOUSE_BATCHING_INTERVAL_MS 1`.
pub const BATCH_WINDOW: Duration = Duration::from_millis(1);

/// v2 design §5.4: eight trailing copies after the queue drains.
pub const REPEAT_COUNT: usize = 8;

/// Accumulates relative motion over the ≤1ms batching window (v2 design §5.3).
///
/// Two rules, both of which have bitten other implementations:
///
/// 1. It SPLITS rather than saturating at `i16` bounds. Clamping a fast flick
///    stops the camera short with no visible cause.
/// 2. It CARRIES the fractional residue. winit's deltas are `f64`, and
///    truncating each one independently means a slow sub-pixel drag rounds to
///    zero forever and the cursor simply never moves.
#[derive(Debug, Default)]
pub struct MotionBatcher {
    dx: f64,
    dy: f64,
}

impl MotionBatcher {
    pub fn new() -> MotionBatcher {
        MotionBatcher::default()
    }

    pub fn accumulate(&mut self, dx: f64, dy: f64) {
        self.dx += dx;
        self.dy += dy;
    }

    pub fn is_empty(&self) -> bool {
        // Below one whole pixel in both axes there is nothing to send yet --
        // but the residue stays, which is what rule 2 above is about.
        self.dx.trunc() == 0.0 && self.dy.trunc() == 0.0
    }

    /// Emit whole-pixel motion, splitting at `i16` bounds, and keep the
    /// fractional remainder for next time.
    pub fn drain(&mut self, out: &mut Vec<InputEvent>) {
        let mut dx = self.dx.trunc();
        let mut dy = self.dy.trunc();
        self.dx -= dx;
        self.dy -= dy;

        while dx != 0.0 || dy != 0.0 {
            let step_x = dx.clamp(i16::MIN as f64, i16::MAX as f64);
            let step_y = dy.clamp(i16::MIN as f64, i16::MAX as f64);
            out.push(InputEvent::MouseMoveRel {
                dx: step_x as i16,
                dy: step_y as i16,
            });
            dx -= step_x;
            dy -= step_y;
        }
    }
}

/// The exponential-backoff trailing repeat of v2 design §5.4.
///
/// A pure iterator over inter-send gaps: 1, 2, 4, 8, 16, 32, 64, 128 ms, so the
/// last copy leaves 255ms after the original. Driven by whatever calls it,
/// which is what makes the schedule testable with no clock.
#[derive(Debug, Default)]
pub struct RepeatSchedule {
    sent: usize,
}

impl RepeatSchedule {
    pub fn new() -> RepeatSchedule {
        RepeatSchedule::default()
    }

    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.sent >= REPEAT_COUNT {
            return None;
        }
        let ms = 1u64 << self.sent;
        self.sent += 1;
        Some(Duration::from_millis(ms))
    }

    pub fn reset(&mut self) {
        self.sent = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_a_device_may_not_send_is_left_out() {
        use crate::permission::{Permissions, KEYBOARD, MOUSE, VIEW};
        let events = [
            InputEvent::KeyDown(0x1e),
            InputEvent::MouseMoveRel { dx: 3, dy: -2 },
            InputEvent::Text('a'),
            InputEvent::ButtonDown(Button::Left),
            InputEvent::KeyUp(0x1e),
        ];
        let mut buf = [InputEvent::Wheel { dv: 0, dh: 0 }; MAX_EVENTS_PER_PACKET];
        let all = Permissions::from_bits(VIEW | KEYBOARD | MOUSE);
        assert_eq!(permitted(&events, all, &mut buf), &events);
        let keys = Permissions::from_bits(VIEW | KEYBOARD);
        assert_eq!(
            permitted(&events, keys, &mut buf),
            &[events[0], events[2], events[4]]
        );
        let mouse = Permissions::from_bits(VIEW | MOUSE);
        assert_eq!(permitted(&events, mouse, &mut buf), &[events[1], events[3]]);
        assert!(permitted(&events, Permissions::SEE_ONLY, &mut buf).is_empty());
    }
    use crate::header::{Header, Kind};

    #[test]
    fn round_trips_through_a_real_header() {
        let events = [
            InputEvent::KeyDown(0x11),
            InputEvent::MouseMoveRel { dx: -3, dy: 7 },
            InputEvent::ButtonDown(Button::X2),
            InputEvent::Wheel { dv: 120, dh: -120 },
            InputEvent::MouseMoveAbs { x: 1920, y: 1080 },
            InputEvent::KeyUp(0x801d),
            InputEvent::Text('ș'),
            InputEvent::Text('🏓'),
        ];
        let mut wire = [0u8; MAX_INPUT_LEN];
        let n = encode(&events, 41, 7, 12_345, &mut wire);

        let header = Header::decode(&wire[..n]).expect("decodes");
        assert_eq!(header.kind, Kind::Input);
        assert_eq!(
            header.total_len as usize, n,
            "total_len must be exact (v1 design §5.1)"
        );
        assert_eq!(header.capture_ts_us, 12_345);
        assert_eq!(
            header.frame_id, 41,
            "frame_id is the NEWEST event's seq (v2 design §4.1)"
        );

        let batch =
            decode(&wire[HEADER_LEN..n], header.frame_id, header.fragment_idx).expect("decodes");
        assert_eq!(batch.newest_seq, 41);
        assert_eq!(batch.events(), &events);
    }

    #[test]
    fn a_full_ring_fits_in_max_input_len() {
        // Every event at its largest encoding. If this overflows, MAX_INPUT_LEN
        // is wrong and encode() would panic on a slice bound in the field.
        let events = [InputEvent::MouseMoveRel {
            dx: i16::MIN,
            dy: i16::MAX,
        }; MAX_EVENTS_PER_PACKET];
        let mut wire = [0u8; MAX_INPUT_LEN];
        let n = encode(&events, u32::MAX, 0, 0, &mut wire);
        assert_eq!(n, MAX_INPUT_LEN);
        let batch = decode(&wire[HEADER_LEN..n], u32::MAX, 0).expect("decodes");
        assert_eq!(batch.events().len(), MAX_EVENTS_PER_PACKET);
    }

    #[test]
    fn hostile_bodies_return_none_rather_than_panicking() {
        // This parses anything that reaches the tunnel (v2 design §10.1).
        assert_eq!(decode(&[], 0, 0), None);
        assert_eq!(
            decode(&[0], 0, 0),
            None,
            "event_count 0 is not a valid packet"
        );
        assert_eq!(decode(&[99], 0, 0), None, "count beyond the max");
        assert_eq!(
            decode(&[1], 0, 0),
            None,
            "count claims one event, body is empty"
        );
        assert_eq!(
            decode(&[1, 0], 0, 0),
            None,
            "KeyDown with a truncated scancode"
        );
        assert_eq!(decode(&[1, 200, 0, 0], 0, 0), None, "unknown event tag");
        assert_eq!(
            decode(&[1, 4, 99], 0, 0),
            None,
            "ButtonDown with an out-of-range button"
        );
    }

    #[test]
    fn the_ring_holds_the_last_eight_events() {
        let mut ring = InputRing::new();
        for i in 0..12u16 {
            ring.push(InputEvent::KeyDown(i));
        }
        let mut out = [0u8; MAX_INPUT_LEN];
        let n = ring.encode_into(0, &mut out).expect("has events");
        let header = Header::decode(&out[..n]).expect("decodes");
        let batch =
            decode(&out[HEADER_LEN..n], header.frame_id, header.fragment_idx).expect("decodes");

        assert_eq!(batch.events().len(), MAX_EVENTS_PER_PACKET);
        assert_eq!(batch.events()[7], InputEvent::KeyDown(11), "newest is last");
        assert_eq!(
            batch.events()[0],
            InputEvent::KeyDown(4),
            "oldest of the last 8"
        );
        assert_eq!(header.frame_id, 11, "seq counts events, starting at 0");
    }

    #[test]
    fn an_empty_ring_encodes_nothing() {
        // v2 design §4.3: event_count is 1..=8. A zero-event packet is not valid,
        // and the trailing repeat must not fire before any input exists.
        let ring = InputRing::new();
        let mut out = [0u8; MAX_INPUT_LEN];
        assert_eq!(ring.encode_into(0, &mut out), None);
    }

    #[test]
    fn a_partial_ring_encodes_only_what_it_has() {
        let mut ring = InputRing::new();
        ring.push(InputEvent::KeyDown(0x11));
        ring.push(InputEvent::KeyUp(0x11));
        let mut out = [0u8; MAX_INPUT_LEN];
        let n = ring.encode_into(0, &mut out).expect("has events");
        let header = Header::decode(&out[..n]).expect("decodes");
        let batch =
            decode(&out[HEADER_LEN..n], header.frame_id, header.fragment_idx).expect("decodes");
        assert_eq!(batch.events().len(), 2);
        assert_eq!(header.frame_id, 1);
    }

    #[test]
    fn every_event_encoding_is_at_most_five_bytes() {
        // MAX_INPUT_LEN is derived from this. If a future event type is bigger,
        // this fails here rather than truncating on the wire.
        for ev in [
            InputEvent::KeyDown(0xffff),
            InputEvent::KeyUp(0xffff),
            InputEvent::MouseMoveRel {
                dx: i16::MIN,
                dy: i16::MIN,
            },
            InputEvent::MouseMoveAbs {
                x: u16::MAX,
                y: u16::MAX,
            },
            InputEvent::ButtonDown(Button::X2),
            InputEvent::ButtonUp(Button::Left),
            InputEvent::Wheel {
                dv: i16::MAX,
                dh: i16::MIN,
            },
        ] {
            let mut buf = [0u8; 8];
            assert!(encode_event(ev, &mut buf) <= 5, "{ev:?} is too large");
        }
    }

    #[test]
    fn wasd_maps_to_the_set_1_positions() {
        // The keys this project exists for. Set 1 make codes.
        assert_eq!(scancode(Key::KeyW), 0x11);
        assert_eq!(scancode(Key::KeyA), 0x1e);
        assert_eq!(scancode(Key::KeyS), 0x1f);
        assert_eq!(scancode(Key::KeyD), 0x20);
    }

    #[test]
    fn extended_keys_carry_the_e0_bit() {
        // v2 design §4.3: E0 rides in the high bit rather than as a second byte.
        assert_eq!(scancode(Key::ControlRight), 0x801d);
        assert_eq!(scancode(Key::AltRight), 0x8038);
        assert_eq!(scancode(Key::ArrowUp), 0x8048);
        assert_eq!(scancode(Key::NumpadEnter), 0x801c);
        assert_eq!(scancode(Key::Delete), 0x8053);
    }

    #[test]
    fn left_and_right_modifiers_are_distinct() {
        // Positional input is the whole point of E4. A game that binds
        // right-shift must not receive left-shift.
        assert_ne!(scancode(Key::ShiftLeft), scancode(Key::ShiftRight));
        assert_ne!(scancode(Key::ControlLeft), scancode(Key::ControlRight));
        assert_ne!(scancode(Key::AltLeft), scancode(Key::AltRight));
    }

    #[test]
    fn no_two_keys_share_a_scancode() {
        // EXHAUSTIVE, not a sample: `scancode` is total and `ALL` enumerates
        // its domain, so this checks the whole table. A collision means two
        // different keys inject as the same key, which stays invisible until a
        // player rebinds and gets the wrong action.
        let mut seen: Vec<u16> = Key::ALL.iter().map(|&k| scancode(k)).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), before, "two keys map to the same scancode");
    }

    #[test]
    fn all_lists_every_variant() {
        // ALL is hand-written, so it can drift from the enum. A new variant
        // that is missing here would silently escape the collision test above.
        // The count is the guard; update it deliberately when adding a key.
        assert_eq!(Key::ALL.len(), 104);
    }

    #[test]
    fn the_extended_bit_never_collides_with_a_base_code() {
        // A base code above 0x7fff would be indistinguishable from an E0 key.
        // Set 1 make codes are all under 0x60, but assert it rather than
        // assume it -- the sink strips this bit to build wScan.
        for &k in Key::ALL {
            assert_eq!(scancode(k) & 0x7fff, scancode(k) & !0x8000);
            assert!(
                scancode(k) & 0x7fff < 0x100,
                "{k:?} has an implausible base code"
            );
        }
    }

    /// One client's packet. The gate tests below are about ONE client unless
    /// they say otherwise, so the generation is fixed here and varied only by
    /// `batch_from`.
    fn batch(events: &[InputEvent], newest_seq: u32) -> InputBatch {
        batch_from(events, newest_seq, 1)
    }

    fn batch_from(events: &[InputEvent], newest_seq: u32, generation: Generation) -> InputBatch {
        let mut wire = [0u8; MAX_INPUT_LEN];
        let n = encode(events, newest_seq, generation, 0, &mut wire);
        decode(&wire[HEADER_LEN..n], newest_seq, generation).expect("round trips")
    }

    #[test]
    fn a_fresh_gate_admits_everything_in_the_first_packet() {
        let mut gate = SequenceGate::new();
        let b = batch(&[InputEvent::KeyDown(0x11), InputEvent::KeyUp(0x11)], 1);
        assert_eq!(gate.admit(&b).len(), 2);
        assert_eq!(gate.last_applied_seq(), Some(1));
    }

    #[test]
    fn a_duplicate_packet_admits_nothing() {
        // The trailing repeat (v2 design §5.4) sends the SAME packet up to eight
        // more times. If this admitted anything, every held key would be
        // re-pressed and every relative delta applied nine times.
        let mut gate = SequenceGate::new();
        let b = batch(&[InputEvent::MouseMoveRel { dx: 10, dy: 0 }], 5);
        assert_eq!(gate.admit(&b).len(), 1);
        assert_eq!(gate.admit(&b).len(), 0);
        assert_eq!(gate.admit(&b).len(), 0);
    }

    #[test]
    fn a_packet_admits_only_its_new_suffix() {
        let mut gate = SequenceGate::new();
        let older = [InputEvent::KeyDown(0x11), InputEvent::KeyDown(0x1e)];
        assert_eq!(gate.admit(&batch(&older, 1)).len(), 2);

        // Next packet carries the same two plus two new ones.
        let all = [
            InputEvent::KeyDown(0x11),
            InputEvent::KeyDown(0x1e),
            InputEvent::KeyUp(0x11),
            InputEvent::KeyUp(0x1e),
        ];
        assert_eq!(
            gate.admit(&batch(&all, 3)),
            &[InputEvent::KeyUp(0x11), InputEvent::KeyUp(0x1e)]
        );
    }

    #[test]
    fn an_out_of_order_packet_admits_nothing() {
        let mut gate = SequenceGate::new();
        gate.admit(&batch(&[InputEvent::KeyDown(0x11)], 10));
        assert_eq!(gate.admit(&batch(&[InputEvent::KeyDown(0x1e)], 4)).len(), 0);
        assert_eq!(gate.last_applied_seq(), Some(10), "must not rewind");
    }

    #[test]
    fn a_gap_larger_than_the_ring_admits_the_whole_packet() {
        // Loss longer than 8 events: everything in the packet is new, and the
        // events between are gone for good. Admitting the whole ring is the
        // best available answer and must not panic or silently drop.
        let mut gate = SequenceGate::new();
        gate.admit(&batch(&[InputEvent::KeyDown(0x11)], 1));
        let far = [InputEvent::KeyUp(0x11); MAX_EVENTS_PER_PACKET];
        assert_eq!(gate.admit(&batch(&far, 100)).len(), MAX_EVENTS_PER_PACKET);
    }

    #[test]
    fn a_restarted_client_is_not_discarded_as_stale() {
        // The defect this exists for. The first client leaves the watermark at
        // 5000; the second starts from 0, and every packet it sends is "older".
        // Without the generation the host discards 5000 events -- minutes of
        // mouse movement -- while the video streams perfectly, so the session
        // looks alive and takes no input at all.
        let mut gate = SequenceGate::new();
        gate.admit(&batch_from(&[InputEvent::KeyDown(0x11)], 5_000, 1));

        let restarted = batch_from(&[InputEvent::ButtonDown(Button::Left)], 0, 2);
        assert_eq!(
            gate.admit(&restarted),
            &[InputEvent::ButtonDown(Button::Left)],
            "the new client's first click must be applied, not swallowed"
        );
        assert_eq!(gate.last_applied_seq(), Some(0), "the watermark rebased");
    }

    #[test]
    fn the_new_generation_is_still_gated_against_itself() {
        // Rebasing must not disarm the gate: the §5.4 retransmits from the NEW
        // client have to stay idempotent, or its first eight events replay on
        // every copy.
        let mut gate = SequenceGate::new();
        gate.admit(&batch_from(&[InputEvent::KeyDown(0x11)], 900, 1));

        let first = batch_from(&[InputEvent::MouseMoveRel { dx: 10, dy: 0 }], 0, 2);
        assert_eq!(gate.admit(&first).len(), 1);
        assert_eq!(gate.admit(&first).len(), 0, "a repeat of it admits nothing");
        assert_eq!(gate.admit(&first).len(), 0);
    }

    #[test]
    fn a_generation_that_goes_back_still_rebases() {
        // The generation is a nonce off the clock, not a counter, so the second
        // client's can be numerically lower than the first's. Any CHANGE is a
        // restart; ordering them would be reading meaning that is not there.
        let mut gate = SequenceGate::new();
        gate.admit(&batch_from(&[InputEvent::KeyDown(0x11)], 400, 60_000));
        assert_eq!(
            gate.admit(&batch_from(&[InputEvent::KeyUp(0x11)], 3, 40))
                .len(),
            1
        );
    }

    #[test]
    fn one_client_keeps_its_watermark_across_a_re_ack() {
        // The alternative fix -- resetting the gate whenever the host re-acks a
        // duplicate SessionStart -- would re-admit the whole ring here. Same
        // generation, so nothing rebases and only the genuinely new event lands.
        let mut gate = SequenceGate::new();
        let held = [
            InputEvent::KeyDown(0x11),
            InputEvent::ButtonDown(Button::Left),
        ];
        assert_eq!(gate.admit(&batch_from(&held, 1, 7)).len(), 2);

        let next = [
            InputEvent::KeyDown(0x11),
            InputEvent::ButtonDown(Button::Left),
            InputEvent::KeyUp(0x11),
        ];
        assert_eq!(
            gate.admit(&batch_from(&next, 2, 7)),
            &[InputEvent::KeyUp(0x11)],
            "the held key and the click must not be re-injected"
        );
    }

    #[test]
    fn the_generation_survives_the_wire() {
        // It rides in a header field that means something else for video, so a
        // round trip is the only proof it is not being overwritten downstream.
        let mut wire = [0u8; MAX_INPUT_LEN];
        let n = encode(&[InputEvent::KeyDown(0x11)], 9, 0xbeef, 0, &mut wire);
        let header = Header::decode(&wire[..n]).expect("decodes");
        assert_eq!(
            header.fragment_idx, 0xbeef,
            "generation rides in fragment_idx"
        );
        let batch =
            decode(&wire[HEADER_LEN..n], header.frame_id, header.fragment_idx).expect("decodes");
        assert_eq!(batch.generation, 0xbeef);
    }

    #[test]
    fn a_ring_stamps_every_packet_with_its_own_generation() {
        let ring = InputRing::with_generation(0x1234);
        assert_eq!(ring.generation(), 0x1234);
        // Two rings built the normal way are two client processes, and the
        // whole mechanism rests on them differing.
        assert_ne!(
            InputRing::new().generation(),
            InputRing::with_generation(InputRing::new().generation().wrapping_add(1)).generation()
        );
    }

    #[test]
    fn comparison_is_wrap_aware() {
        // v1 design §5.1 makes this mandatory on frame_id. A naive `>` would treat
        // every packet after the wrap as ancient and freeze input forever.
        let mut gate = SequenceGate::new();
        gate.admit(&batch(&[InputEvent::KeyDown(0x11)], u32::MAX - 1));
        assert_eq!(
            gate.admit(&batch(&[InputEvent::KeyUp(0x11)], u32::MAX))
                .len(),
            1
        );
        assert_eq!(gate.admit(&batch(&[InputEvent::KeyDown(0x1e)], 0)).len(), 1);
        assert_eq!(gate.admit(&batch(&[InputEvent::KeyUp(0x1e)], 1)).len(), 1);
    }

    #[test]
    fn motion_accumulates_into_one_event() {
        // v2 design §5.3: batching is applied to motion only, summing deltas.
        let mut b = MotionBatcher::new();
        b.accumulate(3.0, -1.0);
        b.accumulate(4.0, -2.0);
        let mut out = Vec::new();
        b.drain(&mut out);
        assert_eq!(out, vec![InputEvent::MouseMoveRel { dx: 7, dy: -3 }]);
        assert!(b.is_empty());
    }

    #[test]
    fn the_batcher_splits_rather_than_saturating() {
        // v2 design §5.3 (checked against Moonlight's source): it splits when
        // accumulated deltas exceed INT16.
        // Saturating would silently clamp a fast flick -- the camera stops
        // short and the player has no idea why.
        let mut b = MotionBatcher::new();
        b.accumulate(70_000.0, 0.0);
        let mut out = Vec::new();
        b.drain(&mut out);
        assert!(out.len() > 1, "must split into multiple events");
        let total: i64 = out
            .iter()
            .map(|e| match e {
                InputEvent::MouseMoveRel { dx, .. } => *dx as i64,
                _ => panic!("only relative motion expected"),
            })
            .sum();
        assert_eq!(total, 70_000, "no motion may be lost in the split");
    }

    #[test]
    fn sub_pixel_motion_is_carried_not_truncated() {
        // winit's DeviceEvent::MouseMotion deltas are f64 (v2 design §4.3). A slow
        // drag of 0.4px per event truncates to 0 every time, and the cursor
        // never moves at all. The residue must carry.
        let mut b = MotionBatcher::new();
        let mut out = Vec::new();
        for _ in 0..10 {
            b.accumulate(0.4, 0.0);
            b.drain(&mut out);
        }
        let total: i64 = out
            .iter()
            .map(|e| match e {
                InputEvent::MouseMoveRel { dx, .. } => *dx as i64,
                _ => 0,
            })
            .sum();
        assert_eq!(total, 4, "ten 0.4px steps must move four pixels");
    }

    #[test]
    fn draining_nothing_produces_nothing() {
        let mut b = MotionBatcher::new();
        let mut out = Vec::new();
        b.drain(&mut out);
        assert!(out.is_empty(), "an empty batcher must not emit a zero move");
    }

    #[test]
    fn the_repeat_schedule_is_exponential_and_finite() {
        // v2 design §5.4: 1, 2, 4, 8, 16, 32, 64, 128 ms as INTER-SEND gaps, so the
        // last copy leaves 255ms after the original. The backoff is the
        // load-bearing part: eight copies packed into 8ms all fit inside one
        // Wi-Fi loss burst; the same eight spread over 255ms do not.
        let mut s = RepeatSchedule::new();
        let delays: Vec<u64> = std::iter::from_fn(|| s.next_delay())
            .map(|d| d.as_millis() as u64)
            .collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 64, 128]);
        assert_eq!(delays.len(), REPEAT_COUNT);
        assert_eq!(delays.iter().sum::<u64>(), 255);
        assert_eq!(s.next_delay(), None, "the schedule must terminate");
    }

    #[test]
    fn resetting_rearms_the_schedule() {
        // Any new event cancels the schedule, since the fresh packet carries
        // the same ring (v2 design §5.4).
        let mut s = RepeatSchedule::new();
        s.next_delay();
        s.next_delay();
        s.reset();
        assert_eq!(s.next_delay(), Some(Duration::from_millis(1)));
    }

    use proptest::prelude::*;

    proptest! {
        /// Under arbitrary drop, reorder and duplication of input packets, the
        /// sequence of events the receiver APPLIES equals the lossless
        /// sequence, truncated at whatever the losses made unrecoverable.
        ///
        /// "Truncated", not "equal": if more than 8 consecutive packets are
        /// lost the events between are genuinely gone, and no gate can invent
        /// them. What must never happen is applying an event twice, applying
        /// them out of order, or applying one the sender never sent.
        #[test]
        fn applied_events_are_a_subsequence_of_the_sent_ones(
            sent in prop::collection::vec(0u16..8, 1..60),
            drops in prop::collection::vec(any::<bool>(), 60),
            dups in prop::collection::vec(any::<bool>(), 60),
        ) {
            let events: Vec<InputEvent> = sent.iter().map(|&k| InputEvent::KeyDown(k)).collect();

            // Sender: one packet per event, each carrying the last 8.
            let mut packets = Vec::new();
            for (i, _) in events.iter().enumerate() {
                let lo = i.saturating_sub(MAX_EVENTS_PER_PACKET - 1);
                packets.push((events[lo..=i].to_vec(), i as u32));
            }

            // Network: drop some, duplicate some.
            let mut on_wire = Vec::new();
            for (i, p) in packets.iter().enumerate() {
                if *drops.get(i).unwrap_or(&false) {
                    continue;
                }
                on_wire.push(p.clone());
                if *dups.get(i).unwrap_or(&false) {
                    on_wire.push(p.clone());
                }
            }

            let mut gate = SequenceGate::new();
            let mut applied = Vec::new();
            for (evs, seq) in &on_wire {
                let b = batch(evs, *seq);
                applied.extend_from_slice(gate.admit(&b));
            }

            // Every applied event appears in the sent stream, in order, once.
            let mut it = events.iter();
            for a in &applied {
                prop_assert!(
                    it.by_ref().any(|e| e == a),
                    "applied an event that was never sent, or applied out of order"
                );
            }
        }
    }
}
