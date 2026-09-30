//! Finding hosts on the network and pairing with one, the way Moonlight does:
//! the client shows a PIN, the user types it into the host's web UI, and each
//! side learns the other's keys over a PIN-authenticated exchange.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pingpong_pairing::discovery;
use pingpong_pairing::pair::{self, Cancel, PairError};

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

/// Whether a paired host answers, and where.
#[derive(Debug, Clone)]
pub struct Reachability {
    pub name: String,
    pub x25519: String,
    /// The pairing-port address that answered as this host.
    pub via: Option<SocketAddr>,
}

/// Ask every paired host (local address first) whether it is up, the way
/// Moonlight polls its PC list. Blocking; hosts are probed in parallel.
pub fn probe(dir: &Path) -> Vec<Reachability> {
    let hosts = Hosts::load(dir);
    let probes: Vec<_> = hosts
        .list()
        .iter()
        .cloned()
        .map(|h| {
            std::thread::spawn(move || {
                let id = h.public().map(|p| p.short_id()).unwrap_or_default();
                let (local, remote) = h.candidates();
                let via = local.into_iter().chain(remote).find_map(|tunnel| {
                    let addr = SocketAddr::new(tunnel.ip(), tunnel.port().wrapping_add(1));
                    match pair::host_info(addr) {
                        Ok((_, host_id, _, _)) if host_id == id => Some(addr),
                        _ => None,
                    }
                });
                Reachability {
                    name: h.name,
                    x25519: h.x25519,
                    via,
                }
            })
        })
        .collect();
    probes.into_iter().filter_map(|t| t.join().ok()).collect()
}

fn describe(e: PairError) -> String {
    match e {
        PairError::WrongPin => "The PIN entered on the host did not match.".to_string(),
        PairError::Refused(reason) => reason,
        other => other.to_string(),
    }
}
