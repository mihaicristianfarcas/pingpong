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
/// Sends are paced, as a console's are: a keyframe here is the whole
/// picture uncompressed (353 KB), which sent in one burst overflows the
/// receive buffer Linux gives a socket (about 200 KB). 200 Mbit/s spreads
/// it over 14 ms; ordinary frames (a few kilobytes) go at once.
const PACE_BYTES_PER_MS: usize = 200_000_000 / 8 / 1000;
const PACE_BURST: usize = 64 * 1024;
const AUDIO_PACKET: Duration = Duration::from_millis(20);
const AUDIO_SAMPLES: usize = 960;

pub enum Command {
    AddRemote(Vec<String>),
    Vibrate(Vibration),
    Disconnect,
    /// Drop this percentage of video packets, as a lossy link would.
    VideoLoss(u8),
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
    ) -> Result<Peer, String> {
        let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("bad offer: {e}"))?;
        let mut config = Rtc::builder().clear_codecs();
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
                    Console::new(rtc, udp, local, record, view).run(rx, &stop);
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
    video_loss: u8,
    rng: u64,
    /// Datagrams waiting for the pacer, and what it may send now.
    paced: std::collections::VecDeque<(SocketAddr, Vec<u8>)>,
    budget: usize,
    budget_at: Instant,
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
            video_loss: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
            paced: std::collections::VecDeque::new(),
            budget: PACE_BURST,
            budget_at: now,
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
            self.pace();
            let pacing = if self.paced.is_empty() { 5 } else { 1 };
            let wake = [
                timeout,
                self.next_video,
                self.next_audio,
                now + Duration::from_millis(pacing),
            ]
            .into_iter()
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
                    Ok((n, from)) => {
                        if let Ok(contents) = buf[..n].try_into() {
                            let _ = self.rtc.handle_input(Input::Receive(
                                Instant::now(),
                                Receive {
                                    proto: Protocol::Udp,
                                    source: from,
                                    destination: self.local,
                                    contents,
                                },
                            ));
                            if self.drain().is_none() {
                                return;
                            }
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

    /// Drain str0m; `None` once the connection is over.
    fn drain(&mut self) -> Option<Instant> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return Some(t),
                Ok(Output::Transmit(t)) => {
                    if self.video_loss > 0 && t.contents.len() > 900 && self.lose() {
                        continue;
                    }
                    self.paced.push_back((t.destination, t.contents.to_vec()));
                    self.pace();
                }
                Ok(Output::Event(e)) => self.event(e),
                Err(_) => return None,
            }
        }
    }

    /// Send what the pacer allows now.
    fn pace(&mut self) {
        let now = Instant::now();
        let earned =
            now.duration_since(self.budget_at).as_micros() as usize * PACE_BYTES_PER_MS / 1000;
        if earned > 0 {
            self.budget = (self.budget + earned).min(PACE_BURST);
            self.budget_at = now;
        }
        while let Some((to, d)) = self.paced.front() {
            if d.len() > self.budget {
                break;
            }
            self.budget -= d.len();
            let _ = self.udp.send_to(d, *to);
            self.paced.pop_front();
        }
    }

    /// Whether to drop this packet, at the configured rate (an xorshift:
    /// repeatable, and no randomness crate for a test double).
    fn lose(&mut self) -> bool {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 100) < self.video_loss as u64
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
            Command::VideoLoss(p) => self.video_loss = p.min(100),
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
                self.record.lock().connected = true;
                let now = Instant::now();
                self.next_video = now;
                self.next_audio = now;
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => self.done = true,
            Event::MediaAdded(m) => match m.kind {
                MediaKind::Video => self.video = Some(m.mid),
                MediaKind::Audio => self.audio = Some(m.mid),
            },
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
