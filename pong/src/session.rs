//! Session lifecycle, the same on every host. One streaming session at a
//! time, at the client's mode on a display made for it: a new client's `SessionStart` takes over (or is
//! refused, per config); a session is acked only once its video pipeline
//! runs, and ends when the client says so, goes quiet, or the pipeline dies.
//! What differs per platform -- the display streamed, capture, input, audio,
//! apps and controllers -- is `platform`'s.
//!
//! An AI agent's session (a client paired as an agent, see `clients`) runs
//! the same way, under rules of its own (docs/ai-agents.md):
//! - it never takes over a person's session, and a person's always takes
//!   over from it;
//! - people may watch it (`flags::WATCH`): the same picture goes to them,
//!   and one may take over the keyboard and mouse, pause the agent, hand
//!   back or stop it (`Control::AgentControl`);
//! - its input is held while the host shows a secure screen, for a while
//!   after someone uses the host's own keyboard or mouse, while paused, and
//!   always when its access is view-only;
//! - it and its watchers hear who drives, and why, in `Control::AgentState`.
//!
//! Every client is held to its permissions (`clients`,
//! `pingpong_proto::permission`), as Apollo holds its clients to theirs: no
//! session without seeing the screen, no app without starting apps, no
//! taking over without that, no watching an agent without watching; input
//! of a kind the client may not send is dropped as it arrives, and the
//! clipboard moves only the ways it may. A change applies to a running
//! session at once: taking away seeing ends it (Apollo's
//! `update_device_info`, `stream.cpp`), anything else is held back from then
//! on, and the client is told (`Control::Permissions`). Clipboard sharing
//! that was off when the session started stays off until the next one: the
//! client set up none.
//!
//! ```text
//! SessionStart ─▶ display at the client's mode ─▶ video pipeline ready
//!             ─▶ input installed ─▶ SessionAck ─▶ frames flow
//! SessionEnd / client silent / pipeline died ─▶ release keys ─▶ stop video
//!             ─▶ the platform puts the host's displays back
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use pingpong_encode::{Codec, EncoderConfig};
use pingpong_input::{InputSink, KeyRepeat, RepeatRate};
use pingpong_proto::control::{
    self, agent_control, agent_state, AckStatus, AgentState, Control, EndReason, LossReport,
    SessionAck, SessionStart,
};
use pingpong_proto::fec::FecPolicy;
use pingpong_proto::input::{InputEvent, SequenceGate, MAX_EVENTS_PER_PACKET};
use pingpong_proto::permission::{self, Permissions};
use pingpong_transport::{Endpoint, Peer, PeerId};

use crate::audio::AudioHandle;
use crate::clients::{Access, Clients, Role};
use crate::config::HostConfig;
use crate::host::SessionStatus;
use crate::platform::{self, Platform};
use crate::video::{self, VideoCmd, VideoHandle};

/// A client silent this long has gone: its session ends. The client pings
/// twice a second while streaming, and keeps looking for the host for as
/// long as this after an interruption (ping-core's `LOST_AFTER`), so a
/// client back within it finds its session, virtual display and all.
const CLIENT_GRACE: Duration = Duration::from_secs(20);
/// Keys and buttons held while the client is this quiet are released on the
/// host: a stuck key is worse than one pressed twice.
const INPUT_RELEASE_AFTER: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(8);
/// Between sessions there is only the linger to watch (someone at the host
/// takes their desktop back): a slower tick, so an idle host sleeps. Session
/// requests wake the loop at once.
const IDLE_TICK: Duration = Duration::from_millis(100);
/// How often an agent session looks at the host (secure screen, local
/// input), and tells its agent and watchers who drives even when nothing
/// changed (control messages are not retransmitted).
const AGENT_CHECK_EVERY: Duration = Duration::from_millis(250);
const AGENT_STATE_EVERY: Duration = Duration::from_secs(1);
/// Input at the host this much more recent than the last the session
/// injected is someone else's: its own takes a moment to register.
const INJECTION_MARGIN: Duration = Duration::from_millis(300);
/// Input at the host in a session's first seconds is the session's own
/// doing (waking the display, resetting the screensaver), not a person's.
const START_GRACE: Duration = Duration::from_secs(3);
/// Agent notes kept for the web UI.
const AGENT_LOG_LEN: usize = 200;

pub enum SessionCmd {
    Start {
        peer: Arc<Peer>,
        req: SessionStart,
    },
    End {
        peer: PeerId,
        reason: EndReason,
    },
    Loss {
        peer: PeerId,
        report: LossReport,
    },
    /// A watcher (or the web UI: peer 0) takes over, hands back, pauses,
    /// resumes or stops the agent (`agent_control::*`).
    AgentControl {
        peer: PeerId,
        op: u8,
    },
    /// A client's permissions changed (web UI): its running session, or
    /// its watching, follows.
    Permissions {
        key: String,
        permissions: Permissions,
    },
    Shutdown,
}

pub struct InputState {
    /// Whose input reaches the host: the session's client, or the watcher
    /// who took over from an agent.
    pub peer: PeerId,
    pub gate: SequenceGate,
    /// Everyone else's (watchers', and the agent's while a watcher drives):
    /// their input is admitted and dropped, so that when one of them gets
    /// the keyboard and mouse, the events its packets repeat for redundancy
    /// (the last eight) are known as old and not typed again.
    pub others: std::collections::HashMap<PeerId, SequenceGate>,
    pub sink: platform::Sink,
    /// `peer`'s input is dropped (an agent's, while held).
    pub held: bool,
    /// What `peer` may send: input of other kinds is dropped.
    pub allowed: Permissions,
    /// When input was last injected: someone at the host is told from the
    /// agent by input more recent than this.
    pub last_injected: Option<Instant>,
    /// The key held down, repeating, where the host's system does not
    /// repeat injected keys (`pingpong_input::repeat`).
    pub repeat: Option<KeyRepeat>,
}

