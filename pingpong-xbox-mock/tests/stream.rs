//! Ping's Xbox client against the mock console, end to end: signing in,
//! the console list, waking the console, a session, the WebRTC connection,
//! the picture and the sound, input both ways, loss and its recovery, and
//! the stream's end from either side. Everything runs on this computer, on
//! loopback; nothing reaches Microsoft.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pingpong_proto::gamepad::{button, GamepadState};
use pingpong_proto::input::Key;
use pingpong_xbox::auth::{self, Auth, AuthError, Offering};
use pingpong_xbox::connection::{
    AudioFrame, Doorbell, End, Event, Input, Sink, Socket, VideoFrame,
};
use pingpong_xbox::consoles;
use pingpong_xbox::http::Http;
use pingpong_xbox::input::{xbutton, KeyFrame, MouseFrame, PadFrame};
use pingpong_xbox::stream::{self, StreamOptions, Target};
use pingpong_xbox::virtual_pad::KeyboardMouse;
use pingpong_xbox_mock::{Cloud, Config, Link, MockConsole, CLOUD_TITLE, CONSOLE_ID, CONSOLE_NAME};

/// What the client received, shared with the test.
#[derive(Default)]
struct Seen {
    frames: u64,
    keyframes: u64,
    first_was_key: Option<bool>,
    audio: u64,
    events: Vec<Event>,
    statuses: Vec<String>,
    /// The longest the picture stood still between two frames.
    longest_gap: Duration,
    last_frame: Option<std::time::Instant>,
}

struct TestSink {
    seen: Arc<Mutex<Seen>>,
    input: Arc<Mutex<Vec<Input>>>,
}

impl Sink for TestSink {
    fn video(&mut self, f: &VideoFrame<'_>) -> bool {
        let mut s = self.seen.lock();
        s.first_was_key.get_or_insert(f.keyframe);
        assert!(f.data.starts_with(&[0, 0, 0, 1]), "Annex B");
        s.frames += 1;
        let now = std::time::Instant::now();
        if let Some(last) = s.last_frame.replace(now) {
            s.longest_gap = s.longest_gap.max(now - last);
        }
        s.keyframes += f.keyframe as u64;
        true
    }

    fn audio(&mut self, a: &AudioFrame<'_>) {
        assert!(!a.data.is_empty());
        self.seen.lock().audio += 1;
    }

    fn event(&mut self, e: Event) {
        let mut s = self.seen.lock();
        if let Event::Status(t) = &e {
            s.statuses.push(t.clone());
        }
        s.events.push(e);
    }

    fn input(&mut self, take: &mut dyn FnMut(Input)) {
        for i in self.input.lock().drain(..) {
            take(i);
        }
    }
}

/// A stream running on a thread of its own, and the means to drive it.
struct Running {
    seen: Arc<Mutex<Seen>>,
    input: Arc<Mutex<Vec<Input>>>,
    bell: Doorbell,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<End, String>>,
}

impl Running {
    fn send(&self, i: Input) {
        self.input.lock().push(i);
        self.bell.ring();
    }

    fn wait(&self, timeout: Duration, done: impl Fn(&Seen) -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if done(&self.seen.lock()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done(&self.seen.lock())
    }

    fn end(self) -> Result<End, String> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().expect("the stream thread")
    }
}

fn start(http: &Http, dir: &Path, target: Target) -> Running {
    start_with(http, dir, target, KeyboardMouse::default())
}

fn start_with(http: &Http, dir: &Path, target: Target, keyboard_mouse: KeyboardMouse) -> Running {
    let options = StreamOptions {
        width: 1280,
        height: 720,
        keyboard_mouse,
        ..StreamOptions::default()
    };
    start_options(http, dir, target, options)
}

fn start_options(http: &Http, dir: &Path, target: Target, options: StreamOptions) -> Running {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let input = Arc::new(Mutex::new(Vec::new()));
    let socket = Socket::bind(stream::route_towards(http)).unwrap();
    let bell = socket.doorbell();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (http, dir, seen, input, stop) = (
            http.clone(),
            dir.to_path_buf(),
            seen.clone(),
            input.clone(),
            stop.clone(),
        );
        std::thread::spawn(move || {
            let mut sink = TestSink { seen, input };
            stream::run(&dir, http, &target, &options, socket, &mut sink, &stop)
        })
    };
    Running {
        seen,
        input,
        bell,
        stop,
        thread,
    }
}

