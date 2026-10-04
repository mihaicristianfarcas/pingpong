//! Control messages (`kind = 3`): session setup, loss recovery, cursor state,
//! latency probes. Body layouts are fixed per opcode, little-endian.
//!
//! Control has no FEC and no transport retransmit. Each message is either
//! retransmitted by its sender until acknowledged (`SessionStart`), naturally
//! repeated (`CursorState` heartbeat, `LossReport`), or re-issued by the loss
//! state machine if its effect does not arrive (`InvalidateRefs`, `RequestIdr`).

use crate::gamepad::GamepadState;
use crate::header::{Header, Kind};
use crate::HEADER_LEN;

pub const MAX_BODY_LEN: usize = 72;
pub const MAX_CONTROL_LEN: usize = HEADER_LEN + MAX_BODY_LEN;

/// Video codecs, as a bitmask in `SessionStart` and a single bit in the ack.
pub mod codec {
    pub const H264: u8 = 1;
    pub const HEVC: u8 = 2;
    pub const AV1: u8 = 4;
}

/// `SessionStart::flags`.
pub mod flags {
    // 1 is retired: sessions always stream a virtual display.
    /// Keep the host's own monitors on during the session (Apollo's default is
    /// to turn them off so the virtual display is the whole desktop).
    pub const KEEP_HOST_DISPLAYS: u8 = 2;
    /// Play audio on the host as well as streaming it.
    pub const HOST_AUDIO: u8 = 4;
    /// Join the session an AI agent is running, as a viewer: the host sends
    /// this client the same picture, and takes its input only after it takes
    /// over (`Control::AgentControl`). The size is the agent's, in the ack.
    pub const WATCH: u8 = 8;
    /// Share the clipboard both ways (`clip`), if the host allows it: its
    /// ack says (`features::CLIPBOARD`).
    pub const CLIPBOARD: u8 = 16;
    /// The client reassembles video of `LAN_PAYLOAD_LEN` shards (the
    /// header's `lan_shards`): the host may send them while it reaches the
    /// client on the local network.
    pub const LAN_SHARDS: u8 = 32;
}

/// `SessionAck::features`: what the host does for this session beyond the
/// stream.
pub mod features {
    /// The clipboard is shared both ways (`clip`).
    pub const CLIPBOARD: u8 = 1;
    /// Video comes in `LAN_PAYLOAD_LEN` shards while the client is on the
    /// host's local network.
    pub const LAN_SHARDS: u8 = 2;
    /// The pointer is in the picture, drawn by the host where the host's
    /// desktop shows it, as Apollo draws it (`display_vram.cpp`). The client
    /// draws none, and the mouse mode is the user's: relative motion unless
    /// they switch (Moonlight's default). Hosts without it leave drawing the
    /// pointer to the client and steer its mode with `CursorState`.
    pub const POINTER_IN_PICTURE: u8 = 4;
}

/// Client -> host: start (or re-describe) the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStart {
    pub width: u16,
    pub height: u16,
    /// Millihertz: fractional refresh rates (59.94) are real.
    pub refresh_mhz: u32,
    pub bitrate_kbps: u32,
    /// `codec::*` bitmask, preference by capability: the host picks the best
    /// it can encode among these (AV1 > HEVC > H.264).
    pub codecs: u8,
    /// Audio channels: 0 = none, 2 = stereo, 6 = 5.1, 8 = 7.1.
    pub audio_channels: u8,
    pub flags: u8,
    /// Slices per frame the client wants (decode parallelism).
    pub slices: u8,
    /// Random per client launch. A changed nonce is a new session even with
    /// identical parameters; an unchanged one makes a retransmit a no-op.
    pub nonce: u32,
    /// What to show: `app::DESKTOP`, or an app the host starts for the
    /// session (Apollo's apps). Absent on the wire from older clients.
    pub app: u8,
    /// The client's keyboard repeat: a held key repeats after this many
    /// milliseconds, then every `repeat_interval_ms`. The host makes the
    /// repeats where its system does not (`pingpong_input::repeat`); 0 =
    /// unknown, the host's own settings. Absent from older clients.
    pub repeat_delay_ms: u16,
    pub repeat_interval_ms: u16,
    /// `video::*` the client can show and asks for. Absent from older
    /// clients (none).
    pub video: u8,
}

