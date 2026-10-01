//! Screen text: what the host's accessibility tree says is on an agent's
//! screen -- the front window's controls with their roles, labels and
//! places -- for models that read text (Jev) and for models that read
//! pictures, beside the screenshot. Only an agent's session asks for it.
//!
//! Control packets (`kind = 3`) with opcodes of their own, which peers that
//! predate them ignore (`Control::decode` refuses them, as it does the
//! clipboard's):
//!
//! ```text
//! Request  op u8 | id u32 | what u8 | x u16 | y u16              agent -> host
//! Part     op u8 | id u32 | total u32 | offset u32 | bytes       host -> agent
//! ```
//!
//! A reply is one encoded [`ScreenText`] cut into parts of [`PART`] bytes,
//! sent at once. There is no ack: a reply is small (at most [`MAX_REPLY`]),
//! and an agent missing a part asks again under the same id, which costs the
//! host one more read of the tree. The clipboard's selective acks
//! (`clip`) are for transfers of megabytes; this is a few dozen datagrams.
//!
//! Places are in stream pixels -- the screenshot's and the clicks' space --
//! so the host converts from its own coordinates before it answers.

use crate::header::{Header, Kind};
use crate::HEADER_LEN;

const OP_REQUEST: u8 = 48;
const OP_PART: u8 = 49;

/// Reply bytes per part: a datagram of at most `MAX_DATAGRAM`.
pub const PART: usize = 1152;
const PART_HEADER: usize = 13;
const _: () = assert!(HEADER_LEN + PART_HEADER + PART <= crate::MAX_DATAGRAM);

/// The largest reply: 114 parts. What a busy window (a mail client, a
/// spreadsheet) lists within `MAX_ELEMENTS` fits; the encoder stops adding
/// elements rather than pass it.
pub const MAX_REPLY: usize = 128 * 1024;
/// Elements in one reply at most. A model reads a few hundred labelled
/// controls well; past that it is mostly a list's rows, cut anyway.
pub const MAX_ELEMENTS: usize = 500;
/// Bytes of one label at most (cut at a character boundary): a control's
/// name, not a document's text.
pub const MAX_LABEL: usize = 200;
/// Bytes of the app's name, the window's title and the note at most.
pub const MAX_NAME: usize = 200;

const VERSION: u8 = 1;

/// Whether a control body is screen text's.
pub fn is_screen(body: &[u8]) -> bool {
    matches!(body.first(), Some(&(OP_REQUEST | OP_PART)))
}

/// What the agent asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// The front window's elements.
    Window,
    /// The element at this point (stream pixels), then each of its parents
    /// up to its window: what a click there would press.
    At { x: u16, y: u16 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    Request {
        id: u32,
        query: Query,
    },
    Part {
        id: u32,
        total: u32,
        offset: u32,
        bytes: &'a [u8],
    },
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Hostile input: anything malformed is `None`.
pub fn decode(body: &[u8]) -> Option<Msg<'_>> {
    let id = u32_at(body, 1)?;
    match *body.first()? {
        OP_REQUEST => {
            let query = match *body.get(5)? {
                0 => Query::Window,
                1 => Query::At {
                    x: u16_at(body, 6)?,
                    y: u16_at(body, 8)?,
                },
                _ => return None,
            };
            Some(Msg::Request { id, query })
        }
        OP_PART => {
            let (total, offset) = (u32_at(body, 5)?, u32_at(body, 9)?);
            let bytes = &body[PART_HEADER..];
            let (t, o) = (total as usize, offset as usize);
            // Every part but the last is whole: a part says exactly where
            // it belongs, so the receiver needs no other bookkeeping.
            if t == 0 || t > MAX_REPLY || !o.is_multiple_of(PART) || o >= t {
                return None;
            }
            (bytes.len() == (t - o).min(PART)).then_some(Msg::Part {
                id,
                total,
                offset,
                bytes,
            })
        }
        _ => None,
    }
}

