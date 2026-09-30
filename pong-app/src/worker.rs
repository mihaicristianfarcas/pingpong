//! One thread talks to the host: every second it asks how things are (the
//! session, pairing requests, devices, the agent's activity), and it carries
//! out what the window asks, one at a time.
//!
//! With no window open only the tray icon is looking, and it needs little:
//! whether the host runs, who streams, who asks to pair. The host is then
//! asked every three seconds, and not for the devices or the agent's log.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::{json, Value};

use crate::api::{Api, Client, Failure, HostState, LogEntry, Status};

#[derive(Debug, Clone)]
pub enum Cmd {
    SubmitPin(u32, String),
    Decline(u32),
    Unpair(String),
    Access(String, String),
    AgentOp(&'static str),
    EndSession,
    PutConfig(Value),
    SignIn {
        user: String,
        password: String,
        setup: bool,
    },
    SignOut,
    /// Whether the Logs page is open (logs are read only then).
    WantLogs(bool),
    /// Whether a window is open (see the module's words on the tray).
    Watched(bool),
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Conn {
    #[default]
    Connecting,
    NotRunning,
    SignIn {
        setup: bool,
    },
    Ready,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub conn: Conn,
    pub state: HostState,
    pub status: Option<Status>,
    pub clients: Vec<Client>,
    pub log: Vec<LogEntry>,
    pub config: Option<Value>,
    pub logs: Option<String>,
    /// Signed in with the local token (nothing to sign out of).
    pub local: bool,
}

#[derive(Debug, Clone)]
pub enum Done {
    Paired(String),
    PinFailed(u32, String),
    Saved { restart: bool },
    Failed(String),
    SignedIn,
    SignInFailed(String),
}

pub enum Msg {
    Snapshot(Box<Snapshot>),
    Done(Done),
}

/// How often the host is asked how things are while a window shows it:
/// the session's numbers move every second.
const WATCHED_EVERY: Duration = Duration::from_secs(1);

/// And while only the tray icon does: a pairing request or a session
/// starting is said within three seconds, at a third of the requests.
const UNWATCHED_EVERY: Duration = Duration::from_secs(3);

pub fn spawn(wake: impl Fn() + Send + 'static) -> (Sender<Cmd>, Receiver<Msg>) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Cmd>();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded::<Msg>();
    std::thread::Builder::new()
        .name("pong-app-worker".into())
        .spawn(move || run(cmd_rx, msg_tx, wake))
        .expect("spawning the worker");
    (cmd_tx, msg_rx)
}

fn run(cmds: Receiver<Cmd>, out: Sender<Msg>, wake: impl Fn()) {
    let mut api = Api::discover();
    let mut snap = Snapshot::default();
    let mut want_logs = false;
    let mut watched = false;
    let mut clients_at: Option<Instant> = None;
    let mut logs_at: Option<Instant> = None;
    loop {
        let every = if watched {
            WATCHED_EVERY
        } else {
            UNWATCHED_EVERY
        };
        match cmds.recv_timeout(every) {
            Ok(cmd) => {
                // Drain what else is queued before looking again.
                let mut next = Some(cmd);
                while let Some(cmd) = next {
                    match cmd {
                        Cmd::WantLogs(w) => {
                            want_logs = w;
                            logs_at = None;
                        }
                        Cmd::Watched(w) => {
                            watched = w;
                            clients_at = None;
                        }
                        Cmd::Refresh => clients_at = None,
                        cmd => {
                            clients_at = None;
                            let done = execute(&mut api, &mut snap, cmd);
                            if out.send(Msg::Done(done)).is_err() {
                                return;
                            }
                        }
                    }
                    next = cmds.try_recv().ok();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let before = snap.conn.clone();
        refresh(
            &mut api,
            &mut snap,
            watched,
            want_logs,
            &mut clients_at,
            &mut logs_at,
        );
        if snap.conn != before {
            match &snap.conn {
                Conn::Ready => tracing::info!(
                    host = snap.state.name,
                    local_token = snap.local,
                    "connected to the host"
                ),
                Conn::NotRunning => tracing::info!("the host is not running"),
                Conn::SignIn { setup } => tracing::info!(setup, "the host wants its admin account"),
                Conn::Connecting => {}
            }
        }
        if out.send(Msg::Snapshot(Box::new(snap.clone()))).is_err() {
            return;
        }
        wake();
    }
}

fn refresh(
    api: &mut Api,
    snap: &mut Snapshot,
    watched: bool,
    want_logs: bool,
    clients_at: &mut Option<Instant>,
    logs_at: &mut Option<Instant>,
) {
    let state = match api.get("/api/state").and_then(|v| {
        serde_json::from_value::<HostState>(v).map_err(|e| Failure::Other(e.to_string()))
    }) {
        Ok(s) => s,
        Err(Failure::NotRunning) => {
            // It may start (and write its certificate and token) any moment.
            *api = Api::discover();
            snap.conn = Conn::NotRunning;
            snap.status = None;
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "host state");
            *api = Api::discover();
            snap.conn = Conn::NotRunning;
            return;
        }
    };
    snap.state = state;
    if !api.has_token() {
        *api = Api::discover();
    }
    if !api.has_token() {
        snap.conn = Conn::SignIn {
            setup: snap.state.setup_needed,
        };
        return;
    }
    snap.local = api.local;
    match api.get("/api/status").and_then(|v| {
        serde_json::from_value::<Status>(v).map_err(|e| Failure::Other(e.to_string()))
    }) {
        Ok(s) => snap.status = Some(s),
        Err(Failure::SignIn) => {
            api.drop_token();
            snap.conn = Conn::SignIn {
                setup: snap.state.setup_needed,
            };
            return;
        }
        Err(Failure::NotRunning) => {
            snap.conn = Conn::NotRunning;
            return;
        }
        Err(e) => tracing::warn!(error = %e, "host status"),
    }
    snap.conn = Conn::Ready;
    if !watched {
        return;
    }
    if clients_at.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
        if let Ok(v) = api.get("/api/clients") {
            snap.clients = serde_json::from_value(v).unwrap_or_default();
        }
        if snap.config.is_none() {
            snap.config = api.get("/api/config").ok();
        }
        *clients_at = Some(Instant::now());
    }
    if let Ok(v) = api.get("/api/agent/log") {
        snap.log = serde_json::from_value(v).unwrap_or_default();
    }
    if want_logs && logs_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
        snap.logs = api.text("/api/logs").ok();
        *logs_at = Some(Instant::now());
    }
}

fn execute(api: &mut Api, snap: &mut Snapshot, cmd: Cmd) -> Done {
    // What is asked, never a PIN or a password.
    match &cmd {
        Cmd::SubmitPin(id, _) => tracing::info!(request = id, "entering a PIN"),
        Cmd::SignIn { user, setup, .. } => tracing::info!(user, setup, "signing in"),
        Cmd::PutConfig(_) => tracing::info!("saving settings"),
        other => tracing::info!(?other, "asking the host"),
    }
    let done = execute_inner(api, snap, cmd);
    match &done {
        Done::Failed(e) | Done::SignInFailed(e) => tracing::warn!(error = e, "the host said no"),
        Done::PinFailed(id, e) => tracing::warn!(request = id, error = e, "pairing failed"),
        Done::Paired(name) => tracing::info!(client = name, "paired"),
        Done::Saved { restart } => tracing::debug!(restart, "done"),
        Done::SignedIn => tracing::info!("signed in"),
    }
    done
}

fn execute_inner(api: &mut Api, snap: &mut Snapshot, cmd: Cmd) -> Done {
    let fail = |e: Failure| Done::Failed(e.to_string());
    match cmd {
        Cmd::SubmitPin(id, pin) => {
            match api.post(&format!("/api/pairing/{id}"), &json!({"pin": pin})) {
                Ok(v) => Done::Paired(
                    v.get("client")
                        .and_then(Value::as_str)
                        .unwrap_or("the device")
                        .to_string(),
                ),
                Err(e) => Done::PinFailed(id, e.to_string()),
            }
        }
        Cmd::Decline(id) => api
            .delete(&format!("/api/pairing/{id}"))
            .map(|_| Done::Saved { restart: false })
            .unwrap_or_else(fail),
        Cmd::Unpair(key) => api
            .post("/api/clients/remove", &json!({"key": key}))
            .map(|_| Done::Saved { restart: false })
            .unwrap_or_else(fail),
        Cmd::Access(key, access) => api
            .post(
                "/api/clients/access",
                &json!({"key": key, "access": access}),
            )
            .map(|_| Done::Saved { restart: false })
            .unwrap_or_else(fail),
        Cmd::AgentOp(op) => api
            .post(&format!("/api/agent/{op}"), &json!({}))
            .map(|_| Done::Saved { restart: false })
            .unwrap_or_else(fail),
        Cmd::EndSession => api
            .delete("/api/session")
            .map(|_| Done::Saved { restart: false })
            .unwrap_or_else(fail),
        Cmd::PutConfig(config) => match api.put("/api/config", &config) {
            Ok(v) => {
                snap.config = Some(config);
                Done::Saved {
                    restart: v
                        .get("restart_needed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }
            }
            Err(e) => {
                snap.config = None;
                fail(e)
            }
        },
        Cmd::SignIn {
            user,
            password,
            setup,
        } => {
            let name = format!("Pong's window on {}", snap.state.name);
            match api.sign_in(&user, &password, setup, &name) {
                Ok(()) => Done::SignedIn,
                Err(e) => Done::SignInFailed(e),
            }
        }
        Cmd::SignOut => {
            api.sign_out();
            snap.conn = Conn::SignIn { setup: false };
            Done::Saved { restart: false }
        }
        Cmd::WantLogs(_) | Cmd::Watched(_) | Cmd::Refresh => Done::Saved { restart: false },
    }
}
