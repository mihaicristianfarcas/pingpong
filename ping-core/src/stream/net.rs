//! The network thread: everything a session receives, and its timers.
//!
//! One loop on one thread, so nothing here needs a lock of its own: receive a
//! datagram (or time out after 4 ms), hand it to the handler for its kind,
//! then run the timers for the session's phase -- racing the handshake,
//! negotiating the session, then streaming (reports, probes, pings, and
//! noticing a host that went quiet).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_audio::player::Player;
use pingpong_proto::audio::{AudioDepacketizer, AudioPacket};
use pingpong_proto::control::{
    self, AckStatus, Control, EndReason, LossReport, SessionAck, SessionStart,
};
use pingpong_proto::header::{Header, Kind};
use pingpong_proto::reassemble::Reassembler;
use pingpong_proto::video::{split_prefix, FrameGate, FrameType, Request, Verdict};
use pingpong_proto::{clock, HEADER_LEN};
use pingpong_transport::Received;

use super::messages::{end_text, held_back, host_warning, refusal, watch_notice, watcher_end_text};
use super::quality::{LossCounter, NetQuality};
use super::{send_control, Codec, Ctx, Event, FrameTiming, StreamSettings, VideoOut};

/// How long to wait for the host before giving up on the session. The host
/// acks only once the virtual display is up, which for a mode it has never
/// shown can take several seconds, and far longer where a Windows host keeps
/// its own monitor in the desktop and that monitor is asleep or switched
/// off: Windows brings it up first (12 to 38 s measured).
const GIVE_UP_AFTER: Duration = Duration::from_secs(45);
const START_RETRY: Duration = Duration::from_millis(250);
const PING_EVERY: Duration = Duration::from_millis(500);
/// Nothing from the host for this long mid-stream: say the connection is
/// interrupted (the host sends at least a Pong every 500 ms).
const INTERRUPTED_AFTER: Duration = Duration::from_secs(1);
/// While interrupted, look for the host afresh on every known path this
/// often: a fresh handshake raced across them finds it on a new path (the
/// Mac changed networks) or after Pong restarted.
const RERACE_EVERY: Duration = Duration::from_secs(2);
/// Give up after this long without the host. Pong keeps the session (and its
/// virtual display) as long, so coming back within it resumes the stream.
const LOST_AFTER: Duration = Duration::from_secs(20);
const REPORT_EVERY: Duration = Duration::from_secs(1);
const AUDIO_LOG_EVERY: Duration = Duration::from_secs(2);
/// How often the tunnel's timers (keepalives, rekeys) are driven.
const TUNNEL_TICK_EVERY: Duration = Duration::from_millis(100);
/// Frames the reassembler holds partially at once.
const TRACKED_FRAMES: usize = 8;

/// A new handshake along every path this often while none has answered: a
/// lost initiation (or one that met a busy host) costs a second, not
/// WireGuard's five. Far longer than any round trip the stream could use.
const RACE_EVERY: Duration = Duration::from_secs(1);
/// How long local candidates get to answer before remote ones are tried too.
/// Generous: the first packet after a long idle can be slow on Wi-Fi.
const REMOTE_AFTER: Duration = Duration::from_millis(300);
/// A session on a remote path probes the local one this often...
const LAN_PROBE_EVERY: Duration = Duration::from_secs(3);
/// ...this many times.
const LAN_PROBES: u32 = 5;

/// How long a watcher waits for the agent's session to start (within
/// `GIVE_UP_AFTER`).
const WATCH_WAIT: Duration = Duration::from_secs(25);
/// How long a watcher sees who drives, after it changes; and how long the
/// person streaming sees what the host's permissions hold back.
const WATCH_NOTICE_FOR: Duration = Duration::from_secs(6);

/// The network thread's body: runs until the stream stops or ends.
pub(super) fn run(ctx: Arc<Ctx>, video: Box<dyn VideoOut>) {
    crate::priority::latency_critical();
    NetLoop::new(ctx, video).run();
}

enum Phase {
    Handshake {
        since: Instant,
        last_initiate: Instant,
    },
    Negotiating {
        since: Instant,
        last_sent: Instant,
    },
    Streaming,
}

/// What the loop does after a datagram has been handled.
enum Next {
    /// Run the timers, then receive again.
    Timers,
    /// Receive again straight away.
    Receive,
    /// The session is over.
    Stop,
}

/// The handshake in flight: one initiation, raced along every candidate.
struct Racing {
    packets: Vec<Vec<u8>>,
    started: Instant,
    sent: Vec<SocketAddr>,
}

/// An interruption in progress: the host has gone quiet mid-stream.
struct Interruption {
    racing: Option<Racing>,
    last_race: Instant,
    /// The countdown last shown, in seconds.
    shown: u64,
}

/// Probing the local network while the session runs on a remote path.
#[derive(Default)]
struct LanProbe {
    last: Option<Instant>,
    sent: u32,
}

