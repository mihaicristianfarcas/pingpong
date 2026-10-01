//! An agent's session with a host: the same stream a person gets (tunnel,
//! loss recovery, input), but as this device's agent identity and without a
//! window -- pictures decoded into memory for screenshots.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use ping_core::input::InputSender;
use ping_core::stats::{Stats, StatsCollector};
use ping_core::store::{self, Hosts, KnownHost};
use ping_core::stream::{ControlSender, Event, Stream, StreamSettings};
use pingpong_proto::control::{self, AgentState, CursorState, SessionAck};
use pingpong_proto::screen::{self, Assembly, Query, ScreenText};

use crate::decode::{FrameStore, HeadlessVideo};

/// What an agent's session asks for.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HeadlessOptions {
    /// The host's display is made this size (a Windows or Mac host), so
    /// screenshots, the display and clicks share one coordinate space.
    pub width: u16,
    pub height: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Only the internet path (tests).
    pub wan_only: bool,
    /// Keep the host's own monitors on beside the agent's display.
    pub keep_host_displays: bool,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        // 1280x800: what Anthropic and OpenAI suggest for computer use, small
        // enough for a model to read whole, big enough for real apps.
        HeadlessOptions {
            width: 1280,
            height: 800,
            fps: 30,
            bitrate_kbps: 12_000,
            wan_only: false,
            keep_host_displays: false,
        }
    }
}

/// What the stream said about itself.
#[derive(Default)]
struct Seen {
    ack: Option<SessionAck>,
    cursor: Option<CursorState>,
    agent: Option<AgentState>,
    /// Why it ended (and whether that was an error).
    ended: Option<(String, bool)>,
    status: Option<String>,
    warning: Option<String>,
    /// The host's answer to a question about the screen, part by part, and
    /// the last one whole.
    screen_parts: Option<Assembly>,
    screen: Option<(u32, Vec<u8>)>,
}

pub struct HeadlessSession {
    stream: Stream,
    pub frames: Arc<FrameStore>,
    seen: Arc<(Mutex<Seen>, Condvar)>,
    /// Numbers the questions about the screen.
    next_question: std::sync::atomic::AtomicU32,
    /// The host answered one; or never did (an older Pong), and is asked
    /// no more.
    screen_answered: std::sync::atomic::AtomicBool,
    screen_unanswered: std::sync::atomic::AtomicBool,
    stats: Arc<StatsCollector>,
    pub host_name: String,
    pub host_os: u8,
}

/// The paired hosts this device's agent can use.
pub fn agent_hosts(data_dir: &Path) -> Vec<KnownHost> {
    Hosts::load(&store::agent_dir(data_dir)).list().to_vec()
}

fn find_host(data_dir: &Path, host: &str) -> Result<KnownHost, String> {
    let hosts = agent_hosts(data_dir);
    if let Some(h) = hosts
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(host) || h.x25519 == host)
    {
        return Ok(h.clone());
    }
    let person = Hosts::load(data_dir);
    let names: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
    if person.find(host).is_some() {
        return Err(format!(
            "This device's AI agent is not paired with {host} yet (only Ping is). Pair it once: `ping pair-agent {host}` \
                or Ping > Agents > Pair, then type the PIN in Pong's web UI on {host}."
        ));
    }
    Err(if names.is_empty() {
        "This device's AI agent is not paired with any host. Pair it with `ping pair-agent \
            HOST` or in Ping > Agents."
            .to_string()
    } else {
        format!(
            "No host named {host}. The agent can use: {}.",
            names.join(", ")
        )
    })
}