impl InputState {
    /// Inject what the client sent and may send, and follow the keys for
    /// repeats.
    pub fn inject(&mut self, events: &[InputEvent], now: Instant) {
        let mut buf = [InputEvent::Wheel { dv: 0, dh: 0 }; MAX_EVENTS_PER_PACKET];
        let events = pingpong_proto::input::permitted(events, self.allowed, &mut buf);
        if events.is_empty() {
            return;
        }
        self.last_injected = Some(now);
        if let Err(e) = self.sink.inject(events) {
            tracing::debug!(error = %e, "input injection");
        }
        let Some(repeat) = self.repeat.as_mut() else {
            return;
        };
        for &ev in events {
            match ev {
                InputEvent::KeyDown(sc) if self.sink.repeats(sc) => repeat.press(sc, now),
                InputEvent::KeyUp(sc) => repeat.release(sc),
                _ => {}
            }
        }
    }

    /// Lift everything held: nothing repeats either.
    pub fn release_all(&mut self) {
        if let Some(r) = self.repeat.as_mut() {
            r.clear();
        }
        let _ = self.sink.release_all();
    }

    /// From now on, input goes in as `allowed` permits. What is held is
    /// let go when that is less than before: a key held down when the
    /// keyboard is taken away must not stay down.
    pub fn allow(&mut self, allowed: Permissions) {
        if allowed == self.allowed {
            return;
        }
        let kinds = permission::KEYBOARD | permission::MOUSE;
        if allowed.bits() & kinds != self.allowed.bits() & kinds {
            self.release_all();
        }
        self.allowed = allowed;
    }

    /// Press the held key again if its repeat is due; when the next is.
    pub fn repeat_tick(&mut self, now: Instant) -> Option<Instant> {
        let repeat = self.repeat.as_mut()?;
        if self.held {
            repeat.clear();
            return None;
        }
        if let Some(sc) = repeat.poll(now) {
            self.last_injected = Some(now);
            if let Err(e) = self.sink.repeat_key(sc) {
                tracing::debug!(error = %e, "key repeat");
            }
        }
        repeat.due()
    }
}

/// How a held key repeats this session: as on the client's keyboard when it
/// says, else as the host's own; `None` where the host's system repeats
/// injected keys itself.
fn key_repeat(sink: &platform::Sink, req: &SessionStart) -> Option<KeyRepeat> {
    let host = sink.key_repeat()?;
    let rate = RepeatRate::from_millis(req.repeat_delay_ms, req.repeat_interval_ms).unwrap_or(host);
    tracing::info!(
        delay_ms = rate.delay.as_millis() as u64,
        interval_ms = rate.interval.as_millis() as u64,
        client = rate != host,
        "held keys repeat"
    );
    Some(KeyRepeat::new(rate))
}

/// One line of an agent's activity, for the web UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentLogEntry {
    pub unix_ms: u64,
    pub client: String,
    pub text: String,
}

/// What the receive thread needs, fast, without going through the session
/// thread: where to send recovery requests and where to inject input.
#[derive(Default)]
pub struct Shared {
    pub active_peer: AtomicU32,
    /// What the session's client may do (`Permissions` bits), for what the
    /// receive thread checks itself (controllers).
    pub client_permissions: AtomicU16,
    pub video: Mutex<Option<Sender<VideoCmd>>>,
    pub input: Mutex<Option<InputState>>,
    /// The session client's controllers, as virtual pads.
    #[cfg(windows)]
    pub pads: Mutex<Option<(PeerId, crate::gamepad::Pads)>>,
    /// Peers watching an agent's session: their recovery requests count.
    pub watchers: Mutex<Vec<PeerId>>,
    /// The running session is an agent's.
    pub agent_session: std::sync::atomic::AtomicBool,
    /// What agents did lately (their `AgentNote`s), newest last.
    pub agent_log: Mutex<VecDeque<AgentLogEntry>>,
    /// The session's clipboard sharing, when its client asked for it.
    pub clip: Mutex<Option<Clip>>,
}

/// Clipboard sharing: here, or (a Windows host, which runs as SYSTEM) in a
/// helper running as the signed-in user (`clipagent`).
pub enum Clip {
    #[cfg(not(windows))]
    Here(pingpong_clipboard::ClipSync),
    #[cfg(windows)]
    Agent(crate::clipagent::ClipAgent),
}

impl Clip {
    /// A clipboard packet from the client.
    pub fn deliver(&self, body: &[u8]) {
        match self {
            #[cfg(not(windows))]
            Clip::Here(c) => c.deliver(body),
            #[cfg(windows)]
            Clip::Agent(a) => a.deliver(body),
        }
    }

    /// Which ways copies go, from now on.
    fn set_directions(&self, d: pingpong_clipboard::Directions) {
        match self {
            #[cfg(not(windows))]
            Clip::Here(c) => c.set_directions(d),
            #[cfg(windows)]
            Clip::Agent(a) => a.set_directions(d),
        }
    }

    fn start(
        bitrate_kbps: u32,
        peer_name: String,
        directions: pingpong_clipboard::Directions,
        send: impl Fn(&[u8]) + Send + 'static,
    ) -> Option<Clip> {
        let rate = pingpong_clipboard::rate_for(bitrate_kbps);
        #[cfg(windows)]
        {
            let _ = peer_name;
            match crate::clipagent::ClipAgent::spawn(rate, directions, send) {
                Ok(a) => Some(Clip::Agent(a)),
                Err(e) => {
                    tracing::warn!(error = e, "cannot share the clipboard");
                    None
                }
            }
        }
        #[cfg(not(windows))]
        {
            let opts = pingpong_clipboard::Options {
                files_dir: std::env::temp_dir().join("Pong Clipboard"),
                rate,
                offer_current: false,
                peer: peer_name,
                directions,
            };
            Some(Clip::Here(pingpong_clipboard::ClipSync::start(opts, send)))
        }
    }
}

impl Shared {
    pub fn is_active(&self, peer: PeerId) -> bool {
        peer != 0 && self.active_peer.load(Ordering::Acquire) == peer
    }

    /// The session's client, or one of its watchers.
    pub fn receives_video(&self, peer: PeerId) -> bool {
        self.is_active(peer) || (peer != 0 && self.watchers.lock().contains(&peer))
    }