/// The largest gaps between audio packets, as sent (the host's clock) and as
/// received, per audio log: tells a stall on the network from one at the host.
#[derive(Default)]
struct AudioGaps {
    last_sent: u32,
    last_arrived: u32,
    max_sent: u32,
    max_arrived: u32,
}

impl AudioGaps {
    fn packet(&mut self, sent_us: u32, arrived_us: u32) {
        if self.last_arrived != 0 {
            self.max_sent = self.max_sent.max(sent_us.wrapping_sub(self.last_sent));
            self.max_arrived = self
                .max_arrived
                .max(arrived_us.wrapping_sub(self.last_arrived));
        }
        self.last_sent = sent_us;
        self.last_arrived = arrived_us;
    }
}

/// `PING_TEST_LOSS=N`: drop N% of incoming media datagrams (loss-recovery
/// tests without a network impairment tool). `N:B` drops in bursts of B.
struct TestLoss {
    percent: u32,
    burst: u32,
    left_in_burst: u32,
    rng: u32,
}

impl TestLoss {
    fn from_env() -> Option<TestLoss> {
        let v = std::env::var("PING_TEST_LOSS").ok()?;
        let (p, b) = v.split_once(':').unwrap_or((&v, "1"));
        Some(TestLoss {
            percent: p.parse().ok()?,
            burst: b.parse().ok()?,
            left_in_burst: 0,
            rng: 0x9E37_79B9,
        })
    }

    /// Whether to drop the next media datagram.
    fn drop_next(&mut self) -> bool {
        if self.left_in_burst > 0 {
            self.left_in_burst -= 1;
            return true;
        }
        // xorshift32
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        if self.rng % 10_000 < self.percent * 100 / self.burst.max(1) {
            self.left_in_burst = self.burst.max(1) - 1;
            return true;
        }
        false
    }
}

/// Everything the network thread keeps between datagrams.
struct NetLoop {
    ctx: Arc<Ctx>,
    video: Box<dyn VideoOut>,
    /// The frame gate's clock starts here.
    epoch: Instant,
    /// Ties the host's `SessionAck` to this session's `SessionStart`.
    nonce: u32,
    start: SessionStart,
    phase: Phase,
    racing: Racing,

    reassembler: Reassembler,
    gate: FrameGate,
    /// When each tracked frame's first datagram arrived: (frame id, µs).
    first_seen: VecDeque<(u32, u32)>,
    loss: LossCounter,
    /// The gate counts lost frames for the session; reports carry the
    /// interval's.
    lost_reported: u64,
    /// The (codec, width, height, video) the decoder is configured for.
    configured: Option<(u8, u16, u16, u8)>,

    audio: Option<Player>,
    depacketizer: AudioDepacketizer,
    audio_packets: Vec<AudioPacket>,
    audio_gaps: AudioGaps,
    last_audio_log: Instant,
    /// The longest this loop spent away from the socket (handling one
    /// datagram), per audio log: tells a stall here from a gap on the network.
    busy_max: Duration,

    last_ping: Instant,
    ping_id: u32,
    last_report: Instant,
    /// This thread's CPU time at the last report.
    net_cpu: Option<Duration>,
    last_tick: Instant,
    lan_probe: LanProbe,
    interruption: Option<Interruption>,
    quality: NetQuality,
    /// The host's warning is shown once (it repeats it in case one is lost).
    host_warned: bool,
    /// A watcher's notice about who drives, shown for a while.
    notice_until: Option<Instant>,
    /// A watcher told the agent is not there yet.
    waiting_for_agent: bool,
    /// Why the host is about to refuse the session, if it said.
    refusal_reason: Option<u8>,
    /// Sharing the clipboard, when the host agreed to.
    clip: Option<pingpong_clipboard::ClipSync>,
    /// What this device may do on the host, as it last said
    /// (`permission::*`; 0 until it has said).
    permissions: u16,
    test_loss: Option<TestLoss>,
}

impl NetLoop {
    fn new(ctx: Arc<Ctx>, video: Box<dyn VideoOut>) -> NetLoop {
        let epoch = Instant::now();
        let nonce = rand_core::RngCore::next_u32(&mut rand_core::OsRng);
        let now = Instant::now();
        let phase = Phase::Handshake {
            since: now,
            last_initiate: now,
        };
        let net_cpu = crate::priority::thread_cpu_time();
        let test_loss = TestLoss::from_env();
        let racing = start_race(&ctx);
        let start = session_start(&ctx.settings, nonce);
        NetLoop {
            video,
            epoch,
            nonce,
            start,
            phase,
            racing,
            reassembler: Reassembler::new(TRACKED_FRAMES),
            gate: FrameGate::new(0),
            first_seen: VecDeque::with_capacity(TRACKED_FRAMES),
            loss: LossCounter::new(),
            lost_reported: 0,
            configured: None,
            audio: None,
            depacketizer: AudioDepacketizer::new(),
            audio_packets: Vec::new(),
            audio_gaps: AudioGaps::default(),
            last_audio_log: now,
            busy_max: Duration::ZERO,
            last_ping: now,
            ping_id: 0,
            last_report: now,
            net_cpu,
            last_tick: now,
            lan_probe: LanProbe::default(),
            interruption: None,
            quality: NetQuality::default(),
            host_warned: false,
            notice_until: None,
            waiting_for_agent: false,
            refusal_reason: None,
            clip: None,
            permissions: 0,
            test_loss,
            ctx,
        }
    }

