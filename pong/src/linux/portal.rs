//! Wayland: the screen and the input through the desktop portal
//! (xdg-desktop-portal), the way desktop sharing is meant to work there.
//!
//! A RemoteDesktop session with a screen cast in it gives both, on GNOME and
//! KDE; where the desktop has no RemoteDesktop portal (wlroots: sway,
//! Hyprland) a ScreenCast session gives the screen alone. The first session
//! asks whoever sits at the host to allow it (the desktop's own dialog);
//! the portal then hands back a restore token, kept in the data directory,
//! and later sessions start without asking until it is revoked.
//!
//! The portal is a D-Bus service, driven here from one small async runtime
//! for the whole process; input goes to it over a queue, in order.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions, Stream,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use pingpong_input::{HeldSet, InputError, InputSink};
use pingpong_proto::input::{Button, InputEvent};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// One thing to tell the portal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    /// A Linux key code, down or up.
    Key(i32, bool),
    /// A keysym (text), down or up.
    Keysym(i32, bool),
    Motion(f64, f64),
    /// In the stream's own (logical) coordinates.
    Absolute(f64, f64),
    /// A Linux button code (BTN_*), down or up.
    Button(i32, bool),
    /// Wheel notches; `true` for the vertical wheel, positive down or right.
    Scroll(bool, i32),
}

enum Cmd {
    Input(Vec<Op>),
    Close,
}

/// What the desktop granted.
pub struct Granted {
    /// The PipeWire connection and the screen's stream on it.
    pub fd: OwnedFd,
    pub node: u32,
    /// The stream's size in the desktop's (logical) coordinates.
    pub size: Option<(u32, u32)>,
    /// Keyboard and pointer: a RemoteDesktop session.
    pub input: bool,
}

pub struct Portal {
    tx: UnboundedSender<Cmd>,
    /// Hung up when the session's task has finished (closed the session).
    done: std::sync::mpsc::Receiver<()>,
}

/// Where every portal call runs. One runtime for the life of the process:
/// ashpd keeps one D-Bus connection for all of them, and that connection
/// lives on the runtime that first made it (with a runtime per session, the
/// second session's calls waited forever on the first's, gone).
fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("portal")
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

impl Portal {
    /// A session on the desktop in `dir`'s name (its restore token). Blocks
    /// until the desktop grants it, or `wait` passes (the first time, someone
    /// has to answer the desktop's dialog).
    pub fn open(dir: &Path, wait: Duration) -> Result<(Portal, Granted), String> {
        let token_path = dir.join("portal-token");
        let (tx, rx) = unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done) = std::sync::mpsc::channel::<()>();
        runtime()?.spawn(async move {
            serve(token_path, rx, ready_tx).await;
            drop(done_tx);
        });
        let portal = Portal { tx, done };
        match ready_rx.recv_timeout(wait) {
            Ok(Ok(granted)) => Ok((portal, granted)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!(
                "the desktop did not allow screen sharing within {} s",
                wait.as_secs()
            )),
        }
    }

    fn sender(&self) -> UnboundedSender<Cmd> {
        self.tx.clone()
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Close);
        // The session closed before the next one opens (a moment; not
        // forever, if the portal stopped answering).
        let _ = self.done.recv_timeout(Duration::from_secs(5));
    }
}

enum Kind {
    Remote(RemoteDesktop, Session<RemoteDesktop>),
    Cast(Session<Screencast>),
}

