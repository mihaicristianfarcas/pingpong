//! Streaming from an Xbox -- a console of the user's, or a game in Xbox
//! Cloud Gaming -- in the same window, through the same decoder, presenter,
//! speakers and controllers as a stream from Pong. The protocol is
//! `pingpong_xbox`'s; this is where it meets Ping's platform layer:
//!
//! - **Video**: each complete H.264 frame goes from the connection's thread
//!   straight to the platform's [`VideoOut`], as a Pong stream's frames do
//!   from its network thread: no queue and no jitter buffer between them.
//! - **Audio**: Opus packets into Ping's own player and its adaptive
//!   buffer (`pingpong_audio::player`).
//! - **Input**: the platform's keys and mouse ([`InputSender`]) and its
//!   controllers ([`ControlSender`]) are handed to the connection, which
//!   its doorbell wakes for them.
//! - **Rumble**: the console's patterns, played as Pong's motor states.
//!
//! Signing in, the console list and the cloud library are here too, for the
//! app and the CLI ([`account`]).

pub mod account;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use pingpong_audio::player::Player;
use pingpong_proto::audio::AudioPacket;
use pingpong_proto::control::{self, AckStatus, Control, SessionAck};
use pingpong_proto::gamepad::GamepadState;
use pingpong_proto::input::{Button, InputEvent};
use pingpong_xbox::connection::{
    AudioFrame, End, Event as XEvent, Input, Sink, Socket, VideoFrame,
};
use pingpong_xbox::input::MouseFrame;
use pingpong_xbox::stream::StreamOptions;

pub use pingpong_xbox::stream::Target;
pub use pingpong_xbox::virtual_pad::KeyboardMouse;

use crate::input::{InputSender, Msg};
use crate::stats::StatsCollector;
use crate::stream::{Codec, Event, EventSink, FrameTiming, StreamSettings, VideoOut};

/// An Xbox to stream from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct XboxSource {
    pub target: Target,
    /// What the keyboard and mouse are to the console.
    #[serde(default)]
    pub keyboard_mouse: KeyboardMouse,
    /// The cloud region, when not the account's default.
    #[serde(default)]
    pub region: Option<String>,
}

struct Ctx {
    stop: AtomicBool,
    stats: Arc<StatsCollector>,
    audio_muted: AtomicBool,
    pads: Sender<GamepadState>,
    bell: pingpong_xbox::connection::Doorbell,
}

/// A stream from an Xbox.
pub(crate) struct XboxStream {
    ctx: Arc<Ctx>,
    net: Option<JoinHandle<()>>,
    input_tx: InputSender,
}

/// Controllers' states and the mute, from other threads.
#[derive(Clone)]
pub(crate) struct Controls(Arc<Ctx>);

impl Controls {
    pub(crate) fn send(&self, msg: Control) {
        // Controllers are all a console takes of Ping's control messages
        // (no clipboard, no agents).
        if let Control::Gamepad(state) = msg {
            let _ = self.0.pads.try_send(state);
            self.0.bell.ring();
        }
    }

    pub(crate) fn set_audio_muted(&self, muted: bool) {
        self.0.audio_muted.store(muted, Ordering::Relaxed);
    }
}

