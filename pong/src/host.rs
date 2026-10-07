//! The host process: one tunnel endpoint for every paired client, a receive
//! loop that dispatches control and input, and the session thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use parking_lot::{Mutex, RwLock};
use pingpong_proto::control::{Control, EndReason};
use pingpong_proto::header::{Header, Kind};
use pingpong_proto::{input, HEADER_LEN};
use pingpong_transport::{Endpoint, Identity, Peer, Received};

use crate::clients::Clients;
use crate::config::HostConfig;

/// What a session looks like from outside (web UI).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SessionStatus {
    pub client: String,
    pub width: u16,
    pub height: u16,
    pub fps: u32,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub started_unix: u64,
    pub encoded_fps: u64,
    pub mbps: f64,
    pub host_latency_ms: f64,
    pub recoveries: u64,
    pub idrs: u64,
    pub client_loss_pct: f64,
    pub rtt_ms: f64,
    /// An AI agent's session: who watches, who drives.
    pub agent: Option<AgentStatus>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AgentStatus {
    /// What the agent may do here.
    pub permissions: pingpong_proto::permission::Permissions,
    /// `control::agent_state::*`.
    pub flags: u8,
    pub watchers: Vec<String>,
    /// The watcher who took over, if one did.
    pub controller: Option<String>,
}

pub struct Host {
    pub data_dir: PathBuf,
    pub pairing: crate::pairing::Pairing,
    pub status: Arc<Mutex<Option<SessionStatus>>>,
    advertisement: Mutex<Option<pingpong_pairing::discovery::Advertisement>>,
    pub endpoint: Arc<Endpoint>,
    pub config: Arc<RwLock<HostConfig>>,
    pub clients: Arc<Mutex<Clients>>,
    /// STUN on the tunnel's socket: answers arrive in the receive loop.
    pub stun: Arc<pingpong_nat::stun::Stun>,
    pub rendezvous: pingpong_nat::keys::RendezvousKeys,
    /// The router's port mapping, as the web UI shows it ("" before the
    /// first try).
    pub port_mapping: Mutex<String>,
    shared: Arc<crate::session::Shared>,
    sessions: Sender<crate::session::SessionCmd>,
    session_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    stop: AtomicBool,
}

pub fn identity_path(dir: &Path) -> PathBuf {
    dir.join("identity.toml")
}

