//! The WebRTC connection to a console or a cloud server, on str0m: Ping
//! makes the offer, the console answers through the streaming service
//! ([`crate::gssv`]), and then this runs the connection on one thread.
//!
//! What it carries (the web client's session, as Greenlight's player sets
//! it up, `packages/player/src/client/lib/player.ts`):
//!
//! - **Video**, H.264, received. str0m runs in RTP mode: it decrypts,
//!   asks for retransmissions (NACK) and unwraps them, and hands over each
//!   packet; [`crate::reassembly`] makes the frames. A frame goes to the
//!   [`Sink`] the moment its last packet arrives: no jitter buffer, which
//!   is where a browser spends its tens of milliseconds. A missing packet
//!   is waited for as long as a retransmission takes on the link; then the
//!   frames are given up, and nothing is handed over until a keyframe,
//!   which is asked for (an RTCP PLI and the control channel's keyframe
//!   request) -- Moonlight's rule (`VideoDepacketizer.c`), where a browser
//!   waits seconds or decodes on and smears.
//! - **Audio**, Opus, stereo, received: each packet straight to the
//!   player, whose own buffer puts them in order and covers a lost one.
//! - **Data channels**: `input` (binary reports, [`crate::input`]),
//!   `control` and `message` (JSON, [`crate::messages`]), and `chat`, which
//!   is opened as the console expects and left quiet (no microphone).
//!
//! str0m's contract is that every change to it (a datagram in, a message
//! out) is followed by draining its output; events never change it
//! themselves here, they set what the loop does next ([`Connection::act`]).

pub mod inputs;
pub mod socket;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(test)]
use str0m::change::SdpOffer;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpPacket, Ssrc};
use str0m::{Candidate, Event as RtcEvent, IceConnectionState, Input as RtcInput, Output, Rtc};

pub use inputs::{Input, InputState};
pub use socket::{Doorbell, Socket};

use crate::ice::IceCandidate;
use crate::input::{parse_server_report, FrameTimes, ServerReport, MAX_REPORT_LEN};
use crate::messages::{self, Incoming, CHANNELS};
use crate::reassembly::{self, Popped, Reassembler, ReassemblyStats};
use crate::rumble::{Motors, Rumble};
use crate::virtual_pad::KeyboardMouse;

/// How long ICE and DTLS may take before the console is called unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// After asking for a keyframe, ask again this long later if none came
/// (Moonlight re-asks for its IDR on the same order).
const KEYFRAME_RETRY: Duration = Duration::from_millis(500);
/// The longest the loop sleeps, so a stop is seen promptly.
const MAX_SLEEP: Duration = Duration::from_millis(50);
/// How long the last messages may take to be acknowledged when the stream
/// ends (a round trip, with room for one retransmission on a LAN).
const GOODBYE_WAIT: Duration = Duration::from_millis(200);

/// What the connection is told about the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The size the picture is shown at, which the console is told: in
    /// pixels, and on the screen in millimetres.
    pub width: u32,
    pub height: u32,
    pub size_mm: (u32, u32),
    /// This installation's id ([`crate::store::Account::install_id`]).
    pub install_id: String,
    /// What the keyboard and mouse are to the console.
    pub keyboard_mouse: KeyboardMouse,
    /// The most video the console may send, in kb/s; `None` leaves it to
    /// the console.
    pub max_kbps: Option<u32>,
}

/// A complete video frame: H.264, Annex B.
pub struct VideoFrame<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
    /// The frame's RTP timestamp (90 kHz).
    pub rtp_time: u32,
    /// When its first packet arrived.
    pub arrived: Instant,
}

/// One Opus packet.
pub struct AudioFrame<'a> {
    pub data: &'a [u8],
    /// The RTP sequence number, extended past its 16 bits.
    pub seq: u64,
}