impl XboxStream {
    pub(crate) fn start(
        source: XboxSource,
        settings: StreamSettings,
        video: Box<dyn VideoOut>,
        events: EventSink,
        stats: Arc<StatsCollector>,
    ) -> Result<XboxStream, String> {
        let http = pingpong_xbox::http::Http::new();
        let socket = Socket::bind(pingpong_xbox::stream::route_towards(&http))
            .map_err(|e| format!("cannot open a UDP socket: {e}"))?;
        let bell = socket.doorbell();
        // Controller states are whole: a full queue (a stalled connection)
        // drops the oldest worth of them, and the next state corrects it.
        let (pads_tx, pads_rx) = crossbeam_channel::bounded(256);
        let ctx = Arc::new(Ctx {
            stop: AtomicBool::new(false),
            stats,
            audio_muted: AtomicBool::new(false),
            pads: pads_tx,
            bell: bell.clone(),
        });
        let (input_tx, input_rx) =
            InputSender::channel(settings.mouse, Arc::new(move || bell.ring()));
        let options = StreamOptions {
            width: settings.width as u32,
            height: settings.height as u32,
            locale: locale(),
            region: source.region.clone(),
            keyboard_mouse: source.keyboard_mouse,
        };
        let dir = crate::store::data_dir();
        let net = {
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("ping-xbox".into())
                .spawn(move || {
                    crate::priority::latency_critical();
                    let mut sink = CoreSink {
                        video,
                        configured: false,
                        size: (options.width, options.height),
                        audio: None,
                        audio_channels: settings.audio_channels,
                        events: events.clone(),
                        ctx: ctx.clone(),
                        input_rx,
                        pads_rx,
                        frame_id: 0,
                        motion: (0.0, 0.0),
                        buttons: 0,
                        epoch: Instant::now(),
                        input_waits: InputWaits::default(),
                    };
                    let outcome = pingpong_xbox::stream::run(
                        &dir,
                        http,
                        &source.target,
                        &options,
                        socket,
                        &mut sink,
                        &ctx.stop,
                    );
                    let (reason, error) = match outcome {
                        Ok(End::Stopped) => return,
                        Ok(End::ConsoleEnded) => ("The console ended the stream.".into(), false),
                        Ok(End::Lost(why)) | Err(why) => (why, true),
                    };
                    events(Event::Ended { reason, error });
                })
                .map_err(|e| e.to_string())?
        };
        Ok(XboxStream {
            ctx,
            net: Some(net),
            input_tx,
        })
    }

    pub(crate) fn input(&self) -> &InputSender {
        &self.input_tx
    }

    pub(crate) fn controls(&self) -> Controls {
        Controls(self.ctx.clone())
    }

    pub(crate) fn stats_collector(&self) -> &Arc<StatsCollector> {
        &self.ctx.stats
    }

    pub(crate) fn is_running(&self) -> bool {
        self.net.as_ref().is_some_and(|n| !n.is_finished())
    }

    /// End the stream: the connection closes and the session is deleted at
    /// the service, so the console is free at once.
    pub(crate) fn stop(&mut self) {
        self.ctx.stop.store(true, Ordering::Relaxed);
        self.ctx.bell.ring();
        if let Some(n) = self.net.take() {
            let _ = n.join();
        }
    }
}

/// The language games are asked to use: the system's, as `LANG` says it.
fn locale() -> String {
    locale_from(std::env::var("LANG").ok().as_deref())
}

/// `en_GB.UTF-8` is `en-GB`; no language, or C, is US English.
fn locale_from(lang: Option<&str>) -> String {
    lang.and_then(|l| {
        let tag = l.split(['.', '@']).next()?.replace('_', "-");
        (tag.len() >= 2 && tag != "POSIX").then_some(tag)
    })
    .unwrap_or_else(|| "en-US".into())
}

/// What the console's session is, to Ping's platform layer: H.264 at the
/// picture's size, the pointer drawn in the picture by the console (so
/// Ping draws none and sends relative motion), Windows' keys, and the
/// input a console takes (keyboard, mouse, controllers; no clipboard).
fn session_ack(width: u32, height: u32, audio_channels: u8) -> SessionAck {
    SessionAck {
        status: AckStatus::Ok,
        codec: control::codec::H264,
        width: width as u16,
        height: height as u16,
        refresh_mhz: 60_000,
        bitrate_kbps: 0,
        audio_channels,
        nonce: 0,
        host: control::host::WINDOWS,
        features: control::features::POINTER_IN_PICTURE,
        video: 0,
        permissions: pingpong_proto::permission::Permissions::PERSON_CONTROL.bits(),
    }
}

/// The DOM's `buttons` bit for a mouse button.
fn button_bit(b: Button) -> u8 {
    match b {
        Button::Left => 1,
        Button::Right => 2,
        Button::Middle => 4,
        Button::X1 => 8,
        Button::X2 => 16,
    }
}

/// How long the window's keys and mouse waited for the connection's
/// thread, logged once a second while there is input: the part of a key's
/// way to the console that is Ping's. Counted once the console has taken
/// the handshake: input from before has waited for the connection itself.
#[derive(Default)]
struct InputWaits {
    live: bool,
    events: u32,
    longest_us: u32,
    since: Option<Instant>,
}

