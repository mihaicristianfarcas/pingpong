//! The mock console's end of the WebRTC connection, on str0m like the
//! client's: it answers the client's offer, then sends the test picture at
//! 60 frames a second and a 440 Hz tone in 20 ms Opus packets, and plays
//! the console's part on the data channels -- the message channel's
//! handshake, the picture's size on the input channel, rumble when A is
//! pressed (and on request), a keyframe when one is asked for, and the
//! console's disconnect.
//!
//! Everything the client sends is written down in the [`Record`] for tests
//! to read.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use pingpong_audio::opus::Encoder as OpusEncoder;
use pingpong_xbox::input::{
    parse_client_report, write_vibration, write_video_size, xbutton, Vibration,
};
use pingpong_xbox::messages::{self, ControlMessage};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::{Codec, FormatParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::h264::Encoder;
use crate::picture::{self, InputView};
use crate::Record;

const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
const AUDIO_PACKET: Duration = Duration::from_millis(20);
const AUDIO_SAMPLES: usize = 960;

pub enum Command {
    AddRemote(Vec<String>),
    Vibrate(Vibration),
    Disconnect,
    /// Drop this percentage of video packets, as a lossy link would.
    VideoLoss(u8),
    Link(Link),
}

/// The network between the console and the client.
///
/// Sends are paced at its rate, as a console's are: at the default rate a
/// keyframe here, the whole picture uncompressed (353 KB), takes 14 ms,
/// where sent in one burst it would overflow the receive buffer Linux
/// gives a socket (about 200 KB); ordinary frames (a few kilobytes) go at
/// once. A slower rate is a slower link: what it cannot send at once
/// waits in its queue, up to `queue`, and what does not fit is dropped,
/// as a router's queue does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    /// Each way: towards the client, and the client's datagrams (its NACKs,
    /// its input) towards the console.
    pub delay: Duration,
    /// Video packets lost on the way, in percent.
    pub video_loss: u8,
    /// Towards the client, in bits a second.
    pub rate_bps: u64,
    /// How much the bottleneck queues, as the time to send it.
    pub queue: Duration,
    /// Every so often, for so long, nothing gets through either way, as
    /// Wi-Fi drops out: (every, for).
    pub outages: Option<(Duration, Duration)>,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            delay: Duration::ZERO,
            video_loss: 0,
            rate_bps: 200_000_000,
            queue: Duration::from_secs(1),
            outages: None,
        }
    }
}

impl Link {
    /// Whether the link is out at `t`, `since` its start.
    fn out(&self, since: Instant, t: Instant) -> bool {
        self.outages.is_some_and(|(every, length)| {
            let every = every.as_nanos().max(1);
            t.saturating_duration_since(since).as_nanos() % every < length.as_nanos()
        })
    }

    /// How long `bytes` take at the link's rate.
    fn time_for(&self, bytes: usize) -> Duration {
        Duration::from_nanos(bytes as u64 * 8 * 1_000_000_000 / self.rate_bps.max(1))
    }
}

/// A connection to one client, on a thread of its own until dropped.
pub struct Peer {
    /// The SDP answer.
    pub answer: String,
    /// The console's candidate, as an SDP line.
    pub candidate: String,
    tx: Sender<Command>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Peer {
    /// Answer `offer`; the console is reached at `ip`.
    pub fn answer(
        offer: &str,
        ip: IpAddr,
        record: Arc<Mutex<Record>>,
        view: Arc<Mutex<InputView>>,
        link: Link,
    ) -> Result<Peer, String> {
        let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("bad offer: {e}"))?;
        // Stats once a second: the round trip measured from the client's
        // congestion feedback says the client sends it.
        let mut config = Rtc::builder()
            .clear_codecs()
            .set_stats_interval(Some(Duration::from_secs(1)));
        let codecs = config.codec_config();
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
        // The encoder makes Constrained Baseline.
        codecs.add_h264(Pt::from(104), Some(Pt::from(105)), true, 0x42e01f);
        let mut rtc = config.build(Instant::now());
        let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
        let local = SocketAddr::new(ip, udp.local_addr().map_err(|e| e.to_string())?.port());
        let candidate = Candidate::host(local, "udp").map_err(|e| e.to_string())?;
        let line = candidate.to_sdp_string();
        rtc.add_local_candidate(candidate);
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| format!("cannot answer: {e}"))?
            .to_sdp_string();
        let (tx, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("xbox-mock-peer".into())
                .spawn(move || {
                    Console::new(rtc, udp, local, record, view, link).run(rx, &stop);
                })
                .map_err(|e| e.to_string())?
        };
        Ok(Peer {
            answer,
            candidate: line,
            tx,
            stop,
            thread: Some(thread),
        })
    }

    pub fn add_remote(&self, lines: Vec<String>) {
        let _ = self.tx.send(Command::AddRemote(lines));
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}