    /// Record what the session's agent says it is doing.
    pub fn agent_note(&self, client: &str, text: &str) {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        tracing::info!(client, "agent: {text}");
        let mut log = self.agent_log.lock();
        if log.len() >= AGENT_LOG_LEN {
            log.pop_front();
        }
        log.push_back(AgentLogEntry {
            unix_ms,
            client: client.to_string(),
            text: text.to_string(),
        });
    }
}

/// Which ways the clipboard goes for a client with these permissions: the
/// host's copies to it if it may read them, its copies here if it may
/// write them.
fn clip_directions(p: Permissions) -> pingpong_clipboard::Directions {
    pingpong_clipboard::Directions {
        send: p.allows(permission::CLIPBOARD_READ),
        receive: p.allows(permission::CLIPBOARD_WRITE),
    }
}

/// Someone watching an agent's session.
struct Watcher {
    peer: Arc<Peer>,
    /// Their request, to answer its retransmits.
    req: SessionStart,
    /// What they may do (they may watch).
    permissions: Permissions,
}

/// What an agent's session has beyond a person's.
struct AgentRun {
    /// The agent's permissions.
    permissions: Permissions,
    watchers: Vec<Watcher>,
    /// The watcher who took over.
    controller: Option<PeerId>,
    paused: bool,
    /// Someone used the host's own keyboard or mouse: held until then.
    local_until: Option<Instant>,
    secure: bool,
    last_check: Instant,
    last_state: Option<(AgentState, Instant)>,
}

impl AgentRun {
    fn state(&self, now: Instant) -> AgentState {
        let mut flags = 0;
        if self.paused {
            flags |= agent_state::PAUSED;
        }
        if self.controller.is_some() {
            flags |= agent_state::TAKEN_OVER;
        }
        if self.local_until.is_some_and(|t| now < t) {
            flags |= agent_state::LOCAL_INPUT;
        }
        if self.secure {
            flags |= agent_state::SECURE_DESKTOP;
        }
        if !self
            .permissions
            .allows_any(permission::KEYBOARD | permission::MOUSE)
        {
            flags |= agent_state::VIEW_ONLY;
        }
        if !self.permissions.allows(permission::UNWATCHED) && self.watchers.is_empty() {
            flags |= agent_state::UNWATCHED;
        }
        AgentState {
            flags,
            watchers: self.watchers.len().min(255) as u8,
        }
    }
}

struct Active {
    peer: Arc<Peer>,
    req: SessionStart,
    ack: SessionAck,
    video: VideoHandle,
    audio: Option<AudioHandle>,
    display: platform::Display,
    started: Instant,
    /// Bitrate (when adaptive) and FEC, from the client's loss reports.
    bitrate: crate::bitrate::BitrateController,
    fec: FecPolicy,
    /// The client is sent LAN-sized shards while on this network.
    lan_shards: bool,
    /// Who the video goes to: the client, then watchers.
    recipients: crate::sender::Recipients,
    /// What the client may do.
    permissions: Permissions,
    /// An agent's session.
    agent: Option<AgentRun>,
}

pub struct SessionManager {
    endpoint: Arc<Endpoint>,
    config: Arc<parking_lot::RwLock<HostConfig>>,
    shared: Arc<Shared>,
    platform: Platform,
    supported: Vec<Codec>,
    active: Option<Active>,
    last_stats: Instant,
    status: Arc<Mutex<Option<SessionStatus>>>,
    clients: Arc<Mutex<Clients>>,
    last_loss: LossReport,
    /// The last request refused, and why: the client resends a request until
    /// it hears back, and each copy must not start another attempt.
    refused: Option<(PeerId, SessionStart, AckStatus)>,
    /// When the key held on the host repeats next.
    repeat_due: Option<Instant>,
}

pub fn send_control(endpoint: &Endpoint, peer: &Peer, msg: Control) {
    let mut buf = [0u8; control::MAX_CONTROL_LEN];
    let n = msg.encode(pingpong_proto::clock::now_us(), &mut buf);
    if let Err(e) = endpoint.send(peer, &buf[..n]) {
        tracing::debug!(error = %e, "control send failed");
    }
}

impl SessionManager {
    pub fn new(
        endpoint: Arc<Endpoint>,
        config: Arc<parking_lot::RwLock<HostConfig>>,
        shared: Arc<Shared>,
        data_dir: &std::path::Path,
        status: Arc<Mutex<Option<SessionStatus>>>,
        clients: Arc<Mutex<Clients>>,
    ) -> SessionManager {
        let platform = Platform::new(data_dir);
        let supported = platform.supported_codecs();
        tracing::info!(codecs = ?supported.iter().map(|c| c.name()).collect::<Vec<_>>(), "encoder capabilities");
        SessionManager {
            endpoint,
            config,
            shared,
            platform,
            supported,
            active: None,
            last_stats: Instant::now(),
            status,
            clients,
            last_loss: LossReport::default(),
            refused: None,
            repeat_due: None,
        }
    }