impl InputWaits {
    fn add(&mut self, queued_us: u32) {
        if !self.live {
            return;
        }
        let wait = pingpong_proto::clock::now_us().wrapping_sub(queued_us);
        self.events += 1;
        self.longest_us = self.longest_us.max(wait);
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::debug!(
                events = self.events,
                longest_ms = format_args!("{:.2}", self.longest_us as f64 / 1000.0),
                "input"
            );
            *self = InputWaits {
                live: true,
                ..InputWaits::default()
            };
        }
    }
}

/// Where the connection's output meets Ping's platform layer.
struct CoreSink {
    video: Box<dyn VideoOut>,
    configured: bool,
    /// The picture's size: what was asked for, until the console says.
    size: (u32, u32),
    audio: Option<Player>,
    audio_channels: u8,
    events: EventSink,
    ctx: Arc<Ctx>,
    input_rx: Receiver<(Msg, u32)>,
    pads_rx: Receiver<GamepadState>,
    frame_id: u32,
    /// Relative motion not yet sent: whole units go, the rest waits.
    motion: (f64, f64),
    buttons: u8,
    epoch: Instant,
    input_waits: InputWaits,
}

impl CoreSink {
    fn started(&mut self) {
        let ack = session_ack(self.size.0, self.size.1, self.audio_channels);
        self.ctx.stats.set_ack(ack);
        (self.events)(Event::Started(ack));
    }

    fn us(&self, at: Instant) -> u32 {
        pingpong_proto::clock::now_us()
            .wrapping_sub(Instant::now().saturating_duration_since(at).as_micros() as u32)
    }

    /// A mouse frame of the motion gathered so far (whole units only) and
    /// the buttons and wheel now.
    fn mouse(&mut self, wheel_x: i32, wheel_y: i32) -> Input {
        let (dx, dy) = (self.motion.0.trunc(), self.motion.1.trunc());
        self.motion.0 -= dx;
        self.motion.1 -= dy;
        Input::Mouse(MouseFrame {
            dx: dx as i32,
            dy: dy as i32,
            wheel_x,
            wheel_y,
            buttons: self.buttons,
        })
    }
}

