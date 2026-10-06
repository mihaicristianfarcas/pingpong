//! Finding hosts on the network and pairing with one, the way Moonlight does:
//! the client shows a PIN, the user types it into the host's web UI, and each
//! side learns the other's keys over a PIN-authenticated exchange.
//!
//! Also whether each paired host is up ([`probe`]): asked on its pairing
//! port on the local network, else with a handshake over the tunnel, which
//! is the only part of a host reachable from the internet.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pingpong_nat::stun::Stun;
use pingpong_pairing::discovery;
use pingpong_pairing::pair::{self, Cancel, PairError};
use pingpong_transport::{Endpoint, Peer, Received};

use crate::store::{Hosts, KnownHost};

/// The port hosts listen for pairing on unless they say otherwise.
pub const DEFAULT_PAIRING_PORT: u16 = 47801;

#[derive(Debug, Clone)]
pub struct Discovered {
    pub name: String,
    pub id: String,
    pub address: IpAddr,
    /// The tunnel port.
    pub port: u16,
    /// "windows", "macos" or "linux".
    pub os: String,
    pub pairing_port: u16,
    pub web_port: u16,
    /// Its network adapters, to wake it by.
    pub wake: Vec<pingpong_pairing::wake::Mac>,
    /// Already paired (its key is in `hosts.toml`).
    pub paired: bool,
}

impl Discovered {
    pub fn pairing_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.pairing_port)
    }
}

/// Browse the local network for hosts for `timeout`.
/// Paired hosts found are remembered at their local address, with the
/// hardware addresses they announce to be woken by.
pub fn discover(dir: &Path, timeout: Duration) -> Result<Vec<Discovered>, String> {
    let mut hosts = Hosts::load(dir);
    let known: Vec<(String, String)> = hosts
        .list()
        .iter()
        .filter_map(|h| Some((h.public()?.short_id(), h.x25519.clone())))
        .collect();
    let mut out: Vec<Discovered> = discovery::browse(timeout)?
        .into_iter()
        .filter_map(|f| {
            let address = *f.addresses.first()?;
            Some(Discovered {
                paired: known.iter().any(|(id, _)| *id == f.id),
                name: f.name,
                id: f.id,
                address,
                port: f.port,
                os: f.os,
                pairing_port: f.pairing_port,
                web_port: f.web_port,
                wake: f.wake,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    for d in &out {
        if let Some((_, key)) = known.iter().find(|(id, _)| *id == d.id) {
            let _ = hosts.set_local_address(key, SocketAddr::new(d.address, d.port));
            let _ = hosts.set_wake(key, &d.wake);
        }
    }
    Ok(out)
}

/// Resolve `HOST[:PAIRING_PORT]` (name, IPv4 or IPv6) to a pairing address.
pub fn resolve(spec: &str) -> Result<SocketAddr, String> {
    let with_port = |s: &str| (s, DEFAULT_PAIRING_PORT).to_socket_addrs();
    let addrs: Vec<SocketAddr> = match spec.to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => with_port(spec.trim_start_matches('[').trim_end_matches(']'))
            .map_err(|e| format!("cannot resolve {spec}: {e}"))?
            .collect(),
    };
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| format!("{spec} has no address"))
}

/// The name this client pairs under, as the host's web UI will list it.
pub fn client_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "Ping".to_string())
}

/// Pair with the host at `addr` (its pairing port) using `pin`, which the user
/// is shown here and types on the host. Blocks until the host answers or five
/// minutes pass. `on_waiting` fires once the host is prompting for the PIN.
/// The host is remembered in `hosts.toml`.
pub fn pair(
    dir: &Path,
    addr: SocketAddr,
    name: &str,
    pin: &str,
    on_waiting: impl FnOnce(),
) -> Result<KnownHost, String> {
    pair_with(dir, addr, name, pin, &Cancel::default(), on_waiting)
}