/// `SessionStart::video` (what the client can show and asks for) and
/// `SessionAck::video` (what the stream is).
pub mod video {
    /// High dynamic range: 10-bit, BT.2020 primaries, the PQ curve (HDR10),
    /// as Moonlight asks Sunshine for it. Asked for by a client whose display
    /// shows HDR; in the ack when the stream is (the host's display then
    /// runs in HDR, and `Control::HdrMetadata` follows).
    pub const HDR: u8 = 1;
    /// Full-resolution colour (4:4:4): text without coloured fringes, as
    /// Moonlight's "YUV 4:4:4" asks Sunshine for.
    pub const YUV444: u8 = 2;
}

/// Host -> client, about once a second while the stream is HDR: the colour
/// volume of the host's display (SMPTE ST 2086) and the light levels of what
/// it shows (CTA-861.3), Sunshine's `SS_HDR_METADATA`, for the client to map
/// the picture onto its own display; and where SDR white sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HdrMetadata {
    /// Red, green and blue primaries, CIE 1931 x and y in units of 0.00002.
    pub primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    /// The display's peak, cd/m².
    pub max_luminance: u16,
    /// Its black, in 0.0001 cd/m².
    pub min_luminance: u16,
    /// The brightest pixel and the brightest frame average, cd/m² (0 =
    /// unknown).
    pub max_cll: u16,
    pub max_fall: u16,
    /// SDR white in the picture, cd/m² (Windows' "SDR content brightness";
    /// 0 = unknown, BT.2408's 203).
    pub sdr_white: u16,
}

impl HdrMetadata {
    /// BT.2020 primaries and D65, a display of `max_luminance` cd/m² with
    /// SDR white at `sdr_white`: what a host says when its display reports
    /// nothing better.
    pub fn bt2020(max_luminance: u16, sdr_white: u16) -> HdrMetadata {
        HdrMetadata {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
            white_point: [15635, 16450],
            max_luminance,
            min_luminance: 50,
            max_cll: 0,
            max_fall: 0,
            sdr_white,
        }
    }
}

/// Apps a session can start with (Apollo's defaults).
pub mod app {
    pub const DESKTOP: u8 = 0;
    pub const STEAM_BIG_PICTURE: u8 = 1;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AckStatus {
    Ok = 0,
    /// The mode could not be set; `width`/`height`/`refresh_mhz` are what was.
    ModeUnsupported = 1,
    VddUnavailable = 2,
    /// Another client is streaming and this one may not take over.
    Busy = 3,
    /// No codec in the request can be encoded here.
    NoCodec = 4,
    /// The host failed to start the stream (capture/encoder error).
    Failed = 5,
    /// A watch request (`flags::WATCH`) with no agent session to watch.
    NothingToWatch = 6,
    /// An agent's request the host does not allow: agents are off for this
    /// identity, or a person is streaming (agents never take over people).
    AgentNotAllowed = 7,
}

impl AckStatus {
    fn from_code(c: u8) -> Option<AckStatus> {
        Some(match c {
            0 => AckStatus::Ok,
            1 => AckStatus::ModeUnsupported,
            2 => AckStatus::VddUnavailable,
            3 => AckStatus::Busy,
            4 => AckStatus::NoCodec,
            5 => AckStatus::Failed,
            6 => AckStatus::NothingToWatch,
            7 => AckStatus::AgentNotAllowed,
            _ => return None,
        })
    }
}

/// Host -> client: what the session actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAck {
    pub status: AckStatus,
    /// The single `codec::*` bit chosen.
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    /// The rate frames will arrive at (may be below the display's refresh if
    /// the host caps it).
    pub refresh_mhz: u32,
    pub bitrate_kbps: u32,
    pub audio_channels: u8,
    /// Echo of the request's nonce.
    pub nonce: u32,
    /// What the host runs (`host::*`), so the client can fit its keys to it
    /// (Command is Command on a Mac). Absent on the wire from older hosts.
    pub host: u8,
    /// `features::*`. Absent on the wire from older hosts (none).
    pub features: u8,
    /// `video::*`: what the stream is. Absent from older hosts (none).
    pub video: u8,
}

/// The host's operating system, in `SessionAck`.
pub mod host {
    pub const WINDOWS: u8 = 0;
    pub const MACOS: u8 = 1;
    pub const LINUX: u8 = 2;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EndReason {
    /// The user quit.
    Quit = 0,
    /// The host has no session for this client (answer to a message that
    /// presupposes one). The client should start a new one.
    NoSession = 1,
    /// Another client took over.
    Replaced = 2,
    /// The host is shutting down.
    Shutdown = 3,
    /// Something failed on the host mid-session.
    Error = 4,
    /// Someone at the host is using it (an agent's session, on a host whose
    /// own monitors it had turned off).
    HostInUse = 5,
}

impl EndReason {
    fn from_code(c: u8) -> EndReason {
        match c {
            0 => EndReason::Quit,
            1 => EndReason::NoSession,
            2 => EndReason::Replaced,
            3 => EndReason::Shutdown,
            5 => EndReason::HostInUse,
            _ => EndReason::Error,
        }
    }
}

/// Pointer shapes the client maps onto its own cursors. A small closed set:
/// what `LoadCursorW` defines. Unknown codes become `Arrow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CursorShape {
    Arrow = 0,
    IBeam = 1,
    Wait = 2,
    Cross = 3,
    SizeAll = 4,
    SizeNS = 5,
    SizeWE = 6,
    SizeNWSE = 7,
    SizeNESW = 8,
    Hand = 9,
    No = 10,
    Help = 11,
    AppStarting = 12,
}