/// The link, once a second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LinkStats {
    pub rtt: Option<Duration>,
    /// Packets lost on the way here, 0..1, since the last.
    pub loss: Option<f32>,
    pub received_kbps: u32,
    pub keyframe_requests: u64,
    /// The video's packets and frames since the stream began.
    pub video: ReassemblyStats,
    /// How long a missing video packet is waited for now.
    pub retransmit_wait: Duration,
}

/// What happened, for the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Progress, for the person waiting ("Waking the console…").
    Status(String),
    /// ICE and DTLS are up: the picture follows.
    Connected,
    /// The console took the handshake: input is live.
    Ready,
    /// The size of the picture the console sends.
    VideoSize {
        width: u32,
        height: u32,
    },
    /// Controller `index`'s motors.
    Rumble {
        index: u8,
        motors: Motors,
    },
    /// What is being played, as the console says (JSON).
    Title(String),
    Stats(LinkStats),
}

/// Where the connection's output goes, on the connection's thread.
pub trait Sink {
    /// A frame to decode; `false` if the decoder failed on it (a keyframe
    /// is then asked for).
    fn video(&mut self, frame: &VideoFrame<'_>) -> bool;
    fn audio(&mut self, frame: &AudioFrame<'_>);
    fn event(&mut self, event: Event);
    /// Input waiting to go out: hand each to `take` (called once the
    /// [`Doorbell`] has rung).
    fn input(&mut self, take: &mut dyn FnMut(Input));
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Asked to stop.
    Stopped,
    /// The console ended the stream.
    ConsoleEnded,
    /// The connection failed or was lost; why, for a person.
    Lost(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chan {
    Input,
    Chat,
    Control,
    Message,
}

impl Chan {
    fn from_label(label: &str) -> Option<Chan> {
        match label {
            "input" => Some(Chan::Input),
            "chat" => Some(Chan::Chat),
            "control" => Some(Chan::Control),
            "message" => Some(Chan::Message),
            _ => None,
        }
    }
}

pub struct Connection {
    rtc: Rtc,
    socket: Socket,
    pending: Option<SdpPendingOffer>,
    audio: Mid,
    video: Mid,
    /// Which media each sender's packets are, once seen.
    ssrcs: Vec<(Ssrc, Mid)>,
    frames: Reassembler,
    /// When the first video packet came: a keyframe is asked for if none
    /// comes within [`KEYFRAME_RETRY`] of it.
    first_video: Option<Instant>,
    channels: Vec<(Chan, ChannelId)>,
    options: Options,
    inputs: InputState,
    rumble: Rumble,
    /// Messages to send, in order (rare: set-up, transactions).
    outbox: VecDeque<(Chan, Vec<u8>)>,
    ready: bool,
    connected: bool,
    started: Instant,
    want_keyframe: bool,
    last_keyframe_request: Option<Instant>,
    keyframe_requests: u64,
    ended: Option<End>,
}

impl Connection {
    /// A connection on `socket`, and the offer to send.
    pub fn offer(socket: Socket, options: Options) -> Result<(Connection, String), String> {
        // RTP mode: the frames are made here (`crate::reassembly`), not by
        // str0m, whose frame assembly holds every later frame for up to two
        // seconds behind a packet that never comes.
        let mut config = Rtc::builder()
            .clear_codecs()
            .set_rtp_mode(true)
            .set_stats_interval(Some(Duration::from_secs(1)));
        let codecs = config.codec_config();
        // Opus as stereo: the web client asks for it the same way, by
        // adding `stereo=1` to the offer (`sdp.ts` `setLocalSDP`).
        codecs.add_config(
            Pt::from(111),
            None,
            Codec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams {
                min_p_time: Some(10),
                use_inband_fec: Some(true),
                stereo: Some(true),
                ..Default::default()
            },
        );
        // H.264 in the web client's order of preference (`sdp.ts`
        // `getDefaultCodecPreferences`): Main, then Constrained Baseline,
        // then Baseline; level 3.1 with asymmetry allowed, so the console
        // may send higher levels. Each with RTX for retransmissions.
        for (pt, rtx, profile) in [
            (102, 103, 0x4d001f),
            (104, 105, 0x42e01f),
            (106, 107, 0x42001f),
        ] {
            codecs.add_h264(Pt::from(pt), Some(Pt::from(rtx)), true, profile);
        }
        let now = Instant::now();
        let mut rtc = config.build(now);
        for c in socket.candidates() {
            rtc.add_local_candidate(c);
        }
        let mut api = rtc.sdp_api();
        let audio = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let video = api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        for (label, protocol) in CHANNELS {
            api.add_channel_with_config(ChannelConfig {
                label: label.into(),
                ordered: true,
                reliability: Reliability::Reliable,
                negotiated: None,
                protocol: protocol.into(),
            });
        }
        let (offer, pending) = api
            .apply()
            .ok_or("the connection could not make an offer")?;
        let mut sdp = offer.to_sdp_string();
        if let Some(kbps) = options.max_kbps {
            sdp = with_max_video_kbps(&sdp, kbps);
        }
        let keyboard = options.keyboard_mouse;
        Ok((
            Connection {
                rtc,
                socket,
                pending: Some(pending),
                audio,
                video,
                ssrcs: Vec::new(),
                frames: Reassembler::new(),
                first_video: None,
                channels: Vec::new(),
                options,
                inputs: InputState::new(keyboard),
                rumble: Rumble::new(),
                outbox: VecDeque::new(),
                ready: false,
                connected: false,
                started: now,
                want_keyframe: false,
                last_keyframe_request: None,
                keyframe_requests: 0,
                ended: None,
            },
            sdp,
        ))
    }

    /// The console's answer.
    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), String> {
        // What the console agreed to send and how (codecs, feedback,
        // header extensions): how a stream adapts to its link depends on
        // it. Nothing secret: no ICE credentials or fingerprints.
        tracing::info!(media = %media_lines(sdp), "the console's answer");
        let answer = SdpAnswer::from_sdp_string(sdp)
            .map_err(|e| format!("The console's answer could not be read: {e}"))?;
        let pending = self.pending.take().ok_or("an answer was already taken")?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| format!("The console's answer was not usable: {e}"))
    }

    /// Our candidates, as the streaming service takes them.
    pub fn local_candidates(&self) -> Vec<IceCandidate> {
        self.socket
            .candidates()
            .iter()
            .map(|c| IceCandidate::local(c.to_sdp_string()))
            .collect()
    }

    /// The console's candidates (SDP lines, Teredo already expanded:
    /// [`crate::ice::with_teredo`]). Ones that cannot be used here (IPv6:
    /// the socket is IPv4) are skipped.
    pub fn add_remote_candidates(&mut self, lines: &[String]) -> usize {
        let mut added = 0;
        for line in lines {
            match Candidate::from_sdp_string(line) {
                Ok(c) if c.addr().is_ipv4() => {
                    self.rtc.add_remote_candidate(c);
                    added += 1;
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, %line, "candidate skipped"),
            }
        }
        added
    }

    /// The doorbell to ring after queueing input for [`Sink::input`].
    pub fn doorbell(&self) -> Doorbell {
        self.socket.doorbell()
    }

    fn ms(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.started).as_secs_f64() * 1000.0
    }

    /// Run until stopped, ended, or lost.
    pub fn run(&mut self, sink: &mut dyn Sink, stop: &AtomicBool) -> End {
        // One buffer for the connection's life: no allocation per datagram.
        let mut buf = vec![0u8; 2048];
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            let timeout = match self.drain(sink) {
                Ok(t) => t,
                Err(end) => return end,
            };
            if let Some(end) = self.ended.take() {
                self.say_goodbye(sink, &mut buf);
                return end;
            }
            if stop.load(Ordering::Relaxed) {
                self.rtc.disconnect();
                return End::Stopped;
            }
            let now = Instant::now();
            if !self.connected && now >= deadline {
                return End::Lost(
                    "The console could not be reached. On another network, its router must \
                     allow it (UPnP, or a forwarded UDP port 3074)."
                        .into(),
                );
            }
            if let Err(end) = self.act(sink, now) {
                return end;
            }
            let wake = [
                Some(timeout),
                self.rumble.next_change(now),
                self.inputs.next_tick().filter(|_| self.ready),
                self.keyframe_retry_at(),
                self.frames.deadline(),
                Some(now + MAX_SLEEP),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now + MAX_SLEEP);
            let wait = wake
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            if let Err(e) = self.receive(&mut buf, wait, sink) {
                return End::Lost(format!("The network failed: {e}"));
            }
        }
    }