/// [`pair`], stoppable from another thread with `cancel`.
pub fn pair_with(
    dir: &Path,
    addr: SocketAddr,
    name: &str,
    pin: &str,
    cancel: &Cancel,
    on_waiting: impl FnOnce(),
) -> Result<KnownHost, String> {
    pair_as(dir, addr, name, pin, false, cancel, on_waiting)
}

/// Pair this device's AI agent (its own identity, in `store::agent_dir`)
/// with the host at `addr`: the host shows the request as an agent's, and
/// holds its sessions to the agent rules.
pub fn pair_agent(
    dir: &Path,
    addr: SocketAddr,
    pin: &str,
    cancel: &Cancel,
    on_waiting: impl FnOnce(),
) -> Result<KnownHost, String> {
    let name = format!("{} (AI agent)", client_name());
    pair_as(
        &crate::store::agent_dir(dir),
        addr,
        &name,
        pin,
        true,
        cancel,
        on_waiting,
    )
}

fn pair_as(
    dir: &Path,
    addr: SocketAddr,
    name: &str,
    pin: &str,
    agent: bool,
    cancel: &Cancel,
    on_waiting: impl FnOnce(),
) -> Result<KnownHost, String> {
    let identity = crate::store::identity(dir)?;
    let (_, _, _, tunnel_port) = pair::host_info(addr).map_err(describe)?;
    let mine = pair::Extras {
        rendezvous: crate::wan::own_keys(dir).map(|k| k.public()),
        rendezvous_secret: None,
        agent,
    };
    let paired = pair::pair_client_with(
        addr,
        name,
        identity.public(),
        &mine,
        pin,
        cancel,
        on_waiting,
    )
    .map_err(describe)?;
    let (x25519, mlkem) = paired.public.to_b64();
    let hex = |k: [u8; 32]| k.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let host = KnownHost {
        name: paired.name,
        address: SocketAddr::new(addr.ip(), tunnel_port).to_string(),
        local_address: None,
        rendezvous: paired.extras.rendezvous.map(hex),
        rendezvous_secret: paired.extras.rendezvous_secret.map(hex),
        wan_addresses: Vec::new(),
        wake: Vec::new(),
        x25519,
        mlkem,
        paired_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    Hosts::load(dir)
        .upsert(host.clone())
        .map_err(|e| e.to_string())?;
    Ok(host)
}

/// How a paired host answered [`probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// It answered as this host on its pairing port, here: on its own
    /// network, or through a VPN such as Tailscale. Pairing works here.
    Pairing(SocketAddr),
    /// It answered as this host over its tunnel only, here (from anywhere).
    Tunnel(SocketAddr),
    /// It did not answer. `away`: this device is on another network than
    /// the host's. The host's NAT lets the probe in only once the host has
    /// seen Ping's presence and opened a path towards it (`wan`), so there
    /// silence does not yet prove that it sleeps.
    Silent { away: bool },
    /// Not asked over the tunnel: a stream as this identity holds it, or is
    /// starting (`store::TunnelLock`).
    Unknown,
}

/// Whether a paired host answers, and where.
#[derive(Debug, Clone)]
pub struct Reachability {
    pub name: String,
    pub x25519: String,
    pub reach: Reach,
}

/// How long hosts get to answer on their pairing port before the silent
/// ones are asked over the tunnel as well. On the local network a host
/// answers in milliseconds; anywhere else the connection only times out.
const LAN_FIRST: Duration = Duration::from_millis(300);
/// How long the tunnel probe waits for handshakes to come back. Along an
/// open path a whole connect across two NATs took 0.35 s
/// (docs/networking.md); a host whose NAT is closed to us never answers.
const TUNNEL_PROBE_FOR: Duration = Duration::from_millis(1500);
/// A fresh initiation this far into the probe, for one lost on the way.
const TUNNEL_RETRY_AFTER: Duration = Duration::from_millis(600);