    pub fn run(mut self, rx: Receiver<SessionCmd>) {
        loop {
            let tick = if self.active.is_some() {
                // A held key's next repeat, if sooner.
                self.repeat_due
                    .map_or(TICK, |t| t.saturating_duration_since(Instant::now()))
                    .min(TICK)
            } else {
                IDLE_TICK
            };
            match rx.recv_timeout(tick) {
                Ok(SessionCmd::Start { peer, req }) => self.start(peer, req),
                Ok(SessionCmd::End { peer, reason }) => {
                    if self.active.as_ref().is_some_and(|a| a.peer.id() == peer) {
                        tracing::info!(?reason, "client ended the session");
                        self.teardown(None);
                    } else {
                        self.drop_watcher(peer);
                    }
                }
                Ok(SessionCmd::AgentControl { peer, op }) => self.agent_control(peer, op),
                Ok(SessionCmd::Permissions { key, permissions }) => {
                    self.permissions_changed(&key, permissions)
                }
                Ok(SessionCmd::Loss { peer, report }) => self.on_loss(peer, report),
                Ok(SessionCmd::Shutdown)
                | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.teardown(Some(EndReason::Shutdown));
                    return;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
    }

    fn ack(&self, peer: &Peer, ack: SessionAck) {
        send_control(&self.endpoint, peer, Control::SessionAck(ack));
    }

    fn refuse(&mut self, peer: &Peer, req: &SessionStart, status: AckStatus) {
        tracing::warn!(?status, "session refused");
        self.refused = Some((peer.id(), *req, status));
        self.send_refusal(peer, req, status);
    }

    fn send_refusal(&self, peer: &Peer, req: &SessionStart, status: AckStatus) {
        if let Some(note) = self.platform.refusal_note(status) {
            send_control(&self.endpoint, peer, note);
        }
        self.ack(
            peer,
            SessionAck {
                status,
                codec: 0,
                width: req.width,
                height: req.height,
                refresh_mhz: req.refresh_mhz,
                bitrate_kbps: req.bitrate_kbps,
                audio_channels: 0,
                nonce: req.nonce,
                host: platform::HOST_KIND,
                features: 0,
                video: 0,
                permissions: 0,
            },
        );
    }

    fn start(&mut self, peer: Arc<Peer>, req: SessionStart) {
        // A retransmitted request for the session already running: ack again.
        if let Some(a) = &self.active {
            if a.peer.id() == peer.id() && a.req == req {
                self.ack(&peer, a.ack);
                return;
            }
        }
        // A copy of a request already refused (it queued up while that
        // attempt ran): the same answer, not another attempt.
        if let Some((p, r, status)) = self.refused {
            if p == peer.id() && r == req {
                self.send_refusal(&peer, &req, status);
                return;
            }
        }
        self.refused = None;
        let cfg = self.config.read().clone();
        let (role, permissions) = self.clients.lock().role_of(&peer.public().x25519);
        if req.flags & control::flags::WATCH != 0 {
            self.watch(peer, req, role, permissions);
            return;
        }
        if self.admit(&peer, &req, role, permissions, &cfg).is_err() {
            return;
        }
        let agent = role == Role::Agent;
        // A restart with new parameters: the platform keeps what it can of
        // the display (Windows reuses its virtual display).
        drop(self.stop_stream());

        let negotiated =
            match crate::negotiate::negotiate(&req, &cfg, &self.supported, |c, asked| {
                self.platform.video_caps(c, asked)
            }) {
                Ok(n) => n,
                Err(status) => {
                    self.refuse(&peer, &req, status);
                    return;
                }
            };
        let crate::negotiate::Negotiated {
            codec,
            fps_mhz,
            bitrate_kbps,
            width,
            height,
            video: picture,
        } = negotiated;
        tracing::info!(
            client = peer.public().short_id(),
            width,
            height,
            fps = fps_mhz as f64 / 1000.0,
            bitrate_kbps,
            codec = codec.name(),
            hdr = picture & control::video::HDR != 0,
            yuv444 = picture & control::video::YUV444 != 0,
            "starting session"
        );
        if let Err(status) = self.platform.preflight() {
            self.refuse(&peer, &req, status);
            return;
        }

        let keep = cfg.keep_host_displays || req.flags & control::flags::KEEP_HOST_DISPLAYS != 0;
        send_control(
            &self.endpoint,
            &peer,
            Control::Progress(control::progress::DISPLAY),
        );
        let display = match self.platform.display(&negotiated, &req, keep, &cfg) {
            Ok(d) => d,
            Err(status) => {
                self.refuse(&peer, &req, status);
                return;
            }
        };

        let encoder = EncoderConfig {
            codec,
            width: width as u32,
            height: height as u32,
            fps_mhz,
            bitrate_bps: bitrate_kbps.saturating_mul(1000),
            preset: cfg.nvenc_preset,
            two_pass: false,
            slices: 1,
            hdr: picture & control::video::HDR != 0,
            yuv444: picture & control::video::YUV444 != 0,
        };
        let params = self.platform.video_params(&display, encoder, &cfg, &req);
        send_control(
            &self.endpoint,
            &peer,
            Control::Progress(control::progress::ENCODER),
        );
        let recipients: crate::sender::Recipients =
            Arc::new(parking_lot::RwLock::new(vec![peer.clone()]));
        let video = match video::start(params, self.endpoint.clone(), recipients.clone()) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "video pipeline failed to start");
                self.platform.abandon(display);
                self.refuse(&peer, &req, AckStatus::Failed);
                return;
            }
        };

        let agent_run = agent.then(|| AgentRun {
            permissions,
            watchers: Vec::new(),
            controller: None,
            paused: false,
            local_until: None,
            secure: false,
            last_check: Instant::now() - AGENT_CHECK_EVERY,
            last_state: None,
        });
        let sink = self.platform.input_sink(&display, width, height);
        let repeat = key_repeat(&sink, &req);
        *self.shared.input.lock() = Some(InputState {
            peer: peer.id(),
            gate: SequenceGate::new(),
            others: std::collections::HashMap::new(),
            sink,
            held: agent_run
                .as_ref()
                .is_some_and(|r| !r.state(Instant::now()).agent_may_act()),
            allowed: permissions,
            last_injected: None,
            repeat,
        });
        self.shared
            .client_permissions
            .store(permissions.bits(), Ordering::Release);
        self.shared.agent_session.store(agent, Ordering::Release);
        if agent {
            tracing::info!(client = self.name_of(&peer), permissions = ?permissions.names(), "an AI agent's session");
        } else {
            tracing::info!(client = self.name_of(&peer), permissions = ?permissions.names(), "a person's session");
        }
        *self.shared.video.lock() = Some(video.commands());
        self.shared.active_peer.store(peer.id(), Ordering::Release);

        let (audio, channels) = match self.platform.audio(
            &display,
            &req,
            bitrate_kbps,
            self.endpoint.clone(),
            peer.clone(),
        ) {
            Some((a, ch)) => (Some(a), ch),
            None => (None, 0),
        };
        self.platform.starting(&peer, &self.shared, &self.endpoint);

