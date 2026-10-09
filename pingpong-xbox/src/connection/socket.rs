//! The connection's UDP socket, its ICE candidates, and the doorbell that
//! wakes the connection's thread when input is waiting.
//!
//! One IPv4 socket bound to every interface, offered as one host candidate
//! (the interface that routes towards the other end) and, when a NAT is in
//! the way, a server-reflexive one learnt by STUN: what a console outside
//! the home network needs to reach this computer, which Greenlight leaves
//! to the browser and does not have ("@TODO: Implement Teredo port
//! selection for remote streaming", `ice.ts`). A wildcard socket does not
//! say which address a datagram came in on, so every datagram is handed to
//! str0m as arriving at the host candidate, the only one it has.
//!
//! The doorbell: input arrives from other threads while this one waits in
//! `recv_from`. Rather than wake every millisecond to look (the cost of a
//! 1 ms timeout, and up to a millisecond of input latency), whoever queues
//! input sends a byte to the socket from a loopback socket of its own, and
//! the loop knows that sender. The byte only wakes the loop: what says
//! input is waiting is the bell's flag, which the loop reads at every wake.
//! Looks removable, is not: a bell whose byte was lost -- read by STUN
//! ([`Socket::discover_public`] reads this socket while the window already
//! takes input), or dropped from a full buffer -- would otherwise stay rung
//! and never ring again, and no input would reach the console for the rest
//! of the stream.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket as RawSocket, Type};
use str0m::Candidate;

/// Receive buffer: a 1080p keyframe arrives as a burst of hundreds of
/// datagrams, faster than the loop drains them at times.
const RECV_BUFFER: usize = 4 << 20;
const SEND_BUFFER: usize = 1 << 20;

/// How long STUN may take to say where a NAT puts this socket: it runs
/// while the session is set up (seconds), so it costs nothing visible.
const STUN_WAIT: Duration = Duration::from_millis(800);

/// Wakes the connection's thread. Cheap to clone; sends at most one byte
/// until the thread has taken what was queued.
#[derive(Clone)]
pub struct Doorbell(Arc<Bell>);

struct Bell {
    udp: UdpSocket,
    to: SocketAddr,
    /// Input is waiting.
    rung: AtomicBool,
}

impl Doorbell {
    pub fn ring(&self) {
        if !self.0.rung.swap(true, Ordering::AcqRel) {
            let _ = self.0.udp.send_to(&[0], self.0.to);
        }
    }

    /// Whether input was queued since the last call; the next input rings
    /// again. Called at every wake, whatever woke the thread.
    pub(crate) fn take(&self) -> bool {
        self.0.rung.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn addr(&self) -> Option<SocketAddr> {
        self.0.udp.local_addr().ok()
    }
}

pub struct Socket {
    pub(crate) udp: UdpSocket,
    /// The host candidate: this computer's address towards the other end,
    /// at the socket's port.
    pub(crate) local: SocketAddr,
    /// Where a NAT shows the socket to the internet, when STUN said.
    public: Option<SocketAddr>,
    pub(crate) bell: Doorbell,
    bell_from: Option<SocketAddr>,
}

impl Socket {
    /// A socket for a connection whose other end is at `towards` (an
    /// address, or a public one for "the internet").
    pub fn bind(towards: IpAddr) -> io::Result<Socket> {
        let raw = RawSocket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Best effort: the system may cap them.
        let _ = raw.set_recv_buffer_size(RECV_BUFFER);
        let _ = raw.set_send_buffer_size(SEND_BUFFER);
        raw.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
        let udp: UdpSocket = raw.into();
        let port = udp.local_addr()?.port();
        let ip = local_ip_towards(towards).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let bell_udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let bell = Doorbell(Arc::new(Bell {
            udp: bell_udp,
            to: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            rung: AtomicBool::new(false),
        }));
        let bell_from = bell.addr();
        Ok(Socket {
            udp,
            local: SocketAddr::new(ip, port),
            public: None,
            bell,
            bell_from,
        })
    }

