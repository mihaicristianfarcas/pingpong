//! Being reachable from the internet with no port forwarding (docs/networking.md).
//!
//! The host learns the public address its NAT gives the tunnel's socket (STUN,
//! on that socket), publishes it as a sealed rendezvous record on the Mainline
//! DHT, and watches its paired clients' records:
//!
//! - a client that is open (Ping running) publishes its presence; the host
//!   sends it a small packet every 20 s, which keeps the host's NAT open
//!   towards it, so when the user clicks, the client's handshake walks
//!   straight in;
//! - a client that wants to connect now publishes an intent; the host answers
//!   with handshake initiations towards it at once (the slower path, for
//!   when the warm one did not hold).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_nat::rendezvous::{client_salt, Kind, Record, Rendezvous, SALT};
use pingpong_transport::PublicIdentity;

use crate::host::Host;

/// NAT mappings for UDP last tens of seconds to minutes: re-probe well inside.
const STUN_EVERY: Duration = Duration::from_secs(25);
/// DHT items expire after about two hours.
const REPUBLISH_EVERY: Duration = Duration::from_secs(30 * 60);
/// Intents older than this are stale.
const INTENT_FRESH: Duration = Duration::from_secs(60);
const IDLE: Duration = Duration::from_secs(5);
/// Keeping a path warm towards a present client.
const WARM_EVERY: Duration = Duration::from_secs(20);
/// A presence record this old means the client has gone.
const PRESENCE_FRESH: Duration = Duration::from_secs(6 * 60);
/// What keeps a path warm: not WireGuard (the tunnel ignores it), not STUN.
const WARM_PACKET: &[u8] = b"\xffpingpong-warm";
/// The lease asked of the router for the tunnel's port, renewed at half.
const MAPPING_LEASE: Duration = Duration::from_secs(3600);
/// A router that did not map is asked again this much later.
const MAPPING_RETRY: Duration = Duration::from_secs(15 * 60);

struct Watched {
    name: String,
    public: PublicIdentity,
    rendezvous: [u8; 32],
}

