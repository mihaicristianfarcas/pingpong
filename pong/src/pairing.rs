//! The host's pairing listener and the queue of clients waiting for a PIN.
//!
//! A client connects and asks to pair; the request waits here until the user
//! types the PIN the client is showing into the web UI (or declines, or five
//! minutes pass). With the PIN, the user may choose what the client may do
//! here; each request says what it gets otherwise
//! (`Permissions::on_pairing`), so the choice can start from that.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pingpong_pairing::pair::{
    self, HostDescription, Incoming, PairError, PairRequest, PIN_TIMEOUT,
};
use pingpong_proto::permission::Permissions;
use serde::Serialize;

use crate::host::Host;

struct Pending {
    id: u32,
    created: Instant,
    client_name: String,
    agent: bool,
    peer: SocketAddr,
    request: Option<PairRequest>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingView {
    pub id: u32,
    pub client_name: String,
    /// An AI agent asks to pair (it will be held to the agent rules).
    pub agent: bool,
    pub peer: String,
    pub waiting_secs: u64,
    /// What it gets unless the user chooses otherwise.
    pub permissions: Permissions,
}

#[derive(Default)]
pub struct Pairing {
    pending: Mutex<Vec<Pending>>,
    next_id: Mutex<u32>,
    /// Wrong PINs recently, for rate limiting guesses.
    failures: Mutex<Vec<Instant>>,
}

pub enum PinResult {
    Paired(String),
    WrongPin,
    NotFound,
    TooManyAttempts,
    Failed(String),
}

impl Pairing {
    /// The requests waiting, each with what it would get from `clients`.
    pub fn list(&self, clients: &crate::clients::Clients) -> Vec<PendingView> {
        self.expire();
        self.pending
            .lock()
            .iter()
            .map(|p| PendingView {
                id: p.id,
                client_name: p.client_name.clone(),
                agent: p.agent,
                peer: p.peer.ip().to_string(),
                waiting_secs: p.created.elapsed().as_secs(),
                permissions: clients.default_permissions(p.agent),
            })
            .collect()
    }

    fn expire(&self) {
        let mut pending = self.pending.lock();
        pending.retain_mut(|p| {
            if !p.request.as_ref().is_some_and(|r| r.is_alive()) {
                tracing::info!(
                    client = p.client_name,
                    "pairing request withdrawn by the client"
                );
                return false;
            }
            if p.created.elapsed() > PIN_TIMEOUT {
                if let Some(r) = p.request.take() {
                    r.refuse("Nobody entered the PIN on the host in time.");
                }
                false
            } else {
                true
            }
        });
    }

    fn add(&self, req: PairRequest) {
        let mut id = self.next_id.lock();
        *id = id.wrapping_add(1).max(1);
        tracing::info!(client = req.client_name, agent = req.agent, peer = %req.peer, "pairing request waiting for a PIN");
        self.pending.lock().push(Pending {
            id: *id,
            created: Instant::now(),
            client_name: req.client_name.clone(),
            agent: req.agent,
            peer: req.peer,
            request: Some(req),
        });
    }

    pub fn decline(&self, id: u32) -> bool {
        let mut pending = self.pending.lock();
        let Some(i) = pending.iter().position(|p| p.id == id) else {
            return false;
        };
        if let Some(r) = pending.remove(i).request {
            r.refuse("The host declined the pairing request.");
        }
        true
    }

    /// Complete request `id` with `pin`; the client gets `permissions`
    /// (None: the default). Blocking (a few round trips).
    pub fn submit(
        &self,
        host: &Host,
        id: u32,
        pin: &str,
        permissions: Option<Permissions>,
    ) -> PinResult {
        {
            let mut f = self.failures.lock();
            f.retain(|t| t.elapsed() < Duration::from_secs(60));
            if f.len() >= 5 {
                return PinResult::TooManyAttempts;
            }
        }
        let request = {
            let mut pending = self.pending.lock();
            let Some(i) = pending.iter().position(|p| p.id == id) else {
                return PinResult::NotFound;
            };
            pending.remove(i).request
        };
        let Some(request) = request else {
            return PinResult::NotFound;
        };
        let name = host.config.read().name.clone();
        let extras = pingpong_pairing::pair::Extras {
            rendezvous: Some(host.rendezvous.public()),
            rendezvous_secret: host.rendezvous.secret,
            agent: false,
        };
        match request.complete(pin, &name, host.endpoint.identity().public(), &extras) {
            Ok((client_name, public, theirs)) => {
                match host.add_client(
                    &client_name,
                    &public,
                    theirs.rendezvous,
                    theirs.agent,
                    permissions,
                ) {
                    Ok(granted) => {
                        tracing::info!(
                            client = client_name,
                            agent = theirs.agent,
                            permissions = ?granted.names(),
                            "paired"
                        );
                        PinResult::Paired(client_name)
                    }
                    Err(e) => PinResult::Failed(e.to_string()),
                }
            }
            Err(PairError::WrongPin) => {
                self.failures.lock().push(Instant::now());
                tracing::warn!("pairing failed: wrong PIN");
                PinResult::WrongPin
            }
            Err(e) => PinResult::Failed(e.to_string()),
        }
    }
}

/// Accept pairing connections forever.
pub fn serve(host: Arc<Host>, port: u16) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(port, error = %e, "cannot listen for pairing");
            return;
        }
    };
    tracing::info!(port, "pairing listener up");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let host = host.clone();
        std::thread::spawn(move || {
            let me = {
                let c = host.config.read();
                HostDescription {
                    name: c.name.clone(),
                    id: host.endpoint.identity().public().short_id(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    port: c.port,
                }
            };
            match pair::accept(stream, &me) {
                Ok(Incoming::Pair(req)) => host.pairing.add(req),
                Ok(Incoming::Info) => {}
                Err(e) => tracing::debug!(error = %e, "pairing connection"),
            }
        });
    }
}