    fn run(mut self) {
        let mut buf = vec![0u8; 65536];
        let mut received_at: Option<Instant> = None;
        while !self.ctx.stop.load(Ordering::Relaxed) {
            if let Some(t) = received_at.take() {
                self.busy_max = self.busy_max.max(t.elapsed());
            }
            let got = self.ctx.endpoint.recv(&mut buf);
            if matches!(got, Ok(Received::Data(..))) {
                received_at = Some(Instant::now());
            }
            let next = match got {
                Ok(Received::Data(_, len)) => self.on_datagram(&buf[..len]),
                Ok(Received::Foreign(from, len)) => {
                    self.ctx.stun.on_datagram(from, &buf[..len]);
                    Next::Timers
                }
                Ok(_) => Next::Timers,
                Err(e) => {
                    tracing::warn!(error = %e, "receive failed");
                    std::thread::sleep(Duration::from_millis(20));
                    Next::Timers
                }
            };
            match next {
                Next::Timers => {}
                Next::Receive => continue,
                Next::Stop => break,
            }
            if self.on_timers().is_break() {
                break;
            }
        }
    }

    /// Tell the app how the session ended, and stop.
    fn end(&self, reason: String, error: bool) {
        (self.ctx.events)(Event::Ended { reason, error });
        self.ctx.stop.store(true, Ordering::Relaxed);
    }

    fn emit(&self, event: Event) {
        (self.ctx.events)(event)
    }

    fn streaming(&self) -> bool {
        matches!(self.phase, Phase::Streaming)
    }

    // Receiving

    fn on_datagram(&mut self, packet: &[u8]) -> Next {
        let Ok(h) = Header::decode(packet) else {
            return Next::Receive;
        };
        if let Some(test_loss) = &mut self.test_loss {
            if matches!(h.kind, Kind::Video | Kind::Audio) && test_loss.drop_next() {
                return Next::Receive;
            }
        }
        match h.kind {
            Kind::Video => self.on_video(&h, packet),
            Kind::Audio => {
                self.on_audio(&h, packet);
                Next::Timers
            }
            Kind::Control => self.on_control(&h, &packet[HEADER_LEN..h.total_len as usize]),
            _ => Next::Timers,
        }
    }

    fn on_video(&mut self, h: &Header, packet: &[u8]) -> Next {
        let now = clock::now_us();
        self.loss.packet(h, Instant::now());
        self.ctx.stats.packet(packet.len());
        if !self.first_seen.iter().any(|&(id, _)| id == h.frame_id) {
            if self.first_seen.len() >= TRACKED_FRAMES {
                self.first_seen.pop_front();
            }
            self.first_seen.push_back((h.frame_id, now));
        }
        let Some(frame) = self.reassembler.push(packet) else {
            return Next::Receive;
        };
        let ty = FrameType::from_flags(frame.keyframe, frame.recovery);
        let (verdict, req) = self.gate.on_frame(frame.frame_id, ty, now_us64(self.epoch));
        if let Some(r) = req {
            request(&self.ctx, r);
        }
        self.ctx.stats.frame_received(verdict == Verdict::Decode);
        if verdict != Verdict::Decode {
            self.reassembler.recycle(frame.data);
            return Next::Receive;
        }
        let Some((host_us, bitstream)) = split_prefix(&frame.data) else {
            self.reassembler.recycle(frame.data);
            return Next::Receive;
        };
        let first_packet_us = self
            .first_seen
            .iter()
            .find(|&&(id, _)| id == frame.frame_id)
            .map(|&(_, t)| t)
            .unwrap_or(now);
        let timing = FrameTiming {
            frame_id: frame.frame_id,
            captured_us: frame.capture_ts_us,
            host_us,
            first_packet_us,
            reassembled_us: clock::now_us(),
        };
        self.ctx.stats.host_latency(host_us);
        self.ctx
            .stats
            .network_latency(timing.reassembled_us.wrapping_sub(first_packet_us));
        if let Err(e) = self.video.decode(bitstream, timing) {
            tracing::warn!(error = %e, "decode failed; asking for an IDR");
            self.ctx.stats.decode_error();
            request(&self.ctx, Request::Idr);
        }
        // The decoder has its own copy now.
        self.reassembler.recycle(frame.data);
        Next::Timers
    }

    fn on_audio(&mut self, h: &Header, packet: &[u8]) {
        let Some(player) = &self.audio else {
            return;
        };
        if h.fragment_idx < 4 {
            self.audio_gaps.packet(h.capture_ts_us, clock::now_us());
        }
        self.depacketizer.push(packet, &mut self.audio_packets);
        for p in self.audio_packets.drain(..) {
            player.push(p);
        }
    }