impl CursorShape {
    pub fn from_code(c: u8) -> CursorShape {
        match c {
            1 => CursorShape::IBeam,
            2 => CursorShape::Wait,
            3 => CursorShape::Cross,
            4 => CursorShape::SizeAll,
            5 => CursorShape::SizeNS,
            6 => CursorShape::SizeWE,
            7 => CursorShape::SizeNWSE,
            8 => CursorShape::SizeNESW,
            9 => CursorShape::Hand,
            10 => CursorShape::No,
            11 => CursorShape::Help,
            12 => CursorShape::AppStarting,
            _ => CursorShape::Arrow,
        }
    }
}

/// What the host's foreground application is doing with the pointer, from a
/// host that leaves drawing it to the client: hidden or clipped means the
/// app has taken the mouse (lock, send relative); otherwise draw this shape
/// and send absolute. A client that the ack tells
/// `features::POINTER_IN_PICTURE` ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub visible: bool,
    pub clipped: bool,
    pub shape: CursorShape,
    /// Host pointer position in stream pixels, for resync when the host moves
    /// the pointer by itself.
    pub x: u16,
    pub y: u16,
}

impl CursorState {
    /// What a host that draws the pointer into the picture tells a client
    /// that does not know `features::POINTER_IN_PICTURE`: draw no pointer,
    /// send relative motion.
    pub const IN_PICTURE: CursorState = CursorState {
        visible: false,
        clipped: false,
        shape: CursorShape::Arrow,
        x: 0,
        y: 0,
    };
}

/// Client -> host, about once a second: what arrived. The host's bitrate
/// controller acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LossReport {
    /// Video datagrams received / expected over the interval (expected counts
    /// every shard of every frame seen, recovered or not).
    pub received: u32,
    pub expected: u32,
    /// Frames that could not be reconstructed, over the interval.
    pub frames_lost: u16,
    /// Frames completed.
    pub frames_ok: u16,
    /// Client-measured RTT, microseconds (0 = unknown).
    pub rtt_us: u32,
    /// Video received over the interval, on the wire, kbit/s (0 = unknown):
    /// what the path actually carried, which congestion control cuts to.
    pub received_kbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    SessionStart(SessionStart),
    SessionAck(SessionAck),
    SessionEnd(EndReason),
    /// Client -> host: send an IDR.
    RequestIdr,
    /// Client -> host: frames `first..=last` never arrived; stop predicting
    /// from them. The host answers with a recovery frame, or an IDR if it can't.
    InvalidateRefs {
        first: u32,
        last: u32,
    },
    CursorState(CursorState),
    LossReport(LossReport),
    /// Latency probe; the peer echoes it as `Pong` immediately.
    Ping {
        id: u32,
        sent_us: u32,
    },
    Pong {
        id: u32,
        sent_us: u32,
    },
    /// Client -> host: one controller's whole state.
    Gamepad(GamepadState),
    /// Host -> client: what the game asked a controller's motors to do
    /// (0..=65535 each; low is the heavy left motor, high the light right).
    Rumble {
        index: u8,
        low: u16,
        high: u16,
    },
    /// Host -> client, over the authenticated tunnel: how to find the host
    /// across the internet (its rendezvous key and record secret). Upgrades
    /// pairings made before pairing exchanged them.
    RendezvousOffer {
        key: [u8; 32],
        secret: [u8; 32],
    },
    /// Client -> host, in answer: the client's rendezvous key.
    RendezvousKey {
        key: [u8; 32],
    },
    /// Host -> client: something about the host the user should know
    /// (`host_warning::*`), shown while streaming. Clients that do not know
    /// it ignore it.
    HostWarning(u8),
    /// Host -> client, while a session starts: the step it is on
    /// (`progress::*`), for the client to show until the first frame.
    Progress(u8),
    /// Watcher -> host: take over from the agent, hand back, pause, resume
    /// or stop it (`agent_control::*`).
    AgentControl(u8),
    /// Host -> the agent and its watchers, about once a second and on every
    /// change: who has the keyboard and mouse, and why.
    AgentState(AgentState),
    /// Agent -> host: what it is doing, in a few words, for the host's
    /// activity log ("left_click (640, 360)").
    AgentNote(AgentNote),
    /// Host -> client: the HDR stream's metadata.
    HdrMetadata(HdrMetadata),
}