impl Host {
    pub fn open(data_dir: PathBuf) -> Result<Arc<Host>, String> {
        pingpong_proto::fec::warm_up();
        std::fs::create_dir_all(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
        let identity =
            Identity::load_or_create(&identity_path(&data_dir)).map_err(|e| e.to_string())?;
        let config = HostConfig::load_or_default(&data_dir);
        let endpoint = Arc::new(
            Endpoint::bind(Arc::new(identity), config.port)
                .map_err(|e| format!("cannot bind UDP port {}: {e}", config.port))?,
        );
        let rendezvous = pingpong_nat::keys::RendezvousKeys::load_or_create(
            &data_dir.join("rendezvous.toml"),
            true,
            pingpong_transport::identity::write_private,
        )
        .map_err(|e| format!("rendezvous keys: {e}"))?;
        let stun = Arc::new(pingpong_nat::stun::Stun::new());
        let clients = Clients::load(&data_dir);
        for c in clients.list() {
            match c.public() {
                Some(p) => {
                    if let Err(e) = endpoint.add_peer(p, None) {
                        tracing::warn!(client = c.name, error = %e, "could not add paired client");
                    }
                }
                None => tracing::warn!(
                    client = c.name,
                    "paired client has a malformed key; skipped"
                ),
            }
        }
        tracing::info!(
            name = config.name,
            port = endpoint.local_port(),
            clients = clients.list().len(),
            id = endpoint.identity().public().short_id(),
            "pong listening"
        );
        let config = Arc::new(RwLock::new(config));

        let shared = Arc::new(crate::session::Shared::default());
        // Unbounded: the receive loop must never wait on the session
        // thread. A session start can take seconds (a virtual display
        // arriving while the host's monitor wakes), and meanwhile the
        // client re-sends its request every 250 ms; a bounded queue filled
        // up and stopped the whole loop -- handshakes, pings, everyone --
        // until the display was ready. Repeats are cheap to drain.
        let (tx, rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(None));
        let clients = Arc::new(Mutex::new(clients));
        let (st, cl) = (status.clone(), clients.clone());
        // Built on its own thread: the display driver handle it owns is
        // not Send, and the session never leaves this thread anyway.
        let (e, c, sh, d) = (
            endpoint.clone(),
            config.clone(),
            shared.clone(),
            data_dir.clone(),
        );
        let session_thread = std::thread::Builder::new()
            .name("session".into())
            .spawn(move || crate::session::SessionManager::new(e, c, sh, &d, st, cl).run(rx))
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Host {
            data_dir,
            pairing: Default::default(),
            status,
            advertisement: Mutex::new(None),
            endpoint,
            config,
            clients,
            stun,
            rendezvous,
            port_mapping: Mutex::new(String::new()),
            shared,
            sessions: tx,
            session_thread: Mutex::new(Some(session_thread)),
            stop: AtomicBool::new(false),
        }))
    }

    /// Pairing listener and LAN advertisement.
    pub fn start_services(self: &Arc<Self>) {
        let (name, port, pairing_port, web_port) = {
            let c = self.config.read();
            (c.name.clone(), c.port, c.pairing_port, c.web_port)
        };
        let h = self.clone();
        std::thread::Builder::new()
            .name("pairing".into())
            .spawn(move || crate::pairing::serve(h, pairing_port))
            .expect("spawning the pairing listener");
        let h = self.clone();
        std::thread::Builder::new()
            .name("web".into())
            .spawn(move || crate::web::serve(h))
            .expect("spawning the web UI");
        let h = self.clone();
        std::thread::Builder::new()
            .name("presence".into())
            .spawn(move || crate::presence::run(h))
            .expect("spawning the rendezvous thread");
        let wake = crate::netif::wake_macs();
        tracing::info!(adapters = ?wake.iter().map(pingpong_pairing::wake::format).collect::<Vec<_>>(), "wake-on-LAN addresses");
        match pingpong_pairing::discovery::advertise(
            &name,
            &self.endpoint.identity().public().short_id(),
            port,
            pairing_port,
            web_port,
            &wake,
        ) {
            Ok(a) => *self.advertisement.lock() = Some(a),
            Err(e) => {
                tracing::warn!(error = %e, "mDNS advertisement failed; clients must add \
                    this host by address")
            }
        }
    }

    /// Act on the running agent session from the web UI
    /// (`agent_control::*`, except taking over: that needs a stream).
    pub fn agent_control(&self, op: u8) {
        let _ = self
            .sessions
            .send(crate::session::SessionCmd::AgentControl { peer: 0, op });
    }

    /// Set what a paired client may do. A session of its that is running,
    /// or its watching, follows at once. What it may now do (only what its
    /// kind can be allowed), or None if there is no such client.
    pub fn set_permissions(
        &self,
        x25519_b64: &str,
        permissions: pingpong_proto::permission::Permissions,
    ) -> std::io::Result<Option<pingpong_proto::permission::Permissions>> {
        let now = self
            .clients
            .lock()
            .set_permissions(x25519_b64, permissions)?;
        if let Some(permissions) = now {
            let _ = self.sessions.send(crate::session::SessionCmd::Permissions {
                key: x25519_b64.to_string(),
                permissions,
            });
        }
        Ok(now)
    }

    /// What agents did lately, oldest first.
    pub fn agent_log(&self) -> Vec<crate::session::AgentLogEntry> {
        self.shared.agent_log.lock().iter().cloned().collect()
    }

    /// End whatever session is running (web UI).
    pub fn end_session(&self) {
        let peer = self.shared.active_peer.load(Ordering::Acquire);
        if peer != 0 {
            let _ = self.sessions.send(crate::session::SessionCmd::End {
                peer,
                reason: EndReason::Quit,
            });
        }
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn stop_flag(&self) -> &AtomicBool {
        &self.stop
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.sessions.send(crate::session::SessionCmd::Shutdown);
    }

    /// Admit a newly paired client to the tunnel, with these permissions
    /// (None: what a client of its kind gets by default).
    pub fn add_client(
        &self,
        name: &str,
        public: &pingpong_transport::PublicIdentity,
        rendezvous: Option<[u8; 32]>,
        agent: bool,
        permissions: Option<pingpong_proto::permission::Permissions>,
    ) -> std::io::Result<pingpong_proto::permission::Permissions> {
        let client = self
            .clients
            .lock()
            .add(name, public, rendezvous, agent, permissions)?;
        self.endpoint
            .add_peer(public.clone(), None)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Paired again while connected: what it may do now applies.
        let _ = self.sessions.send(crate::session::SessionCmd::Permissions {
            key: client.x25519.clone(),
            permissions: client.permissions,
        });
        Ok(client.permissions)
    }

    /// Forget a client: it can no longer complete a handshake.
    pub fn remove_client(&self, x25519_b64: &str) -> std::io::Result<bool> {
        let removed = self.clients.lock().remove(x25519_b64)?;
        if let Some(c) = &removed {
            if let Some(p) = c.public() {
                if let Some(peer) = self.endpoint.remove_peer(&p.x25519) {
                    let _ = self.sessions.send(crate::session::SessionCmd::End {
                        peer: peer.id(),
                        reason: EndReason::Quit,
                    });
                }
            }
        }
        Ok(removed.is_some())
    }

    /// The receive loop. Runs on the calling thread until `shutdown`.
    pub fn serve(self: &Arc<Self>) {
        // Input from the client arrives here.
        #[cfg(not(windows))]
        crate::priority::latency_critical();
        let mut buf = vec![0u8; 65536];
        let mut last_tick = Instant::now();
        while !self.stop.load(Ordering::Relaxed) {
            match self.endpoint.recv(&mut buf) {
                Ok(Received::Data(peer, len)) => self.on_packet(&peer, &buf[..len]),
                Ok(Received::Foreign(from, len)) => {
                    self.stun.on_datagram(from, &buf[..len]);
                }
                Ok(_) => {}
                // A signal (Ctrl-C on a Mac or Linux): the loop looks at `stop`.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "receive failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            if last_tick.elapsed() >= Duration::from_millis(100) {
                last_tick = Instant::now();
                self.endpoint.tick();
            }
        }
        // Let the session restore the displays and tell the client before the
        // process goes.
        let _ = self.sessions.send(crate::session::SessionCmd::Shutdown);
        if let Some(t) = self.session_thread.lock().take() {
            let _ = t.join();
        }
        tracing::info!("pong stopped");
    }

    fn reply(&self, peer: &Peer, msg: Control) {
        let mut out = [0u8; pingpong_proto::control::MAX_CONTROL_LEN];
        let n = msg.encode(pingpong_proto::clock::now_us(), &mut out);
        let _ = self.endpoint.send(peer, &out[..n]);
    }

    fn on_packet(&self, peer: &Arc<Peer>, packet: &[u8]) {
        let Ok(header) = Header::decode(packet) else {
            return;
        };
        let body = &packet[HEADER_LEN..header.total_len as usize];
        match header.kind {
            Kind::Control if pingpong_proto::clip::is_clip(body) => {
                if self.shared.is_active(peer.id()) {
                    if let Some(clip) = self.shared.clip.lock().as_ref() {
                        clip.deliver(body);
                    }
                }
            }
            Kind::Control => {
                let Some(msg) = Control::decode(body) else {
                    return;
                };
                self.on_control(peer, msg);
            }
            Kind::Input => self.on_input(peer, &header, body),
            Kind::Video | Kind::Audio => {}
        }
    }

    fn on_control(&self, peer: &Arc<Peer>, msg: Control) {
        if matches!(msg, Control::SessionStart(_)) {
            // Tell the client how to find us across the internet (pairings
            // made before pairing carried it learn it here).
            if let Some(secret) = self.rendezvous.secret {
                self.reply(
                    peer,
                    Control::RendezvousOffer {
                        key: self.rendezvous.public(),
                        secret,
                    },
                );
            }
        }
        match msg {
            Control::RendezvousKey { key } => {
                let (x, _) = peer.public().to_b64();
                match self.clients.lock().set_rendezvous(&x, key) {
                    Ok(true) => {
                        tracing::info!(client = %peer.public().short_id(), "client can now connect from the internet")
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "could not save the client's rendezvous key")
                    }
                }
            }
            Control::Ping { id, sent_us } => self.reply(peer, Control::Pong { id, sent_us }),
            Control::SessionStart(req) => {
                let _ = self.sessions.send(crate::session::SessionCmd::Start {
                    peer: peer.clone(),
                    req,
                });
            }
            Control::SessionEnd(reason) => {
                let _ = self.sessions.send(crate::session::SessionCmd::End {
                    peer: peer.id(),
                    reason,
                });
            }
            Control::AgentControl(op) => {
                let _ = self
                    .sessions
                    .send(crate::session::SessionCmd::AgentControl {
                        peer: peer.id(),
                        op,
                    });
            }
            Control::AgentNote(note) => {
                if self.shared.is_active(peer.id())
                    && self.shared.agent_session.load(Ordering::Acquire)
                {
                    let name = self
                        .clients
                        .lock()
                        .name_of(&peer.public().x25519)
                        .unwrap_or("agent")
                        .to_string();
                    self.shared.agent_note(&name, note.as_str());
                }
            }
            Control::LossReport(report) => {
                // A streaming client reports every second. One this host has
                // no session for (Pong restarted under it) gets no video, so
                // asks for nothing else that would tell it: tell it here.
                if !self.shared.receives_video(peer.id()) {
                    self.reply(peer, Control::SessionEnd(EndReason::NoSession));
                    return;
                }
                let _ = self.sessions.send(crate::session::SessionCmd::Loss {
                    peer: peer.id(),
                    report,
                });
            }
            #[cfg(windows)]
            Control::Gamepad(state) => {
                // A client that may not use controllers: its pads hear
                // nothing, and centre themselves (`gamepad::STALE_AFTER`).
                let may = pingpong_proto::permission::Permissions::from_bits(
                    self.shared.client_permissions.load(Ordering::Acquire),
                )
                .allows(pingpong_proto::permission::CONTROLLER);
                if may && self.shared.is_active(peer.id()) {
                    if let Some((owner, pads)) = self.shared.pads.lock().as_ref() {
                        if *owner == peer.id() {
                            pads.apply(state);
                        }
                    }
                }
            }
            Control::RequestIdr | Control::InvalidateRefs { .. } => {
                if !self.shared.receives_video(peer.id()) {
                    // A client that believes in a session the host does not
                    // have (host restarted, session replaced): tell it, so it
                    // starts a new one instead of asking forever.
                    self.reply(peer, Control::SessionEnd(EndReason::NoSession));
                    return;
                }
                if let Some(v) = self.shared.video.lock().as_ref() {
                    let cmd = match msg {
                        Control::InvalidateRefs { first, last } => {
                            crate::video::VideoCmd::Invalidate { first, last }
                        }
                        _ => crate::video::VideoCmd::Idr,
                    };
                    let _ = v.try_send(cmd);
                }
            }
            _ => {}
        }
    }

    fn on_input(&self, peer: &Peer, header: &Header, body: &[u8]) {
        let Some(batch) = input::decode(body, header.frame_id, header.fragment_idx) else {
            return;
        };
        let mut guard = self.shared.input.lock();
        let Some(state) = guard.as_mut() else { return };
        if state.peer != peer.id() {
            // Not theirs to drive now (a watcher, or the agent while a
            // watcher drives): seen, so it is not replayed later, and dropped.
            if self.shared.receives_video(peer.id()) {
                state.others.entry(peer.id()).or_default().admit(&batch);
            }
            return;
        }
        let events = state.gate.admit(&batch);
        if !events.is_empty() && !state.held {
            state.inject(events, Instant::now());
        }
    }
}
