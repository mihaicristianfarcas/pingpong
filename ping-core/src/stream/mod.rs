//! One streaming session with one host: tunnel, negotiation, video receive and
//! loss recovery, input, latency probes, and statistics.
//!
//! Threads:
//!   net    recv → reassemble → frame gate → decode; control; timers (`net`)
//!   input  batch + send input (see `crate::input`)
//! The decoder's output and rendering belong to the platform layer, reached
//! through [`VideoOut`].
//!
//! - `net`: the network thread, which runs the session.
//! - `quality`: loss accounting for the host's bitrate controller, and the
//!   connection warning.
//! - `messages`: what the person streaming is told when a session ends or is
//!   refused.

mod messages;
mod net;
mod quality;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use pingpong_proto::clock;
use pingpong_proto::control::{
    self, agent_control, agent_state, AgentState, Control, CursorState, EndReason, SessionAck,
};
use pingpong_transport::{Endpoint, Identity, Peer, PublicIdentity};

use crate::input::{InputSender, InputThread};
use crate::stats::{Stats, StatsCollector};

/// Why a stream ended when no handshake came back: the host is off, or does
/// not know this identity (Pong ignores keys it has not paired, silently).
/// Callers that know which identity it was say it better (an agent's,
/// `ping_agent::headless`).
pub const NO_ANSWER: &str =
    "The host did not answer. Is Pong running, and is this client paired with it?";

/// Moonlight's default bitrate for a mode: interpolated over its resolution
/// table, linear in frame rate up to 60 and by the square root beyond.
pub fn default_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    const TABLE: [(f64, f64); 6] = [
        (640.0 * 360.0, 1.0),
        (854.0 * 480.0, 2.0),
        (1280.0 * 720.0, 5.0),
        (1920.0 * 1080.0, 10.0),
        (2560.0 * 1440.0, 20.0),
        (3840.0 * 2160.0, 40.0),
    ];
    let pixels = width as f64 * height as f64;
    let res = match TABLE.iter().position(|&(p, _)| pixels <= p) {
        Some(0) => TABLE[0].1,
        Some(i) => {
            let (p0, f0) = TABLE[i - 1];
            let (p1, f1) = TABLE[i];
            f0 + (f1 - f0) * (pixels - p0) / (p1 - p0)
        }
        None => TABLE[TABLE.len() - 1].1,
    };
    let fps = fps as f64;
    let rate = if fps <= 60.0 {
        fps / 30.0
    } else {
        (fps / 60.0).sqrt() * 60.0 / 30.0
    };
    ((res * rate).round() as u32).max(1) * 1000
}

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamSettings {
    pub width: u16,
    pub height: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// `control::codec::*` bitmask of what the client will accept.
    pub codecs: u8,
    pub audio_channels: u8,
    /// Keep the host's speakers playing too (Moonlight's "play audio on host").
    pub host_audio: bool,
    pub vsync: bool,
    /// Present on a display link, one frame per refresh (Moonlight's "frame
    /// pacing"): smoother, but about a refresh more latency.
    pub frame_pacing: bool,
    pub keep_host_displays: bool,
    pub slices: u8,
    /// What to stream: `control::app::DESKTOP` or an app the host opens.
    pub app: u8,
    /// Watch the AI agent working on the host instead of starting a session
    /// (the picture is the agent's; Ctrl+Alt+Shift+T takes over).
    pub watch: bool,
    /// Share the clipboard with the host both ways, if it allows.
    pub clipboard: bool,
    /// Swapped buttons, reversed scrolling.
    pub mouse: crate::input::MouseOptions,
    /// `control::video::*` to ask for: what the user wants and this
    /// computer can show (HDR, 4:4:4).
    pub video: u8,
    /// An AI agent's stream (ping-agent): what is said to a person
    /// streaming about the input the host ignores is left out; the agent
    /// hears it in words for its model (`Event::Permissions`).
    pub agent: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        StreamSettings {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 50_000,
            codecs: control::codec::H264 | control::codec::HEVC,
            audio_channels: 2,
            host_audio: false,
            vsync: true,
            frame_pacing: false,
            keep_host_displays: false,
            slices: 1,
            app: control::app::DESKTOP,
            watch: false,
            clipboard: false,
            mouse: crate::input::MouseOptions::default(),
            video: 0,
            agent: false,
        }
    }
}

/// What a stream comes from.
pub enum Source {
    /// A paired Pong host, through the tunnel.
    Pong {
        identity: Arc<Identity>,
        host: HostTarget,
    },
}