impl HeadlessSession {
    /// Connect to `host` as this device's agent and wait for its first
    /// picture (at most `timeout`).
    pub fn connect(
        data_dir: &Path,
        host: &str,
        opts: &HeadlessOptions,
        timeout: Duration,
    ) -> Result<HeadlessSession, String> {
        let dir = store::agent_dir(data_dir);
        let known = find_host(data_dir, host)?;
        let target = ping_core::session::host_target(&dir, &known, opts.wan_only)?;
        let identity = Arc::new(store::identity(&dir)?);
        let host_id = target.public.short_id();
        let frames = Arc::new(FrameStore::default());
        let stats = Arc::new(StatsCollector::default());
        let seen: Arc<(Mutex<Seen>, Condvar)> = Arc::default();
        let events: ping_core::stream::EventSink = {
            let seen = seen.clone();
            Arc::new(move |ev| {
                let (lock, cond) = &*seen;
                let mut s = lock.lock();
                match ev {
                    Event::Started(ack) => {
                        s.ack = Some(ack);
                        // "Starting the video…" is over.
                        s.status = None;
                    }
                    Event::Cursor(c) => s.cursor = Some(c),
                    Event::Agent(a) => s.agent = Some(a),
                    Event::Ended { reason, error } => s.ended = Some((reason, error)),
                    Event::Status(t) => s.status = Some(t),
                    Event::Notice(t) => s.status = t,
                    Event::Warning(t) => s.warning = t,
                    Event::Rumble { .. } => {}
                    Event::ScreenPart {
                        id,
                        total,
                        offset,
                        bytes,
                    } => {
                        if s.screen_parts.as_ref().is_none_or(|a| a.id != id) {
                            s.screen_parts = Assembly::new(id, total);
                        }
                        let done = s
                            .screen_parts
                            .as_mut()
                            .is_some_and(|a| a.add(total, offset, &bytes) && a.complete());
                        if done {
                            let a = s.screen_parts.take().expect("there");
                            s.screen = Some((a.id, a.into_bytes()));
                        }
                    }
                }
                cond.notify_all();
            })
        };
        let settings = StreamSettings {
            width: opts.width & !1,
            height: opts.height & !1,
            fps: opts.fps.clamp(5, 60),
            bitrate_kbps: opts.bitrate_kbps.max(1000),
            codecs: control::codec::H264 | control::codec::HEVC,
            audio_channels: 0,
            host_audio: false,
            vsync: false,
            frame_pacing: false,
            keep_host_displays: opts.keep_host_displays,
            slices: 1,
            app: control::app::DESKTOP,
            watch: false,
            clipboard: false,
        };
        let video = Box::new(HeadlessVideo::new(frames.clone(), stats.clone()));
        let stream = Stream::start(identity, target, settings, video, events, stats.clone())?;
        // Look for the host on the local network meanwhile, as Ping does.
        if !opts.wan_only {
            let candidates = stream.candidates();
            let dir = dir.clone();
            std::thread::spawn(move || {
                let found = ping_core::pair::discover(&dir, Duration::from_millis(1500))
                    .unwrap_or_default();
                if let Some(f) = found.iter().find(|f| f.id == host_id) {
                    candidates.add_local(std::net::SocketAddr::new(f.address, f.port));
                }
            });
        }
        let deadline = Instant::now() + timeout;
        {
            let (lock, cond) = &*seen;
            let mut s = lock.lock();
            while s.ack.is_none() && s.ended.is_none() {
                if cond.wait_until(&mut s, deadline).timed_out() {
                    break;
                }
            }
            if let Some((reason, _)) = &s.ended {
                return Err(reason.clone());
            }
            if s.ack.is_none() {
                return Err(format!(
                    "{} did not start the session within {} s.",
                    known.name,
                    timeout.as_secs()
                ));
            }
        }
        if frames
            .wait_first(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_secs(5)),
            )
            .is_none()
        {
            return Err(format!(
                "{} started the session but sent no picture.",
                known.name
            ));
        }
        let host_os = seen.0.lock().ack.map_or(control::host::WINDOWS, |a| a.host);
        Ok(HeadlessSession {
            stream,
            frames,
            seen,
            next_question: std::sync::atomic::AtomicU32::new(1),
            screen_answered: std::sync::atomic::AtomicBool::new(false),
            screen_unanswered: std::sync::atomic::AtomicBool::new(false),
            stats,
            host_name: known.name,
            host_os,
        })
    }

    /// The stream's size: screenshots' and clicks' coordinates.
    pub fn size(&self) -> (u32, u32) {
        let s = self.seen.0.lock();
        match s.ack {
            Some(a) => (a.width as u32, a.height as u32),
            None => (0, 0),
        }
    }

    pub fn input(&self) -> &InputSender {
        self.stream.input()
    }

    pub fn controls(&self) -> ControlSender {
        self.stream.controls()
    }

    /// Who drives, as the host last said (None: it has not said).
    pub fn agent_state(&self) -> Option<AgentState> {
        self.seen.0.lock().agent
    }

    pub fn cursor(&self) -> Option<CursorState> {
        self.seen.0.lock().cursor
    }

    /// Why the session ended, if it has.
    pub fn ended(&self) -> Option<String> {
        if let Some((reason, _)) = &self.seen.0.lock().ended {
            return Some(reason.clone());
        }
        (!self.stream.is_running()).then(|| "The session ended.".to_string())
    }

    /// Something the stream is saying now ("Reconnecting…"), if anything.
    pub fn notice(&self) -> Option<String> {
        let s = self.seen.0.lock();
        s.warning.clone().or_else(|| s.status.clone())
    }

    pub fn stats(&self) -> Stats {
        self.stats.snapshot()
    }

    pub fn ack(&self) -> Option<SessionAck> {
        self.seen.0.lock().ack
    }

    /// What the host's accessibility tree says about the screen (see
    /// `pingpong_proto::screen`). A reply lost on the way is asked for
    /// again; a host that never answers (an older Pong) is not asked again.
    pub fn screen_text(&self, query: Query) -> Result<ScreenText, String> {
        use std::sync::atomic::Ordering;
        if self.screen_unanswered.load(Ordering::Relaxed) {
            return Err(OLD_HOST.into());
        }
        let id = self.next_question.fetch_add(1, Ordering::Relaxed);
        let packet = screen::request_packet(id, query);
        let (lock, cond) = &*self.seen;
        for attempt in 0..SCREEN_ASKS {
            self.controls().send_packet(&packet);
            let deadline = Instant::now() + SCREEN_WAIT;
            let mut s = lock.lock();
            loop {
                if let Some((got, _)) = &s.screen {
                    if *got == id {
                        let (_, bytes) = s.screen.take().expect("there");
                        self.screen_answered.store(true, Ordering::Relaxed);
                        return ScreenText::decode(&bytes)
                            .ok_or_else(|| "The host's answer was malformed.".to_string());
                    }
                }
                // Parts of this answer arrived: it is coming, and wanted
                // whole, not asked for again.
                let arriving = s.screen_parts.as_ref().is_some_and(|a| a.id == id);
                if cond.wait_until(&mut s, deadline).timed_out() && !arriving {
                    break;
                }
                if s.ended.is_some() {
                    return Err("The session ended.".into());
                }
            }
            tracing::debug!(id, attempt, "no answer about the screen yet");
        }
        if !self.screen_answered.load(Ordering::Relaxed) {
            self.screen_unanswered.store(true, Ordering::Relaxed);
            return Err(OLD_HOST.into());
        }
        Err("The host did not say what is on the screen in time.".into())
    }

    pub fn close(mut self) {
        self.stream.stop();
    }
}

/// How long one question about the screen waits for its answer, and how
/// many times it is asked. A read takes up to 0.6 s on the host, and the
/// first one in an app longer (`pong/src/mac/a11y.rs`).
const SCREEN_WAIT: Duration = Duration::from_millis(1500);
const SCREEN_ASKS: usize = 2;

const OLD_HOST: &str = "The host does not read its screen as text (its Pong predates it).";
