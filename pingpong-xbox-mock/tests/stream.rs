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
use pingpong_xbox::input::xbutton;
use pingpong_xbox::stream::{self, StreamOptions, Target};
use pingpong_xbox_mock::{Cloud, Config, MockConsole, CLOUD_TITLE, CONSOLE_ID, CONSOLE_NAME};

/// What the client received, shared with the test.
#[derive(Default)]
struct Seen {
    frames: u64,
    keyframes: u64,
    first_was_key: Option<bool>,
    audio: u64,
    events: Vec<Event>,
    statuses: Vec<String>,
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
            let options = StreamOptions {
                width: 1280,
                height: 720,
                ..StreamOptions::default()
            };
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
        t == "/streaming/characteristics/dimensionschanged" && c.contains("\"horizontal\":1280")
    })));
    assert!(mock.wait_for(LONG, |r| r.gamepads.contains(&(0, true))));

    // A controller's A, then the keyboard's Enter (A as well): both reach
    // the console as the first controller, and the console's rumble comes
    // back.
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
    assert!(mock
        .record()
        .transactions
        .iter()
        .any(|(kind, id)| kind == "TransactionComplete" && id == "mock-disconnect"));
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