    /// Wait up to `wait` for a datagram (or the doorbell) and take it in,
    /// then the input waiting, if any.
    fn receive(
        &mut self,
        buf: &mut [u8],
        wait: Duration,
        sink: &mut dyn Sink,
    ) -> std::io::Result<()> {
        let _ = self.socket.udp.set_read_timeout(Some(wait));
        let input = match self.socket.udp.recv_from(buf) {
            // The doorbell's byte only wakes the loop; its flag is read below.
            Ok((_, from)) if self.socket.is_bell(from) => None,
            Ok((n, from)) => buf[..n].try_into().ok().map(|contents| {
                RtcInput::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source: from,
                        destination: self.socket.local,
                        contents,
                    },
                )
            }),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Some(RtcInput::Timeout(Instant::now()))
            }
            // Windows reports an ICMP "port unreachable" for an earlier send
            // as a failed receive; nothing is wrong with the socket.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => None,
            Err(e) => return Err(e),
        };
        if let Some(input) = input {
            if let Err(e) = self.rtc.handle_input(input) {
                tracing::debug!(error = %e, "datagram refused");
            }
        }
        // At every wake, not only the bell's: input does not wait behind
        // the video datagrams queued ahead of the bell's byte (a keyframe is
        // hundreds), nor for a byte that never comes (see `socket`).
        if self.socket.bell.take() {
            let inputs = &mut self.inputs;
            let now = Instant::now();
            sink.input(&mut |i| inputs.apply(i, now));
        }
        Ok(())
    }

    /// Say what is left to say before going -- the console's disconnect is
    /// a transaction the client completes -- and wait, briefly, for the
    /// console to have it: the session is deleted right after, and the
    /// console may not wait for a message still on its way.
    fn say_goodbye(&mut self, sink: &mut dyn Sink, buf: &mut [u8]) {
        let mut chans = Vec::new();
        while let Some((chan, msg)) = self.outbox.pop_front() {
            self.write(chan, &msg);
            chans.push(chan);
            if self.drain(sink).is_err() {
                return;
            }
        }
        let deadline = Instant::now() + GOODBYE_WAIT;
        while Instant::now() < deadline {
            let ids: Vec<ChannelId> = chans.iter().filter_map(|&c| self.channel(c)).collect();
            let unacked: usize = ids
                .into_iter()
                .map(|id| self.rtc.channel(id).map_or(0, |mut c| c.buffered_amount()))
                .sum();
            if unacked == 0 {
                return;
            }
            if self.receive(buf, Duration::from_millis(10), sink).is_err()
                || self.drain(sink).is_err()
            {
                return;
            }
        }
    }

    /// Drain str0m's output: send what it sends, take in what it says.
    /// Returns when it next wants the time.
    fn drain(&mut self, sink: &mut dyn Sink) -> Result<Instant, End> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return Ok(t),
                Ok(Output::Transmit(t)) => {
                    if let Err(e) = self.socket.udp.send_to(&t.contents, t.destination) {
                        tracing::trace!(error = %e, "send");
                    }
                }
                Ok(Output::Event(e)) => self.on_event(e, sink),
                Err(e) => return Err(End::Lost(format!("The connection failed: {e}"))),
            }
        }
    }

    /// Changes to str0m that events asked for, and the timers: each change
    /// is drained before the next.
    fn act(&mut self, sink: &mut dyn Sink, now: Instant) -> Result<(), End> {
        while let Some((chan, msg)) = self.outbox.pop_front() {
            self.write(chan, &msg);
            self.drain(sink)?;
        }
        // A frame given up at its deadline, with nothing arriving since.
        if self.frames.deadline().is_some_and(|d| now >= d) {
            self.take_frames(sink, now);
        }
        if self.want_keyframe || self.keyframe_retry_at().is_some_and(|t| now >= t) {
            self.want_keyframe = false;
            self.request_keyframe(now);
            self.drain(sink)?;
        }
        if self.ready {
            self.inputs.tick(now);
            while let Some(msg) = self.inputs.take_control() {
                self.write(Chan::Control, msg.as_bytes());
                self.drain(sink)?;
            }
            let mut report = [0u8; MAX_REPORT_LEN];
            let ms = self.ms(now);
            while let Some(n) = self.inputs.take_report(now, ms, &mut report) {
                self.write(Chan::Input, &report[..n]);
                self.drain(sink)?;
            }
        }
        self.rumble.poll(now, |index, motors| {
            sink.event(Event::Rumble { index, motors })
        });
        Ok(())
    }

    fn channel(&self, chan: Chan) -> Option<ChannelId> {
        self.channels
            .iter()
            .find(|(c, _)| *c == chan)
            .map(|&(_, id)| id)
    }

    /// Send on a data channel. Messages are sent as binary, as the web
    /// client sends them (it encodes its JSON to bytes first).
    fn write(&mut self, chan: Chan, data: &[u8]) {
        let Some(id) = self.channel(chan) else {
            tracing::debug!(?chan, "not open; message dropped");
            return;
        };
        match self.rtc.channel(id).map(|mut c| c.write(true, data)) {
            Some(Ok(true)) => {}
            Some(Ok(false)) => tracing::debug!(?chan, "channel full; message dropped"),
            Some(Err(e)) => tracing::debug!(?chan, error = %e, "channel write"),
            None => {}
        }
    }

    /// Queue a message for [`Connection::act`] to send.
    fn send_later(&mut self, chan: Chan, msg: String) {
        self.outbox.push_back((chan, msg.into_bytes()));
    }

    /// When to ask for a keyframe again, while one is awaited: the last
    /// request's has not come, or none was made and the stream began
    /// without one. A keyframe that began to arrive since is given as
    /// long: on a slow link a big one takes half a second, and asking
    /// again sends a second one down the link that is short already.
    fn keyframe_retry_at(&self) -> Option<Instant> {
        if !self.connected || !self.frames.awaiting_keyframe() {
            return None;
        }
        let asked = self.last_keyframe_request.or(self.first_video)?;
        let arriving = self.frames.keyframe_arriving_since();
        Some(arriving.map_or(asked, |a| a.max(asked)) + KEYFRAME_RETRY)
    }

    fn request_keyframe(&mut self, now: Instant) {
        self.last_keyframe_request = Some(now);
        self.keyframe_requests += 1;
        if let Some(stream) = self.rtc.direct_api().stream_rx_by_mid(self.video, None) {
            stream.request_keyframe(KeyframeRequestKind::Pli);
        }
        self.send_later(Chan::Control, messages::control::keyframe_request(true));
        tracing::debug!("keyframe requested");
    }

    fn on_event(&mut self, e: RtcEvent, sink: &mut dyn Sink) {
        match e {
            RtcEvent::Connected => {
                self.connected = true;
                tracing::info!("connected to the console");
                sink.event(Event::Connected);
            }
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.ended = Some(End::Lost("The connection to the console was lost.".into()));
            }
            RtcEvent::ChannelOpen(id, label) => {
                let Some(chan) = Chan::from_label(&label) else {
                    return;
                };
                self.channels.push((chan, id));
                if chan == Chan::Message {
                    self.send_later(Chan::Message, messages::handshake(&crate::uuid_v4()));
                }
            }
            RtcEvent::ChannelData(d) => self.on_channel_data(d, sink),
            RtcEvent::ChannelClose(id) => {
                if self.channel(Chan::Input) == Some(id) && self.ended.is_none() {
                    self.ended = Some(End::Lost("The console closed the input channel.".into()));
                }
            }
            RtcEvent::RtpPacket(p) => self.on_rtp(p, sink),
            RtcEvent::MediaIngressStats(s) if s.mid == self.video => {
                sink.event(Event::Stats(LinkStats {
                    rtt: s.rtt,
                    loss: s.loss,
                    received_kbps: 0,
                    keyframe_requests: self.keyframe_requests,
                    video: self.frames.stats(),
                    retransmit_wait: self.frames.wait(),
                }));
            }
            RtcEvent::PeerStats(p) => {
                let video = self.frames.stats();
                tracing::debug!(
                    packets = video.packets,
                    missing = video.missing,
                    recovered = video.recovered,
                    too_late = video.too_late,
                    given_up = video.given_up,
                    frames = video.frames,
                    keyframes = video.keyframes,
                    keyframe_requests = self.keyframe_requests,
                    wait_ms = self.frames.wait().as_millis() as u64,
                    "video"
                );
                sink.event(Event::Stats(LinkStats {
                    rtt: p.rtt,
                    loss: p.ingress_loss_fraction,
                    received_kbps: (p.peer_bytes_rx.saturating_mul(8) / 1000) as u32,
                    keyframe_requests: self.keyframe_requests,
                    video,
                    retransmit_wait: self.frames.wait(),
                }));
            }
            _ => {}
        }
    }

    /// Which media a sender's packets are.
    fn media_of(&mut self, ssrc: Ssrc) -> Option<Mid> {
        if let Some(&(_, mid)) = self.ssrcs.iter().find(|(s, _)| *s == ssrc) {
            return Some(mid);
        }
        let mid = self.rtc.direct_api().stream_rx(&ssrc)?.mid();
        self.ssrcs.push((ssrc, mid));
        Some(mid)
    }

    fn on_rtp(&mut self, p: RtpPacket, sink: &mut dyn Sink) {
        let Some(mid) = self.media_of(p.header.ssrc) else {
            return;
        };
        if mid == self.audio {
            if !p.payload.is_empty() {
                sink.audio(&AudioFrame {
                    data: &p.payload,
                    seq: *p.seq_no,
                });
            }
            return;
        }
        if mid != self.video {
            return;
        }
        let now = Instant::now();
        self.first_video.get_or_insert(now);
        self.frames.push(
            reassembly::Packet {
                seq: *p.seq_no,
                time: p.time.numer(),
                marker: p.header.marker,
                payload: p.payload,
                arrived: p.timestamp,
            },
            now,
        );
        self.take_frames(sink, now);
    }

    /// Hand the frames that are whole to the decoder.
    fn take_frames(&mut self, sink: &mut dyn Sink, now: Instant) {
        loop {
            let (decoded, rtp_time, arrived) = match self.frames.pop(now) {
                None => return,
                Some(Popped::Lost) => {
                    tracing::debug!("video loss; waiting for a keyframe");
                    self.want_keyframe = true;
                    continue;
                }
                Some(Popped::Frame(f)) => {
                    let rtp_time = f.time as u32;
                    let decoded = sink.video(&VideoFrame {
                        data: f.data,
                        keyframe: f.keyframe,
                        rtp_time,
                        arrived: f.arrived,
                    });
                    (decoded, rtp_time, f.arrived)
                }
            };
            if !decoded {
                self.frames.need_keyframe();
                self.want_keyframe = true;
                continue;
            }
            // When the frame arrived and went to the decoder. Decoding and
            // presenting happen elsewhere, so the hand-off stands for them.
            let handed = self.ms(now) as u32;
            self.inputs.frame_shown(FrameTimes {
                server_key: rtp_time,
                first_packet_ms: self.ms(arrived) as u32,
                submitted_ms: handed,
                decoded_ms: handed,
                rendered_ms: handed,
            });
        }
    }

    fn on_channel_data(&mut self, d: ChannelData, sink: &mut dyn Sink) {
        let Some(chan) = self
            .channels
            .iter()
            .find(|(_, id)| *id == d.id)
            .map(|&(c, _)| c)
        else {
            return;
        };
        match chan {
            Chan::Message => match messages::parse_incoming(&d.data) {
                Some(Incoming::HandshakeAck) => self.on_handshake(sink),
                Some(Incoming::Disconnect { transaction }) => {
                    self.send_later(
                        Chan::Message,
                        messages::complete_transaction(&transaction, &serde_json::json!("")),
                    );
                    tracing::info!("the console ended the stream");
                    self.ended = Some(End::ConsoleEnded);
                }
                Some(Incoming::TitleInfo(info)) => sink.event(Event::Title(info)),
                Some(Incoming::Dialog { transaction })
                | Some(Incoming::OtherTransaction { transaction, .. }) => {
                    self.send_later(Chan::Message, messages::cancel_transaction(&transaction));
                }
                Some(Incoming::Other) | None => {}
            },
            Chan::Input => match parse_server_report(&d.data) {
                Some(ServerReport::Vibration(v)) => self.rumble.start(&v, Instant::now()),
                Some(ServerReport::VideoSize { width, height }) => {
                    sink.event(Event::VideoSize { width, height })
                }
                None => {}
            },
            Chan::Control | Chan::Chat => {}
        }
    }

    /// The console took the handshake: authorize, open input, and say what
    /// this client is.
    fn on_handshake(&mut self, sink: &mut dyn Sink) {
        if self.ready {
            return;
        }
        tracing::info!(
            keyboard_mouse = ?self.options.keyboard_mouse,
            "the console took the handshake"
        );
        self.send_later(Chan::Control, messages::control::authorization());
        // The web client asks for its resolution here too, once connected.
        let alias = messages::resolution_alias(self.options.height);
        self.send_later(Chan::Control, messages::control::resolution(alias));
        let mut opening = [0u8; MAX_REPORT_LEN];
        let n = self
            .inputs
            .opening_report(self.ms(Instant::now()), &mut opening);
        self.outbox.push_back((Chan::Input, opening[..n].to_vec()));
        for (target, data) in messages::client_configuration(
            &self.options.install_id,
            self.options.width,
            self.options.height,
            self.options.size_mm,
        ) {
            self.send_later(
                Chan::Message,
                messages::message(&crate::uuid_v4(), target, &data),
            );
        }
        self.inputs.start();
        self.ready = true;
        sink.event(Event::Ready);
    }
}