impl Sink for CoreSink {
    fn video(&mut self, f: &VideoFrame<'_>) -> bool {
        if !self.configured {
            if let Err(e) = self
                .video
                .configure(Codec::H264, self.size.0, self.size.1, 0)
            {
                tracing::warn!(error = %e, "no decoder for the console's picture");
                return false;
            }
            self.configured = true;
        }
        let now_us = pingpong_proto::clock::now_us();
        self.frame_id = self.frame_id.wrapping_add(1);
        let timing = FrameTiming {
            frame_id: self.frame_id,
            captured_us: 0,
            host_us: 0,
            first_packet_us: self.us(f.arrived),
            reassembled_us: now_us,
        };
        let stats = &self.ctx.stats;
        stats.packet(f.data.len());
        stats.frame_received(true);
        stats.network_latency(now_us.wrapping_sub(timing.first_packet_us));
        match self.video.decode(f.data, timing) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, "decode");
                stats.decode_error();
                false
            }
        }
    }

    fn audio(&mut self, a: &AudioFrame<'_>) {
        if self.audio_channels == 0 {
            return;
        }
        if self.audio.is_none() {
            // Started with the first packet: the console's audio is stereo.
            self.audio = crate::stream::start_audio(2);
            if self.audio.is_none() {
                self.audio_channels = 0;
                return;
            }
        }
        if let Some(player) = &self.audio {
            player.set_muted(self.ctx.audio_muted.load(Ordering::Relaxed));
            player.push(AudioPacket {
                // RTP's sequence, extended: the player's reorder buffer
                // wants it increasing, which the low 32 bits are for days.
                seq: a.seq as u32,
                capture_ts_us: self.epoch.elapsed().as_micros() as u32,
                recovered: false,
                data: a.data.to_vec(),
            });
        }
    }

    fn event(&mut self, e: XEvent) {
        match e {
            XEvent::Status(text) => (self.events)(Event::Status(text)),
            XEvent::Connected => {}
            XEvent::Ready => {
                self.input_waits.live = true;
                self.started();
            }
            XEvent::VideoSize { width, height } => {
                tracing::info!(width, height, "the console's picture");
                if (width, height) != self.size && width > 0 && height > 0 {
                    self.size = (width, height);
                    self.started();
                }
            }
            XEvent::Rumble { index, motors } => (self.events)(Event::Rumble {
                index,
                low: motors.low,
                high: motors.high,
            }),
            XEvent::Title(info) => tracing::info!(%info, "title"),
            XEvent::Stats(s) => {
                if let Some(rtt) = s.rtt {
                    self.ctx.stats.rtt(rtt.as_micros() as u32);
                }
                if let Some(loss) = s.loss {
                    self.ctx.stats.loss(control::LossReport {
                        received: 10_000 - (loss.clamp(0.0, 1.0) * 10_000.0) as u32,
                        expected: 10_000,
                        frames_lost: 0,
                        frames_ok: 0,
                        rtt_us: s.rtt.map_or(0, |r| r.as_micros() as u32),
                        received_kbps: s.received_kbps,
                    });
                }
                self.ctx.stats.gate(pingpong_proto::video::GateStats {
                    idr_requests: s.keyframe_requests,
                    ..Default::default()
                });
            }
        }
    }

    fn input(&mut self, take: &mut dyn FnMut(Input)) {
        while let Ok(state) = self.pads_rx.try_recv() {
            take(Input::Pad(state));
        }
        let mut moved = false;
        while let Ok((msg, queued_us)) = self.input_rx.try_recv() {
            self.input_waits.add(queued_us);
            match msg {
                Msg::Motion(dx, dy) => {
                    self.motion.0 += dx;
                    self.motion.1 += dy;
                    moved = true;
                }
                Msg::Event(ev) => match ev {
                    InputEvent::KeyDown(sc) | InputEvent::KeyUp(sc) => {
                        if let Some(key) = pingpong_xbox::keymap::key_for_scancode(sc) {
                            take(Input::Key {
                                key,
                                down: matches!(ev, InputEvent::KeyDown(_)),
                            });
                        }
                    }
                    InputEvent::MouseMoveRel { dx, dy } => {
                        self.motion.0 += dx as f64;
                        self.motion.1 += dy as f64;
                        moved = true;
                    }
                    InputEvent::ButtonDown(b) | InputEvent::ButtonUp(b) => {
                        if matches!(ev, InputEvent::ButtonDown(_)) {
                            self.buttons |= button_bit(b);
                        } else {
                            self.buttons &= !button_bit(b);
                        }
                        take(self.mouse(0, 0));
                        moved = false;
                    }
                    InputEvent::Wheel { dv, dh } => {
                        take(self.mouse(dh as i32, dv as i32));
                        moved = false;
                    }
                    // A console takes relative motion only, and no text.
                    InputEvent::MouseMoveAbs { .. } | InputEvent::Text(_) => {}
                },
            }
        }
        if moved {
            take(self.mouse(0, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_locale_comes_from_lang() {
        assert_eq!(locale_from(Some("en_GB.UTF-8")), "en-GB");
        assert_eq!(locale_from(Some("ro_RO@euro")), "ro-RO");
        assert_eq!(locale_from(Some("C")), "en-US");
        assert_eq!(locale_from(Some("POSIX")), "en-US");
        assert_eq!(locale_from(None), "en-US");
    }

    #[test]
    fn the_console_draws_its_own_pointer() {
        let ack = session_ack(1920, 1080, 2);
        assert_ne!(ack.features & control::features::POINTER_IN_PICTURE, 0);
        assert_eq!(ack.codec, control::codec::H264);
        assert_eq!((ack.width, ack.height), (1920, 1080));
    }

    #[test]
    fn mouse_buttons_are_the_doms_bits() {
        assert_eq!(button_bit(Button::Left), 1);
        assert_eq!(button_bit(Button::Right), 2);
        assert_eq!(button_bit(Button::Middle), 4);
    }
}