/// How long a deleted session's console still reads its connection, as a
/// console's end of a stream outlives the service's session: a client's
/// last message, sent just before it deletes the session, is still read
/// and written down.
const CLOSING_GRACE: Duration = Duration::from_secs(1);

impl Drop for Peer {
    /// The console stops once it has its goodbye, or after the grace; not
    /// waited for here, where the service is locked.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let stop = self.stop.clone();
        let closing = std::thread::Builder::new()
            .name("xbox-mock-closing".into())
            .spawn(move || {
                let deadline = Instant::now() + CLOSING_GRACE;
                while !thread.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = thread.join();
            });
        if closing.is_err() {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

struct Console {
    rtc: Rtc,
    udp: UdpSocket,
    local: SocketAddr,
    record: Arc<Mutex<Record>>,
    view: Arc<Mutex<InputView>>,
    video: Option<Mid>,
    audio: Option<Mid>,
    channels: Vec<(String, ChannelId)>,
    connected: bool,
    encoder: Encoder,
    opus: Option<OpusEncoder>,
    frame: u64,
    samples: u64,
    next_video: Instant,
    next_audio: Instant,
    /// Messages to send on (label, binary data).
    outbox: Vec<(&'static str, Vec<u8>)>,
    link: Link,
    rng: u64,
    /// Datagrams on their way to the client, each with when it gets there.
    outbound: std::collections::VecDeque<(Instant, SocketAddr, Vec<u8>)>,
    /// When the link has sent everything queued on it.
    link_free: Instant,
    /// When the link came up: its outages count from here.
    link_since: Instant,
    /// The client's datagrams on their way here, with when they get here.
    inbound: std::collections::VecDeque<(Instant, SocketAddr, Vec<u8>)>,
    /// The A button's last state, to rumble on its press.
    a_held: bool,
    done: bool,
}

impl Console {
    fn new(
        rtc: Rtc,
        udp: UdpSocket,
        local: SocketAddr,
        record: Arc<Mutex<Record>>,
        view: Arc<Mutex<InputView>>,
        link: Link,
    ) -> Console {
        let now = Instant::now();
        Console {
            rtc,
            udp,
            local,
            record,
            view,
            video: None,
            audio: None,
            channels: Vec::new(),
            connected: false,
            encoder: Encoder::new(picture::WIDTH, picture::HEIGHT),
            opus: OpusEncoder::new(2, 96_000).ok(),
            frame: 0,
            samples: 0,
            next_video: now,
            next_audio: now,
            outbox: Vec::new(),
            link,
            rng: 0x9E37_79B9_7F4A_7C15,
            outbound: std::collections::VecDeque::new(),
            link_free: now,
            link_since: now,
            inbound: std::collections::VecDeque::new(),
            a_held: false,
            done: false,
        }
    }

    fn run(mut self, rx: Receiver<Command>, stop: &std::sync::atomic::AtomicBool) {
        let mut buf = vec![0u8; 2048];
        while !self.done && !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let Some(timeout) = self.drain() else {
                return;
            };
            while let Ok(c) = rx.try_recv() {
                self.command(c);
                if self.drain().is_none() {
                    return;
                }
            }
            while let Some((label, data)) = (!self.outbox.is_empty()).then(|| self.outbox.remove(0))
            {
                self.write(label, &data);
                if self.drain().is_none() {
                    return;
                }
            }
            let now = Instant::now();
            if self.connected && now >= self.next_video {
                self.next_video += FRAME;
                self.send_video(now);
                if self.drain().is_none() {
                    return;
                }
            }
            if self.connected && now >= self.next_audio {
                self.next_audio += AUDIO_PACKET;
                self.send_audio(now);
                if self.drain().is_none() {
                    return;
                }
            }
            self.flush();
            if self.deliver().is_none() {
                return;
            }
            let wake = [
                Some(timeout),
                Some(self.next_video),
                Some(self.next_audio),
                Some(now + Duration::from_millis(5)),
                self.outbound.front().map(|d| d.0),
                self.inbound.front().map(|d| d.0),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now);
            let wait = wake
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            // Every datagram waiting, not one a pass: a console behind on
            // its picture (a debug build on a loaded CI runner) would
            // otherwise read the client's messages ever later.
            let _ = self.udp.set_nonblocking(false);
            let _ = self.udp.set_read_timeout(Some(wait));
            let mut waited = false;
            loop {
                match self.udp.recv_from(&mut buf) {
                    Ok(_) if self.link.out(self.link_since, Instant::now()) => {}
                    Ok((n, from)) if !self.link.delay.is_zero() => {
                        let at = Instant::now() + self.link.delay;
                        self.inbound.push_back((at, from, buf[..n].to_vec()));
                    }
                    Ok((n, from)) => {
                        if self.take_in(from, &buf[..n]).is_none() {
                            return;
                        }
                    }
                    Err(_) if !waited => {
                        let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                    Err(_) => break,
                }
                if !waited {
                    waited = true;
                    let _ = self.udp.set_nonblocking(true);
                }
            }
        }
    }

    /// A datagram from the client, to str0m; `None` once the connection
    /// is over.
    fn take_in(&mut self, from: SocketAddr, data: &[u8]) -> Option<()> {
        if let Ok(contents) = data.try_into() {
            let _ = self.rtc.handle_input(Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source: from,
                    destination: self.local,
                    contents,
                },
            ));
            self.drain()?;
        }
        Some(())
    }

    /// The client's datagrams whose way here is over.
    fn deliver(&mut self) -> Option<()> {
        let now = Instant::now();
        while self.inbound.front().is_some_and(|d| d.0 <= now) {
            let (_, from, data) = self.inbound.pop_front()?;
            self.take_in(from, &data)?;
        }
        Some(())
    }

    /// Drain str0m; `None` once the connection is over.
    fn drain(&mut self) -> Option<Instant> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return Some(t),
                Ok(Output::Transmit(t)) => {
                    if self.link.video_loss > 0 && t.contents.len() > 900 && self.lose() {
                        continue;
                    }
                    let now = Instant::now();
                    let start = self.link_free.max(now);
                    if start - now > self.link.queue {
                        continue; // the link's queue is full
                    }
                    if self.link.out(self.link_since, start) {
                        continue;
                    }
                    self.link_free = start + self.link.time_for(t.contents.len());
                    let at = self.link_free + self.link.delay;
                    self.outbound
                        .push_back((at, t.destination, t.contents.to_vec()));
                }
                Ok(Output::Event(e)) => self.event(e),
                Err(_) => return None,
            }
        }
    }

    /// Send what has reached the end of the link.
    fn flush(&mut self) {
        let now = Instant::now();
        while let Some((at, to, d)) = self.outbound.front() {
            if *at > now {
                break;
            }
            let _ = self.udp.send_to(d, *to);
            self.outbound.pop_front();
        }
    }

    /// Whether to drop this packet, at the configured rate (an xorshift:
    /// repeatable, and no randomness crate for a test double).
    fn lose(&mut self) -> bool {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 100) < self.link.video_loss as u64
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::AddRemote(lines) => {
                for l in lines {
                    if let Ok(c) = Candidate::from_sdp_string(&l) {
                        self.rtc.add_remote_candidate(c);
                    }
                }
            }
            Command::Vibrate(v) => self.outbox.push(("input", write_vibration(&v).to_vec())),
            Command::Disconnect => self.outbox.push((
                "message",
                messages::console::disconnect("mock-disconnect").into_bytes(),
            )),
            Command::VideoLoss(p) => self.link.video_loss = p.min(100),
            Command::Link(link) => self.link = link,
        }
    }

    fn write(&mut self, label: &str, data: &[u8]) {
        let Some(&(_, id)) = self.channels.iter().find(|(l, _)| l == label) else {
            return;
        };
        if let Some(mut c) = self.rtc.channel(id) {
            // The console sends its JSON as text, its reports as binary.
            let _ = c.write(label == "input", data);
        }
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Connected => {
                self.connected = true;
                let now = Instant::now();
                let mut record = self.record.lock();
                record.connected = true;
                record.video_started = Some(now);
                self.next_video = now;
                self.next_audio = now;
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => self.done = true,
            Event::MediaAdded(m) => match m.kind {
                MediaKind::Video => self.video = Some(m.mid),
                MediaKind::Audio => self.audio = Some(m.mid),
            },
            Event::PeerStats(p) => {
                if let Some(rtt) = p.rtt {
                    let mut record = self.record.lock();
                    record.feedback_rtts.push(rtt);
                }
            }
            Event::KeyframeRequest(_) => {
                self.record.lock().plis += 1;
                self.encoder.request_keyframe();
            }
            Event::ChannelOpen(id, label) => {
                self.record.lock().channels.push(label.clone());
                self.channels.push((label, id));
            }
            Event::ChannelData(d) => {
                let Some(label) = self
                    .channels
                    .iter()
                    .find(|(_, id)| *id == d.id)
                    .map(|(l, _)| l.clone())
                else {
                    return;
                };
                self.channel_data(&label, &d.data);
            }
            _ => {}
        }
    }

    fn channel_data(&mut self, label: &str, data: &[u8]) {
        match label {
            "message" => {
                let Some((kind, id, target, content)) = messages::parse_envelope(data) else {
                    return;
                };
                match kind.as_str() {
                    "Handshake" => {
                        self.record.lock().handshake = true;
                        self.outbox.push((
                            "message",
                            messages::console::handshake_ack(&id).into_bytes(),
                        ));
                    }
                    "Message" => self.record.lock().messages.push((target, content)),
                    "TransactionComplete" | "ReceiverCancel" => {
                        if id == "mock-disconnect" {
                            self.done = true;
                        }
                        self.record.lock().transactions.push((kind, id));
                    }
                    _ => {}
                }
            }
            "control" => match messages::parse_control(data) {
                Some(ControlMessage::Authorization { key }) => {
                    self.record.lock().authorization = Some(key);
                }
                Some(ControlMessage::GamepadChanged { index, added }) => {
                    self.record.lock().gamepads.push((index, added));
                }
                Some(ControlMessage::KeyframeRequest { .. }) => {
                    self.record.lock().keyframe_requests += 1;
                    self.encoder.request_keyframe();
                }
                _ => {}
            },
            "input" => {
                let Some(report) = parse_client_report(data) else {
                    self.record.lock().bad_reports += 1;
                    return;
                };
                if report.touch_points.is_some() {
                    self.outbox.push((
                        "input",
                        write_video_size(picture::WIDTH as u32, picture::HEIGHT as u32).to_vec(),
                    ));
                }
                self.show_input(&report);
                let mut record = self.record.lock();
                record.reports.push(report);
                record.reports_at.push(Instant::now());
            }
            _ => {}
        }
    }

    /// Draw what the report says, and rumble on a press of A.
    fn show_input(&mut self, report: &pingpong_xbox::input::ClientReport) {
        let mut view = self.view.lock();
        if let Some(pad) = report.pads.iter().find(|p| p.index == 0) {
            view.buttons = pad.buttons;
            view.left = (pad.left_x, pad.left_y);
            view.right = (pad.right_x, pad.right_y);
            view.left_trigger = pad.left_trigger;
            view.right_trigger = pad.right_trigger;
            let a = pad.buttons & xbutton::A != 0;
            if a && !self.a_held {
                self.outbox.push((
                    "input",
                    write_vibration(&Vibration {
                        index: 0,
                        left: 60,
                        right: 30,
                        duration_ms: 150,
                        ..Default::default()
                    })
                    .to_vec(),
                ));
            }
            self.a_held = a;
        }
        for k in &report.keys {
            view.key = Some((k.vk, k.down));
        }
        for m in &report.mouse {
            view.mouse.0 = (view.mouse.0 + m.dx).clamp(-1000, 1000);
            view.mouse.1 = (view.mouse.1 + m.dy).clamp(-1000, 1000);
            view.mouse_buttons = m.buttons;
        }
    }

    fn send_video(&mut self, now: Instant) {
        let Some(mid) = self.video else {
            return;
        };
        let view = *self.view.lock();
        let pic = picture::draw(self.frame, &view);
        let (data, key) = self.encoder.encode(&pic);
        let time = MediaTime::from_90khz(self.frame * 1500);
        self.frame += 1;
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::H264)
            .map(|p| p.pt())
        else {
            return;
        };
        if writer.write(pt, now, time, data).is_ok() {
            let mut r = self.record.lock();
            r.frames += 1;
            r.keyframes += key as u64;
        }
    }

    fn send_audio(&mut self, now: Instant) {
        let (Some(mid), Some(opus)) = (self.audio, self.opus.as_mut()) else {
            return;
        };
        let start = self.samples;
        let pcm: Vec<f32> = (0..AUDIO_SAMPLES * 2)
            .map(|i| {
                let t = (start + (i / 2) as u64) as f32 / 48_000.0;
                (t * 440.0 * std::f32::consts::TAU).sin() * 0.2
            })
            .collect();
        let mut out = [0u8; 1500];
        let Ok(n) = opus.encode(&pcm, &mut out) else {
            return;
        };
        let time = MediaTime::new(self.samples, Frequency::FORTY_EIGHT_KHZ);
        self.samples += AUDIO_SAMPLES as u64;
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
        else {
            return;
        };
        if writer.write(pt, now, time, out[..n].to_vec()).is_ok() {
            self.record.lock().audio_packets += 1;
        }
    }
}