fn sign_in(http: &Http, dir: &Path) {
    let code = auth::start_sign_in(http).unwrap();
    assert_eq!(code.user_code, "MOCK1234");
    let account =
        auth::finish_sign_in(http, &code, dir, &AtomicBool::new(false)).expect("signed in");
    assert_eq!(account.gamertag, "Mock Player");
    assert!(!account.install_id.is_empty());
}

const LONG: Duration = Duration::from_secs(20);

#[test]
fn a_console_streams_end_to_end() {
    let mock = MockConsole::start(Config::default()).unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    assert_eq!(mock.record().sign_ins, 1);

    let mut auth = Auth::load(http.clone(), dir.path()).unwrap();
    let list = consoles::list(&mut auth).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].name.as_str(), list[0].state()),
        (CONSOLE_NAME, "Asleep")
    );
    let friends = pingpong_xbox::people::friends(&mut auth).unwrap();
    assert_eq!(friends.len(), 2);
    assert!(friends[0].online);
    assert_eq!(friends[0].activity, "Mock Game");

    let running = start(
        &http,
        dir.path(),
        Target::Console {
            id: CONSOLE_ID.into(),
            name: CONSOLE_NAME.into(),
        },
    );
    assert!(
        running.wait(LONG, |s| s.frames >= 30 && s.audio >= 10),
        "picture and sound arrive"
    );
    // The data channels' set-up runs beside the picture's: waited for, not
    // read at once (a loaded runner had the picture first).
    assert!(mock.wait_for(LONG, |r| r.channels.len() == 4
        && r.handshake
        && r.authorization.is_some()));
    let record = mock.record();
    assert_eq!(record.woken, 1, "the sleeping console was woken first");
    assert_eq!(record.plays.len(), 1);
    assert_eq!(record.plays[0].kind, "home");
    assert_eq!(
        record.plays[0].device["dev"]["displayInfo"]["dimensions"]["widthInPixels"],
        1280
    );
    assert_eq!(
        record.channels,
        vec!["input", "chat", "control", "message"],
        "the data channels, in the web client's order"
    );
    assert!(record.handshake);
    assert_eq!(
        record.authorization.as_deref(),
        Some("4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E")
    );
    assert_eq!(record.sdp_configuration["input"]["maxVersion"], 8);
    {
        let seen = running.seen.lock();
        assert_eq!(
            seen.first_was_key,
            Some(true),
            "decoding starts on a keyframe"
        );
        assert!(seen.statuses.iter().any(|s| s.contains("Waking")));
        assert!(seen.events.contains(&Event::Connected));
        assert!(seen.events.contains(&Event::Ready));
        assert!(seen.events.contains(&Event::VideoSize {
            width: 640,
            height: 360
        }));
    }
    assert!(mock.wait_for(LONG, |r| r.messages.iter().any(|(t, c)| {
        // 1280x720 at 96 dpi: 338x190 mm.
        t == "/streaming/characteristics/dimensionschanged"
            && c.contains("\"horizontal\":338")
            && c.contains("\"preferredWidth\":1280")
    })));
    assert!(mock.wait_for(LONG, |r| r.gamepads.contains(&(0, true))));

    // A controller's A reaches the console as the first controller, and
    // the console's rumble comes back.
    running.send(Input::Pad(GamepadState {
        index: 0,
        connected: true,
        buttons: button::A,
        left_x: 20_000,
        right_trigger: 255,
        ..Default::default()
    }));
    assert!(
        mock.wait_for(LONG, |r| r.reports.iter().any(|rep| rep.pads.iter().any(
            |p| p.buttons & xbutton::A != 0 && p.left_x == 20_000 && p.right_trigger == 65535
        )))
    );
    assert!(
        running.wait(LONG, |s| s.events.iter().any(
            |e| matches!(e, Event::Rumble { index: 0, motors } if !motors.is_off())
        )),
        "the console's rumble reached the client"
    );
    assert_eq!(mock.input_view().buttons & xbutton::A, xbutton::A);
    running.send(Input::Pad(GamepadState {
        index: 0,
        connected: true,
        ..Default::default()
    }));
    // The keys are a keyboard to a console (the automatic choice): W and
    // Shift reach it as the Windows keys Microsoft's web client sends (the
    // left Shift's own), and no controller button with them.
    for (key, down) in [
        (Key::KeyW, true),
        (Key::ShiftLeft, true),
        (Key::ShiftLeft, false),
        (Key::KeyW, false),
    ] {
        running.send(Input::Key { key, down });
    }
    let typed = [
        KeyFrame {
            vk: b'W',
            down: true,
        },
        KeyFrame {
            vk: 0xA0,
            down: true,
        },
        KeyFrame {
            vk: 0xA0,
            down: false,
        },
        KeyFrame {
            vk: b'W',
            down: false,
        },
    ];
    assert!(until(LONG, || mock_keys(&mock).ends_with(&typed)));
    assert_eq!(mock_last_pad_buttons(&mock), Some(0));
    assert_eq!(mock.record().bad_reports, 0);

    // A second controller is announced when it appears.
    running.send(Input::Pad(GamepadState {
        index: 1,
        connected: true,
        buttons: button::B,
        ..Default::default()
    }));
    assert!(mock.wait_for(LONG, |r| r.gamepads.contains(&(1, true))));

    // Loss: frames stop until a keyframe, which the client asks for; then
    // they flow again.
    let before = mock.record();
    mock.set_video_loss(40);
    assert!(
        mock.wait_for(LONG, |r| r.keyframe_requests > before.keyframe_requests
            && r.plis > before.plis),
        "a lost frame makes the client ask for a keyframe, both ways"
    );
    mock.set_video_loss(0);
    let frames = running.seen.lock().frames;
    let keyframes = running.seen.lock().keyframes;
    assert!(
        running.wait(LONG, |s| s.keyframes > keyframes && s.frames > frames + 30),
        "the picture recovers on the keyframe"
    );

    // The console ends the stream.
    mock.disconnect();
    let end = running.thread.join().unwrap();
    assert_eq!(end, Ok(End::ConsoleEnded));
    assert!(mock.wait_for(LONG, |r| r.ended_sessions == 1));
    // Waited for, not read at once: the client leaves once the console's
    // side has acknowledged its answer, which can be before the mock's
    // peer has read it (seen on Windows).
    assert!(mock
        .wait_for(LONG, |r| r.transactions.iter().any(|(kind, id)| kind
            == "TransactionComplete"
            && id == "mock-disconnect")));
}