    pub fn doorbell(&self) -> Doorbell {
        self.bell.clone()
    }

    pub(crate) fn is_bell(&self, from: SocketAddr) -> bool {
        Some(from) == self.bell_from
    }

    /// Ask STUN servers where a NAT puts this socket, unless it is on a
    /// loopback or public address already.
    pub fn discover_public(&mut self) {
        if self.local.ip().is_loopback() || !is_private(self.local.ip()) {
            return;
        }
        let stun = pingpong_nat::stun::Stun::new();
        stun.resolve_servers();
        stun.probe(|d, to| {
            let _ = self.udp.send_to(d, to);
        });
        let deadline = Instant::now() + STUN_WAIT;
        let mut buf = [0u8; 576];
        while stun.answers() < 2 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let _ = self
                .udp
                .set_read_timeout(Some(left.max(Duration::from_millis(1))));
            match self.udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    stun.on_datagram(from, &buf[..n]);
                }
                Err(_) => break,
            }
        }
        self.public = stun
            .public()
            .into_iter()
            .find(|a| a.is_ipv4() && a.ip() != self.local.ip());
        if let Some(public) = self.public {
            tracing::debug!(%public, symmetric = stun.looks_symmetric(), "public address");
        }
    }

    /// The candidates to offer.
    pub fn candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        match Candidate::host(self.local, "udp") {
            Ok(c) => out.push(c),
            Err(e) => tracing::warn!(error = %e, "no host candidate"),
        }
        if let Some(public) = self.public {
            if let Ok(c) = Candidate::server_reflexive(public, self.local, "udp") {
                out.push(c);
            }
        }
        out
    }
}

/// The address this computer sends from towards `ip`: a connected UDP
/// socket asks the routing table without sending anything.
fn local_ip_towards(ip: IpAddr) -> Option<IpAddr> {
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((ip, 9)).ok()?;
    let local = probe.local_addr().ok()?.ip();
    (!local.is_unspecified()).then_some(local)
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_link_local()
                || v4.octets()[0] == 100 && v4.octets()[1] & 0xC0 == 64
        }
        IpAddr::V6(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_towards_loopback_offers_loopback() {
        let s = Socket::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert!(s.local.ip().is_loopback());
        let c = s.candidates();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].addr(), s.local);
    }

    #[test]
    fn the_doorbell_reaches_the_socket_once_until_taken() {
        let s = Socket::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let bell = s.doorbell();
        bell.ring();
        bell.ring();
        s.udp
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut buf = [0u8; 16];
        let (_, from) = s.udp.recv_from(&mut buf).unwrap();
        assert!(s.is_bell(from));
        s.udp
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(s.udp.recv_from(&mut buf).is_err(), "rang once");
        assert!(bell.take());
        assert!(!bell.take(), "taken once");
        bell.ring();
        assert!(s.udp.recv_from(&mut buf).is_ok());
    }

    #[test]
    fn a_ring_whose_byte_someone_else_read_is_still_taken() {
        // STUN reads the socket while the stream starts; the window takes
        // input meanwhile.
        let s = Socket::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let bell = s.doorbell();
        bell.ring();
        s.udp
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut buf = [0u8; 16];
        s.udp.recv_from(&mut buf).unwrap();
        bell.ring();
        assert!(bell.take(), "the input is still owed");
        bell.ring();
        assert!(
            s.udp.recv_from(&mut buf).is_ok(),
            "and the bell rings again"
        );
    }

    #[test]
    fn private_addresses_are_told_from_public_ones() {
        assert!(is_private("192.168.1.20".parse().unwrap()));
        assert!(is_private("10.0.0.1".parse().unwrap()));
        // Carrier-grade NAT.
        assert!(is_private("100.64.1.1".parse().unwrap()));
        assert!(!is_private("203.0.113.7".parse().unwrap()));
    }
}