    fn on_control(&mut self, h: &Header, body: &[u8]) -> Next {
        if pingpong_proto::clip::is_clip(body) {
            if let Some(c) = &self.clip {
                c.deliver(body);
            }
            return Next::Timers;
        }
        let Some(msg) = Control::decode(body) else {
            return Next::Receive;
        };
        match msg {
            Control::SessionAck(ack) if ack.nonce == self.nonce => return self.on_session_ack(ack),
            Control::SessionEnd(EndReason::NoSession) => {
                // The host restarted or lost our session: ask again.
                if self.streaming() {
                    tracing::info!("host has no session for us; renegotiating");
                    self.configured = None;
                    self.phase = Phase::Negotiating {
                        since: Instant::now(),
                        last_sent: Instant::now() - START_RETRY,
                    };
                }
            }
            Control::SessionEnd(reason) if self.ctx.settings.watch => {
                self.end(watcher_end_text(reason, &self.ctx.host_name), false);
                return Next::Stop;
            }
            Control::SessionEnd(reason) => {
                self.end(
                    end_text(reason, &self.ctx.host_name),
                    reason != EndReason::Quit,
                );
                return Next::Stop;
            }
            Control::CursorState(c) => self.emit(Event::Cursor(c)),
            Control::AgentState(state) => {
                let changed = self.ctx.agent_state.lock().replace(state) != Some(state);
                if changed {
                    self.emit(Event::Agent(state));
                    if self.ctx.settings.watch {
                        self.emit(Event::Notice(Some(watch_notice(
                            state,
                            &self.ctx.host_name,
                        ))));
                        self.notice_until = Some(Instant::now() + WATCH_NOTICE_FOR);
                    }
                }
            }
            Control::Rumble { index, low, high } => self.emit(Event::Rumble { index, low, high }),
            Control::HdrMetadata(m) => self.video.hdr_metadata(m),
            Control::Progress(step) if !self.streaming() => {
                use pingpong_proto::control::progress;
                let text = match step {
                    progress::DISPLAY => {
                        Some(format!("{} is setting up a display…", self.ctx.host_name))
                    }
                    progress::ENCODER => Some("Starting the video…".to_string()),
                    _ => None,
                };
                if let Some(text) = text {
                    self.emit(Event::Status(text));
                }
            }
            Control::HostWarning(code)
                if code == pingpong_proto::control::host_warning::CAPTURE_BLOCKED =>
            {
                self.refusal_reason = Some(code);
            }
            Control::HostWarning(code) if !self.host_warned => {
                self.host_warned = true;
                if let Some(text) = host_warning(code, &self.ctx.host_name) {
                    tracing::warn!("{text}");
                    self.emit(Event::Warning(Some(text)));
                }
            }
            Control::Permissions(bits) => self.on_permissions(bits),
            Control::RendezvousOffer { key, secret } => self.on_rendezvous_offer(key, secret),
            Control::Pong { sent_us, .. } => self.on_pong(sent_us, h.capture_ts_us),
            Control::Ping { id, sent_us } => send_control(&self.ctx, Control::Pong { id, sent_us }),
            _ => {}
        }
        Next::Timers
    }

    fn on_session_ack(&mut self, ack: SessionAck) -> Next {
        // Watching an agent that is still starting (its program, then its
        // session): wait.
        if ack.status == AckStatus::NothingToWatch && self.ctx.settings.watch {
            if let Phase::Negotiating { since, .. } = &self.phase {
                if since.elapsed() < WATCH_WAIT {
                    if !self.waiting_for_agent {
                        self.waiting_for_agent = true;
                        self.emit(Event::Status(format!(
                            "Waiting for the agent to start on {}…",
                            self.ctx.host_name
                        )));
                    }
                    return Next::Receive;
                }
            }
        }
        if ack.status != AckStatus::Ok {
            // The host's own reason, when it sent one.
            let why = self
                .refusal_reason
                .and_then(|code| host_warning(code, &self.ctx.host_name))
                .unwrap_or_else(|| {
                    refusal(ack.status, self.ctx.settings.watch, &self.ctx.host_name)
                });
            self.end(why, true);
            return Next::Stop;
        }
        let key = (ack.codec, ack.width, ack.height, ack.video);
        if self.configured != Some(key) {
            let Some(codec) = Codec::from_bit(ack.codec) else {
                self.end("The host chose an unknown codec".into(), true);
                return Next::Stop;
            };
            if let Err(e) =
                self.video
                    .configure(codec, ack.width as u32, ack.height as u32, ack.video)
            {
                self.end(format!("Cannot decode {}: {e}", codec.name()), true);
                return Next::Stop;
            }
            self.configured = Some(key);
            // A (re)negotiated session starts with an IDR.
            self.gate = FrameGate::new(now_us64(self.epoch));
            self.lost_reported = 0;
            self.gate.set_rtt(self.ctx.rtt_us.load(Ordering::Relaxed));
            self.reassembler = Reassembler::new(TRACKED_FRAMES);
            tracing::info!(
                codec = codec.name(),
                width = ack.width,
                height = ack.height,
                fps = ack.refresh_mhz as f64 / 1000.0,
                mbps = ack.bitrate_kbps / 1000,
                "session started"
            );
        }
        if !self.streaming() {
            // A (re)negotiated session restarts the host's audio sequence.
            self.audio = None;
            self.depacketizer = AudioDepacketizer::new();
            if ack.audio_channels > 0 {
                self.audio = start_audio(ack.audio_channels);
            }
            if let Some(p) = &self.audio {
                p.set_muted(self.ctx.audio_muted.load(Ordering::Relaxed));
            }
            *self.ctx.ack.lock() = Some(ack);
            self.ctx.stats.set_ack(ack);
            self.clip = None;
            if ack.features & control::features::CLIPBOARD != 0 {
                self.clip = Some(start_clipboard(&self.ctx, &ack));
            }
            self.emit(Event::Started(ack));
            self.phase = Phase::Streaming;
            self.permissions = 0;
            self.on_permissions(ack.permissions);
        }
        Next::Timers
    }