async fn serve(
    token_path: PathBuf,
    mut rx: UnboundedReceiver<Cmd>,
    ready: std::sync::mpsc::SyncSender<Result<Granted, String>>,
) {
    let saved = std::fs::read_to_string(&token_path).unwrap_or_default();
    // Until the desktop answers, which may be never (a dialog nobody sees):
    // a close ends the wait.
    let started = tokio::select! {
        r = start(&saved) => r,
        _ = rx.recv() => return,
    };
    let (kind, stream, token) = match started {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Each token works once; the new one is for next time.
    if let Some(t) = token {
        if let Err(e) = std::fs::write(&token_path, t) {
            tracing::warn!(error = %e, "could not keep the portal's restore token");
        }
    }
    let node = stream.pipe_wire_node_id();
    let fd = match Screencast::new().await {
        Ok(sc) => match &kind {
            Kind::Remote(_, session) => {
                sc.open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
                    .await
            }
            Kind::Cast(session) => {
                sc.open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
                    .await
            }
        },
        Err(e) => Err(e),
    };
    let fd = match fd {
        Ok(fd) => fd,
        Err(e) => {
            let _ = ready.send(Err(format!("no PipeWire connection from the portal: {e}")));
            return;
        }
    };
    let size = stream
        .size()
        .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32));
    let input = matches!(kind, Kind::Remote(..));
    tracing::info!(node, ?size, input, "the desktop is shared");

    // Keep the screen on while it streams.
    let inhibit = match InhibitProxy::new().await {
        Ok(p) => p
            .inhibit(
                None,
                enumflags2::BitFlags::from(InhibitFlags::Idle),
                InhibitOptions::default().set_reason("Pong is streaming this screen"),
            )
            .await
            .map_err(|e| tracing::info!(error = %e, "the screen may blank during the session"))
            .ok(),
        Err(e) => {
            tracing::info!(error = %e, "no Inhibit portal: the screen may blank during the \
                session");
            None
        }
    };

    let _ = ready.send(Ok(Granted {
        fd,
        node,
        size,
        input,
    }));
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Input(ops) => {
                if let Kind::Remote(rd, session) = &kind {
                    for op in ops {
                        if let Err(e) = apply(rd, session, node, op).await {
                            tracing::debug!(error = %e, ?op, "portal input");
                        }
                    }
                }
            }
            Cmd::Close => break,
        }
    }
    if let Some(r) = inhibit {
        let _ = r.close().await;
    }
    let _ = match &kind {
        Kind::Remote(_, s) => s.close().await,
        Kind::Cast(s) => s.close().await,
    };
}