        // A person's client may share the clipboard, the ways it may (an
        // agent's, and watchers, never do).
        let clipboard = !agent
            && cfg.clipboard
            && req.flags & control::flags::CLIPBOARD != 0
            && permissions.allows_any(permission::CLIPBOARD_READ | permission::CLIPBOARD_WRITE);
        let clipboard =
            clipboard && self.start_clipboard(&peer, bitrate_kbps, clip_directions(permissions));

        // A client that reassembles LAN-sized shards gets them while it is
        // on this network (the sender looks at its address every frame).
        let lan_shards = req.flags & control::flags::LAN_SHARDS != 0;
        video.set_lan_shards(lan_shards);
        let mut features = 0;
        if clipboard {
            features |= control::features::CLIPBOARD;
        }
        if lan_shards {
            features |= control::features::LAN_SHARDS;
        }
        // Every host draws the pointer into the picture (`video`).
        features |= control::features::POINTER_IN_PICTURE;
        let ack = SessionAck {
            status: AckStatus::Ok,
            codec: codec_bit(codec),
            width,
            height,
            refresh_mhz: fps_mhz,
            bitrate_kbps,
            audio_channels: channels,
            nonce: req.nonce,
            host: platform::HOST_KIND,
            features,
            video: picture,
            permissions: permissions.bits(),
        };
        self.ack(&peer, ack);
        video.open();
        self.active = Some(Active {
            peer,
            req,
            ack,
            video,
            audio,
            display,
            started: Instant::now(),
            bitrate: crate::bitrate::BitrateController::new(bitrate_kbps, cfg.adaptive_bitrate),
            fec: FecPolicy::DEFAULT,
            lan_shards,
            recipients,
            permissions,
            agent: agent_run,
        });
        self.platform.started(&req);
    }

    /// Whether `peer` may have the host now: only if it may see the screen
    /// (and start the app it asks for); agents only where agents are
    /// allowed, never over a person; and a running session is taken over
    /// only as the rules and its permissions say. `Err` means refused, and
    /// the client has been told.
    fn admit(
        &mut self,
        peer: &Arc<Peer>,
        req: &SessionStart,
        role: Role,
        permissions: Permissions,
        cfg: &HostConfig,
    ) -> Result<(), ()> {
        let agent = role == Role::Agent;
        if agent && (!cfg.agents || !permissions.allows(permission::VIEW)) {
            tracing::warn!(
                client = self.name_of(peer),
                "an agent's session refused: agents are off here, or this one may not see"
            );
            self.refuse(peer, req, AckStatus::AgentNotAllowed);
            return Err(());
        }
        if !permissions.allows(permission::VIEW) {
            tracing::warn!(
                client = self.name_of(peer),
                "a session refused: this device may not see the screen"
            );
            self.refuse(peer, req, AckStatus::NotAllowed);
            return Err(());
        }
        if req.app != control::app::DESKTOP && !permissions.allows(permission::LAUNCH) {
            tracing::warn!(
                client = self.name_of(peer),
                app = req.app,
                "a session refused: this device may not start apps"
            );
            self.refuse(peer, req, AckStatus::AppNotAllowed);
            return Err(());
        }
        self.platform.claim();
        if let Some(a) = &self.active {
            if a.peer.id() != peer.id() {
                match (a.agent.is_some(), agent) {
                    // A person's session is never an agent's to take.
                    (false, true) => {
                        tracing::info!(
                            "an agent asked for the host while a person streams; refused"
                        );
                        self.refuse(peer, req, AckStatus::AgentNotAllowed);
                        return Err(());
                    }
                    // A person always takes over from an agent.
                    (true, false) => tracing::info!("a person is taking over from the agent"),
                    _ if !cfg.allow_takeover => {
                        self.refuse(peer, req, AckStatus::Busy);
                        return Err(());
                    }
                    (false, false) if !permissions.allows(permission::TAKE_OVER) => {
                        tracing::info!(
                            client = self.name_of(peer),
                            "a device that may not take over asked while another streams; refused"
                        );
                        self.refuse(peer, req, AckStatus::Busy);
                        return Err(());
                    }
                    _ => tracing::info!("another client is taking over the session"),
                }
                send_control(
                    &self.endpoint,
                    &a.peer,
                    Control::SessionEnd(EndReason::Replaced),
                );
                // Not to the one starting this session (a watcher who now
                // streams): it is the same peer, and would end its new one.
                self.end_watchers_except(EndReason::Replaced, peer.id());
            }
        }
        Ok(())
    }

    /// Share the clipboard with the session's client, the ways it may go.
    /// False if it could not be started.
    fn start_clipboard(
        &self,
        peer: &Arc<Peer>,
        bitrate_kbps: u32,
        directions: pingpong_clipboard::Directions,
    ) -> bool {
        let (endpoint, to) = (self.endpoint.clone(), peer.clone());
        let clip = Clip::start(bitrate_kbps, self.name_of(peer), directions, move |p| {
            let _ = endpoint.send(&to, p);
        });
        let started = clip.is_some();
        *self.shared.clip.lock() = clip;
        started
    }

    fn name_of(&self, peer: &Peer) -> String {
        self.clients
            .lock()
            .name_of(&peer.public().x25519)
            .unwrap_or("unknown")
            .to_string()
    }

    /// A person asks to watch the agent's session: the same picture, from
    /// its next keyframe.
    fn watch(&mut self, peer: Arc<Peer>, req: SessionStart, role: Role, permissions: Permissions) {
        let name = self.name_of(&peer);
        if role == Role::Person && !permissions.allows(permission::VIEW | permission::WATCH) {
            tracing::warn!(
                client = name,
                "a watch refused: this device may not watch agents"
            );
            self.send_refusal(&peer, &req, AckStatus::NotAllowed);
            return;
        }
        // Refused without remembering it (as `refuse` would): the agent's
        // session may start any moment, and the watcher asks again.
        let Some(a) = self.active.as_mut() else {
            self.send_refusal(&peer, &req, AckStatus::NothingToWatch);
            return;
        };
        let Some(run) = a.agent.as_mut() else {
            self.send_refusal(&peer, &req, AckStatus::NothingToWatch);
            return;
        };
        // Agents watch nobody; and nobody watches themselves.
        if role == Role::Agent || a.peer.id() == peer.id() {
            self.send_refusal(&peer, &req, AckStatus::NothingToWatch);
            return;
        }
        // The agent's picture, with the watcher's own permissions.
        let ack = SessionAck {
            nonce: req.nonce,
            audio_channels: 0,
            permissions: permissions.bits(),
            ..a.ack
        };
        match run.watchers.iter_mut().find(|w| w.peer.id() == peer.id()) {
            Some(w) if w.req == req => {}
            Some(w) => {
                w.req = req;
                a.video.command(VideoCmd::Idr);
            }
            None => {
                tracing::info!(client = name, "watching the agent's session");
                run.watchers.push(Watcher {
                    peer: peer.clone(),
                    req,
                    permissions,
                });
                a.recipients.write().push(peer.clone());
                self.shared.watchers.lock().push(peer.id());
                // The watcher decodes from a keyframe.
                a.video.command(VideoCmd::Idr);
                run.last_state = None;
            }
        }
        self.ack(&peer, ack);
    }

    /// Tell every watcher the session is over, and forget them.
    fn end_watchers(&mut self, reason: EndReason) {
        self.end_watchers_except(reason, 0);
    }

    fn end_watchers_except(&mut self, reason: EndReason, except: PeerId) {
        let Some(run) = self.active.as_mut().and_then(|a| a.agent.as_mut()) else {
            return;
        };
        for w in run.watchers.drain(..) {
            if w.peer.id() != except {
                send_control(&self.endpoint, &w.peer, Control::SessionEnd(reason));
            }
        }
        self.shared.watchers.lock().clear();
    }

    fn drop_watcher(&mut self, peer: PeerId) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        let Some(run) = a.agent.as_mut() else { return };
        let before = run.watchers.len();
        run.watchers.retain(|w| w.peer.id() != peer);
        if run.watchers.len() == before {
            return;
        }
        tracing::info!("a watcher left the agent's session");
        a.recipients.write().retain(|p| p.id() != peer);
        self.shared.watchers.lock().retain(|&p| p != peer);
        if run.controller == Some(peer) {
            run.controller = None;
        }
        run.last_state = None;
    }

    /// A client's permissions changed: its session, or its watching, follows
    /// at once, and it is told.
    fn permissions_changed(&mut self, key: &str, permissions: Permissions) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        if a.peer.public().to_b64().0 == key {
            tracing::info!(permissions = ?permissions.names(), "the session's client's permissions changed");
            if !permissions.allows(permission::VIEW) {
                tracing::info!("the client may no longer see the screen; ending its session");
                self.teardown(Some(EndReason::NotAllowed));
                return;
            }
            a.permissions = permissions;
            self.shared
                .client_permissions
                .store(permissions.bits(), Ordering::Release);
            if let Some(clip) = self.shared.clip.lock().as_ref() {
                clip.set_directions(clip_directions(permissions));
            }
            if let Some(run) = a.agent.as_mut() {
                run.permissions = permissions;
                // `agent_tick` works out who drives, and tells them.
                run.last_state = None;
            } else if let Some(input) = self.shared.input.lock().as_mut() {
                input.allow(permissions);
            }
            send_control(
                &self.endpoint,
                &a.peer,
                Control::Permissions(permissions.bits()),
            );
            return;
        }
        let Some(run) = a.agent.as_mut() else { return };
        let Some(w) = run
            .watchers
            .iter_mut()
            .find(|w| w.peer.public().to_b64().0 == key)
        else {
            return;
        };
        tracing::info!(permissions = ?permissions.names(), "a watcher's permissions changed");
        w.permissions = permissions;
        let (peer, id) = (w.peer.clone(), w.peer.id());
        if !permissions.allows(permission::VIEW | permission::WATCH) {
            send_control(
                &self.endpoint,
                &peer,
                Control::SessionEnd(EndReason::NotAllowed),
            );
            self.drop_watcher(id);
            return;
        }
        run.last_state = None;
        send_control(
            &self.endpoint,
            &peer,
            Control::Permissions(permissions.bits()),
        );
    }

    /// A watcher (or the web UI, `peer` 0) acts on the agent.
    fn agent_control(&mut self, peer: PeerId, op: u8) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        let Some(run) = a.agent.as_mut() else { return };
        if peer != 0 && !run.watchers.iter().any(|w| w.peer.id() == peer) {
            return;
        }
        match op {
            agent_control::TAKE_OVER if peer != 0 => {
                let may = run.watchers.iter().any(|w| {
                    w.peer.id() == peer
                        && w.permissions
                            .allows_any(permission::KEYBOARD | permission::MOUSE)
                });
                if !may {
                    tracing::info!(
                        "a watcher that may use neither the keyboard nor the mouse cannot take over"
                    );
                    return;
                }
                if run.controller != Some(peer) {
                    tracing::info!("a watcher took over from the agent");
                    run.controller = Some(peer);
                }
            }
            agent_control::HAND_BACK => {
                if run.controller.take().is_some() {
                    tracing::info!("handed back to the agent");
                }
            }
            agent_control::PAUSE => {
                if !run.paused {
                    tracing::info!("the agent is paused");
                    run.paused = true;
                }
            }
            agent_control::RESUME => {
                if run.paused {
                    tracing::info!("the agent is resumed");
                    run.paused = false;
                }
            }
            agent_control::STOP => {
                tracing::info!("the agent was stopped");
                self.teardown(Some(EndReason::Quit));
                return;
            }
            _ => return,
        }
        run.last_state = None;
    }

    /// An agent's session, now and then: look at the host, decide whose
    /// input reaches it, and tell the agent and its watchers.
    fn agent_tick(&mut self, now: Instant) {
        let hold_secs = self.config.read().agent_local_input_hold_secs;
        let Some(a) = self.active.as_mut() else {
            return;
        };
        let Some(run) = a.agent.as_mut() else { return };
        if now.duration_since(run.last_check) >= AGENT_CHECK_EVERY {
            run.last_check = now;
            let secure = platform::secure_screen();
            if secure != run.secure {
                tracing::info!(secure, "the host's secure screen");
                run.secure = secure;
            }
            let grace_over = a.started + START_GRACE;
            if hold_secs > 0 && now > grace_over {
                // Input more recent than the last the agent injected (and than
                // the session's start) is someone else's.
                let injected = self.shared.input.lock().as_ref().and_then(|i| {
                    use pingpong_input::InputSink;
                    i.last_injected.max(i.sink.last_sent())
                });
                let baseline = injected.map_or(grace_over, |t| t.max(grace_over));
                let since_baseline = now.duration_since(baseline);
                if let Some(idle) = platform::host_input_idle() {
                    if idle + INJECTION_MARGIN < since_baseline {
                        // Where the session turned the host's own monitors
                        // off (a Windows host, isolated), the person there
                        // sees nothing to use: the agent gives the host back.
                        if platform::isolates_host_displays(&a.req, &self.config.read()) {
                            tracing::info!(
                                "someone is using the host, whose monitors the \
                                    agent's session turned off; ending it"
                            );
                            self.teardown(Some(EndReason::HostInUse));
                            return;
                        }
                        let until = now.checked_sub(idle).unwrap_or(now)
                            + Duration::from_secs(hold_secs as u64);
                        if run.local_until.is_none_or(|t| t < now) {
                            tracing::info!("someone is using the host; holding the agent's input");
                        }
                        run.local_until = Some(until);
                    }
                }
            }
            // Watchers gone quiet have left.
            let gone: Vec<PeerId> = run
                .watchers
                .iter()
                .filter(|w| self.endpoint.since_rx(&w.peer).unwrap_or_default() > CLIENT_GRACE)
                .map(|w| w.peer.id())
                .collect();
            for peer in gone {
                self.drop_watcher(peer);
            }
        }
        let Some(a) = self.active.as_mut() else {
            return;
        };
        let Some(run) = a.agent.as_mut() else { return };
        let state = run.state(now);
        // Whose input goes in, as far as it may: a watcher who took over,
        // else the agent unless something holds it.
        let (owner, held, allowed) = match run
            .controller
            .and_then(|c| run.watchers.iter().find(|w| w.peer.id() == c))
        {
            Some(w) => (w.peer.id(), false, w.permissions),
            None => (a.peer.id(), !state.agent_may_act(), run.permissions),
        };
        if let Some(input) = self.shared.input.lock().as_mut() {
            if input.peer != owner || input.held != held {
                input.release_all();
                if input.peer != owner {
                    // The owner's gate goes aside, and the new owner's comes
                    // back, with what it has sent (and was dropped) so far.
                    let theirs = input.others.remove(&owner).unwrap_or_default();
                    let mine = std::mem::replace(&mut input.gate, theirs);
                    input.others.insert(input.peer, mine);
                }
                input.peer = owner;
                input.held = held;
            }
            input.allow(allowed);
        }
        let due = match run.last_state {
            Some((last, at)) => last != state || now.duration_since(at) >= AGENT_STATE_EVERY,
            None => true,
        };
        if due {
            if run.last_state.is_none_or(|(last, _)| last != state) {
                tracing::info!(
                    flags = state.flags,
                    watchers = state.watchers,
                    "agent state"
                );
            }
            run.last_state = Some((state, now));
            send_control(&self.endpoint, &a.peer, Control::AgentState(state));
            for w in &run.watchers {
                send_control(&self.endpoint, &w.peer, Control::AgentState(state));
            }
        }
    }

    /// Stop the stream: input released, video and audio stopped. Returns the
    /// session's display, for the caller to keep or give back.
    fn stop_stream(&mut self) -> Option<platform::Display> {
        self.shared.active_peer.store(0, Ordering::Release);
        self.shared.client_permissions.store(0, Ordering::Release);
        self.shared.agent_session.store(false, Ordering::Release);
        self.shared.watchers.lock().clear();
        *self.shared.video.lock() = None;
        // Out of the lock first: stopping waits for its thread.
        let clip = self.shared.clip.lock().take();
        drop(clip);
        if let Some(mut input) = self.shared.input.lock().take() {
            input.release_all();
            self.platform.input_done(&input.sink);
        }
        *self.status.lock() = None;
        let a = self.active.take()?;
        a.video.stop();
        if let Some(audio) = a.audio {
            audio.stop();
        }
        tracing::info!(secs = a.started.elapsed().as_secs(), "session stopped");
        Some(a.display)
    }

    fn teardown(&mut self, notify: Option<EndReason>) {
        if let (Some(reason), Some(a)) = (notify, &self.active) {
            send_control(&self.endpoint, &a.peer, Control::SessionEnd(reason));
        }
        self.end_watchers(notify.unwrap_or(EndReason::Quit));
        let ran = self.active.is_some();
        let display = self.stop_stream();
        self.platform.ended(
            display,
            ran,
            notify == Some(EndReason::Shutdown),
            &self.shared,
        );
    }

    fn on_loss(&mut self, peer: PeerId, report: LossReport) {
        let Some(a) = &mut self.active else { return };
        if a.peer.id() != peer {
            return;
        }
        if let Some(kbps) = a.bitrate.on_report(&report) {
            tracing::info!(
                kbps,
                requested = a.ack.bitrate_kbps,
                loss_pct = 100.0 * (1.0 - report.received as f64 / report.expected.max(1) as f64),
                frames_lost = report.frames_lost,
                rtt_ms = report.rtt_us as f64 / 1000.0,
                "adapting bitrate"
            );
            a.video
                .command(VideoCmd::Bitrate(kbps.saturating_mul(1000)));
        }
        // Most of the video lost while in LAN-sized shards: this path's MTU
        // is smaller than Ethernet's (a VPN between two private networks,
        // say). Path-sized datagrams from now on.
        let lost =
            report.expected >= 20 && report.received.min(report.expected) * 2 < report.expected;
        if a.lan_shards && lost {
            tracing::warn!(
                received = report.received,
                expected = report.expected,
                "LAN-sized datagrams are not getting through; back to path-sized ones"
            );
            a.lan_shards = false;
            a.video.set_lan_shards(false);
        }
        let fec = a.bitrate.fec();
        if fec != a.fec {
            tracing::info!(
                parity_pct = fec.percent,
                min_parity = fec.min_parity,
                link_loss_pct = 100.0 * a.bitrate.link_loss(),
                "adapting FEC to the link's loss"
            );
            a.fec = fec;
            a.video.set_fec(fec);
        }
        self.last_loss = report;
    }

    fn tick(&mut self) {
        let Some(a) = self.active.as_mut() else {
            self.platform.idle();
            return;
        };

        if !a.video.is_running() {
            tracing::error!("video pipeline stopped; ending the session");
            self.teardown(Some(EndReason::Error));
            return;
        }
        let quiet = self.endpoint.since_rx(&a.peer).unwrap_or_default();
        if quiet > CLIENT_GRACE {
            tracing::info!("client went silent; ending the session");
            self.teardown(None);
            return;
        }
        self.repeat_due = None;
        if let Some(input) = self.shared.input.lock().as_mut() {
            if quiet > INPUT_RELEASE_AFTER {
                // Only what is held: a no-op once released.
                input.release_all();
            } else {
                self.repeat_due = input.repeat_tick(Instant::now());
            }
        }
        let second = self.last_stats.elapsed() >= Duration::from_secs(1);
        let mut out = self.platform.tick(&mut a.display, Instant::now(), second);
        if second {
            // The pointer is in the picture. A client older than
            // `features::POINTER_IN_PICTURE` (or one watching an agent,
            // joining later) hears it this way, once a second: control
            // messages are not retransmitted.
            out.push(Control::CursorState(control::CursorState::IN_PICTURE));
            // An HDR stream's metadata, the same way.
            if a.ack.video & control::video::HDR != 0 {
                out.push(Control::HdrMetadata(self.platform.hdr_metadata(&a.display)));
            }
        }
        for msg in out {
            send_control(&self.endpoint, &a.peer, msg);
            for w in a.agent.iter().flat_map(|r| r.watchers.iter()) {
                send_control(&self.endpoint, &w.peer, msg);
            }
        }
        if a.agent.is_some() {
            self.agent_tick(Instant::now());
        }
        if second {
            self.report_second();
        }
    }

    /// Once a second while streaming: the session's status for the web UI
    /// and Pong's window, and the stream's line in the log.
    fn report_second(&mut self) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        self.last_stats = Instant::now();
        let v = &a.video.stats;
        let s = &a.video.send_stats;
        let take = |c: &std::sync::atomic::AtomicU64| c.swap(0, Ordering::Relaxed);
        let hist = v.encode_us.lock().take();
        let host_ms = hist
            .as_ref()
            .map(|h| h.percentiles().0 as f64 / 1000.0)
            .unwrap_or(0.0);
        let lat = hist.map(|h| h.report()).unwrap_or_default();
        let l = self.last_loss;
        // Names first: the status below locks the client list too, and a
        // lock taken twice in one statement is a deadlock.
        let agent = a.agent.as_ref().map(|run| {
            let clients = self.clients.lock();
            let name = |p: &Peer| {
                clients
                    .name_of(&p.public().x25519)
                    .unwrap_or("unknown")
                    .to_string()
            };
            crate::host::AgentStatus {
                access: Access::of(run.permissions).name().to_string(),
                permissions: run.permissions,
                flags: run.state(Instant::now()).flags,
                watchers: run.watchers.iter().map(|w| name(&w.peer)).collect(),
                controller: run
                    .controller
                    .and_then(|c| run.watchers.iter().find(|w| w.peer.id() == c))
                    .map(|w| name(&w.peer)),
            }
        });
        *self.status.lock() = Some(SessionStatus {
            client: self
                .clients
                .lock()
                .name_of(&a.peer.public().x25519)
                .unwrap_or("unknown")
                .to_string(),
            width: a.ack.width,
            height: a.ack.height,
            fps: (a.ack.refresh_mhz + 500) / 1000,
            codec: codec_name(a.ack.codec).to_string(),
            bitrate_kbps: a.bitrate.current_kbps(),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_sub(a.started.elapsed().as_secs()),
            encoded_fps: v.encoded.load(Ordering::Relaxed),
            mbps: s.bytes.load(Ordering::Relaxed) as f64 * 8.0 / 1e6,
            host_latency_ms: host_ms,
            recoveries: v.recoveries.load(Ordering::Relaxed),
            idrs: v.idrs.load(Ordering::Relaxed),
            client_loss_pct: if l.expected > 0 {
                100.0 * l.expected.saturating_sub(l.received) as f64 / l.expected as f64
            } else {
                0.0
            },
            rtt_ms: l.rtt_us as f64 / 1000.0,
            agent,
        });
        tracing::info!(
            fps = take(&v.encoded),
            new = take(&v.captured),
            repeat = take(&v.repeated),
            idr = take(&v.idrs),
            rfi = take(&v.recoveries),
            mbps = format!("{:.1}", take(&s.bytes) as f64 * 8.0 / 1e6),
            pkts = take(&s.packets),
            send_fail = take(&s.failed),
            pace_ms = take(&s.pace_wait_us) / 1000,
            audio_pkts = a.audio.as_ref().map_or(0, |x| take(&x.packets)),
            "stream {lat}"
        );
    }
}

/// The wire's codec bit for `codec`.
fn codec_bit(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => control::codec::H264,
        Codec::Hevc => control::codec::HEVC,
        Codec::Av1 => control::codec::AV1,
    }
}

/// A codec bit's name, for the status.
fn codec_name(bit: u8) -> &'static str {
    match bit {
        control::codec::HEVC => "HEVC",
        control::codec::AV1 => "AV1",
        _ => "H.264",
    }
}