    /// What the host says this device may do here: the clipboard follows,
    /// and the person streaming hears what it holds back.
    fn on_permissions(&mut self, bits: u16) {
        if bits == 0 || bits == self.permissions {
            return;
        }
        self.permissions = bits;
        tracing::info!(
            permissions = ?pingpong_proto::permission::Permissions::from_bits(bits).names(),
            "what this device may do on the host"
        );
        if let Some(clip) = &self.clip {
            clip.set_directions(clip_directions(bits));
        }
        self.emit(Event::Permissions(bits));
        // A watcher hears who drives instead (`watch_notice`), an agent in
        // its own words (ping-agent).
        if self.ctx.settings.watch || self.ctx.settings.agent {
            return;
        }
        if let Some(text) = held_back(bits, &self.ctx.host_name) {
            tracing::info!("{text}");
            self.emit(Event::Notice(Some(text)));
            self.notice_until = Some(Instant::now() + WATCH_NOTICE_FOR);
        }
    }

    /// The host offers its rendezvous keys (so it can be found from the
    /// internet): keep them, and answer with ours.
    fn on_rendezvous_offer(&self, key: [u8; 32], secret: [u8; 32]) {
        let Some(dir) = &self.ctx.data_dir else {
            return;
        };
        let (x, _) = self.ctx.peer.public().to_b64();
        if let Ok(true) = crate::store::Hosts::load(dir).set_rendezvous(&x, key, secret) {
            tracing::info!("the host can now be reached from the internet");
        }
        if let Some(mine) = crate::wan::own_keys(dir) {
            send_control(&self.ctx, Control::RendezvousKey { key: mine.public() });
        }
    }

    /// The answer to one of our pings: a round trip, and a clock sample.
    fn on_pong(&mut self, sent_us: u32, host_us: u32) {
        let now = clock::now_us();
        self.ctx.stats.clock_sample(sent_us, host_us, now);
        let rtt = now.wrapping_sub(sent_us) as u64;
        let prev = self.ctx.rtt_us.load(Ordering::Relaxed);
        let smoothed = if prev == 0 { rtt } else { (prev * 7 + rtt) / 8 };
        self.ctx.rtt_us.store(smoothed, Ordering::Relaxed);
        self.ctx.rtt_floor_us.fetch_min(rtt, Ordering::Relaxed);
        self.ctx.stats.rtt(rtt as u32);
        self.gate.set_rtt(smoothed);
    }

    // Timers

    fn on_timers(&mut self) -> ControlFlow<()> {
        let now = Instant::now();
        if self.notice_until.is_some_and(|t| now >= t) && self.interruption.is_none() {
            self.notice_until = None;
            self.emit(Event::Notice(None));
        }
        if let Some(r) = self.gate.poll(now_us64(self.epoch)) {
            if self.streaming() {
                request(&self.ctx, r);
            }
        }
        if now.duration_since(self.last_tick) >= TUNNEL_TICK_EVERY {
            self.last_tick = now;
            self.ctx.endpoint.tick();
        }
        match self.phase {
            Phase::Handshake {
                since,
                last_initiate,
            } => self.handshake_tick(now, since, last_initiate)?,
            Phase::Negotiating { since, last_sent } => {
                self.negotiating_tick(now, since, last_sent)?
            }
            Phase::Streaming => self.streaming_tick(now)?,
        }
        if self.last_ping.elapsed() >= PING_EVERY && !matches!(self.phase, Phase::Handshake { .. })
        {
            self.last_ping = now;
            self.ping_id = self.ping_id.wrapping_add(1);
            send_control(
                &self.ctx,
                Control::Ping {
                    id: self.ping_id,
                    sent_us: clock::now_us(),
                },
            );
        }
        ControlFlow::Continue(())
    }