fn until(timeout: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

/// The first controller in the last report that had it.
fn mock_last_pad(mock: &MockConsole) -> Option<PadFrame> {
    mock.record()
        .reports
        .iter()
        .rev()
        .find_map(|r| r.pads.iter().find(|p| p.index == 0).copied())
}

/// Every key the console was sent, in order.
fn mock_keys(mock: &MockConsole) -> Vec<KeyFrame> {
    mock.record()
        .reports
        .iter()
        .flat_map(|r| r.keys.iter().copied())
        .collect()
}

/// The first controller's buttons in the last report that had it.
fn mock_last_pad_buttons(mock: &MockConsole) -> Option<u16> {
    mock.record()
        .reports
        .iter()
        .rev()
        .find_map(|r| r.pads.iter().find(|p| p.index == 0).map(|p| p.buttons))
}

#[test]
fn a_cloud_game_queues_then_connects_with_the_transfer_token() {
    let mock = MockConsole::start(Config {
        queue_polls: 2,
        ..Config::default()
    })
    .unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let mut auth = Auth::load(http.clone(), dir.path()).unwrap();
    assert_eq!(auth.cloud_offering(false).unwrap(), Some(Offering::Cloud));
    let token = auth.streaming_token(Offering::Cloud).unwrap();
    let service = pingpong_xbox::gssv::Service::new(
        http.clone(),
        &token,
        None,
        pingpong_xbox::gssv::Kind::Cloud,
    )
    .unwrap();
    let titles = service.titles().unwrap();
    assert_eq!(titles[0].title_id, CLOUD_TITLE);
    assert!(titles[0].playable);
    let names =
        pingpong_xbox::gssv::products(&http, &token.market, &[titles[0].product_id.clone()])
            .unwrap();
    assert_eq!(names[&titles[0].product_id].name, "Mock Game");

    let running = start(
        &http,
        dir.path(),
        Target::Cloud {
            title_id: CLOUD_TITLE.into(),
            name: "Mock Game".into(),
        },
    );
    assert!(running.wait(LONG, |s| s.frames >= 10));
    assert_eq!(mock.record().transfer_tokens, vec!["mock-transfer-token"]);
    // A cloud game gets the keys as a controller (the automatic choice):
    // Y is the controller's Y, and no key.
    assert!(mock.wait_for(LONG, |r| r.gamepads.contains(&(0, true))));
    running.send(Input::Key {
        key: Key::KeyY,
        down: true,
    });
    assert!(until(LONG, || mock_last_pad_buttons(&mock) == Some(xbutton::Y)));
    running.send(Input::Key {
        key: Key::KeyY,
        down: false,
    });
    assert!(until(LONG, || mock_last_pad_buttons(&mock) == Some(0)));
    assert!(mock_keys(&mock).is_empty());
    assert!(running
        .seen
        .lock()
        .statuses
        .iter()
        .any(|s| s.starts_with("In the queue")));
    // Stopping from this side ends the session at the service.
    assert_eq!(running.end(), Ok(End::Stopped));
    assert_eq!(mock.record().ended_sessions, 1);
}

#[test]
fn an_account_without_game_pass_plays_free_games_or_is_told() {
    let mock = MockConsole::start(Config {
        cloud: Cloud::FreeToPlay,
        ..Config::default()
    })
    .unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let mut auth = Auth::load(http.clone(), dir.path()).unwrap();
    assert_eq!(
        auth.cloud_offering(false).unwrap(),
        Some(Offering::CloudFree)
    );

    let mock = MockConsole::start(Config {
        cloud: Cloud::None,
        ..Config::default()
    })
    .unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let running = start(
        &http,
        dir.path(),
        Target::Cloud {
            title_id: CLOUD_TITLE.into(),
            name: "Mock Game".into(),
        },
    );
    let err = running.thread.join().unwrap().unwrap_err();
    assert!(err.contains("not offered"), "{err}");
}

#[test]
fn a_revoked_sign_in_asks_to_sign_in_again() {
    let mock = MockConsole::start(Config::default()).unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let store = pingpong_xbox::store::AccountStore::new(dir.path());
    let mut account = store.load();
    // Everything expired, and the refresh token no longer accepted.
    account.refresh_token = "revoked".into();
    account.access_expires = 0;
    account.user_token.expires = 0;
    account.web_token.expires = 0;
    store.save(&account).unwrap();
    let mut auth = Auth::load(http, dir.path()).unwrap();
    assert_eq!(auth.web_token().err(), Some(AuthError::SignedOut));
    assert_eq!(
        Auth::load(Http::with_mock(None), tempfile::tempdir().unwrap().path()).err(),
        Some(AuthError::SignedOut)
    );
}

#[test]
fn expired_tokens_are_renewed_from_the_refresh_token() {
    let mock = MockConsole::start(Config::default()).unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let store = pingpong_xbox::store::AccountStore::new(dir.path());
    let mut account = store.load();
    account.access_expires = 0;
    account.user_token.expires = 0;
    account.web_token.expires = 0;
    store.save(&account).unwrap();
    let mut auth = Auth::load(http, dir.path()).unwrap();
    assert!(auth.web_token().is_ok());
    assert_eq!(mock.record().refreshes, 1);
    // Kept: the next use costs nothing.
    let mut again = Auth::load(Http::with_mock(Some(mock.url())), dir.path()).unwrap();
    again.web_token().unwrap();
    assert_eq!(mock.record().refreshes, 1);
}

#[test]
fn stopping_while_queued_ends_at_once_and_frees_the_session() {
    // A queue that never moves.
    let mock = MockConsole::start(Config {
        queue_polls: 10_000,
        ..Config::default()
    })
    .unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let running = start(
        &http,
        dir.path(),
        Target::Cloud {
            title_id: CLOUD_TITLE.into(),
            name: "Mock Game".into(),
        },
    );
    assert!(running.wait(LONG, |s| s
        .statuses
        .iter()
        .any(|t| t.starts_with("In the queue"))));
    let asked = std::time::Instant::now();
    assert_eq!(running.end(), Ok(End::Stopped));
    // A state poll is a second apart: the stop does not wait for the next.
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(mock.record().ended_sessions, 1);
}

#[test]
fn a_key_reaches_the_console_within_milliseconds_while_the_picture_flows() {
    // Each press is timed from the moment it is handed over to the moment
    // the console reads it, with video arriving all the while, so the
    // connection's thread is busy with datagrams when the key comes.
    // Measured on an Apple silicon Mac, loopback, three runs of 40 keys: a
    // median of 0.18-0.20 ms and at most 0.84 ms in a release build; 0.48-
    // 0.63 ms and at most 9.8 ms in a debug one.
    let mock = MockConsole::start(Config::default()).unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let running = start(
        &http,
        dir.path(),
        Target::Console {
            id: CONSOLE_ID.into(),
            name: CONSOLE_NAME.into(),
        },
    );
    assert!(running.wait(LONG, |s| s.frames >= 30));
    assert!(mock.wait_for(LONG, |r| r.gamepads.contains(&(0, true))));
    let mut waits = Vec::new();
    for i in 0..40 {
        let sent = std::time::Instant::now();
        running.send(Input::Key {
            key: Key::KeyW,
            down: i % 2 == 0,
        });
        assert!(until(LONG, || key_arrival(&mock.record(), i).is_some()));
        let arrived = key_arrival(&mock.record(), i).unwrap();
        waits.push(arrived.saturating_duration_since(sent));
        // Off the picture's 60 Hz beat, so presses meet it at every phase.
        std::thread::sleep(Duration::from_millis(7));
    }
    waits.sort();
    let (median, slowest) = (waits[waits.len() / 2], waits[waits.len() - 1]);
    // Loose, for a loaded CI runner: a key that waited for the loop's
    // timers or for a lost doorbell would take tens of milliseconds, or
    // forever.
    eprintln!("a key's way to the console: median {median:?}, slowest {slowest:?}");
    assert!(
        median < Duration::from_millis(20),
        "median {median:?}, slowest {slowest:?}"
    );
    assert!(slowest < Duration::from_millis(250), "slowest {slowest:?}");
    assert_eq!(running.end(), Ok(End::Stopped));
}

/// When the report carrying the `n`th key the console was sent arrived.
fn key_arrival(record: &pingpong_xbox_mock::Record, n: usize) -> Option<std::time::Instant> {
    let mut seen = 0;
    for (report, at) in record.reports.iter().zip(&record.reports_at) {
        seen += report.keys.len();
        if seen > n {
            return Some(*at);
        }
    }
    None
}

#[test]
fn the_mouse_aims_and_fires_in_the_shooters_layout() {
    let mock = MockConsole::start(Config::default()).unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let running = start_with(
        &http,
        dir.path(),
        Target::Console {
            id: CONSOLE_ID.into(),
            name: CONSOLE_NAME.into(),
        },
        KeyboardMouse::Shooter,
    );
    assert!(mock.wait_for(LONG, |r| r.gamepads.contains(&(0, true))));
    // W held, the mouse moving right with its left button down.
    running.send(Input::Key {
        key: Key::KeyW,
        down: true,
    });
    for _ in 0..20 {
        running.send(Input::Mouse(MouseFrame {
            dx: 6,
            buttons: 1,
            ..Default::default()
        }));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(until(LONG, || mock_last_pad(&mock).is_some_and(|p| {
        p.left_y == i16::MAX && p.right_x > 0 && p.right_trigger == u16::MAX
    })));
    // Still, and the button up: back to the centre, the trigger let go.
    running.send(Input::Mouse(MouseFrame::default()));
    running.send(Input::Key {
        key: Key::KeyW,
        down: false,
    });
    assert!(until(LONG, || mock_last_pad(&mock).is_some_and(|p| {
        p.left_y == 0 && p.right_x == 0 && p.right_trigger == 0
    })));
    let record = mock.record();
    assert!(
        record
            .reports
            .iter()
            .all(|r| r.mouse.is_empty() && r.keys.is_empty()),
        "the console saw a controller only"
    );
    assert_eq!(record.bad_reports, 0);
    assert_eq!(running.end(), Ok(End::Stopped));
}

fn console() -> Target {
    Target::Console {
        id: CONSOLE_ID.into(),
        name: CONSOLE_NAME.into(),
    }
}

#[test]
fn the_console_hears_how_the_link_is_doing() {
    // A console sets its bitrate by the client's congestion feedback
    // (TWCC): the console measures the round trip from it, once a second.
    let mock = MockConsole::start(Config {
        link: Link {
            delay: Duration::from_millis(10),
            ..Link::default()
        },
        ..Config::default()
    })
    .unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let running = start(&http, dir.path(), console());
    assert!(running.wait(LONG, |s| s.frames >= 1));
    assert!(
        mock.wait_for(LONG, |r| r.feedback_rtts.len() >= 2),
        "no round trip from the client's feedback"
    );
    // Twice the link's 10 ms, and what the two loops add (loose for CI).
    let rtts = mock.record().feedback_rtts;
    assert!(
        rtts.iter()
            .all(|r| *r >= Duration::from_millis(20) && *r < Duration::from_millis(500)),
        "{rtts:?}"
    );
    assert_eq!(running.end(), Ok(End::Stopped));
}

#[test]
fn the_picture_keeps_coming_through_three_percent_loss() {
    // Lost packets are asked for again and used when they come. With
    // str0m's NACK window of 100 packets, a retransmission came too late to
    // count, every keyframe lacked a packet, and not one frame came
    // through this link in 15 s; with 1,000, 57 frames a second (release
    // build, `examples/link.rs`).
    let mock = MockConsole::start(Config {
        link: Link {
            delay: Duration::from_millis(10),
            video_loss: 3,
            rate_bps: 30_000_000,
            queue: Duration::from_millis(200),
            outages: None,
        },
        ..Config::default()
    })
    .unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    let running = start(&http, dir.path(), console());
    assert!(running.wait(LONG, |s| s.frames >= 1), "no first frame");
    let (frames, at) = (running.seen.lock().frames, std::time::Instant::now());
    running.seen.lock().longest_gap = Duration::ZERO;
    std::thread::sleep(Duration::from_secs(4));
    let (now, longest) = {
        let s = running.seen.lock();
        (s.frames, s.longest_gap)
    };
    let fps = (now - frames) as f64 / at.elapsed().as_secs_f64();
    // Loose, for a debug build on a loaded CI runner: 60 is sent.
    assert!(fps >= 20.0, "{fps:.1} frames a second");
    assert!(
        longest < Duration::from_millis(1500),
        "stood still {longest:?}"
    );
    assert_eq!(running.end(), Ok(End::Stopped));
}

#[test]
fn the_console_is_asked_for_the_screens_resolution_and_a_chosen_bitrate() {
    let mock = MockConsole::start(Config::default()).unwrap();
    let http = Http::with_mock(Some(mock.url()));
    let dir = tempfile::tempdir().unwrap();
    sign_in(&http, dir.path());
    // A MacBook Pro 16"'s screen, and a bitrate chosen in Settings.
    let running = start_options(
        &http,
        dir.path(),
        console(),
        StreamOptions {
            width: 3456,
            height: 2160,
            max_kbps: Some(8000),
            ..StreamOptions::default()
        },
    );
    assert!(mock.wait_for(LONG, |r| r.resolution.is_some()));
    let record = mock.record();
    assert_eq!(record.resolution.as_deref(), Some("1440"));
    assert_eq!(record.video_max_kbps, Some(8000));
    assert_eq!(running.end(), Ok(End::Stopped));

    // Automatic: the console chooses; a 720p window asks for 720p.
    let running = start(&http, dir.path(), console());
    assert!(mock.wait_for(LONG, |r| r.resolution.as_deref() == Some("720HQ")));
    assert_eq!(mock.record().video_max_kbps, None);
    assert_eq!(running.end(), Ok(End::Stopped));
}
