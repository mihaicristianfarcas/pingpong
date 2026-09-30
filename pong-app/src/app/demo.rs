//! PONG_UI_DEMO: driving the window without clicking, for checks and
//! screenshots (see docs/ui.md).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{Context, Window};

use crate::api::{Client, LogEntry, Pending, Session, Status};
use crate::worker::{Conn, Snapshot};

use super::{now_ms, Page, PongApp};

/// PONG_UI_DEMO: a comma-separated list: a page's name (`overview`,
/// `devices`, `general`, `video`, `network`, `agents`, `logs`), `sample`
/// (a made-up session, request and devices, for screenshots), `signin`,
/// `setup`, `offline`, `signin-as=USER:PASSWORD`, `snapshot=PATH`, `quit`.
#[derive(Default)]
pub(super) struct Demo {
    at: Option<Instant>,
    /// Show the made-up host instead of the real one.
    pub(super) sample: bool,
    /// Pretend the connection is in this state.
    pub(super) conn: Option<Conn>,
    page: Option<Page>,
    snapshot: Option<PathBuf>,
    /// `signin-as=USER:PASSWORD`: type them and sign in.
    sign_in_as: Option<(String, String)>,
    quit: bool,
}

impl Demo {
    /// The steps `PONG_UI_DEMO` asks for, if it is set.
    pub(super) fn from_env() -> Demo {
        let mut demo = Demo::default();
        if let Ok(spec) = std::env::var("PONG_UI_DEMO") {
            for part in spec.split(',').map(str::trim) {
                if let Some(p) = part.strip_prefix("snapshot=") {
                    demo.snapshot = Some(PathBuf::from(p));
                } else if part == "quit" {
                    demo.quit = true;
                } else if part == "sample" {
                    demo.sample = true;
                } else if part == "signin" {
                    demo.conn = Some(Conn::SignIn { setup: false });
                } else if part == "setup" {
                    demo.conn = Some(Conn::SignIn { setup: true });
                } else if let Some(creds) = part.strip_prefix("signin-as=") {
                    demo.sign_in_as = creds
                        .split_once(':')
                        .map(|(u, p)| (u.to_string(), p.to_string()));
                } else if part == "offline" {
                    demo.conn = Some(Conn::NotRunning);
                } else if let Some(p) = Page::parse(part) {
                    demo.page = Some(p);
                }
            }
            demo.at = Some(Instant::now() + Duration::from_millis(1800));
        }
        demo
    }
}

impl PongApp {
    pub(super) fn run_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.demo.at.is_none_or(|at| Instant::now() < at) {
            return;
        }
        if let Some((user, password)) = self.demo.sign_in_as.take() {
            self.user.update(cx, |f, cx| f.set_text(user, cx));
            self.password.update(cx, |f, cx| f.set_text(password, cx));
            self.sign_in(cx);
            self.demo.at = Some(Instant::now() + Duration::from_millis(2500));
            return;
        }
        if let Some(p) = self.demo.page.take() {
            window.activate_window();
            cx.activate(true);
            self.set_page(p, cx);
            self.demo.at = Some(Instant::now() + Duration::from_millis(1500));
            return;
        }
        if let Some(path) = self.demo.snapshot.take() {
            match pingpong_ui::snapshot(window, &path) {
                Ok(()) => tracing::info!(path = %path.display(), "snapshot saved"),
                Err(e) => tracing::warn!(error = e, "snapshot not saved"),
            }
        }
        self.demo.at = None;
        if self.demo.quit {
            cx.quit();
        }
    }
}

/// PONG_UI_DEMO=sample: a made-up host at work, for screenshots.
pub(super) fn sample(s: &mut Snapshot) {
    let now = now_ms() / 1000;
    s.conn = Conn::Ready;
    s.state.name = "gaming-pc".into();
    s.state.os = "windows".into();
    let status = s.status.get_or_insert_with(Status::default);
    status.name = "gaming-pc".into();
    if status.id.is_empty() {
        status.id = "7f3a91c2".into();
    }
    status.version = "0.3.0".into();
    status.port = 47800;
    status.internet = true;
    status.public = vec!["203.0.113.24:47800".into()];
    status.clients = 3;
    status.tunnels = 1;
    status.session = Some(Session {
        client: "macbook".into(),
        width: 3024,
        height: 1890,
        fps: 120,
        codec: "hevc".into(),
        bitrate_kbps: 100_000,
        started_unix: now - 754,
        encoded_fps: 119.0,
        mbps: 62.4,
        host_latency_ms: 4.8,
        recoveries: 2,
        client_loss_pct: 0.03,
        rtt_ms: 3.1,
        agent: None,
    });
    status.pending = vec![Pending {
        id: 7,
        client_name: "MacBook Air".into(),
        agent: false,
        peer: "192.168.1.23:53112".into(),
        waiting_secs: 14,
    }];
    s.clients = vec![
        Client {
            name: "macbook".into(),
            id: "c41d09aa".into(),
            key: "k1".into(),
            paired_at: now - 86_400 * 2,
            online: true,
            agent: false,
            access: "control".into(),
        },
        Client {
            name: "macbook agent".into(),
            id: "a90b7e13".into(),
            key: "k2".into(),
            paired_at: now - 86_400,
            online: false,
            agent: true,
            access: "control".into(),
        },
        Client {
            name: "living-room".into(),
            id: "5be2f0d7".into(),
            key: "k3".into(),
            paired_at: now - 86_400 * 40,
            online: false,
            agent: false,
            access: "control".into(),
        },
    ];
    let ms = now_ms();
    s.log = vec![
        LogEntry {
            unix_ms: ms - 184_000,
            client: "macbook agent".into(),
            text: "key Super".into(),
        },
        LogEntry {
            unix_ms: ms - 181_000,
            client: "macbook agent".into(),
            text: "type \"notepad\"".into(),
        },
        LogEntry {
            unix_ms: ms - 176_000,
            client: "macbook agent".into(),
            text: "key Return".into(),
        },
        LogEntry {
            unix_ms: ms - 150_000,
            client: "macbook agent".into(),
            text: "click (212, 188)".into(),
        },
    ];
}