    fn handshake_tick(
        &mut self,
        now: Instant,
        since: Instant,
        last_initiate: Instant,
    ) -> ControlFlow<()> {
        if self.ctx.peer.is_established() {
            tracing::info!(
                ms = since.elapsed().as_millis() as u64,
                segmented = self.ctx.peer.segmented_sends(),
                path = %self.path(),
                "tunnel up"
            );
            self.emit(Event::Status("Starting the stream…".into()));
            self.phase = Phase::Negotiating {
                since: now,
                last_sent: now - START_RETRY,
            };
        } else if since.elapsed() > self.ctx.handshake_limit {
            self.end(super::NO_ANSWER.into(), true);
            return ControlFlow::Break(());
        } else if last_initiate.elapsed() > RACE_EVERY {
            self.phase = Phase::Handshake {
                since,
                last_initiate: now,
            };
            self.racing = start_race(&self.ctx);
        } else {
            race(&self.ctx, &mut self.racing);
        }
        ControlFlow::Continue(())
    }

    fn negotiating_tick(
        &mut self,
        now: Instant,
        since: Instant,
        last_sent: Instant,
    ) -> ControlFlow<()> {
        if since.elapsed() > GIVE_UP_AFTER {
            self.end("The host did not start the stream.".into(), true);
            return ControlFlow::Break(());
        }
        if last_sent.elapsed() >= START_RETRY {
            self.phase = Phase::Negotiating {
                since,
                last_sent: now,
            };
            send_control(&self.ctx, Control::SessionStart(self.start));
        }
        ControlFlow::Continue(())
    }

    fn streaming_tick(&mut self, now: Instant) -> ControlFlow<()> {
        self.probe_lan(now);
        if let Some(player) = &self.audio {
            player.set_muted(self.ctx.audio_muted.load(Ordering::Relaxed));
            if self.last_audio_log.elapsed() >= AUDIO_LOG_EVERY {
                self.last_audio_log = now;
                self.log_audio();
            }
        }
        if self.last_report.elapsed() >= REPORT_EVERY {
            self.report(now);
        }
        self.watch_connection(now)
    }

    /// On a remote path while the host is known on the local network: prefer
    /// the LAN, as Moonlight does. A ping sent over it moves the session
    /// there (roaming) if the host answers.
    fn probe_lan(&mut self, now: Instant) {
        if self.lan_probe.sent >= LAN_PROBES
            || self
                .lan_probe
                .last
                .is_some_and(|t| t.elapsed() < LAN_PROBE_EVERY)
        {
            return;
        }
        let local = self.ctx.candidates.lock().0.clone();
        let on_local = self.ctx.peer.addr().is_some_and(|a| local.contains(&a));
        if !on_local && !local.is_empty() {
            self.lan_probe = LanProbe {
                last: Some(now),
                sent: self.lan_probe.sent + 1,
            };
            let mut out = [0u8; control::MAX_CONTROL_LEN];
            let n = Control::Ping {
                id: u32::MAX,
                sent_us: clock::now_us(),
            }
            .encode(clock::now_us(), &mut out);
            for &addr in &local {
                let _ = self.ctx.endpoint.send_via(&self.ctx.peer, &out[..n], addr);
            }
        } else if on_local && self.lan_probe.sent > 0 {
            tracing::info!(path = %self.path(), "moved to the local network");
            self.lan_probe.sent = LAN_PROBES;
        }
    }

    fn log_audio(&mut self) {
        let Some(player) = &self.audio else {
            return;
        };
        let s = player.stats();
        tracing::debug!(
            buffered_ms = s.buffered_ms,
            decoded = s.decoded,
            concealed = s.concealed,
            recovered = self.depacketizer.recovered,
            trimmed = s.trimmed,
            underruns = s.underruns,
            peak = s.peak,
            channel_peaks = ?&s.channel_peaks[..player_channels(&s)],
            max_send_gap_ms = self.audio_gaps.max_sent as f32 / 1000.0,
            max_arrival_gap_ms = self.audio_gaps.max_arrived as f32 / 1000.0,
            max_busy_ms = self.busy_max.as_secs_f32() * 1000.0,
            "audio"
        );
        self.busy_max = Duration::ZERO;
        self.audio_gaps.max_sent = 0;
        self.audio_gaps.max_arrived = 0;
    }

