//! LAN discovery over mDNS/DNS-SD, the way Moonlight finds Sunshine hosts.
//!
//! Pong advertises `_pingpong._udp.local.` with its tunnel port, and TXT
//! records for its id, its pairing and web ports, its system, and the
//! hardware addresses to wake it by. Ping browses for it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

pub const SERVICE_TYPE: &str = "_pingpong._udp.local.";

/// Keeps the advertisement alive; dropping it withdraws the service.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// `wake`: the hardware addresses of the host's network adapters, for
/// Wake-on-LAN (see [`crate::wake`]).
pub fn advertise(
    name: &str,
    id: &str,
    port: u16,
    pairing_port: u16,
    web_port: u16,
    wake: &[crate::wake::Mac],
) -> Result<Advertisement, String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let host = format!("{}.local.", name.to_lowercase().replace(' ', "-"));
    let mut props = vec![
        ("id", id.to_string()),
        ("pair", pairing_port.to_string()),
        ("web", web_port.to_string()),
        ("v", "3".to_string()),
        // "windows" or "macos": the client shows the right kind of machine.
        ("os", std::env::consts::OS.to_string()),
    ];
    if !wake.is_empty() {
        props.push((
            "mac",
            wake.iter()
                .map(crate::wake::format)
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    let info = ServiceInfo::new(SERVICE_TYPE, name, &host, "", port, &props[..])
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).map_err(|e| e.to_string())?;
    Ok(Advertisement { daemon, fullname })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub id: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    /// The host's system ("windows", "macos"); hosts from before it was
    /// announced are Windows.
    pub os: String,
    pub pairing_port: u16,
    pub web_port: u16,
    /// Its network adapters, to wake it by (none from hosts before it was
    /// announced).
    pub wake: Vec<crate::wake::Mac>,
}

/// How good an address is for reaching a host found on the local network:
/// its private IPv4 first, then other IPv4, IPv6, and last the shared
/// 100.64/10 range (Tailscale and other overlays: a detour through a second
/// tunnel). `None`: unusable. A link-local IPv6 address comes without the
/// interface it belongs to, so nothing can send to it.
fn rank(a: &IpAddr) -> Option<u8> {
    match a {
        IpAddr::V4(v4) if v4.is_loopback() || v4.is_unspecified() => None,
        IpAddr::V4(v4) if v4.is_private() => Some(0),
        IpAddr::V4(v4) if v4.octets()[0] == 100 && v4.octets()[1] & 0xC0 == 64 => Some(3),
        IpAddr::V4(_) => Some(1),
        IpAddr::V6(v6) if v6.is_loopback() || v6.segments()[0] & 0xFFC0 == 0xFE80 => None,
        IpAddr::V6(_) => Some(2),
    }
}

/// Browse for `timeout`, returning every host that resolved, its best
/// address first (see [`rank`]).
pub fn browse(timeout: Duration) -> Result<Vec<Found>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let rx = daemon.browse(SERVICE_TYPE).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut found: HashMap<String, Found> = HashMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let prop = |k: &str| info.get_property_val_str(k).map(str::to_string);
                // A host resolves more than once (per interface, per record
                // type), each time with the addresses known so far: keep them all.
                let mut addresses: Vec<IpAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|a| a.to_ip_addr())
                    .collect();
                if let Some(before) = found.get(info.get_fullname()) {
                    addresses.extend(
                        before
                            .addresses
                            .iter()
                            .filter(|a| !addresses.contains(a))
                            .copied()
                            .collect::<Vec<_>>(),
                    );
                }
                addresses.retain(|a| rank(a).is_some());
                addresses.sort_by_key(rank);
                let name = info
                    .get_fullname()
                    .trim_end_matches(SERVICE_TYPE)
                    .trim_end_matches('.')
                    .to_string();
                found.insert(
                    info.get_fullname().to_string(),
                    Found {
                        name,
                        id: prop("id").unwrap_or_default(),
                        addresses,
                        port: info.get_port(),
                        os: prop("os").unwrap_or_else(|| "windows".into()),
                        pairing_port: prop("pair").and_then(|p| p.parse().ok()).unwrap_or(47801),
                        web_port: prop("web").and_then(|p| p.parse().ok()).unwrap_or(47802),
                        wake: prop("mac")
                            .map(|m| crate::wake::parse_list(&m))
                            .unwrap_or_default(),
                    },
                );
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::rank;
    use std::net::IpAddr;

    #[test]
    fn the_lan_address_beats_tailscale_and_link_local_is_dropped() {
        let mut v: Vec<IpAddr> = [
            "100.101.102.103",
            "fe80::ebc4:df97:2d24:4cd6",
            "192.168.1.20",
            "2001:db8::1",
            "127.0.0.1",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        v.retain(|a| rank(a).is_some());
        v.sort_by_key(rank);
        let v: Vec<String> = v.iter().map(|a| a.to_string()).collect();
        assert_eq!(v, ["192.168.1.20", "2001:db8::1", "100.101.102.103"]);
    }
}