pub fn run(host: Arc<Host>) {
    let Some(secret) = host.rendezvous.secret else {
        return;
    };
    host.stun.resolve_servers();
    let rv = match Rendezvous::join() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "cannot join the DHT; clients can only connect from \
                the local network");
            return;
        }
    };
    let port = host.endpoint.local_port();
    let mut published: Option<(Vec<SocketAddr>, Instant)> = None;
    let mut last_probe: Option<Instant> = None;
    let mut seen: HashMap<[u8; 32], u64> = HashMap::new();
    let mut present: HashMap<[u8; 32], (Vec<SocketAddr>, Instant, [u8; 32])> = HashMap::new();
    let mut last_warm = Instant::now();
    let salt = client_salt(&host.rendezvous.public());
    let mut turn = 0usize;
    let mut mapping = Mapping::default();

    while !host.stopping() {
        let (internet, want_mapping) = {
            let c = host.config.read();
            (c.internet_access, c.port_mapping)
        };
        if !internet {
            mapping.release(&host);
            std::thread::sleep(IDLE);
            continue;
        }
        if want_mapping {
            mapping.keep(&host, port);
        } else {
            mapping.release(&host);
        }
        if last_probe.is_none_or(|t| t.elapsed() >= STUN_EVERY) {
            last_probe = Some(Instant::now());
            host.stun.probe(|d, to| {
                let _ = host.endpoint.send_raw(d, to);
            });
            std::thread::sleep(Duration::from_millis(800));
        }

        let mut endpoints = host.stun.public();
        if let Some(m) = &mapping.current {
            if !endpoints.contains(&m.external) {
                endpoints.push(m.external);
            }
        }
        endpoints.extend(pingpong_nat::keys::global_ipv6(port));
        let changed = published
            .as_ref()
            .is_none_or(|(e, at)| *e != endpoints || at.elapsed() >= REPUBLISH_EVERY);
        if !endpoints.is_empty() && changed {
            match rv.publish(
                &host.rendezvous.seed,
                &secret,
                SALT,
                &Record::new(Kind::Host, endpoints.clone()),
            ) {
                Ok(()) => {
                    if published.as_ref().is_none_or(|(e, _)| *e != endpoints) {
                        tracing::info!(endpoints = ?endpoints, symmetric_nat = host.stun.looks_symmetric(), "reachable from the internet");
                    }
                    published = Some((endpoints, Instant::now()));
                }
                Err(e) => tracing::debug!(error = %e, "rendezvous publish failed"),
            }
        }

        // One paired client per round: each look-up is a few DHT round trips.
        let watched: Vec<Watched> = host
            .clients
            .lock()
            .list()
            .iter()
            .filter_map(|c| {
                Some(Watched {
                    name: c.name.clone(),
                    public: c.public()?,
                    rendezvous: c.rendezvous_key()?,
                })
            })
            .collect();
        if watched.is_empty() {
            std::thread::sleep(IDLE);
            continue;
        }
        let c = &watched[turn % watched.len()];
        turn = turn.wrapping_add(1);
        match rv.resolve(&c.rendezvous, &secret, &salt) {
            Some(r)
                if r.kind == Kind::Intent
                    && r.age() < INTENT_FRESH
                    && seen.get(&c.rendezvous) != Some(&r.nonce) =>
            {
                seen.insert(c.rendezvous, r.nonce);
                punch(&host, c, &r.endpoints);
            }
            Some(r) if r.kind == Kind::Presence && r.age() < PRESENCE_FRESH => {
                let fresh_until = Instant::now() + PRESENCE_FRESH.saturating_sub(r.age());
                if present
                    .get(&c.rendezvous)
                    .is_none_or(|(e, _, _)| *e != r.endpoints)
                {
                    tracing::info!(client = c.name, endpoints = ?r.endpoints, "keeping a path open to a client");
                }
                present.insert(c.rendezvous, (r.endpoints, fresh_until, c.public.x25519));
            }
            _ => {}
        }

        // Keep the host's NAT open towards every present client not
        // streaming right now.
        if last_warm.elapsed() >= WARM_EVERY {
            last_warm = Instant::now();
            present.retain(|_, (_, until, _)| *until > Instant::now());
            for (endpoints, _, x25519) in present.values() {
                let busy = host.endpoint.peer_by_key(x25519).is_some_and(|p| {
                    host.endpoint
                        .since_rx(&p)
                        .is_some_and(|d| d < Duration::from_secs(10))
                });
                if !busy {
                    for &ep in endpoints {
                        let _ = host.endpoint.send_raw(WARM_PACKET, ep);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    mapping.release(&host);
}

/// The router's forwarding of the tunnel's port, when it grants one.
#[derive(Default)]
struct Mapping {
    current: Option<pingpong_nat::portmap::Mapping>,
    due: Option<Instant>,
    /// Why the last try failed (said once, not every retry).
    failed: Option<String>,
}

impl Mapping {
    /// Map the port, or renew the mapping, when it is time.
    fn keep(&mut self, host: &Host, port: u16) {
        if self.due.is_some_and(|d| Instant::now() < d) {
            return;
        }
        match self.current.as_mut() {
            Some(m) => match m.renew() {
                Ok(()) => self.due = Some(Instant::now() + renew_after(m.lease)),
                Err(e) => {
                    tracing::warn!(error = e, "the router would not renew the port mapping");
                    self.current = None;
                    self.due = Some(Instant::now() + Duration::from_secs(60));
                }
            },
            None => match pingpong_nat::portmap::map(port, MAPPING_LEASE) {
                Ok(m) => {
                    tracing::info!(protocol = m.protocol(), external = %m.external, lease_secs = m.lease.as_secs(), "the router forwards the tunnel's port");
                    *host.port_mapping.lock() = format!(
                        "UDP {} forwarded from {} ({})",
                        port,
                        m.external,
                        m.protocol()
                    );
                    self.due = Some(Instant::now() + renew_after(m.lease));
                    self.failed = None;
                    self.current = Some(m);
                }
                Err(e) => {
                    if self.failed.as_ref() != Some(&e) {
                        tracing::info!(
                            reason = e,
                            "no port mapping on the router; hole punching still works"
                        );
                    }
                    *host.port_mapping.lock() =
                        format!("Not forwarded: {}", e.split("; ").next().unwrap_or(&e));
                    self.failed = Some(e);
                    self.due = Some(Instant::now() + MAPPING_RETRY);
                }
            },
        }
    }

    /// Give the port back (internet access or mapping turned off, or Pong
    /// stopping).
    fn release(&mut self, host: &Host) {
        if let Some(m) = self.current.take() {
            m.remove();
        }
        self.due = None;
        self.failed = None;
        host.port_mapping.lock().clear();
    }
}

fn renew_after(lease: Duration) -> Duration {
    // A permanent mapping is checked now and then (the router may restart).
    if lease.is_zero() {
        Duration::from_secs(30 * 60)
    } else {
        (lease / 2).max(Duration::from_secs(60))
    }
}

/// Send handshake initiations towards a client that wants to connect, a few
/// times over a second: each opens our NAT towards it.
fn punch(host: &Host, c: &Watched, endpoints: &[SocketAddr]) {
    let Some(peer) = host.endpoint.peer_by_key(&c.public.x25519) else {
        return;
    };
    if peer.is_established()
        && host
            .endpoint
            .since_rx(&peer)
            .is_some_and(|d| d < Duration::from_secs(5))
    {
        return;
    }
    tracing::info!(client = c.name, endpoints = ?endpoints, "client wants to connect from \
        the internet; punching");
    let Ok(packets) = host.endpoint.initiation(&peer) else {
        return;
    };
    for _ in 0..4 {
        for &ep in endpoints {
            let _ = host.endpoint.send_initiation(&peer, &packets, ep);
        }
        std::thread::sleep(Duration::from_millis(250));
        if host.stop_flag().load(Ordering::Relaxed) {
            return;
        }
    }
}