    /// The once-a-second loss report to the host (which steers its bitrate
    /// with it), and the connection warning judged from the same second.
    fn report(&mut self, now: Instant) {
        let cpu = crate::priority::thread_cpu_time();
        if let (Some(cpu), Some(before)) = (cpu, self.net_cpu) {
            tracing::debug!(
                cpu_pct =
                    (cpu - before).as_secs_f32() * 100.0 / self.last_report.elapsed().as_secs_f32(),
                "net thread"
            );
        }
        self.net_cpu = cpu;
        self.last_report = now;
        let g = self.gate.stats();
        self.loss.settle(now);
        self.loss.take_rate(now);
        self.loss.report.frames_lost = g
            .lost
            .saturating_sub(self.lost_reported)
            .min(u16::MAX as u64) as u16;
        self.lost_reported = g.lost;
        self.loss.report.rtt_us = self.ctx.rtt_us.load(Ordering::Relaxed) as u32;
        self.ctx.stats.loss(self.loss.report);
        self.ctx.stats.gate(g);
        send_control(&self.ctx, Control::LossReport(self.loss.report));
        // Judged on the second's lowest round trip, not the smoothed one: a
        // queue raises every sample, while the smoothing drags one slow answer
        // (a host still starting up) across seconds.
        let floor = self.ctx.rtt_floor_us.swap(u64::MAX, Ordering::Relaxed);
        let judged = LossReport {
            rtt_us: if floor == u64::MAX { 0 } else { floor as u32 },
            ..self.loss.report
        };
        match self.quality.judge(&judged) {
            Some(true) => {
                tracing::info!("poor connection");
                self.emit(Event::Warning(Some(format!(
                    "Poor connection to {}",
                    self.ctx.host_name
                ))));
            }
            Some(false) => self.emit(Event::Warning(None)),
            None => {}
        }
        self.loss.report = LossReport::default();
    }

    /// A host that goes quiet mid-stream: say so, look for it on every path,
    /// and give up after `LOST_AFTER`.
    fn watch_connection(&mut self, now: Instant) -> ControlFlow<()> {
        let quiet = self
            .ctx
            .endpoint
            .since_rx(&self.ctx.peer)
            .unwrap_or_default();
        if quiet > LOST_AFTER {
            self.end(
                format!(
                    "Lost contact with {}. Check that it is on and connected, then try again.",
                    self.ctx.host_name
                ),
                true,
            );
            return ControlFlow::Break(());
        }
        if quiet > INTERRUPTED_AFTER {
            let i = self.interruption.get_or_insert_with(|| {
                tracing::info!("the host went quiet; reconnecting");
                Interruption {
                    racing: None,
                    last_race: now - RERACE_EVERY,
                    shown: u64::MAX,
                }
            });
            if i.last_race.elapsed() >= RERACE_EVERY {
                i.last_race = now;
                i.racing = Some(start_race(&self.ctx));
            } else if let Some(r) = &mut i.racing {
                race(&self.ctx, r);
            }
            let left = LOST_AFTER.saturating_sub(quiet).as_secs() + 1;
            if left != i.shown {
                i.shown = left;
                let text = format!(
                    "Connection to {} interrupted\nReconnecting… {left}",
                    self.ctx.host_name
                );
                self.emit(Event::Notice(Some(text)));
            }
        } else if self.interruption.take().is_some() {
            tracing::info!(path = %self.path(), "the host is back");
            self.emit(Event::Notice(None));
        }
        ControlFlow::Continue(())
    }

    /// The path the tunnel uses now, for the log.
    fn path(&self) -> String {
        self.ctx
            .peer
            .addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    }
}

/// The `SessionStart` this client asks with.
fn session_start(settings: &StreamSettings, nonce: u32) -> SessionStart {
    let mut flags = 0;
    if settings.keep_host_displays {
        flags |= control::flags::KEEP_HOST_DISPLAYS;
    }
    if settings.host_audio {
        flags |= control::flags::HOST_AUDIO;
    }
    if settings.watch {
        flags |= control::flags::WATCH;
    }
    if settings.clipboard && !settings.watch {
        flags |= control::flags::CLIPBOARD;
    }
    // The reassembler takes LAN-sized shards (header `lan_shards`);
    // PINGPONG_LAN_SHARDS=0 asks for path-sized ones (diagnostics).
    if !std::env::var("PINGPONG_LAN_SHARDS").is_ok_and(|v| v == "0") {
        flags |= control::flags::LAN_SHARDS;
    }
    let (repeat_delay_ms, repeat_interval_ms) = crate::keyboard::repeat_rate()
        .map(|r| r.to_millis())
        .unwrap_or((0, 0));
    SessionStart {
        width: settings.width & !1,
        height: settings.height & !1,
        refresh_mhz: settings.fps.saturating_mul(1000),
        bitrate_kbps: settings.bitrate_kbps,
        codecs: settings.codecs,
        audio_channels: settings.audio_channels,
        flags,
        slices: settings.slices.max(1),
        nonce,
        app: settings.app,
        repeat_delay_ms,
        repeat_interval_ms,
        video: settings.video,
        exact_refresh_mhz: exact_rate(settings.fps, display_refresh_mhz()),
    }
}

