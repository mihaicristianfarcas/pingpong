//! A mock Xbox, for testing Ping's Xbox streaming without a console or an
//! account: Microsoft's sign-in, Xbox Live's tokens, the console list and
//! its commands, the streaming service, and a console at the end of a real
//! WebRTC connection that streams a test picture and a tone and takes
//! input -- all on this computer, at the paths and in the shapes the real
//! services use.
//!
//! A client is pointed at it with `PING_XBOX_MOCK=http://ADDRESS:PORT`
//! (`pingpong_xbox::http`). The `xbox-mock` program runs one by hand; the
//! tests start one each ([`MockConsole::start`]) and read what it received
//! ([`Record`]).
//!
//! It is a test double, written from the same reading of Greenlight as the
//! client. It proves the client speaks what Greenlight speaks and that its
//! pipeline works end to end; what only a real console can prove (that the
//! console still speaks it) is listed in `docs/xbox.md`.

mod h264;
mod http;
mod peer;
mod picture;
mod service;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
pub use pingpong_xbox::input::{ClientReport, Vibration};
use serde_json::Value;

pub use h264::{Encoder, Picture};
pub use picture::{draw as test_picture, InputView, HEIGHT, WIDTH};
pub use service::{CLOUD_TITLE, CONSOLE_ID, CONSOLE_NAME};

/// What the mock account may do in the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cloud {
    /// Game Pass Ultimate: the whole catalogue.
    GamePass,
    /// Free-to-play games only.
    FreeToPlay,
    /// Not offered.
    None,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the web APIs listen (port 0: any).
    pub bind: SocketAddr,
    pub cloud: Cloud,
    /// The console starts asleep: a stream must wake it.
    pub console_asleep: bool,
    /// Polls of the sign-in answered "not yet" before it completes.
    pub pending_polls: u32,
    /// State polls a cloud session spends in the queue.
    pub queue_polls: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            cloud: Cloud::GamePass,
            console_asleep: true,
            pending_polls: 1,
            queue_polls: 1,
        }
    }
}

/// A session the client started.
#[derive(Debug, Clone, PartialEq)]
pub struct Play {
    /// `home` or `cloud`.
    pub kind: String,
    pub target: String,
    /// The `X-MS-Device-Info` header, as JSON.
    pub device: Value,
    pub settings: Value,
}

/// What the mock saw. Tests read it.
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub sign_ins: u32,
    pub refreshes: u32,
    pub woken: u32,
    pub plays: Vec<Play>,
    pub transfer_tokens: Vec<String>,
    pub keepalives: u32,
    pub ended_sessions: u32,
    pub sdp_configuration: Value,
    pub client_candidates: Vec<String>,
    /// ICE and DTLS came up.
    pub connected: bool,
    /// Data channels the client opened, by label.
    pub channels: Vec<String>,
    pub handshake: bool,
    pub authorization: Option<String>,
    pub gamepads: Vec<(u8, bool)>,
    /// Message channel messages: (target, content).
    pub messages: Vec<(String, String)>,
    /// Transactions the client completed or turned down: (type, id).
    pub transactions: Vec<(String, String)>,
    pub reports: Vec<ClientReport>,
    pub bad_reports: u32,
    /// Keyframes asked for on the control channel, and by RTCP.
    pub keyframe_requests: u32,
    pub plis: u32,
    pub frames: u64,
    pub keyframes: u64,
    pub audio_packets: u64,
}

pub struct MockConsole {
    server: http::Server,
    service: Arc<Mutex<service::Service>>,
    record: Arc<Mutex<Record>>,
}

impl MockConsole {
    pub fn start(config: Config) -> std::io::Result<MockConsole> {
        let record = Arc::new(Mutex::new(Record::default()));
        let service = Arc::new(Mutex::new(service::Service::new(
            config.clone(),
            record.clone(),
        )));
        let handler: http::Handler = {
            let service = service.clone();
            Arc::new(move |r| service.lock().handle(r))
        };
        let server = http::Server::start(config.bind, handler)?;
        Ok(MockConsole {
            server,
            service,
            record,
        })
    }

    /// What `PING_XBOX_MOCK` is set to for a client to use this mock.
    pub fn url(&self) -> String {
        format!("http://{}", self.server.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr
    }

    pub fn record(&self) -> Record {
        self.record.lock().clone()
    }

    /// Wait until `done` holds for what the mock saw, or `timeout`.
    pub fn wait_for(&self, timeout: Duration, done: impl Fn(&Record) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done(&self.record.lock()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done(&self.record.lock())
    }

    /// The input the console shows it last received.
    pub fn input_view(&self) -> InputView {
        *self.service.lock().view.lock()
    }

    fn each_peer(&self, command: impl Fn() -> peer::Command) {
        for p in self.service.lock().peers() {
            p.send(command());
        }
    }

    /// Rumble a controller of every connected client.
    pub fn vibrate(&self, v: Vibration) {
        self.each_peer(|| peer::Command::Vibrate(v));
    }

    /// End every stream from the console's side.
    pub fn disconnect(&self) {
        self.each_peer(|| peer::Command::Disconnect);
    }

    /// Drop `percent` of video packets from now on.
    pub fn set_video_loss(&self, percent: u8) {
        self.each_peer(|| peer::Command::VideoLoss(percent));
    }
}