/// Ask every paired host whether it is up, the way Moonlight polls its PC
/// list (`computermanager.cpp`: serverinfo at every address it knows).
/// First the pairing port, on the local network; then, for the hosts that
/// did not answer there, a handshake over the tunnel, where a stream
/// connects from anywhere. The pairing port is never reachable from the
/// internet (docs/networking.md, "Ports"): asking only there would call
/// every host asleep away from home. Blocking: up to a few seconds.
pub fn probe(dir: &Path) -> Vec<Reachability> {
    let hosts: Vec<KnownHost> = Hosts::load(dir).list().to_vec();
    let (tx, rx) = crossbeam_channel::unbounded();
    for (i, h) in hosts.iter().enumerate() {
        let (tx, h) = (tx.clone(), h.clone());
        std::thread::spawn(move || {
            let _ = tx.send((i, ask_pairing_port(&h)));
        });
    }
    drop(tx);
    // Per host: the pairing port's answer, once it has given one.
    let mut lan: Vec<Option<Option<SocketAddr>>> = vec![None; hosts.len()];
    let lan_by = Instant::now() + LAN_FIRST;
    while let Ok((i, answer)) = rx.recv_deadline(lan_by) {
        lan[i] = Some(answer);
    }

    let silent: Vec<usize> = (0..hosts.len())
        .filter(|&i| lan[i].flatten().is_none())
        .collect();
    let tunnel = if silent.is_empty() {
        None
    } else {
        let asked: Vec<&KnownHost> = silent.iter().map(|&i| &hosts[i]).collect();
        ask_tunnel(dir, &asked)
    };
    let tunnel_answer = |i: usize| -> Option<SocketAddr> {
        let (answers, _) = tunnel.as_ref()?;
        answers[silent.iter().position(|&s| s == i)?]
    };

    // The pairing ports still trying, for the hosts the tunnel did not
    // answer for either.
    while (0..hosts.len()).any(|i| lan[i].is_none() && tunnel_answer(i).is_none()) {
        let Ok((i, answer)) = rx.recv() else { break };
        lan[i] = Some(answer);
    }

    hosts
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let reach = match (lan[i].flatten(), tunnel_answer(i), &tunnel) {
                (Some(at), _, _) => Reach::Pairing(at),
                (None, Some(at), _) => Reach::Tunnel(at),
                (None, None, Some((_, ours))) => Reach::Silent {
                    away: is_away(ours, &h.wan_addresses),
                },
                (None, None, None) => Reach::Unknown,
            };
            Reachability {
                name: h.name.clone(),
                x25519: h.x25519.clone(),
                reach,
            }
        })
        .collect()
}

/// The host's pairing port, at each of its addresses but the public ones
/// its rendezvous record gave (the pairing port is local; only the tunnel
/// is reachable there, and a connection would only time out). Asked all at
/// once, the first to answer as this host wins: away from home, through a
/// VPN, the saved home address only times out (3 s) while the VPN's answers
/// in milliseconds.
fn ask_pairing_port(h: &KnownHost) -> Option<SocketAddr> {
    let id = h.public().map(|p| p.short_id()).unwrap_or_default();
    let (local, remote) = h.candidates();
    let (tx, rx) = crossbeam_channel::unbounded();
    for tunnel in local
        .into_iter()
        .chain(remote)
        .filter(|a| !h.wan_addresses.contains(&a.to_string()))
    {
        let (tx, id) = (tx.clone(), id.clone());
        std::thread::spawn(move || {
            let addr = SocketAddr::new(tunnel.ip(), tunnel.port().wrapping_add(1));
            let answered = matches!(pair::host_info(addr), Ok((_, host_id, _, _)) if host_id == id);
            let _ = tx.send(answered.then_some(addr));
        });
    }
    drop(tx);
    rx.iter().flatten().next()
}