/// A whole control packet (header and body) around `body`.
fn packet(body: &[u8]) -> Vec<u8> {
    let len = HEADER_LEN + body.len();
    let header = Header {
        keyframe: false,
        recovery: false,
        lan_shards: false,
        frame_end: false,
        kind: Kind::Control,
        total_len: len as u16,
        fragment_idx: 0,
        data_shards: 0,
        parity_shards: 0,
        fec_block_idx: 0,
        frame_len: 0,
        capture_ts_us: crate::clock::now_us(),
        frame_id: 0,
    };
    let mut out = vec![0u8; len];
    let mut h = [0u8; HEADER_LEN];
    header
        .encode(&mut h)
        .expect("control header is always valid");
    out[..HEADER_LEN].copy_from_slice(&h);
    out[HEADER_LEN..].copy_from_slice(body);
    out
}

pub fn request_packet(id: u32, query: Query) -> Vec<u8> {
    let mut b = vec![OP_REQUEST];
    b.extend_from_slice(&id.to_le_bytes());
    match query {
        Query::Window => b.extend_from_slice(&[0, 0, 0, 0, 0]),
        Query::At { x, y } => {
            b.push(1);
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
        }
    }
    packet(&b)
}

/// The packets that carry `text` as the reply to request `id`.
pub fn reply_packets(id: u32, text: &ScreenText) -> Vec<Vec<u8>> {
    let data = text.encode();
    let total = data.len() as u32;
    data.chunks(PART)
        .enumerate()
        .map(|(i, bytes)| {
            let mut b = Vec::with_capacity(PART_HEADER + bytes.len());
            b.push(OP_PART);
            for v in [id, total, (i * PART) as u32] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(bytes);
            packet(&b)
        })
        .collect()
}

/// Receiving one reply, part by part.
pub struct Assembly {
    pub id: u32,
    data: Vec<u8>,
    have: Vec<bool>,
    missing: usize,
}

impl Assembly {
    /// None when it is too large to take.
    pub fn new(id: u32, total: u32) -> Option<Assembly> {
        let total = total as usize;
        if total == 0 || total > MAX_REPLY {
            return None;
        }
        let parts = total.div_ceil(PART);
        Some(Assembly {
            id,
            data: vec![0; total],
            have: vec![false; parts],
            missing: parts,
        })
    }

    /// Take a part; false if it does not belong (another size).
    pub fn add(&mut self, total: u32, offset: u32, bytes: &[u8]) -> bool {
        if total as usize != self.data.len() {
            return false;
        }
        let start = offset as usize;
        let i = start / PART;
        if !start.is_multiple_of(PART) || i >= self.have.len() {
            return false;
        }
        let Some(slot) = self.data.get_mut(start..start + bytes.len()) else {
            return false;
        };
        if !self.have[i] {
            slot.copy_from_slice(bytes);
            self.have[i] = true;
            self.missing -= 1;
        }
        true
    }

    pub fn complete(&self) -> bool {
        self.missing == 0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

/// What kind of thing an element is: a small closed set every platform's
/// roles map onto (AX roles, UI Automation control types, AT-SPI roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Other = 0,
    Button = 1,
    Link = 2,
    MenuItem = 3,
    Tab = 4,
    CheckBox = 5,
    Radio = 6,
    Switch = 7,
    TextField = 8,
    TextArea = 9,
    ComboBox = 10,
    Slider = 11,
    ListItem = 12,
    Row = 13,
    Cell = 14,
    TreeItem = 15,
    Heading = 16,
    Text = 17,
    Image = 18,
    Group = 19,
    Toolbar = 20,
    Dialog = 21,
    Window = 22,
    Menu = 23,
    MenuBar = 24,
    ScrollBar = 25,
    ProgressBar = 26,
    Table = 27,
    List = 28,
}

impl Role {
    const ALL: [Role; 29] = [
        Role::Other,
        Role::Button,
        Role::Link,
        Role::MenuItem,
        Role::Tab,
        Role::CheckBox,
        Role::Radio,
        Role::Switch,
        Role::TextField,
        Role::TextArea,
        Role::ComboBox,
        Role::Slider,
        Role::ListItem,
        Role::Row,
        Role::Cell,
        Role::TreeItem,
        Role::Heading,
        Role::Text,
        Role::Image,
        Role::Group,
        Role::Toolbar,
        Role::Dialog,
        Role::Window,
        Role::Menu,
        Role::MenuBar,
        Role::ScrollBar,
        Role::ProgressBar,
        Role::Table,
        Role::List,
    ];