/// What `Control::AgentControl` asks.
pub mod agent_control {
    /// The watcher's keyboard and mouse drive the host; the agent's are held.
    pub const TAKE_OVER: u8 = 1;
    /// The agent drives again.
    pub const HAND_BACK: u8 = 2;
    /// Hold the agent's input without taking over.
    pub const PAUSE: u8 = 3;
    pub const RESUME: u8 = 4;
    /// End the agent's session.
    pub const STOP: u8 = 5;
}

/// `AgentState::flags`: why the agent's input is held, if it is.
pub mod agent_state {
    /// A watcher took over, or paused the agent.
    pub const PAUSED: u8 = 1;
    /// Someone at the host used its own keyboard or mouse just now.
    pub const LOCAL_INPUT: u8 = 2;
    /// The host shows a secure screen (sign-in, lock, UAC): a person's job.
    pub const SECURE_DESKTOP: u8 = 4;
    /// The agent may look but not act (its access on the host is view-only).
    pub const VIEW_ONLY: u8 = 8;
    /// A watcher has the keyboard and mouse.
    pub const TAKEN_OVER: u8 = 16;
    /// What holds the agent's input.
    pub const HELD: u8 = PAUSED | LOCAL_INPUT | SECURE_DESKTOP | VIEW_ONLY | TAKEN_OVER;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentState {
    /// `agent_state::*`.
    pub flags: u8,
    /// Clients watching the session.
    pub watchers: u8,
}

impl AgentState {
    /// The agent's input reaches the host.
    pub fn agent_may_act(&self) -> bool {
        self.flags & agent_state::HELD == 0
    }
}

/// A short UTF-8 note, cut at a character boundary to fit a datagram.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AgentNote {
    len: u8,
    bytes: [u8; AgentNote::MAX],
}

impl AgentNote {
    pub const MAX: usize = 62;

    pub fn new(text: &str) -> AgentNote {
        let mut end = text.len().min(Self::MAX);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; Self::MAX];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        AgentNote {
            len: end as u8,
            bytes,
        }
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

impl std::fmt::Debug for AgentNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// What `Control::Progress` reports.
pub mod progress {
    /// Making the virtual display (seconds on Windows).
    pub const DISPLAY: u8 = 1;
    /// Starting capture and the encoder.
    pub const ENCODER: u8 = 2;
}

/// What `Control::HostWarning` reports.
pub mod host_warning {
    /// The host cannot act on the client's keyboard and mouse (on a Mac: Pong
    /// lacks the Accessibility permission).
    pub const INPUT_BLOCKED: u8 = 1;
    /// The host cannot capture its screen (on a Mac: Pong lacks the Screen
    /// Recording permission). Sent before the refusal it explains.
    pub const CAPTURE_BLOCKED: u8 = 2;
}

const OP_SESSION_START: u8 = 2;
const OP_SESSION_ACK: u8 = 3;
const OP_SESSION_END: u8 = 4;
const OP_CURSOR_STATE: u8 = 5;
const OP_REQUEST_IDR: u8 = 6;
const OP_INVALIDATE: u8 = 7;
const OP_LOSS_REPORT: u8 = 8;
const OP_PING: u8 = 9;
const OP_PONG: u8 = 10;
const OP_GAMEPAD: u8 = 11;
const OP_RUMBLE: u8 = 12;
const OP_RENDEZVOUS_OFFER: u8 = 13;
const OP_RENDEZVOUS_KEY: u8 = 14;
const OP_HOST_WARNING: u8 = 15;
const OP_PROGRESS: u8 = 16;
const OP_AGENT_CONTROL: u8 = 17;
const OP_AGENT_STATE: u8 = 18;
const OP_AGENT_NOTE: u8 = 19;
const OP_HDR_METADATA: u8 = 20;

struct W<'a> {
    b: &'a mut [u8],
    n: usize,
}

impl W<'_> {
    fn u8(&mut self, v: u8) {
        self.b[self.n] = v;
        self.n += 1;
    }
    fn u16(&mut self, v: u16) {
        self.b[self.n..self.n + 2].copy_from_slice(&v.to_le_bytes());
        self.n += 2;
    }
    fn u32(&mut self, v: u32) {
        self.b[self.n..self.n + 4].copy_from_slice(&v.to_le_bytes());
        self.n += 4;
    }
    fn bytes(&mut self, v: &[u8]) {
        self.b[self.n..self.n + v.len()].copy_from_slice(v);
        self.n += v.len();
    }
}