/// An SDP's lines about its media, one string: the sections, bandwidth,
/// codecs and their parameters, feedback and header extensions.
fn media_lines(sdp: &str) -> String {
    const KEPT: [&str; 6] = ["m=", "b=", "a=rtpmap", "a=fmtp", "a=rtcp-fb", "a=extmap"];
    sdp.lines()
        .map(str::trim_end)
        .filter(|l| KEPT.iter().any(|k| l.starts_with(k)))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `sdp` with the most the video may take, `kbps`: a `b=AS` line in its
/// video section, which a sender keeps under (RFC 4566 §5.8). Better
/// xCloud's bitrate limit is this line in the offer (`sdp.ts`
/// `patchSdpBitrate`); a line already there is replaced.
fn with_max_video_kbps(sdp: &str, kbps: u32) -> String {
    let line = format!("b=AS:{kbps}");
    let mut out = Vec::new();
    let mut video = false;
    let mut placed = false;
    for l in sdp.split("\r\n") {
        if l.starts_with("m=") {
            video = l.starts_with("m=video");
        } else if video && !placed {
            if l.starts_with("b=AS:") {
                out.push(line.as_str());
                placed = true;
                continue;
            }
            // b= comes after i= and c=, before the attributes.
            if !(l.starts_with("i=") || l.starts_with("c=") || l.starts_with("b=")) {
                out.push(line.as_str());
                placed = true;
            }
        }
        out.push(l);
    }
    out.join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use pingpong_proto::input::Key;

    /// Hands over the input queued in it.
    struct Queued(Vec<Input>);

    impl Sink for Queued {
        fn video(&mut self, _: &VideoFrame<'_>) -> bool {
            true
        }
        fn audio(&mut self, _: &AudioFrame<'_>) {}
        fn event(&mut self, _: Event) {}
        fn input(&mut self, take: &mut dyn FnMut(Input)) {
            for i in self.0.drain(..) {
                take(i);
            }
        }
    }

    #[test]
    fn input_goes_out_though_stun_read_the_doorbells_byte() {
        let socket = Socket::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let bell = socket.doorbell();
        let mut sink = Queued(vec![Input::Key {
            key: Key::KeyW,
            down: true,
        }]);
        bell.ring();
        // Read off the socket as STUN reads it while the session starts.
        let mut buf = [0u8; 2048];
        socket
            .udp
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let (_, from) = socket.udp.recv_from(&mut buf).unwrap();
        assert!(socket.is_bell(from));

        let (mut connection, _) = Connection::offer(
            socket,
            Options {
                width: 1280,
                height: 720,
                size_mm: (338, 190),
                install_id: "test".into(),
                keyboard_mouse: KeyboardMouse::Native,
                max_kbps: None,
            },
        )
        .unwrap();
        // Nothing arrives: the loop wakes on its timeout, and looks.
        connection
            .receive(&mut buf, Duration::from_millis(5), &mut sink)
            .unwrap();
        assert!(sink.0.is_empty(), "the input was taken");
    }

    #[test]
    fn a_maximum_bitrate_goes_in_the_video_section_only() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:2\r\n";
        let out = with_max_video_kbps(sdp, 8000);
        assert_eq!(out.matches("b=AS:").count(), 1);
        assert!(out.contains(
            "m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\nb=AS:8000\r\na=mid:1"
        ));
        // Said twice, the second replaces the first.
        let again = with_max_video_kbps(&out, 5000);
        assert_eq!(again.matches("b=AS:").count(), 1);
        assert!(again.contains("b=AS:5000"));
    }

    #[test]
    fn the_offer_says_the_maximum_when_there_is_one() {
        let offer = |max_kbps| {
            let socket = Socket::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
            let options = Options {
                width: 1920,
                height: 1080,
                size_mm: (508, 286),
                install_id: "test".into(),
                keyboard_mouse: KeyboardMouse::Native,
                max_kbps,
            };
            Connection::offer(socket, options).unwrap().1
        };
        assert!(!offer(None).contains("b=AS"));
        let sdp = offer(Some(12_000));
        let video = &sdp[sdp.find("m=video").unwrap()..];
        assert!(video.contains("\r\nb=AS:12000\r\n"), "{sdp}");
        // str0m's own reading of it is unchanged.
        assert!(SdpOffer::from_sdp_string(&sdp).is_ok());
    }

    #[test]
    fn the_answer_is_logged_without_its_secrets() {
        let sdp = "v=0\r\na=ice-ufrag:abcd\r\na=ice-pwd:secret\r\n\
                   a=fingerprint:sha-256 AA:BB\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
                   a=rtpmap:102 H264/90000\r\na=rtcp-fb:102 transport-cc\r\n\
                   a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
                   a=candidate:1 1 udp 1 192.168.1.20 3074 typ host\r\n";
        let lines = media_lines(sdp);
        assert!(lines.contains("a=rtcp-fb:102 transport-cc"));
        assert!(lines.contains("m=video"));
        for secret in ["secret", "abcd", "AA:BB", "192.168"] {
            assert!(!lines.contains(secret), "{lines}");
        }
    }
}