    /// Unknown codes (a newer host's roles) are `Other`.
    pub fn from_code(c: u8) -> Role {
        Role::ALL.get(c as usize).copied().unwrap_or(Role::Other)
    }

    /// In a person's words, for a model to read.
    pub fn name(self) -> &'static str {
        match self {
            Role::Other => "element",
            Role::Button => "button",
            Role::Link => "link",
            Role::MenuItem => "menu item",
            Role::Tab => "tab",
            Role::CheckBox => "checkbox",
            Role::Radio => "radio button",
            Role::Switch => "switch",
            Role::TextField => "text field",
            Role::TextArea => "text area",
            Role::ComboBox => "pop-up menu",
            Role::Slider => "slider",
            Role::ListItem => "list item",
            Role::Row => "row",
            Role::Cell => "cell",
            Role::TreeItem => "tree item",
            Role::Heading => "heading",
            Role::Text => "text",
            Role::Image => "image",
            Role::Group => "group",
            Role::Toolbar => "toolbar",
            Role::Dialog => "dialog",
            Role::Window => "window",
            Role::Menu => "menu",
            Role::MenuBar => "menu bar",
            Role::ScrollBar => "scroll bar",
            Role::ProgressBar => "progress bar",
            Role::Table => "table",
            Role::List => "list",
        }
    }

    /// Something a click does something to (not text, a picture or a box
    /// around others).
    pub fn is_control(self) -> bool {
        !matches!(
            self,
            Role::Other
                | Role::Text
                | Role::Image
                | Role::Group
                | Role::Toolbar
                | Role::Dialog
                | Role::Window
                | Role::Heading
                | Role::Table
                | Role::List
                | Role::ProgressBar
        )
    }
}

/// `Element::flags`.
pub mod flags {
    /// It has the keyboard focus.
    pub const FOCUSED: u8 = 1;
    /// It is greyed out: a click does nothing.
    pub const DISABLED: u8 = 2;
    /// A password field: its value is never read.
    pub const SECRET: u8 = 4;
    /// Checked, selected or on.
    pub const ON: u8 = 8;
}

/// One thing on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub role: Role,
    /// `flags::*`.
    pub flags: u8,
    /// What it says or is called. Never a text field's contents.
    pub label: String,
    /// Its box, in stream pixels.
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// For `Query::Window`, how deep under the window it sits (0: one of
    /// the window's own children). For `Query::At`, 0 is the element at the
    /// point and each next one is the parent of the one before.
    pub depth: u8,
}

