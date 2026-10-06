//! A stream from start to end, the same for every client (the window, the
//! CLI, the tests against the mock console): sign in, wake the console if
//! it sleeps, start a session and wait for it (the cloud may queue it),
//! negotiate the connection, run it, keep the session alive, end it.
//! Greenlight's `streammanager.ts` and its stream page
//! (`renderer/pages/stream/[serverid].tsx`) do the same between them.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::{Auth, AuthError, Offering};
use crate::connection::{Connection, End, Event, Options, Sink, Socket};
use crate::consoles::{self, Command};
use crate::gssv::{failure_message, Kind, PlaySettings, Service, SessionState};
use crate::http::Http;
use crate::ice::with_teredo;

/// How long a session may stay in "Provisioning" (a console waking from
/// sleep takes tens of seconds); a cloud queue is not counted.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(120);

/// What to stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Target {
    /// A console of the account's, by its id.
    Console { id: String, name: String },
    /// A cloud title, by its id.
    Cloud { title_id: String, name: String },
}

impl Target {
    pub fn name(&self) -> &str {
        match self {
            Target::Console { name, .. } | Target::Cloud { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOptions {
    /// The size the picture is shown at.
    pub width: u32,
    pub height: u32,
    pub locale: String,
    /// The cloud region to play in, when not the account's default.
    pub region: Option<String>,
    pub keyboard_as_controller: bool,
}

impl Default for StreamOptions {
    fn default() -> Self {
        StreamOptions {
            width: 1920,
            height: 1080,
            locale: "en-US".into(),
            region: None,
            keyboard_as_controller: true,
        }
    }
}

/// What the socket's host candidate is chosen towards: the interface that
/// routes to the internet (only the routing table is asked; nothing is
/// sent), or the mock console's address.
pub fn route_towards(http: &Http) -> IpAddr {
    http.mock_ip()
        .unwrap_or(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)))
}

/// Status for the person waiting.
fn status(sink: &mut dyn Sink, text: impl Into<String>) {
    let text = text.into();
    tracing::info!("{text}");
    sink.event(Event::Status(text));
}

fn auth_error(e: AuthError) -> String {
    e.to_string()
}

/// Run a stream of `target` on `socket` until it ends. The account is the
/// one kept in `dir`. `Err` says why it could not start or failed.
pub fn run(
    dir: &Path,
    http: Http,
    target: &Target,
    options: &StreamOptions,
    socket: Socket,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
) -> Result<End, String> {
    status(sink, "Signing in to Xbox…");
    let mut auth = Auth::load(http.clone(), dir).map_err(auth_error)?;
    let (kind, offering, server) = match target {
        Target::Console { id, .. } => (Kind::Home, Offering::Home, id.clone()),
        Target::Cloud { title_id, .. } => {
            let offering = auth
                .cloud_offering(false)
                .map_err(auth_error)?
                .ok_or("Xbox Cloud Gaming is not offered to this account here.")?;
            (Kind::Cloud, offering, title_id.clone())
        }
    };
    let token = auth.streaming_token(offering).map_err(auth_error)?;
    let service = Service::new(http.clone(), &token, options.region.as_deref(), kind)
        .map_err(|e| e.to_string())?;

    if let Target::Console { id, name } = target {
        wake(&mut auth, id, name, sink);
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(End::Stopped);
    }

    status(sink, format!("Starting {}…", target.name()));
    let settings = PlaySettings {
        width: options.width,
        height: options.height,
        locale: options.locale.clone(),
        ..PlaySettings::default()
    };
    let session = service
        .play(&server, &settings)
        .map_err(|e| e.to_string())?;
    let end = run_session(
        &mut auth, &service, &session, target, options, socket, sink, stop,
    );
    // End the session, whatever happened, so the console is free now and
    // not when the service gives up on it; quickly, as whoever ended the
    // stream may be waiting for this thread.
    if let Err(e) = service.quick().stop(&session) {
        tracing::debug!(error = %e, "ending the session");
    }
    end
}

/// Wake a sleeping console before asking it for a stream: the service
/// would wait for it anyway, and this says what is happening.
fn wake(auth: &mut Auth, id: &str, name: &str, sink: &mut dyn Sink) {
    let console = match consoles::list(auth) {
        Ok(list) => list.into_iter().find(|c| c.id == id),
        Err(e) => {
            tracing::debug!(error = %e, "console list");
            None
        }
    };
    let Some(console) = console else {
        return;
    };
    if let Some(why) = console.cannot_stream() {
        // Said, then tried anyway: the list can be behind the console.
        status(sink, why);
    }
    if !console.is_on() {
        status(sink, format!("Waking {name}…"));
        if let Err(e) = consoles::send(auth, id, Command::WakeUp) {
            tracing::warn!(error = %e, "wake");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    auth: &mut Auth,
    service: &Service,
    session: &str,
    target: &Target,
    options: &StreamOptions,
    mut socket: Socket,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
) -> Result<End, String> {
    // Learn the public address while the session is set up.
    socket.discover_public();
    if !wait_provisioned(auth, service, session, target, sink, stop)? {
        return Ok(End::Stopped);
    }

    status(sink, "Connecting…");
    let (mut connection, offer) = Connection::offer(
        socket,
        Options {
            width: options.width,
            height: options.height,
            install_id: install_id(auth),
            keyboard_as_controller: options.keyboard_as_controller,
        },
    )?;
    let stopped = || stop.load(Ordering::Relaxed);
    let answer = match service.exchange_sdp(session, &offer, stop) {
        Ok(a) => a,
        Err(_) if stopped() => return Ok(End::Stopped),
        Err(e) => return Err(e.to_string()),
    };
    connection.accept_answer(&answer)?;
    let remote = match service.exchange_ice(session, &connection.local_candidates(), stop) {
        Ok(r) => r,
        Err(_) if stopped() => return Ok(End::Stopped),
        Err(e) => return Err(e.to_string()),
    };
    let usable = connection.add_remote_candidates(&with_teredo(&remote));
    if usable == 0 {
        return Err("The console offered no address this computer can use.".into());
    }

    let _keepalive = Keepalive::start(service.quick(), session.to_owned());
    Ok(connection.run(sink, stop))
}

fn install_id(auth: &mut Auth) -> String {
    if auth.account.install_id.is_empty() {
        auth.account.install_id = crate::uuid_v4();
    }
    auth.account.install_id.clone()
}

/// Wait until the session is ready; `false` if stopped first.
fn wait_provisioned(
    auth: &mut Auth,
    service: &Service,
    session: &str,
    target: &Target,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
) -> Result<bool, String> {
    let mut provisioning_since = Instant::now();
    let mut connected = false;
    let mut queued = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        match service.state(session).map_err(|e| e.to_string())? {
            SessionState::Provisioned => return Ok(true),
            SessionState::Provisioning | SessionState::Other(_) => {
                if provisioning_since.elapsed() > PROVISION_TIMEOUT {
                    return Err(format!(
                        "{} did not get ready in time. Is it on, or asleep, and online?",
                        target.name()
                    ));
                }
            }
            SessionState::WaitingForResources => {
                provisioning_since = Instant::now();
                if !queued {
                    queued = true;
                    let wait = match target {
                        Target::Cloud { title_id, .. } => {
                            service.wait_time(title_id).ok().flatten()
                        }
                        Target::Console { .. } => None,
                    };
                    status(
                        sink,
                        match wait {
                            Some(s) if s >= 60 => {
                                format!("In the queue: about {} minutes.", s.div_ceil(60))
                            }
                            Some(_) => "In the queue: less than a minute.".to_owned(),
                            None => "In the queue…".to_owned(),
                        },
                    );
                }
            }
            SessionState::ReadyToConnect if !connected => {
                connected = true;
                let token = auth.transfer_token().map_err(auth_error)?;
                service
                    .connect(session, &token)
                    .map_err(|e| e.to_string())?;
            }
            SessionState::ReadyToConnect => {}
            SessionState::Failed { code, message } => {
                return Err(failure_message(&code, &message));
            }
        }
        if !crate::gssv::pause(stop, crate::gssv::STATE_POLL_EVERY) {
            return Ok(false);
        }
    }
}

/// Keeps a session alive on a thread of its own (an HTTPS round trip has
/// no place on the connection's thread), until dropped.
struct Keepalive {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Keepalive {
    fn start(service: Service, session: String) -> Keepalive {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("xbox-keepalive".into())
                .spawn(move || {
                    let mut last = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(200));
                        if last.elapsed() >= crate::gssv::KEEPALIVE_EVERY {
                            last = Instant::now();
                            if let Err(e) = service.keepalive(&session) {
                                tracing::debug!(error = %e, "keepalive");
                            }
                        }
                    }
                })
                .ok()
        };
        Keepalive { stop, thread }
    }
}

impl Drop for Keepalive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