/// This computer's display refresh, to the millihertz, where the platform
/// says.
fn display_refresh_mhz() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        crate::mac::display_refresh_mhz()
    }
    #[cfg(windows)]
    {
        crate::win::display_refresh_mhz()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

/// The display's own rate when `fps` is it, rounded (59.94 Hz for "60"),
/// so the stream neither drops nor repeats a frame every few seconds
/// against the display's refresh; 0 when it is not. Within 1%, as Sunshine
/// takes Moonlight's `clientRefreshRateX100` (`rtsp.cpp`).
fn exact_rate(fps: u32, display_mhz: Option<u32>) -> u32 {
    let asked = fps.saturating_mul(1000);
    match display_mhz {
        Some(d) if d.abs_diff(asked) as u64 * 100 <= asked as u64 => d,
        _ => 0,
    }
}

fn now_us64(epoch: Instant) -> u64 {
    epoch.elapsed().as_micros() as u64
}

/// A new race: a new initiation, sent along every candidate. (Not
/// `initiation`: with one in flight it would hand back nothing for 5 s, and
/// the race would stall on an initiation that was lost.)
fn start_race(ctx: &Ctx) -> Racing {
    let packets = ctx
        .endpoint
        .fresh_initiation(&ctx.peer)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot form a handshake initiation");
            Vec::new()
        });
    let mut r = Racing {
        packets,
        started: Instant::now(),
        sent: Vec::new(),
    };
    race(ctx, &mut r);
    r
}

/// Send the current initiation to every candidate not yet tried: local ones
/// at once, remote ones once the local network has had `REMOTE_AFTER` to
/// answer (or straight away when there is no local candidate).
fn race(ctx: &Ctx, r: &mut Racing) {
    let (local, remote) = ctx.candidates.lock().clone();
    let remote_due = local.is_empty() || r.started.elapsed() >= REMOTE_AFTER;
    let due = local.iter().chain(remote.iter().filter(|_| remote_due));
    for &addr in due {
        if r.packets.is_empty() || r.sent.contains(&addr) {
            continue;
        }
        r.sent.push(addr);
        if let Err(e) = ctx.endpoint.send_initiation(&ctx.peer, &r.packets, addr) {
            tracing::debug!(%addr, error = %e, "initiation not sent");
        }
    }
}

fn start_audio(channels: u8) -> Option<Player> {
    #[cfg(target_os = "macos")]
    let result = Player::start(channels, pingpong_audio::coreaudio::open);
    #[cfg(windows)]
    let result = Player::start(channels, pingpong_audio::wasapi::open_output);
    #[cfg(target_os = "linux")]
    let result = Player::start(channels, pingpong_audio::cpal_out::open);
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    let result: Result<Player, String> = Err("no audio output on this platform yet".into());
    match result {
        Ok(p) => {
            tracing::info!(channels, "audio playing");
            Some(p)
        }
        Err(e) => {
            tracing::warn!(error = %e, "no audio");
            None
        }
    }
}

/// Which ways the clipboard goes, as the host's permissions for this device
/// say: our copies to the host if it may write there, the host's to us if
/// it may read them.
fn clip_directions(permissions: u16) -> pingpong_clipboard::Directions {
    use pingpong_proto::permission::{Permissions, CLIPBOARD_READ, CLIPBOARD_WRITE};
    let p = Permissions::from_bits(permissions);
    pingpong_clipboard::Directions {
        send: p.allows(CLIPBOARD_WRITE),
        receive: p.allows(CLIPBOARD_READ),
    }
}

/// Share the clipboard with the host for this session.
fn start_clipboard(ctx: &Ctx, ack: &SessionAck) -> pingpong_clipboard::ClipSync {
    let (endpoint, peer) = (ctx.endpoint.clone(), ctx.peer.clone());
    let opts = pingpong_clipboard::Options {
        files_dir: std::env::temp_dir().join("Ping Clipboard"),
        rate: pingpong_clipboard::rate_for(ack.bitrate_kbps),
        offer_current: true,
        peer: ctx.host_name.clone(),
        directions: clip_directions(ack.permissions),
    };
    pingpong_clipboard::ClipSync::start(opts, move |p| {
        let _ = endpoint.send(&peer, p);
    })
}

/// Channels with a level to show (trailing silent slots of a stereo stream
/// are not worth logging).
fn player_channels(s: &pingpong_audio::player::PlayerStats) -> usize {
    s.channel_peaks
        .iter()
        .rposition(|&p| p > 0.0)
        .map_or(2, |i| (i + 1).max(2))
}

fn request(ctx: &Ctx, r: Request) {
    match r {
        Request::Idr => {
            tracing::info!("requesting an IDR");
            send_control(ctx, Control::RequestIdr);
        }
        Request::Invalidate { first, last } => {
            tracing::debug!(first, last, "requesting reference invalidation");
            send_control(ctx, Control::InvalidateRefs { first, last });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_rate_replaces_a_rounded_one() {
        assert_eq!(exact_rate(60, Some(59_940)), 59_940);
        assert_eq!(exact_rate(120, Some(119_880)), 119_880);
        assert_eq!(exact_rate(144, Some(143_981)), 143_981);
        // A rate the user chose below the display's is theirs alone.
        assert_eq!(exact_rate(60, Some(120_000)), 0);
        assert_eq!(exact_rate(60, None), 0);
    }
}
