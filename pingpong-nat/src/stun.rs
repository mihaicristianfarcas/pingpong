//! Just enough STUN (RFC 8489) to learn the public address a NAT gives our
//! UDP socket: Binding requests out, XOR-MAPPED-ADDRESS back. It runs on the
//! tunnel's own socket, so the address learned is the tunnel's.
//!
//! STUN and WireGuard share the socket without confusion: a STUN message
//! starts with two zero bits and carries the magic cookie at bytes 4..8,
//! while every WireGuard message starts with a type byte 1..=9 followed by
//! three zeros.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const MAPPED_ADDRESS: u16 = 0x0001;

/// Public STUN servers tried, in order. Two different ones tell us whether
/// the NAT maps the socket to the same public port for every destination.
pub const SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun1.l.google.com:19302",
];

pub type TransactionId = [u8; 12];

pub fn transaction_id() -> TransactionId {
    let mut id = [0u8; 12];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut id);
    id
}

/// A Binding request with no attributes (20 bytes).
pub fn binding_request(id: &TransactionId) -> [u8; 20] {
    let mut m = [0u8; 20];
    m[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    m[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    m[8..20].copy_from_slice(id);
    m
}

/// Whether a datagram is a STUN message at all.
pub fn is_stun(d: &[u8]) -> bool {
    d.len() >= 20 && d[0] & 0xC0 == 0 && d[4..8] == MAGIC.to_be_bytes()
}

/// The transaction and mapped address of a Binding success response.
pub fn parse_response(d: &[u8]) -> Option<(TransactionId, SocketAddr)> {
    if !is_stun(d) || u16::from_be_bytes([d[0], d[1]]) != BINDING_SUCCESS {
        return None;
    }
    let len = u16::from_be_bytes([d[2], d[3]]) as usize;
    let body = d.get(20..20 + len)?;
    let id: TransactionId = d[8..20].try_into().ok()?;
    let mut plain = None;
    let mut off = 0;
    while off + 4 <= body.len() {
        let ty = u16::from_be_bytes([body[off], body[off + 1]]);
        let alen = u16::from_be_bytes([body[off + 2], body[off + 3]]) as usize;
        let value = body.get(off + 4..off + 4 + alen)?;
        match ty {
            XOR_MAPPED_ADDRESS => return decode_address(value, Some(&id)).map(|a| (id, a)),
            MAPPED_ADDRESS => plain = decode_address(value, None),
            _ => {}
        }
        off += 4 + alen.div_ceil(4) * 4;
    }
    plain.map(|a| (id, a))
}

fn decode_address(v: &[u8], xor_id: Option<&TransactionId>) -> Option<SocketAddr> {
    let family = *v.get(1)?;
    let mut port = u16::from_be_bytes([*v.get(2)?, *v.get(3)?]);
    let cookie = MAGIC.to_be_bytes();
    if xor_id.is_some() {
        port ^= (MAGIC >> 16) as u16;
    }
    let ip = match family {
        1 => {
            let mut b: [u8; 4] = v.get(4..8)?.try_into().ok()?;
            if xor_id.is_some() {
                for (x, c) in b.iter_mut().zip(cookie) {
                    *x ^= c;
                }
            }
            IpAddr::V4(Ipv4Addr::from(b))
        }
        2 => {
            let mut b: [u8; 16] = v.get(4..20)?.try_into().ok()?;
            if let Some(id) = xor_id {
                let key: Vec<u8> = cookie.iter().chain(id.iter()).copied().collect();
                for (x, k) in b.iter_mut().zip(key) {
                    *x ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// STUN over a socket someone else reads (the tunnel's): requests go out
/// through `send`, and the reader hands datagrams to [`Stun::on_datagram`].
#[derive(Default)]
pub struct Stun {
    servers: Mutex<Vec<SocketAddr>>,
    pending: Mutex<HashMap<TransactionId, (SocketAddr, Instant)>>,
    /// server -> (what it saw, when)
    seen: Mutex<HashMap<SocketAddr, (SocketAddr, Instant)>>,
}

/// Answers older than this are forgotten (NAT mappings change).
const FRESH: Duration = Duration::from_secs(120);

impl Stun {
    pub fn new() -> Stun {
        Stun::default()
    }

    /// Resolve the servers (DNS; blocking). IPv4 only: the tunnel's public
    /// IPv6 address, if any, is its own and needs no discovery.
    pub fn resolve_servers(&self) {
        let found: Vec<SocketAddr> = SERVERS
            .iter()
            .filter_map(|s| s.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4))
            .collect();
        if !found.is_empty() {
            *self.servers.lock().unwrap() = found;
        }
    }

    /// Ask the first two servers where they see us.
    pub fn probe(&self, send: impl Fn(&[u8], SocketAddr)) {
        let servers: Vec<SocketAddr> = self
            .servers
            .lock()
            .unwrap()
            .iter()
            .take(2)
            .copied()
            .collect();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(10));
        for server in servers {
            let id = transaction_id();
            pending.insert(id, (server, Instant::now()));
            send(&binding_request(&id), server);
        }
    }

    /// Offer a datagram from the socket; true if it was a STUN answer.
    pub fn on_datagram(&self, from: SocketAddr, d: &[u8]) -> bool {
        let Some((id, mapped)) = parse_response(d) else {
            return false;
        };
        let Some((server, _)) = self.pending.lock().unwrap().remove(&id) else {
            return true;
        };
        if server.ip() == from.ip() || from.ip().to_canonical() == server.ip() {
            self.seen
                .lock()
                .unwrap()
                .insert(server, (mapped, Instant::now()));
        }
        true
    }

    /// Our public addresses as recently seen, most common first.
    pub fn public(&self) -> Vec<SocketAddr> {
        let seen = self.seen.lock().unwrap();
        let mut counts: Vec<(SocketAddr, usize)> = Vec::new();
        for (addr, at) in seen.values() {
            if at.elapsed() > FRESH {
                continue;
            }
            match counts.iter_mut().find(|(a, _)| a == addr) {
                Some((_, n)) => *n += 1,
                None => counts.push((*addr, 1)),
            }
        }
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        counts.into_iter().map(|(a, _)| a).collect()
    }

    /// How many servers have answered recently.
    pub fn answers(&self) -> usize {
        self.seen
            .lock()
            .unwrap()
            .values()
            .filter(|(_, at)| at.elapsed() < FRESH)
            .count()
    }

    /// Two servers saw different public ports: a new mapping per destination
    /// (symmetric NAT), which punching cannot get through.
    pub fn looks_symmetric(&self) -> bool {
        let seen = self.seen.lock().unwrap();
        let fresh: Vec<SocketAddr> = seen
            .values()
            .filter(|(_, at)| at.elapsed() < FRESH)
            .map(|(a, _)| *a)
            .collect();
        fresh.windows(2).any(|w| w[0] != w[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5769 §2.2: a sample IPv4 Binding response.
    #[test]
    fn decodes_the_rfc_5769_ipv4_response() {
        let msg: [u8; 80] = [
            0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34,
            0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74,
            0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01,
            0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91, 0xf5, 0x99,
            0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b,
            0xe7, 0xd7, 0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
        ];
        let (id, addr) = parse_response(&msg).unwrap();
        assert_eq!(
            id,
            [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae]
        );
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn answers_are_matched_to_requests() {
        let stun = Stun::new();
        *stun.servers.lock().unwrap() = vec!["198.51.100.1:3478".parse().unwrap()];
        let sent = std::cell::RefCell::new(Vec::new());
        stun.probe(|d, to| sent.borrow_mut().push((d.to_vec(), to)));
        let (req, server) = sent.borrow()[0].clone();
        let id: TransactionId = req[8..20].try_into().unwrap();
        // The server's answer: 203.0.113.9:47800, XOR-encoded.
        let mut resp = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        resp.extend_from_slice(&id);
        resp.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        resp.extend_from_slice(&(47800u16 ^ 0x2112).to_be_bytes());
        for (b, c) in [203u8, 0, 113, 9].iter().zip(MAGIC.to_be_bytes()) {
            resp.push(b ^ c);
        }
        assert!(stun.on_datagram(server, &resp));
        assert_eq!(
            stun.public(),
            vec!["203.0.113.9:47800".parse::<SocketAddr>().unwrap()]
        );
        assert!(!stun.looks_symmetric());
    }

    #[test]
    fn stun_and_wireguard_never_look_alike() {
        let req = binding_request(&transaction_id());
        assert!(is_stun(&req));
        for ty in 1u8..=9 {
            let mut wg = [0u8; 32];
            wg[0] = ty;
            assert!(!is_stun(&wg));
        }
    }
}
