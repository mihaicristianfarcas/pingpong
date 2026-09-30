//! Waking a sleeping host, as Moonlight's "Wake PC" does: a magic packet for
//! each hardware address the host announced, broadcast on every local
//! network and sent to wherever the host was last seen.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

use pingpong_pairing::wake;

use crate::store::KnownHost;

/// The ports magic packets conventionally go to ("discard" and "echo").
const PORTS: [u16; 2] = [9, 7];

/// Send the host its magic packets. Returns how many went out; an error
/// when it has none to be woken by (paired before it announced them, and
/// not seen on the local network since).
pub fn wake(host: &KnownHost) -> Result<usize, String> {
    let macs: Vec<wake::Mac> = host.wake.iter().filter_map(|m| wake::parse(m)).collect();
    if macs.is_empty() {
        return Err(format!(
            "{} has not said how to wake it; it must be seen on the local network once \
                (awake) first",
            host.name
        ));
    }
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_broadcast(true).map_err(|e| e.to_string())?;

    let mut targets: Vec<Ipv4Addr> = vec![Ipv4Addr::BROADCAST];
    // Each local network's own broadcast address: 255.255.255.255 only
    // leaves by the default route's interface.
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if let Some(b) = v4.broadcast.filter(|_| !v4.ip.is_loopback()) {
                targets.push(b);
            }
        }
    }
    // Where it was: a switch that still knows its port delivers it there,
    // and a router forwarding the port could pass it on from the internet.
    for a in host.local_address.iter().chain(host.wan_addresses.iter()) {
        if let Ok(SocketAddr::V4(v4)) = a.parse::<SocketAddr>() {
            targets.push(*v4.ip());
        }
    }
    targets.sort();
    targets.dedup();

    let mut sent = 0;
    for mac in &macs {
        let packet = wake::magic_packet(mac);
        for ip in &targets {
            for port in PORTS {
                if socket.send_to(&packet, (*ip, port)).is_ok() {
                    sent += 1;
                }
            }
        }
    }
    tracing::info!(host = %host.name, adapters = macs.len(), targets = ?targets, sent, "wake-on-LAN sent");
    Ok(sent)
}