struct R<'a> {
    b: &'a [u8],
    n: usize,
}

impl R<'_> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.n)?;
        self.n += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_le_bytes(self.b.get(self.n..self.n + 2)?.try_into().ok()?);
        self.n += 2;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.b.get(self.n..self.n + 4)?.try_into().ok()?);
        self.n += 4;
        Some(v)
    }
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let v: [u8; N] = self.b.get(self.n..self.n + N)?.try_into().ok()?;
        self.n += N;
        Some(v)
    }
}

impl Control {
    fn encode_body(&self, w: &mut W) {
        match *self {
            Control::SessionStart(s) => {
                w.u8(OP_SESSION_START);
                w.u16(s.width);
                w.u16(s.height);
                w.u32(s.refresh_mhz);
                w.u32(s.bitrate_kbps);
                w.u8(s.codecs);
                w.u8(s.audio_channels);
                w.u8(s.flags);
                w.u8(s.slices);
                w.u32(s.nonce);
                w.u8(s.app);
                w.u16(s.repeat_delay_ms);
                w.u16(s.repeat_interval_ms);
                w.u8(s.video);
            }
            Control::SessionAck(a) => {
                w.u8(OP_SESSION_ACK);
                w.u8(a.status as u8);
                w.u8(a.codec);
                w.u16(a.width);
                w.u16(a.height);
                w.u32(a.refresh_mhz);
                w.u32(a.bitrate_kbps);
                w.u8(a.audio_channels);
                w.u32(a.nonce);
                w.u8(a.host);
                w.u8(a.features);
                w.u8(a.video);
            }
            Control::SessionEnd(r) => {
                w.u8(OP_SESSION_END);
                w.u8(r as u8);
            }
            Control::CursorState(c) => {
                w.u8(OP_CURSOR_STATE);
                w.u8(c.visible as u8 | (c.clipped as u8) << 1);
                w.u8(c.shape as u8);
                w.u16(c.x);
                w.u16(c.y);
            }
            Control::RequestIdr => w.u8(OP_REQUEST_IDR),
            Control::InvalidateRefs { first, last } => {
                w.u8(OP_INVALIDATE);
                w.u32(first);
                w.u32(last);
            }
            Control::LossReport(l) => {
                w.u8(OP_LOSS_REPORT);
                w.u32(l.received);
                w.u32(l.expected);
                w.u16(l.frames_lost);
                w.u16(l.frames_ok);
                w.u32(l.rtt_us);
                w.u32(l.received_kbps);
            }
            Control::Ping { id, sent_us } => {
                w.u8(OP_PING);
                w.u32(id);
                w.u32(sent_us);
            }
            Control::Pong { id, sent_us } => {
                w.u8(OP_PONG);
                w.u32(id);
                w.u32(sent_us);
            }
            Control::Gamepad(g) => {
                w.u8(OP_GAMEPAD);
                w.u8(g.index);
                w.u16(g.seq);
                w.u8(u8::from(g.connected));
                w.u32(g.buttons);
                w.u8(g.left_trigger);
                w.u8(g.right_trigger);
                for v in [g.left_x, g.left_y, g.right_x, g.right_y] {
                    w.u16(v as u16);
                }
            }
            Control::Rumble { index, low, high } => {
                w.u8(OP_RUMBLE);
                w.u8(index);
                w.u16(low);
                w.u16(high);
            }
            Control::RendezvousOffer { key, secret } => {
                w.u8(OP_RENDEZVOUS_OFFER);
                w.bytes(&key);
                w.bytes(&secret);
            }
            Control::HostWarning(code) => {
                w.u8(OP_HOST_WARNING);
                w.u8(code);
            }
            Control::Progress(step) => {
                w.u8(OP_PROGRESS);
                w.u8(step);
            }
            Control::RendezvousKey { key } => {
                w.u8(OP_RENDEZVOUS_KEY);
                w.bytes(&key);
            }
            Control::AgentControl(op) => {
                w.u8(OP_AGENT_CONTROL);
                w.u8(op);
            }
            Control::AgentState(s) => {
                w.u8(OP_AGENT_STATE);
                w.u8(s.flags);
                w.u8(s.watchers);
            }
            Control::HdrMetadata(m) => {
                w.u8(OP_HDR_METADATA);
                for [x, y] in m.primaries {
                    w.u16(x);
                    w.u16(y);
                }
                w.u16(m.white_point[0]);
                w.u16(m.white_point[1]);
                w.u16(m.max_luminance);
                w.u16(m.min_luminance);
                w.u16(m.max_cll);
                w.u16(m.max_fall);
                w.u16(m.sdr_white);
            }
            Control::AgentNote(n) => {
                w.u8(OP_AGENT_NOTE);
                w.u8(n.len);
                w.bytes(&n.bytes[..n.len as usize]);
            }
        }
    }