/// A handshake with each of `hosts` along every path a stream would race,
/// from this identity's own tunnel port: a host's warm path is open towards
/// that port only (`wan`). Returns where each answered, and this device's
/// public IPv4 addresses (STUN, on the same socket). None when it could not
/// ask: a stream holds the tunnel, or one is starting.
fn ask_tunnel(dir: &Path, hosts: &[&KnownHost]) -> Option<(Vec<Option<SocketAddr>>, Vec<IpAddr>)> {
    // DNS first: a stream starting meanwhile waits for the lock, not for this.
    let identity = Arc::new(crate::store::identity(dir).ok()?);
    let paths: Vec<Vec<SocketAddr>> = hosts
        .iter()
        .map(|h| {
            let (local, remote) = h.candidates();
            local.into_iter().chain(remote).collect()
        })
        .collect();
    let stun = Stun::new();
    let want_stun = hosts.iter().any(|h| !h.wan_addresses.is_empty());
    if want_stun {
        stun.resolve_servers();
    }

    let _lock = crate::store::TunnelLock::for_borrowing(dir)?;
    let endpoint = Endpoint::bind(identity, crate::store::tunnel_port(dir)).ok()?;
    endpoint.set_recv_timeout(Duration::from_millis(20)).ok()?;
    if want_stun {
        stun.probe(|d, to| {
            let _ = endpoint.send_raw(d, to);
        });
    }
    let peers: Vec<Option<(Arc<Peer>, Vec<SocketAddr>)>> = hosts
        .iter()
        .zip(paths)
        .map(|(h, paths)| {
            let peer = endpoint
                .add_peer(h.public()?, paths.first().copied())
                .ok()?;
            Some((peer, paths))
        })
        .collect();
    let initiate = || {
        for (peer, paths) in peers.iter().flatten() {
            if peer.is_established() {
                continue;
            }
            let Ok(packets) = endpoint.fresh_initiation(peer) else {
                continue;
            };
            for &to in paths {
                let _ = endpoint.send_initiation(peer, &packets, to);
            }
        }
    };
    initiate();
    let started = Instant::now();
    let mut retried = false;
    let mut buf = vec![0u8; 65_536];
    while started.elapsed() < TUNNEL_PROBE_FOR
        && !peers.iter().flatten().all(|(p, _)| p.is_established())
    {
        if crate::store::stream_waiting() {
            return None;
        }
        if !retried && started.elapsed() >= TUNNEL_RETRY_AFTER {
            retried = true;
            initiate();
        }
        // Handshake answers are taken in by the endpoint itself.
        if let Ok(Received::Foreign(from, n)) = endpoint.recv(&mut buf) {
            stun.on_datagram(from, &buf[..n]);
        }
    }
    let answered = peers
        .iter()
        .map(|p| {
            let (peer, _) = p.as_ref()?;
            peer.is_established().then(|| peer.addr()).flatten()
        })
        .collect();
    let ours = stun.public().iter().map(|a| a.ip()).collect();
    Some((answered, ours))
}

/// This device is on another network than the host: none of the public
/// addresses STUN saw for us is the one the host publishes. Not knowing
/// (no STUN answer, or no IPv4 address published) counts as the same
/// network, where a silent host is asleep.
fn is_away(ours: &[IpAddr], published: &[String]) -> bool {
    let theirs: Vec<IpAddr> = published
        .iter()
        .filter_map(|a| a.parse::<SocketAddr>().ok())
        .map(|a| a.ip())
        .filter(IpAddr::is_ipv4)
        .collect();
    let ours: Vec<&IpAddr> = ours.iter().filter(|a| a.is_ipv4()).collect();
    !ours.is_empty() && !theirs.is_empty() && !theirs.iter().any(|t| ours.contains(&t))
}