impl Element {
    pub fn center(&self) -> (u16, u16) {
        (
            self.x.saturating_add(self.w / 2),
            self.y.saturating_add(self.h / 2),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Status {
    #[default]
    Ok = 0,
    /// The host cannot read its tree here (no permission, a platform or
    /// session without one, a secure screen): `note` says why.
    Unavailable = 1,
}

/// What the host's accessibility tree says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenText {
    pub status: Status,
    /// Why it is unavailable, or what to know about it ("the walk stopped
    /// at its time limit").
    pub note: String,
    /// The front app and its window's title.
    pub app: String,
    pub window: String,
    pub elements: Vec<Element>,
    /// More was on screen than `elements` holds.
    pub truncated: bool,
}

impl ScreenText {
    pub fn unavailable(note: impl Into<String>) -> ScreenText {
        ScreenText {
            status: Status::Unavailable,
            note: note.into(),
            ..ScreenText::default()
        }
    }

    /// Bytes, within `MAX_REPLY`: elements that would not fit are left out
    /// (and `truncated` set).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION, self.status as u8, 0];
        for s in [&self.note, &self.app, &self.window] {
            put_str(&mut out, s, MAX_NAME);
        }
        let count_at = out.len();
        out.extend_from_slice(&[0, 0]);
        let mut count = 0u16;
        let mut truncated = self.truncated;
        for e in &self.elements {
            if count as usize >= MAX_ELEMENTS
                || out.len() + 13 + e.label.len().min(MAX_LABEL) > MAX_REPLY
            {
                truncated = true;
                break;
            }
            out.extend_from_slice(&[e.role as u8, e.flags, e.depth]);
            for v in [e.x, e.y, e.w, e.h] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            put_str(&mut out, &e.label, MAX_LABEL);
            count += 1;
        }
        out[count_at..count_at + 2].copy_from_slice(&count.to_le_bytes());
        out[2] = truncated as u8;
        out
    }

    /// Hostile input: anything malformed is `None`.
    pub fn decode(b: &[u8]) -> Option<ScreenText> {
        if b.len() > MAX_REPLY || *b.first()? != VERSION {
            return None;
        }
        let status = match *b.get(1)? {
            0 => Status::Ok,
            1 => Status::Unavailable,
            _ => return None,
        };
        let truncated = *b.get(2)? != 0;
        let mut at = 3;
        let note = get_str(b, &mut at, MAX_NAME)?;
        let app = get_str(b, &mut at, MAX_NAME)?;
        let window = get_str(b, &mut at, MAX_NAME)?;
        let count = u16_at(b, at)? as usize;
        at += 2;
        if count > MAX_ELEMENTS {
            return None;
        }
        let mut elements = Vec::with_capacity(count);
        for _ in 0..count {
            let fixed = b.get(at..at + 11)?;
            at += 11;
            let n = |i: usize| u16::from_le_bytes([fixed[i], fixed[i + 1]]);
            let (x, y, w, h) = (n(3), n(5), n(7), n(9));
            elements.push(Element {
                role: Role::from_code(fixed[0]),
                flags: fixed[1],
                depth: fixed[2],
                x,
                y,
                w,
                h,
                label: get_str(b, &mut at, MAX_LABEL)?,
            });
        }
        (at == b.len()).then_some(ScreenText {
            status,
            note,
            app,
            window,
            elements,
            truncated,
        })
    }
}

/// `s`, cut to `max` bytes at a character boundary.
pub fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn put_str(out: &mut Vec<u8>, s: &str, max: usize) {
    let s = cut(s, max);
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn get_str(b: &[u8], at: &mut usize, max: usize) -> Option<String> {
    let n = u16_at(b, *at)? as usize;
    if n > max {
        return None;
    }
    let s = std::str::from_utf8(b.get(*at + 2..*at + 2 + n)?).ok()?;
    *at += 2 + n;
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(p: &[u8]) -> &[u8] {
        let h = Header::decode(p).unwrap();
        &p[HEADER_LEN..h.total_len as usize]
    }

    fn element(label: &str, role: Role) -> Element {
        Element {
            role,
            flags: flags::FOCUSED,
            label: label.into(),
            x: 10,
            y: 20,
            w: 80,
            h: 24,
            depth: 1,
        }
    }

    fn sample(n: usize) -> ScreenText {
        ScreenText {
            status: Status::Ok,
            note: String::new(),
            app: "Mail".into(),
            window: "Inbox – 3 unread".into(),
            elements: (0..n)
                .map(|i| element(&format!("Message {i} from Zoë"), Role::Row))
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn requests_round_trip_and_are_not_control_or_clipboard() {
        for q in [Query::Window, Query::At { x: 640, y: 799 }] {
            let p = request_packet(7, q);
            let b = body(&p);
            assert!(is_screen(b));
            assert!(!crate::clip::is_clip(b));
            assert!(crate::control::Control::decode(b).is_none());
            assert_eq!(decode(b), Some(Msg::Request { id: 7, query: q }));
        }
    }

    #[test]
    fn a_reply_comes_back_whole_from_its_parts_in_any_order() {
        let text = sample(300);
        let mut parts = reply_packets(9, &text);
        assert!(parts.len() > 1);
        parts.reverse();
        let mut got: Option<Assembly> = None;
        for p in &parts {
            let Some(Msg::Part {
                id,
                total,
                offset,
                bytes,
            }) = decode(body(p))
            else {
                panic!("a part")
            };
            let a = got.get_or_insert_with(|| Assembly::new(id, total).unwrap());
            assert!(a.add(total, offset, bytes));
            // A repeat changes nothing.
            assert!(a.add(total, offset, bytes));
        }
        let a = got.unwrap();
        assert!(a.complete());
        assert_eq!(ScreenText::decode(&a.into_bytes()), Some(text));
    }

    #[test]
    fn a_reply_missing_a_part_is_not_complete() {
        let text = sample(100);
        let parts = reply_packets(1, &text);
        let mut a = None;
        for p in parts.iter().skip(1) {
            if let Some(Msg::Part {
                id,
                total,
                offset,
                bytes,
            }) = decode(body(p))
            {
                a.get_or_insert_with(|| Assembly::new(id, total).unwrap())
                    .add(total, offset, bytes);
            }
        }
        assert!(!a.unwrap().complete());
    }

    #[test]
    fn a_huge_window_is_cut_to_fit_and_says_so() {
        let mut text = sample(MAX_ELEMENTS + 50);
        for e in &mut text.elements {
            e.label = "x".repeat(MAX_LABEL * 2);
        }
        let bytes = text.encode();
        assert!(bytes.len() <= MAX_REPLY);
        let back = ScreenText::decode(&bytes).unwrap();
        assert!(back.truncated);
        assert!(back.elements.len() <= MAX_ELEMENTS);
        assert!(back.elements.iter().all(|e| e.label.len() == MAX_LABEL));
    }

    #[test]
    fn labels_are_cut_at_a_character_boundary() {
        let mut text = sample(1);
        text.elements[0].label = "é".repeat(MAX_LABEL);
        let back = ScreenText::decode(&text.encode()).unwrap();
        assert!(back.elements[0].label.len() <= MAX_LABEL);
        assert!(back.elements[0].label.chars().all(|c| c == 'é'));
    }

    #[test]
    fn unavailable_says_why() {
        let t = ScreenText::unavailable("Pong lacks the Accessibility permission.");
        assert_eq!(ScreenText::decode(&t.encode()), Some(t));
    }

    #[test]
    fn malformed_parts_are_refused() {
        let ok = |total: u32, offset: u32, len: usize| {
            let mut b = vec![OP_PART];
            for v in [1u32, total, offset] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend(std::iter::repeat_n(0u8, len));
            decode(&b).is_some()
        };
        assert!(ok(10, 0, 10));
        assert!(ok(PART as u32 + 5, PART as u32, 5));
        assert!(!ok(0, 0, 0), "an empty reply");
        assert!(!ok(10, 0, 9), "short");
        assert!(!ok(10, 0, 11), "long");
        assert!(!ok(PART as u32 * 2, 5, PART), "an offset off the grid");
        assert!(!ok(PART as u32, PART as u32, 0), "past the end");
        assert!(!ok(MAX_REPLY as u32 + 1, 0, PART), "too large");
        assert!(
            decode(&[OP_REQUEST, 1, 0, 0, 0, 2]).is_none(),
            "unknown query"
        );
        assert!(
            decode(&[OP_REQUEST, 1, 0, 0, 0, 1, 5]).is_none(),
            "short point"
        );
        assert!(decode(&[]).is_none());
    }

    #[test]
    fn malformed_screen_text_is_refused() {
        let good = sample(3).encode();
        assert!(ScreenText::decode(&good).is_some());
        for cut in 0..good.len() {
            assert!(ScreenText::decode(&good[..cut]).is_none(), "cut at {cut}");
        }
        let mut longer = good.clone();
        longer.push(0);
        assert!(ScreenText::decode(&longer).is_none(), "trailing bytes");
        let mut bad_utf8 = sample(1).encode();
        let last = bad_utf8.len() - 1;
        bad_utf8[last] = 0xff;
        assert!(ScreenText::decode(&bad_utf8).is_none());
        let mut version = good;
        version[0] = 9;
        assert!(ScreenText::decode(&version).is_none());
    }

    #[test]
    fn unknown_roles_read_as_other() {
        assert_eq!(Role::from_code(200), Role::Other);
        for (i, r) in Role::ALL.iter().enumerate() {
            assert_eq!(*r as u8 as usize, i);
            assert_eq!(Role::from_code(i as u8), *r);
        }
    }
}