impl Source {
    /// What to call it ("Connecting to …", the window's title).
    pub fn name(&self) -> &str {
        match self {
            Source::Pong { host, .. } => &host.name,
        }
    }

    /// The Pong host's short id, for finding it on the local network.
    pub fn host_id(&self) -> Option<String> {
        match self {
            Source::Pong { host, .. } => Some(host.public.short_id()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostTarget {
    pub name: String,
    /// Addresses on the local network (from discovery), tried first.
    pub local: Vec<SocketAddr>,
    /// Other addresses (as paired or entered), tried shortly after.
    pub remote: Vec<SocketAddr>,
    pub public: PublicIdentity,
    /// Finding the host across the internet, when pairing set it up.
    pub wan: Option<crate::wan::Wan>,
    /// Where this client keeps its hosts, so what the host tells it about
    /// reaching it from the internet is remembered.
    pub data_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    fn from_bit(bit: u8) -> Option<Codec> {
        match bit {
            control::codec::H264 => Some(Codec::H264),
            control::codec::HEVC => Some(Codec::Hevc),
            control::codec::AV1 => Some(Codec::Av1),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

/// Per-frame timing carried from the network to the screen.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTiming {
    pub frame_id: u32,
    /// When the host picked the frame up, on the host's clock.
    pub captured_us: u32,
    /// Host capture → hand-off to the network, from the frame prefix.
    pub host_us: u32,
    pub first_packet_us: u32,
    pub reassembled_us: u32,
}

/// The platform's decoder + presenter.
pub trait VideoOut: Send {
    /// A session was (re)negotiated: expect `codec` at this size from now
    /// on, its pictures as `video` says (`control::video`: HDR, 4:4:4).
    fn configure(&mut self, codec: Codec, width: u32, height: u32, video: u8)
        -> Result<(), String>;
    /// The host's HDR metadata, while the stream is HDR.
    fn hdr_metadata(&mut self, _metadata: pingpong_proto::control::HdrMetadata) {}
    /// Decode one complete, decodable access unit.
    fn decode(&mut self, bitstream: &[u8], timing: FrameTiming) -> Result<(), String>;
}

pub enum Event {
    /// Human-readable progress ("Connecting…").
    Status(String),
    /// Shown over the picture mid-stream ("Reconnecting…"); None clears it.
    Notice(Option<String>),
    /// The network is struggling (Moonlight's connection warning), or no
    /// longer (None).
    Warning(Option<String>),
    Started(SessionAck),
    Cursor(CursorState),
    /// An agent's session: who drives, and why the agent may not (sent to
    /// the agent and to its watchers, on every change).
    Agent(AgentState),
    /// What this device may do on the host (`permission::*` bits), when the
    /// host says: as the session starts, and when it changes.
    Permissions(u16),
    /// The host wants controller `index`'s motors at these strengths.
    Rumble {
        index: u8,
        low: u16,
        high: u16,
    },
    Ended {
        reason: String,
        error: bool,
    },
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// A running stream, from whichever [`Source`].
pub struct Stream(Inner);

enum Inner {
    Pong(PongStream),
}

/// A stream from a Pong host.
struct PongStream {
    ctx: Arc<Ctx>,
    net: Option<JoinHandle<()>>,
    input: Option<InputThread>,
    input_tx: InputSender,
}

struct Ctx {
    endpoint: Arc<Endpoint>,
    peer: Arc<Peer>,
    settings: StreamSettings,
    events: EventSink,
    stop: AtomicBool,
    stats: Arc<StatsCollector>,
    ack: Mutex<Option<SessionAck>>,
    rtt_us: AtomicU64,
    /// The lowest round trip since the last loss report (u64::MAX: none).
    rtt_floor_us: AtomicU64,
    /// Candidate paths to the host: (local, remote).
    candidates: Mutex<(Vec<SocketAddr>, Vec<SocketAddr>)>,
    /// STUN on the stream's socket (answers arrive in the net loop).
    stun: Arc<pingpong_nat::stun::Stun>,
    /// Set when the stream stops, for helpers that outlive a loop iteration.
    stopped: Arc<AtomicBool>,
    /// How long the handshake may take: longer when punching across NATs.
    handshake_limit: Duration,
    data_dir: Option<std::path::PathBuf>,
    host_name: String,
    /// Play the host's audio as silence (the stream window is in the
    /// background, and the user asked for that).
    audio_muted: AtomicBool,
    /// An agent's session: the host's last word on who drives.
    agent_state: Mutex<Option<AgentState>>,
    /// Held for as long as the tunnel is ([`crate::store::TunnelLock`]).
    _tunnel_lock: Option<crate::store::TunnelLock>,
}

/// Sends control messages to the host from another thread (controllers).
#[derive(Clone)]
pub struct ControlSender(Controls);

#[derive(Clone)]
enum Controls {
    Pong(Arc<Ctx>),
}

impl ControlSender {
    /// Silence the host's audio (or not), without stopping the stream.
    pub fn set_audio_muted(&self, muted: bool) {
        match &self.0 {
            Controls::Pong(ctx) => ctx.audio_muted.store(muted, Ordering::Relaxed),
        }
    }

    pub fn send(&self, msg: Control) {
        match &self.0 {
            Controls::Pong(ctx) => send_control(ctx, msg),
        }
    }

    /// Who drives the agent's session, as the host last said.
    pub fn agent_state(&self) -> Option<AgentState> {
        match &self.0 {
            Controls::Pong(ctx) => *ctx.agent_state.lock(),
        }
    }

    /// Watching an agent: take the keyboard and mouse, or give them back.
    pub fn toggle_take_over(&self) {
        let Controls::Pong(ctx) = &self.0;
        let taken = self
            .agent_state()
            .is_some_and(|s| s.flags & agent_state::TAKEN_OVER != 0);
        let op = if taken {
            agent_control::HAND_BACK
        } else {
            agent_control::TAKE_OVER
        };
        tracing::info!(take_over = !taken, "agent control");
        // Twice: control messages are not retransmitted, and repeats are harmless.
        for _ in 0..2 {
            send_control(ctx, Control::AgentControl(op));
        }
    }
}

/// Adds paths to the host from another thread (discovery).
#[derive(Clone)]
pub struct Candidates(Arc<Ctx>);

impl Candidates {
    /// Discovery found the host on the local network: race that path too
    /// (it only matters until the tunnel is up).
    pub fn add_local(&self, addr: SocketAddr) {
        let mut c = self.0.candidates.lock();
        if !c.0.contains(&addr) {
            tracing::debug!(%addr, "host found on the local network");
            c.0.push(addr);
        }
    }

    /// The host's public address, from its rendezvous record.
    pub fn add_remote(&self, addr: SocketAddr) {
        let mut c = self.0.candidates.lock();
        if !c.0.contains(&addr) && !c.1.contains(&addr) {
            c.1.push(addr);
        }
    }
}

impl Stream {
    /// Start streaming from `source`.
    pub fn open(
        source: Source,
        settings: StreamSettings,
        video: Box<dyn VideoOut>,
        events: EventSink,
        stats: Arc<StatsCollector>,
    ) -> Result<Stream, String> {
        match source {
            Source::Pong { identity, host } => {
                Stream::start(identity, host, settings, video, events, stats)
            }
        }
    }

    /// Start streaming from a Pong host.
    pub fn start(
        identity: Arc<Identity>,
        host: HostTarget,
        settings: StreamSettings,
        video: Box<dyn VideoOut>,
        events: EventSink,
        stats: Arc<StatsCollector>,
    ) -> Result<Stream, String> {
        pingpong_proto::fec::warm_up();
        // Taken before the port: the host list's probe borrows it between
        // streams, and must not handshake as this identity while one runs.
        let tunnel_lock = host
            .data_dir
            .as_deref()
            .and_then(crate::store::TunnelLock::for_stream);
        // The client's own stable port, so its NAT mapping (and a host's warm
        // path towards it) carries over between streams; any port if busy.
        let port = host
            .data_dir
            .as_deref()
            .map(crate::store::tunnel_port)
            .unwrap_or(0);
        let endpoint = match Endpoint::bind(identity.clone(), port) {
            Ok(e) => e,
            Err(_) => {
                Endpoint::bind(identity, 0).map_err(|e| format!("cannot open a UDP socket: {e}"))?
            }
        };
        let endpoint = Arc::new(endpoint);
        endpoint.set_service_class(true);
        let _ = endpoint.set_recv_timeout(Duration::from_millis(4));
        let first = host.local.first().or(host.remote.first()).copied();
        if first.is_none() && host.wan.is_none() {
            return Err("the host has no address".into());
        }
        let peer = endpoint
            .add_peer(host.public.clone(), first)
            .map_err(|e| e.to_string())?;
        let ctx = Arc::new(Ctx {
            endpoint,
            peer,
            settings,
            events,
            stop: AtomicBool::new(false),
            stats,
            ack: Mutex::new(None),
            rtt_us: AtomicU64::new(0),
            rtt_floor_us: AtomicU64::new(u64::MAX),
            candidates: Mutex::new((host.local, host.remote)),
            stun: Arc::new(pingpong_nat::stun::Stun::new()),
            stopped: Arc::new(AtomicBool::new(false)),
            handshake_limit: if host.wan.is_some() {
                Duration::from_secs(45)
            } else {
                Duration::from_secs(15)
            },
            data_dir: host.data_dir.clone(),
            host_name: host.name.clone(),
            audio_muted: AtomicBool::new(false),
            agent_state: Mutex::new(None),
            _tunnel_lock: tunnel_lock,
        });
        if let Some(wan) = host.wan.clone() {
            let (endpoint, peer, stun, stop) = (
                ctx.endpoint.clone(),
                ctx.peer.clone(),
                ctx.stun.clone(),
                ctx.stopped.clone(),
            );
            let candidates = Candidates(ctx.clone());
            std::thread::Builder::new()
                .name("ping-wan".into())
                .spawn(move || {
                    crate::wan::connect(wan, endpoint, peer, stun, stop, move |a| {
                        candidates.add_remote(a)
                    })
                })
                .map_err(|e| e.to_string())?;
        }
        let (input, input_tx) =
            InputThread::spawn(ctx.endpoint.clone(), ctx.peer.clone(), ctx.settings.mouse);
        let net = {
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("ping-net".into())
                .spawn(move || net::run(ctx, video))
                .map_err(|e| e.to_string())?
        };
        Ok(Stream(Inner::Pong(PongStream {
            ctx,
            net: Some(net),
            input: Some(input),
            input_tx,
        })))
    }

    pub fn input(&self) -> &InputSender {
        match &self.0 {
            Inner::Pong(p) => &p.input_tx,
        }
    }

    pub fn controls(&self) -> ControlSender {
        match &self.0 {
            Inner::Pong(p) => ControlSender(Controls::Pong(p.ctx.clone())),
        }
    }

    /// For adding paths to a Pong host while the handshake is under way.
    pub fn candidates(&self) -> Option<Candidates> {
        match &self.0 {
            Inner::Pong(p) => Some(Candidates(p.ctx.clone())),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats_collector().snapshot()
    }

    pub fn stats_collector(&self) -> &Arc<StatsCollector> {
        match &self.0 {
            Inner::Pong(p) => &p.ctx.stats,
        }
    }

    /// The Pong host's answer to the session, once it has given one.
    pub fn ack(&self) -> Option<SessionAck> {
        match &self.0 {
            Inner::Pong(p) => *p.ctx.ack.lock(),
        }
    }

    pub fn is_running(&self) -> bool {
        match &self.0 {
            Inner::Pong(p) => p.net.as_ref().is_some_and(|n| !n.is_finished()),
        }
    }

    /// End the stream and stop every thread.
    pub fn stop(&mut self) {
        match &mut self.0 {
            Inner::Pong(p) => p.stop(),
        }
    }
}

impl PongStream {
    /// End the session: tell the host (so it restores its displays now rather
    /// than after the silence timeout) and stop every thread.
    fn stop(&mut self) {
        self.ctx.stopped.store(true, Ordering::Relaxed);
        if !self.ctx.stop.swap(true, Ordering::Relaxed) {
            for _ in 0..3 {
                send_control(&self.ctx, Control::SessionEnd(EndReason::Quit));
            }
        }
        if let Some(n) = self.net.take() {
            let _ = n.join();
        }
        if let Some(i) = self.input.take() {
            i.stop();
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn send_control(ctx: &Ctx, msg: Control) {
    let mut out = [0u8; control::MAX_CONTROL_LEN];
    let n = msg.encode(clock::now_us(), &mut out);
    if let Err(e) = ctx.endpoint.send(&ctx.peer, &out[..n]) {
        tracing::debug!(error = %e, "control send");
    }
}

#[cfg(test)]
mod tests {
    use super::default_bitrate_kbps;

    #[test]
    fn default_bitrates_match_moonlight() {
        assert_eq!(default_bitrate_kbps(1920, 1080, 60), 20_000);
        assert_eq!(default_bitrate_kbps(1280, 720, 60), 10_000);
        assert_eq!(default_bitrate_kbps(3840, 2160, 60), 80_000);
        assert_eq!(default_bitrate_kbps(1920, 1080, 30), 10_000);
        // 120 fps scales by sqrt(2), not 2.
        assert_eq!(default_bitrate_kbps(1920, 1080, 120), 28_000);
        // A 14" MacBook Pro's notch-free native mode at 120 Hz.
        assert_eq!(default_bitrate_kbps(3024, 1890, 120), 81_000);
    }
}