fn describe(e: PairError) -> String {
    match e {
        PairError::WrongPin => "The PIN entered on the host did not match.".to_string(),
        PairError::Refused(reason) => reason,
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_transport::{Identity, PublicIdentity};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_device_behind_another_public_address_is_away() {
        let home: IpAddr = "203.0.113.7".parse().unwrap();
        let hotspot: IpAddr = "198.51.100.20".parse().unwrap();
        let published = vec!["203.0.113.7:47800".to_string()];
        assert!(is_away(&[hotspot], &published));
        assert!(!is_away(&[home], &published));
        // Not knowing is the same network.
        assert!(!is_away(&[], &published));
        assert!(!is_away(&[hotspot], &[]));
        assert!(!is_away(&[hotspot], &["[2001:db8::1]:47800".to_string()]));
    }

    /// A host on this machine: a tunnel endpoint that answers handshakes
    /// from `client`, as Pong's receive loop does, until dropped.
    struct LoopbackHost {
        public: PublicIdentity,
        port: u16,
        stop: Arc<AtomicBool>,
    }

    impl LoopbackHost {
        fn start(client: &PublicIdentity) -> LoopbackHost {
            let identity = Identity::generate();
            let public = identity.public().clone();
            let endpoint = Endpoint::bind(Arc::new(identity), 0).unwrap();
            endpoint.add_peer(client.clone(), None).unwrap();
            let port = endpoint.local_port();
            let stop = Arc::new(AtomicBool::new(false));
            {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 65_536];
                    while !stop.load(Ordering::Relaxed) {
                        let _ = endpoint.recv(&mut buf);
                    }
                });
            }
            LoopbackHost { public, port, stop }
        }

        fn known(&self) -> KnownHost {
            let (x, m) = self.public.to_b64();
            KnownHost::by_hand("gaming-pc", &format!("127.0.0.1:{}", self.port), &x, &m)
        }
    }

    impl Drop for LoopbackHost {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// A data folder with this client's identity, a free tunnel port and
    /// `host` paired.
    fn client_dir(name: &str) -> (std::path::PathBuf, PublicIdentity) {
        let dir = std::env::temp_dir().join(format!("ping-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let free = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        std::fs::write(
            dir.join("port"),
            free.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
        let public = crate::store::identity(&dir).unwrap().public().clone();
        (dir, public)
    }

    #[test]
    fn a_host_reachable_only_over_its_tunnel_is_up() {
        let _serial = crate::store::tests::serial();
        let (dir, client) = client_dir("probe-tunnel");
        let host = LoopbackHost::start(&client);
        Hosts::load(&dir).upsert(host.known()).unwrap();
        // Nothing listens on its pairing port: as from the internet.
        let reach = probe(&dir);
        assert_eq!(reach.len(), 1);
        assert_eq!(
            reach[0].reach,
            Reach::Tunnel(format!("127.0.0.1:{}", host.port).parse().unwrap())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_that_does_not_know_this_client_is_silent() {
        let _serial = crate::store::tests::serial();
        let (dir, _) = client_dir("probe-stranger");
        let stranger = Identity::generate().public().clone();
        let host = LoopbackHost::start(&stranger);
        Hosts::load(&dir).upsert(host.known()).unwrap();
        let reach = probe(&dir);
        assert_eq!(reach[0].reach, Reach::Silent { away: false });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tunnel_is_not_asked_while_a_stream_holds_it() {
        let _serial = crate::store::tests::serial();
        let (dir, client) = client_dir("probe-streaming");
        let host = LoopbackHost::start(&client);
        Hosts::load(&dir).upsert(host.known()).unwrap();
        let stream = crate::store::TunnelLock::for_stream(&dir).unwrap();
        assert_eq!(probe(&dir)[0].reach, Reach::Unknown);
        drop(stream);
        assert!(matches!(probe(&dir)[0].reach, Reach::Tunnel(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_answering_on_its_pairing_port_is_found_there() {
        let _serial = crate::store::tests::serial();
        let (dir, _) = client_dir("probe-pairing");
        let host = Identity::generate();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pairing = listener.local_addr().unwrap();
        let me = pingpong_pairing::pair::HostDescription {
            name: "gaming-pc".into(),
            id: host.public().short_id(),
            version: "test".into(),
            port: pairing.port() - 1,
        };
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = pingpong_pairing::pair::accept(stream, &me);
            }
        });
        let (x, m) = host.public().to_b64();
        // Its saved address answers on the pairing port; another one (as a
        // home address seen from away) is refused.
        let mut known = KnownHost::by_hand(
            "gaming-pc",
            &format!("127.0.0.1:{}", pairing.port() - 1),
            &x,
            &m,
        );
        known.local_address = Some("127.0.0.1:9".into());
        Hosts::load(&dir).upsert(known).unwrap();
        assert_eq!(probe(&dir)[0].reach, Reach::Pairing(pairing));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