/// A RemoteDesktop session where the desktop has the portal, else a
/// ScreenCast one; the stream, and the token for next time.
async fn start(saved: &str) -> Result<(Kind, Stream, Option<String>), String> {
    let restore = |kind: &str| saved.strip_prefix(kind).map(str::to_string);
    let cast_sources = || {
        SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(enumflags2::BitFlags::from(SourceType::Monitor))
            .set_multiple(false)
    };
    match RemoteDesktop::new().await {
        Ok(rd) => {
            let token = restore("rd:");
            let session = rd
                .create_session(CreateSessionOptions::default())
                .await
                .map_err(|e| e.to_string())?;
            rd.select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await
            .and_then(|r| r.response())
            .map_err(|e| format!("choosing the devices: {e}"))?;
            let sc = Screencast::new().await.map_err(|e| e.to_string())?;
            sc.select_sources(&session, cast_sources())
                .await
                .and_then(|r| r.response())
                .map_err(|e| format!("choosing the screen: {e}"))?;
            let started = rd
                .start(&session, None, StartOptions::default())
                .await
                .and_then(|r| r.response())
                .map_err(|e| format!("the desktop did not allow sharing the screen: {e}"))?;
            let stream = started
                .streams()
                .first()
                .cloned()
                .ok_or("the desktop shared no screen")?;
            let token = started.restore_token().map(|t| format!("rd:{t}"));
            if !started.devices().contains(DeviceType::Pointer) {
                tracing::warn!("the desktop did not share the pointer");
            }
            Ok((Kind::Remote(rd, session), stream, token))
        }
        Err(e) => {
            tracing::info!(error = %e, "no RemoteDesktop portal: the screen alone, and no \
                input from the client");
            let token = restore("sc:");
            let sc = Screencast::new()
                .await
                .map_err(|e| format!("no ScreenCast portal either: {e}"))?;
            let session = sc
                .create_session(CreateSessionOptions::default())
                .await
                .map_err(|e| e.to_string())?;
            sc.select_sources(
                &session,
                cast_sources()
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await
            .and_then(|r| r.response())
            .map_err(|e| format!("choosing the screen: {e}"))?;
            let started = sc
                .start(&session, None, StartCastOptions::default())
                .await
                .and_then(|r| r.response())
                .map_err(|e| format!("the desktop did not allow sharing the screen: {e}"))?;
            let stream = started
                .streams()
                .first()
                .cloned()
                .ok_or("the desktop shared no screen")?;
            let token = started.restore_token().map(|t| format!("sc:{t}"));
            Ok((Kind::Cast(session), stream, token))
        }
    }
}

async fn apply(
    rd: &RemoteDesktop,
    s: &Session<RemoteDesktop>,
    node: u32,
    op: Op,
) -> Result<(), ashpd::Error> {
    let state = |down: bool| {
        if down {
            KeyState::Pressed
        } else {
            KeyState::Released
        }
    };
    match op {
        Op::Key(code, down) => {
            rd.notify_keyboard_keycode(s, code, state(down), Default::default())
                .await
        }
        Op::Keysym(sym, down) => {
            rd.notify_keyboard_keysym(s, sym, state(down), Default::default())
                .await
        }
        Op::Motion(dx, dy) => {
            rd.notify_pointer_motion(s, dx, dy, Default::default())
                .await
        }
        Op::Absolute(x, y) => {
            rd.notify_pointer_motion_absolute(s, node, x, y, Default::default())
                .await
        }
        Op::Button(code, down) => {
            rd.notify_pointer_button(s, code, state(down), Default::default())
                .await
        }
        Op::Scroll(vertical, steps) => {
            let axis = if vertical {
                Axis::Vertical
            } else {
                Axis::Horizontal
            };
            rd.notify_pointer_axis_discrete(s, axis, steps, Default::default())
                .await
        }
    }
}

/// A notch of the wheel, in the protocol's units (Windows' WHEEL_DELTA).
const NOTCH: i32 = 120;

/// The client's input, as portal calls.
pub struct PortalSink {
    tx: UnboundedSender<Cmd>,
    map: Mapping,
    held: HeldSet,
}

impl PortalSink {
    pub fn new(
        portal: &Portal,
        picture: (u32, u32, u32, u32),
        size: Option<(u32, u32)>,
    ) -> PortalSink {
        let size = size.unwrap_or((picture.2, picture.3));
        PortalSink {
            tx: portal.sender(),
            map: Mapping {
                picture,
                size,
                wheel: (0, 0),
            },
            held: HeldSet::new(),
        }
    }
}

impl InputSink for PortalSink {
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError> {
        let mut ops = Vec::with_capacity(events.len());
        for &ev in events {
            self.held.observe(ev);
            self.map.ops(ev, &mut ops);
        }
        if ops.is_empty() {
            return Ok(());
        }
        self.tx
            .send(Cmd::Input(ops))
            .map_err(|_| InputError::Unavailable)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let events = self.held.drain_release_events();
        self.map.wheel = (0, 0);
        self.inject(&events)
    }
}

/// Stream pixels and scancodes to the portal's terms. Pure, so tested.
struct Mapping {
    /// Where the picture sits in the stream (x, y, w, h).
    picture: (u32, u32, u32, u32),
    /// The shared screen in the desktop's coordinates.
    size: (u32, u32),
    /// Wheel movement short of a notch, (vertical, horizontal).
    wheel: (i32, i32),
}

impl Mapping {
    fn ops(&mut self, ev: InputEvent, out: &mut Vec<Op>) {
        match ev {
            InputEvent::KeyDown(sc) | InputEvent::KeyUp(sc) => {
                if let Some(code) = pingpong_input::evdev::key_code(sc) {
                    out.push(Op::Key(code as i32, matches!(ev, InputEvent::KeyDown(_))));
                }
            }
            InputEvent::MouseMoveRel { dx, dy } => out.push(Op::Motion(dx as f64, dy as f64)),
            InputEvent::MouseMoveAbs { x, y } => {
                let (px, py, pw, ph) = self.picture;
                let (sw, sh) = self.size;
                let map = |v: u16, p: u32, len: u32, s: u32| {
                    let v = (v as f64 - p as f64).clamp(0.0, len.max(1) as f64 - 1.0);
                    v * s as f64 / len.max(1) as f64
                };
                out.push(Op::Absolute(map(x, px, pw, sw), map(y, py, ph, sh)));
            }
            InputEvent::ButtonDown(b) | InputEvent::ButtonUp(b) => {
                // BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA.
                let code = match b {
                    Button::Left => 0x110,
                    Button::Right => 0x111,
                    Button::Middle => 0x112,
                    Button::X1 => 0x113,
                    Button::X2 => 0x114,
                };
                out.push(Op::Button(code, matches!(ev, InputEvent::ButtonDown(_))));
            }
            InputEvent::Wheel { dv, dh } => {
                // The protocol's wheel is Windows': up is positive; the
                // portal's is Wayland's: down is.
                self.wheel.0 += dv as i32;
                self.wheel.1 += dh as i32;
                let (v, h) = (self.wheel.0 / NOTCH, self.wheel.1 / NOTCH);
                self.wheel.0 -= v * NOTCH;
                self.wheel.1 -= h * NOTCH;
                if v != 0 {
                    out.push(Op::Scroll(true, -v));
                }
                if h != 0 {
                    out.push(Op::Scroll(false, h));
                }
            }
            InputEvent::Text(c) => {
                if let Some(sym) = keysym(c) {
                    out.push(Op::Keysym(sym, true));
                    out.push(Op::Keysym(sym, false));
                }
            }
        }
    }
}

/// The X keysym that types `c`.
fn keysym(c: char) -> Option<i32> {
    let cp = c as u32;
    Some(match c {
        '\n' | '\r' => 0xFF0D,
        '\t' => 0xFF09,
        _ if cp < 0x20 || cp == 0x7F => return None,
        _ if cp <= 0x7E || (0xA0..=0xFF).contains(&cp) => cp as i32,
        _ => (0x0100_0000 | cp) as i32,
    })
}

/// Whether this session is a Wayland one: the portal, not X11. `PONG_CAPTURE`
/// (`x11` or `portal`) decides it where the environment does not tell.
pub fn wayland_session() -> bool {
    match std::env::var("PONG_CAPTURE").as_deref() {
        Ok("x11") => false,
        Ok("portal") => true,
        _ => {
            std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
                || std::env::var_os("WAYLAND_DISPLAY").is_some()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> Mapping {
        // A 1920x1080 desktop, letterboxed into a 1280x800 stream.
        Mapping {
            picture: (0, 40, 1280, 720),
            size: (1920, 1080),
            wheel: (0, 0),
        }
    }

    #[test]
    fn pointer_lands_where_it_points() {
        let mut out = Vec::new();
        map().ops(InputEvent::MouseMoveAbs { x: 640, y: 400 }, &mut out);
        assert_eq!(out, [Op::Absolute(960.0, 540.0)]);
        out.clear();
        map().ops(InputEvent::MouseMoveAbs { x: 10, y: 5 }, &mut out);
        assert_eq!(
            out,
            [Op::Absolute(15.0, 0.0)],
            "above the picture: its top edge"
        );
    }

    #[test]
    fn keys_buttons_wheel_and_text() {
        let mut m = map();
        let mut out = Vec::new();
        m.ops(InputEvent::KeyDown(0x1E), &mut out);
        m.ops(InputEvent::ButtonUp(Button::Right), &mut out);
        m.ops(InputEvent::Wheel { dv: 60, dh: 0 }, &mut out);
        m.ops(InputEvent::Wheel { dv: 60, dh: -240 }, &mut out);
        m.ops(InputEvent::Text('é'), &mut out);
        assert_eq!(
            out,
            [
                Op::Key(30, true),
                Op::Button(0x111, false),
                Op::Scroll(true, -1),
                Op::Scroll(false, -2),
                Op::Keysym(0xE9, true),
                Op::Keysym(0xE9, false),
            ]
        );
    }
}