    /// Encode header + body into `out`, returning the packet length.
    pub fn encode(&self, send_ts_us: u32, out: &mut [u8; MAX_CONTROL_LEN]) -> usize {
        let mut w = W {
            b: &mut out[HEADER_LEN..],
            n: 0,
        };
        self.encode_body(&mut w);
        let len = HEADER_LEN + w.n;
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
            capture_ts_us: send_ts_us,
            frame_id: 0,
        };
        let mut h = [0u8; HEADER_LEN];
        header
            .encode(&mut h)
            .expect("control header is always valid");
        out[..HEADER_LEN].copy_from_slice(&h);
        len
    }

    /// Decode a control body (the bytes after the header). Hostile input:
    /// anything malformed is `None`, never a panic.
    pub fn decode(body: &[u8]) -> Option<Control> {
        let mut r = R { b: body, n: 0 };
        Some(match r.u8()? {
            OP_SESSION_START => Control::SessionStart(SessionStart {
                width: r.u16()?,
                height: r.u16()?,
                refresh_mhz: r.u32()?,
                bitrate_kbps: r.u32()?,
                codecs: r.u8()?,
                audio_channels: r.u8()?,
                flags: r.u8()?,
                slices: r.u8()?,
                nonce: r.u32()?,
                app: r.u8().unwrap_or(app::DESKTOP),
                repeat_delay_ms: r.u16().unwrap_or(0),
                repeat_interval_ms: r.u16().unwrap_or(0),
                video: r.u8().unwrap_or(0),
            }),
            OP_SESSION_ACK => Control::SessionAck(SessionAck {
                status: AckStatus::from_code(r.u8()?)?,
                codec: r.u8()?,
                width: r.u16()?,
                height: r.u16()?,
                refresh_mhz: r.u32()?,
                bitrate_kbps: r.u32()?,
                audio_channels: r.u8()?,
                nonce: r.u32()?,
                host: r.u8().unwrap_or(host::WINDOWS),
                features: r.u8().unwrap_or(0),
                video: r.u8().unwrap_or(0),
            }),
            OP_SESSION_END => Control::SessionEnd(EndReason::from_code(r.u8()?)),
            OP_CURSOR_STATE => {
                let f = r.u8()?;
                Control::CursorState(CursorState {
                    visible: f & 1 != 0,
                    clipped: f & 2 != 0,
                    shape: CursorShape::from_code(r.u8()?),
                    x: r.u16()?,
                    y: r.u16()?,
                })
            }
            OP_REQUEST_IDR => Control::RequestIdr,
            OP_INVALIDATE => Control::InvalidateRefs {
                first: r.u32()?,
                last: r.u32()?,
            },
            OP_LOSS_REPORT => Control::LossReport(LossReport {
                received: r.u32()?,
                expected: r.u32()?,
                frames_lost: r.u16()?,
                frames_ok: r.u16()?,
                rtt_us: r.u32()?,
                // Absent from clients that predate it.
                received_kbps: r.u32().unwrap_or(0),
            }),
            OP_PING => Control::Ping {
                id: r.u32()?,
                sent_us: r.u32()?,
            },
            OP_PONG => Control::Pong {
                id: r.u32()?,
                sent_us: r.u32()?,
            },
            OP_GAMEPAD => Control::Gamepad(GamepadState {
                index: r.u8()?,
                seq: r.u16()?,
                connected: r.u8()? & 1 != 0,
                buttons: r.u32()?,
                left_trigger: r.u8()?,
                right_trigger: r.u8()?,
                left_x: r.u16()? as i16,
                left_y: r.u16()? as i16,
                right_x: r.u16()? as i16,
                right_y: r.u16()? as i16,
            }),
            OP_RUMBLE => Control::Rumble {
                index: r.u8()?,
                low: r.u16()?,
                high: r.u16()?,
            },
            OP_RENDEZVOUS_OFFER => Control::RendezvousOffer {
                key: r.array()?,
                secret: r.array()?,
            },
            OP_RENDEZVOUS_KEY => Control::RendezvousKey { key: r.array()? },
            OP_HOST_WARNING => Control::HostWarning(r.u8()?),
            OP_PROGRESS => Control::Progress(r.u8()?),
            OP_AGENT_CONTROL => Control::AgentControl(r.u8()?),
            OP_AGENT_STATE => Control::AgentState(AgentState {
                flags: r.u8()?,
                watchers: r.u8()?,
            }),
            OP_HDR_METADATA => {
                let mut primaries = [[0u16; 2]; 3];
                for p in &mut primaries {
                    *p = [r.u16()?, r.u16()?];
                }
                Control::HdrMetadata(HdrMetadata {
                    primaries,
                    white_point: [r.u16()?, r.u16()?],
                    max_luminance: r.u16()?,
                    min_luminance: r.u16()?,
                    max_cll: r.u16()?,
                    max_fall: r.u16()?,
                    sdr_white: r.u16()?,
                })
            }
            OP_AGENT_NOTE => {
                let len = r.u8()? as usize;
                if len > AgentNote::MAX {
                    return None;
                }
                let text = std::str::from_utf8(r.b.get(r.n..r.n + len)?).ok()?;
                Control::AgentNote(AgentNote::new(text))
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(c: Control) {
        let mut out = [0u8; MAX_CONTROL_LEN];
        let n = c.encode(1234, &mut out);
        let h = Header::decode(&out[..n]).unwrap();
        assert_eq!(h.kind, Kind::Control);
        assert_eq!(h.total_len as usize, n);
        assert_eq!(Control::decode(&out[HEADER_LEN..n]), Some(c));
    }

    #[test]
    fn every_message_round_trips() {
        round_trip(Control::SessionStart(SessionStart {
            width: 3024,
            height: 1890,
            refresh_mhz: 120_000,
            bitrate_kbps: 100_000,
            codecs: codec::H264 | codec::HEVC,
            audio_channels: 2,
            flags: flags::KEEP_HOST_DISPLAYS | flags::CLIPBOARD,
            slices: 4,
            nonce: 0xDEAD_BEEF,
            app: app::DESKTOP,
            repeat_delay_ms: 225,
            repeat_interval_ms: 30,
            video: video::HDR | video::YUV444,
        }));
        round_trip(Control::SessionAck(SessionAck {
            status: AckStatus::Ok,
            codec: codec::HEVC,
            width: 3024,
            height: 1890,
            refresh_mhz: 120_000,
            bitrate_kbps: 100_000,
            audio_channels: 2,
            nonce: 7,
            host: host::MACOS,
            features: features::CLIPBOARD,
            video: video::HDR,
        }));
        round_trip(Control::HdrMetadata(HdrMetadata {
            max_cll: 1000,
            max_fall: 400,
            ..HdrMetadata::bt2020(1499, 240)
        }));
        round_trip(Control::SessionEnd(EndReason::Replaced));
        round_trip(Control::SessionEnd(EndReason::HostInUse));
        round_trip(Control::RequestIdr);
        round_trip(Control::InvalidateRefs {
            first: u32::MAX - 1,
            last: 3,
        });
        round_trip(Control::CursorState(CursorState {
            visible: true,
            clipped: false,
            shape: CursorShape::IBeam,
            x: 100,
            y: 200,
        }));
        round_trip(Control::LossReport(LossReport {
            received: 10_000,
            expected: 10_020,
            frames_lost: 1,
            frames_ok: 119,
            rtt_us: 3_500,
            received_kbps: 41_000,
        }));
        round_trip(Control::Ping { id: 1, sent_us: 99 });
        round_trip(Control::Pong { id: 1, sent_us: 99 });
        round_trip(Control::Gamepad(GamepadState {
            index: 2,
            seq: 65535,
            connected: true,
            buttons: crate::gamepad::button::A | crate::gamepad::button::DPAD_LEFT,
            left_trigger: 255,
            right_trigger: 1,
            left_x: -32768,
            left_y: 32767,
            right_x: -1,
            right_y: 0,
        }));
        round_trip(Control::Rumble {
            index: 1,
            low: 65535,
            high: 12,
        });
        round_trip(Control::RendezvousOffer {
            key: [1; 32],
            secret: [2; 32],
        });
        round_trip(Control::RendezvousKey { key: [3; 32] });
        round_trip(Control::HostWarning(host_warning::INPUT_BLOCKED));
        round_trip(Control::Progress(progress::DISPLAY));
        round_trip(Control::AgentControl(agent_control::TAKE_OVER));
        round_trip(Control::AgentState(AgentState {
            flags: agent_state::PAUSED | agent_state::TAKEN_OVER,
            watchers: 2,
        }));
        round_trip(Control::AgentNote(AgentNote::new("left_click (640, 360)")));
        round_trip(Control::AgentNote(AgentNote::new("")));
    }

    #[test]
    fn agent_notes_are_cut_at_a_character_boundary() {
        let long = "é".repeat(40); // 80 bytes
        let n = AgentNote::new(&long);
        assert_eq!(n.as_str().len(), 62);
        assert_eq!(n.as_str(), "é".repeat(31));
        let mut out = [0u8; MAX_CONTROL_LEN];
        let len = Control::AgentNote(n).encode(0, &mut out);
        assert!(len <= MAX_CONTROL_LEN);
        // A length past the maximum, or bytes that are not UTF-8, are refused.
        assert_eq!(Control::decode(&[OP_AGENT_NOTE, 63]), None);
        assert_eq!(Control::decode(&[OP_AGENT_NOTE, 2, 0xff, 0xfe]), None);
        assert_eq!(Control::decode(&[OP_AGENT_NOTE, 4, b'a']), None);
    }

    #[test]
    fn held_agent_state() {
        assert!(AgentState::default().agent_may_act());
        for f in [
            agent_state::PAUSED,
            agent_state::LOCAL_INPUT,
            agent_state::SECURE_DESKTOP,
            agent_state::VIEW_ONLY,
            agent_state::TAKEN_OVER,
        ] {
            assert!(!AgentState {
                flags: f,
                watchers: 0
            }
            .agent_may_act());
        }
    }

    #[test]
    fn truncated_bodies_are_rejected_not_panicked_on() {
        let mut out = [0u8; MAX_CONTROL_LEN];
        let n = Control::SessionStart(SessionStart {
            width: 1,
            height: 1,
            refresh_mhz: 1,
            bitrate_kbps: 1,
            codecs: 1,
            audio_channels: 0,
            flags: 0,
            slices: 1,
            nonce: 1,
            app: app::STEAM_BIG_PICTURE,
            repeat_delay_ms: 500,
            repeat_interval_ms: 40,
            video: video::YUV444,
        })
        .encode(0, &mut out);
        // What follows the nonce is optional: an older client's request.
        let optional = 1 + 2 + 2 + 1;
        for cut in HEADER_LEN..n - optional {
            assert_eq!(Control::decode(&out[HEADER_LEN..cut]), None);
        }
        match Control::decode(&out[HEADER_LEN..n - optional]) {
            Some(Control::SessionStart(s)) => assert_eq!(
                (s.app, s.repeat_delay_ms, s.repeat_interval_ms, s.video),
                (app::DESKTOP, 0, 0, 0)
            ),
            other => panic!("{other:?}"),
        }
        match Control::decode(&out[HEADER_LEN..n - 3]) {
            Some(Control::SessionStart(s)) => assert_eq!(
                (s.app, s.repeat_delay_ms, s.repeat_interval_ms, s.video),
                (app::STEAM_BIG_PICTURE, 500, 0, 0)
            ),
            other => panic!("{other:?}"),
        }
        match Control::decode(&out[HEADER_LEN..n - 1]) {
            Some(Control::SessionStart(s)) => assert_eq!((s.repeat_interval_ms, s.video), (40, 0)),
            other => panic!("{other:?}"),
        }
        // So is the ack's last (the host's system): an older host is Windows.
        let n = Control::SessionAck(SessionAck {
            status: AckStatus::Ok,
            codec: codec::HEVC,
            width: 1,
            height: 1,
            refresh_mhz: 1,
            bitrate_kbps: 1,
            audio_channels: 0,
            nonce: 1,
            host: host::MACOS,
            features: features::CLIPBOARD,
            video: video::HDR,
        })
        .encode(0, &mut out);
        match Control::decode(&out[HEADER_LEN..n - 3]) {
            Some(Control::SessionAck(a)) => assert_eq!((a.host, a.features), (host::WINDOWS, 0)),
            other => panic!("{other:?}"),
        }
        // And the features after it: an older host offers none.
        match Control::decode(&out[HEADER_LEN..n - 2]) {
            Some(Control::SessionAck(a)) => assert_eq!((a.host, a.features), (host::MACOS, 0)),
            other => panic!("{other:?}"),
        }
        // And the video after them: an older host streams SDR 4:2:0.
        match Control::decode(&out[HEADER_LEN..n - 1]) {
            Some(Control::SessionAck(a)) => {
                assert_eq!((a.features, a.video), (features::CLIPBOARD, 0))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(Control::decode(&[]), None);
        assert_eq!(Control::decode(&[0]), None);
        assert_eq!(Control::decode(&[255]), None);
    }
}
