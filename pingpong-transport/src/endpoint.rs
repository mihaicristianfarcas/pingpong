//! One UDP socket, one pq-boringtun `Tunn` per peer.
//!
//! The host serves every paired client from the same port, as a WireGuard
//! interface does; the client runs the same code with a single peer. Incoming
//! datagrams are routed to a peer the way pq-boringtun's own device layer does
//! it: data and responses by receiver index, a static-KEM initiation by
//! decrypting the initiator's identity with our ML-KEM key, and segmented
//! handshake pieces by the route their first segment established.
//!
//! Three things must not be broken here (each cost a debugging session once):
//! 1. `WriteManyToNetwork` is a SEGMENTED PQ handshake: every segment is its own
//!    datagram and segment 0 goes first.
//! 2. After `decapsulate` hands back something to send, it must be called again
//!    with an empty datagram until `Done`, or queued packets stall.
//! 3. Every datagram that authenticates updates the peer's address: that is
//!    roaming, and it is how a client that changes networks keeps its stream.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use boringtun::noise::handshake::{parse_pqs_handshake_anon, parse_pqs_segment0_anon};
use boringtun::noise::{Packet, Tunn, TunnResult};
use parking_lot::{Mutex, RwLock};
use socket2::{Domain, Protocol, Socket, Type};

use crate::identity::{Identity, PublicIdentity};

/// Path MTU the handshake is segmented for. 1280 is `PQS_MIN_PATH_MTU`: the
/// floor for static-KEM auth, and the IPv6 minimum, so it crosses any path.
pub const PATH_MTU: u16 = 1280;

/// WireGuard data overhead: type(4) + receiver(4) + counter(8) + tag(16).
pub const WG_OVERHEAD: usize = 32;

/// Largest inner packet that keeps the outer datagram inside `PATH_MTU` over
/// IPv6 (40) + UDP (8), the worst case the handshake math also assumes.
pub const MAX_INNER: usize = PATH_MTU as usize - 48 - WG_OVERHEAD;

const SCRATCH: usize = 65_535;
/// Datagrams per batched send. Measured on a Windows host (Realtek, 1 Gbit/s)
/// at 115 Mbit/s: 16 per call takes a quarter off the sender's CPU and
/// delivers frames as fast as one per call; a whole 64-datagram pacing group
/// in one call held the sender until it was all on the wire, and frames
/// reached the client a millisecond later.
const SEND_BATCH: usize = 16;
const SOCKET_BUFFER: usize = 8 << 20;
/// How long `recv` blocks before returning `Received::Timeout`.
pub const RECV_TIMEOUT: Duration = Duration::from_millis(100);
const KEEPALIVE_SECS: u16 = 25;

thread_local! {
    static ENCAP: RefCell<Vec<u8>> = RefCell::new(vec![0u8; SCRATCH]);
    static WIRE: RefCell<Vec<u8>> = RefCell::new(vec![0u8; SCRATCH]);
}

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    NoAddress,
    WireGuard(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "socket: {e}"),
            TransportError::NoAddress => write!(f, "peer address not known yet"),
            TransportError::WireGuard(e) => write!(f, "wireguard: {e}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<io::Error> for TransportError {
    fn from(e: io::Error) -> Self {
        TransportError::Io(e)
    }
}

/// A tunnel peer's index: the `Tunn` index, also the high 24 bits of every
/// receiver index it is addressed by.
pub type PeerId = u32;

pub struct Peer {
    id: PeerId,
    public: PublicIdentity,
    tunn: Mutex<Tunn>,
    addr: Mutex<Option<SocketAddr>>,
    /// Milliseconds since the endpoint's epoch of the last datagram that
    /// authenticated (any kind, keepalives included). 0 = never.
    last_rx_ms: AtomicU64,
    rx_datagrams: AtomicU64,
    tx_datagrams: AtomicU64,
    segmented_sends: AtomicUsize,
}

impl Peer {
    pub fn id(&self) -> PeerId {
        self.id
    }

    pub fn public(&self) -> &PublicIdentity {
        &self.public
    }

    pub fn addr(&self) -> Option<SocketAddr> {
        *self.addr.lock()
    }

    pub fn set_addr(&self, addr: SocketAddr) {
        *self.addr.lock() = Some(unmap(addr));
    }

    /// A session key exists. False briefly during every rekey, so it is not a
    /// liveness signal on its own; see `since_rx`.
    pub fn is_established(&self) -> bool {
        self.tunn.lock().stats().0.is_some()
    }

    /// Time since the current session key was installed.
    pub fn since_handshake(&self) -> Option<Duration> {
        self.tunn.lock().stats().0
    }

    /// pq-boringtun's own RTT estimate, from the last handshake.
    pub fn handshake_rtt_ms(&self) -> Option<u32> {
        self.tunn.lock().stats().4
    }

    pub fn rx_datagrams(&self) -> u64 {
        self.rx_datagrams.load(Ordering::Relaxed)
    }

    pub fn tx_datagrams(&self) -> u64 {
        self.tx_datagrams.load(Ordering::Relaxed)
    }

    /// How many handshake messages went out segmented. The segmented PQ
    /// handshake is what this project exists to exercise, and it failing is
    /// otherwise silent.
    pub fn segmented_sends(&self) -> usize {
        self.segmented_sends.load(Ordering::Relaxed)
    }
}

/// What one `recv` produced.
pub enum Received {
    /// An inner packet from `peer`, written to the caller's buffer.
    Data(Arc<Peer>, usize),
    /// A datagram that was not WireGuard (STUN, rendezvous), copied to the
    /// caller's buffer.
    Foreign(SocketAddr, usize),
    /// Handshake, keepalive, or a datagram that failed to authenticate.
    Control,
    /// Nothing arrived within `RECV_TIMEOUT`.
    Timeout,
}

enum Post {
    Nothing,
    Send(Vec<Vec<u8>>),
    Delivered(usize),
    Failed(String),
}

fn classify(result: TunnResult<'_>, deliver_into: Option<&mut [u8]>) -> Post {
    match result {
        TunnResult::Done => Post::Nothing,
        TunnResult::Err(e) => Post::Failed(format!("{e:?}")),
        TunnResult::WriteToNetwork(p) => Post::Send(vec![p.to_vec()]),
        TunnResult::WriteManyToNetwork(segments) => {
            Post::Send(segments.iter().map(|s| s.to_vec()).collect())
        }
        TunnResult::WriteToTunnelV4(p, _) | TunnResult::WriteToTunnelV6(p, _) => match deliver_into
        {
            Some(buf) => {
                let n = p.len().min(buf.len());
                buf[..n].copy_from_slice(&p[..n]);
                Post::Delivered(n)
            }
            None => Post::Nothing,
        },
    }
}

/// The tunnel state for one peer: pq-boringtun with static ML-KEM
/// authentication, handshakes segmented for `PATH_MTU`.
pub(crate) fn new_tunn(
    own: &Identity,
    peer: &PublicIdentity,
    index: u32,
) -> Result<Tunn, TransportError> {
    let mut tunn = Tunn::new(
        own.x25519().clone(),
        x25519_dalek::PublicKey::from(peer.x25519),
        None,
        Some(KEEPALIVE_SECS),
        index,
        None,
    );
    // Order matters: set_pq_static_auth refuses a path MTU below its floor.
    tunn.set_pq_path_mtu(PATH_MTU)
        .map_err(|e| TransportError::WireGuard(format!("set_pq_path_mtu: {e:?}")))?;
    tunn.set_pq_static_auth(own.mlkem().clone(), peer.mlkem_key())
        .map_err(|e| TransportError::WireGuard(format!("set_pq_static_auth: {e:?}")))?;
    Ok(tunn)
}

/// `::ffff:a.b.c.d` -> `a.b.c.d`, so addresses compare and display as users
/// expect on a dual-stack socket.
fn unmap(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        v4 => v4,
    }
}

pub struct Endpoint {
    socket: UdpSocket,
    /// Batched sends on `socket` (see [`Endpoint::send_batch`]).
    batch: quinn_udp::UdpSocketState,
    /// A batched send failed where single ones then worked: batch no more.
    batch_off: AtomicBool,
    /// At most this many datagrams per call: `SEND_BATCH`, or
    /// PINGPONG_SEND_BATCH=N (0 or 1: one per call).
    batch_cap: usize,
    /// Batched receives (macOS; PINGPONG_RECV_BATCH=N datagrams a call, 0: off).
    #[cfg(target_os = "macos")]
    inbox: Option<Mutex<Inbox>>,
    dual_stack: bool,
    identity: Arc<Identity>,
    peers: RwLock<HashMap<PeerId, Arc<Peer>>>,
    by_key: RwLock<HashMap<[u8; 32], PeerId>>,
    seg_routes: Mutex<HashMap<(SocketAddr, u32), (PeerId, Instant)>>,
    epoch: Instant,
    test_loss: Option<TestLoss>,
}

/// Received datagrams dropped on purpose, for tests without a network
/// impairment tool: `PINGPONG_TEST_WIRE_LOSS=N[:B]` drops N% of all tunnel
/// datagrams in bursts of B; `PINGPONG_TEST_HANDSHAKE_LOSS=N` drops N% of
/// handshake datagrams only (rekeys under loss); `PINGPONG_TEST_BLACKOUT=S:D`
/// sends and receives nothing from S seconds after the endpoint opened, for
/// D seconds (a dropped connection).
struct TestLoss {
    wire: (u32, u32),
    handshake: u32,
    rng: AtomicU32,
    burst: AtomicU32,
    blackout: Option<(Duration, Duration)>,
}

impl TestLoss {
    fn from_env() -> Option<TestLoss> {
        let wire = std::env::var("PINGPONG_TEST_WIRE_LOSS").ok().and_then(|v| {
            let (p, b) = v.split_once(':').unwrap_or((&v, "1"));
            Some((p.parse().ok()?, b.parse::<u32>().ok()?.max(1)))
        });
        let handshake = std::env::var("PINGPONG_TEST_HANDSHAKE_LOSS")
            .ok()
            .and_then(|v| v.parse().ok());
        let blackout = std::env::var("PINGPONG_TEST_BLACKOUT").ok().and_then(|v| {
            let (s, d) = v.split_once(':')?;
            Some((
                Duration::from_secs_f64(s.parse().ok()?),
                Duration::from_secs_f64(d.parse().ok()?),
            ))
        });
        if wire.is_none() && handshake.is_none() && blackout.is_none() {
            return None;
        }
        tracing::warn!(
            ?wire,
            ?handshake,
            ?blackout,
            "dropping datagrams on purpose (test)"
        );
        Some(TestLoss {
            wire: wire.unwrap_or((0, 1)),
            handshake: handshake.unwrap_or(0),
            rng: AtomicU32::new(0x9E37_79B9),
            burst: AtomicU32::new(0),
            blackout,
        })
    }

    fn dark(&self, since_open: Duration) -> bool {
        self.blackout
            .is_some_and(|(start, len)| since_open >= start && since_open < start + len)
    }

    /// Per 10 000.
    fn roll(&self) -> u32 {
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng.store(x, Ordering::Relaxed);
        x % 10_000
    }

    fn drops(&self, datagram: &[u8]) -> bool {
        if datagram[0] != 4 && self.roll() < self.handshake * 100 {
            return true;
        }
        let (pct, burst) = self.wire;
        if pct == 0 {
            return false;
        }
        let left = self.burst.load(Ordering::Relaxed);
        if left > 0 {
            self.burst.store(left - 1, Ordering::Relaxed);
            return true;
        }
        if self.roll() < pct * 100 / burst {
            self.burst.store(burst - 1, Ordering::Relaxed);
            return true;
        }
        false
    }
}

impl Endpoint {
    /// Bind `port` on every address (IPv6 and IPv4). Port 0 picks one.
    pub fn bind(identity: Arc<Identity>, port: u16) -> io::Result<Endpoint> {
        let (socket, dual_stack) = match bind_dual_stack(port) {
            Ok(s) => (s, true),
            Err(_) => {
                let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
                s.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port).into())?;
                (s, false)
            }
        };
        let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
        let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
        let batch = batch_state(&socket)?;
        socket.set_read_timeout(Some(RECV_TIMEOUT))?;
        Ok(Endpoint {
            socket: socket.into(),
            batch,
            batch_off: AtomicBool::new(false),
            batch_cap: std::env::var("PINGPONG_SEND_BATCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(SEND_BATCH)
                .max(1),
            #[cfg(target_os = "macos")]
            inbox: recv_batch().map(|n| Mutex::new(Inbox::new(n))),
            dual_stack,
            identity,
            peers: RwLock::new(HashMap::new()),
            by_key: RwLock::new(HashMap::new()),
            seg_routes: Mutex::new(HashMap::new()),
            epoch: Instant::now(),
            test_loss: TestLoss::from_env(),
        })
    }

    /// Mark what this socket sends for the network's priority queues, as
    /// Moonlight does: `interactive` for a client (its input and control;
    /// Wi-Fi's voice queue), else video (a host's stream; the video queue).
    /// macOS only for now (Windows needs qWAVE or a system QoS policy).
    /// PINGPONG_SERVICE_CLASS=0 leaves traffic unmarked (diagnostics).
    pub fn set_service_class(&self, interactive: bool) {
        if std::env::var("PINGPONG_SERVICE_CLASS").is_ok_and(|v| v == "0") {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            // <sys/socket.h>: SO_NET_SERVICE_TYPE, NET_SERVICE_TYPE_VI / _VO.
            const SO_NET_SERVICE_TYPE: libc::c_int = 0x1116;
            let class: libc::c_int = if interactive { 4 } else { 3 };
            // SAFETY: a valid socket and a c_int option value.
            let rc = unsafe {
                libc::setsockopt(
                    self.socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    SO_NET_SERVICE_TYPE,
                    &class as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                tracing::debug!(error = %io::Error::last_os_error(), "network service class not set");
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = interactive;
    }

    /// How long `recv` blocks when nothing arrives (default `RECV_TIMEOUT`).
    pub fn set_recv_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.socket.set_read_timeout(Some(timeout))
    }

    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    /// Time since `peer` last sent anything that authenticated.
    pub fn since_rx(&self, peer: &Peer) -> Option<Duration> {
        match peer.last_rx_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(self.now_ms().saturating_sub(ms))),
        }
    }

    /// Add a peer, or return the existing one with the same key.
    pub fn add_peer(
        &self,
        public: PublicIdentity,
        addr: Option<SocketAddr>,
    ) -> Result<Arc<Peer>, TransportError> {
        if let Some(existing) = self.peer_by_key(&public.x25519) {
            if let Some(a) = addr {
                existing.set_addr(a);
            }
            return Ok(existing);
        }
        let id = {
            let peers = self.peers.read();
            loop {
                let candidate =
                    (rand_core::RngCore::next_u32(&mut rand_core::OsRng) & 0x00FF_FFFF).max(1);
                if !peers.contains_key(&candidate) {
                    break candidate;
                }
            }
        };
        let tunn = new_tunn(&self.identity, &public, id)?;
        let peer = Arc::new(Peer {
            id,
            public: public.clone(),
            tunn: Mutex::new(tunn),
            addr: Mutex::new(addr.map(unmap)),
            last_rx_ms: AtomicU64::new(0),
            rx_datagrams: AtomicU64::new(0),
            tx_datagrams: AtomicU64::new(0),
            segmented_sends: AtomicUsize::new(0),
        });
        self.peers.write().insert(id, peer.clone());
        self.by_key.write().insert(public.x25519, id);
        Ok(peer)
    }

    pub fn remove_peer(&self, key: &[u8; 32]) -> Option<Arc<Peer>> {
        let id = self.by_key.write().remove(key)?;
        self.peers.write().remove(&id)
    }

    pub fn peer_by_key(&self, key: &[u8; 32]) -> Option<Arc<Peer>> {
        let id = *self.by_key.read().get(key)?;
        self.peers.read().get(&id).cloned()
    }

    pub fn peer(&self, id: PeerId) -> Option<Arc<Peer>> {
        self.peers.read().get(&id).cloned()
    }

    pub fn peers(&self) -> Vec<Arc<Peer>> {
        self.peers.read().values().cloned().collect()
    }

    fn wire_addr(&self, addr: SocketAddr) -> SocketAddr {
        match (self.dual_stack, addr) {
            (true, SocketAddr::V4(v4)) => {
                SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
            }
            _ => addr,
        }
    }

    /// Send a raw datagram, bypassing the tunnel (NAT traversal probes).
    pub fn send_raw(&self, bytes: &[u8], to: SocketAddr) -> io::Result<()> {
        if self.dark() {
            return Ok(());
        }
        self.socket.send_to(bytes, self.wire_addr(to)).map(|_| ())
    }

    /// In a test blackout: nothing goes out or comes in.
    fn dark(&self) -> bool {
        self.test_loss
            .as_ref()
            .is_some_and(|t| t.dark(self.epoch.elapsed()))
    }

    fn transmit(
        &self,
        peer: &Peer,
        packets: &[Vec<u8>],
        to: SocketAddr,
    ) -> Result<(), TransportError> {
        if packets.len() > 1 {
            peer.segmented_sends.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                segments = packets.len(),
                "sending segmented PQ handshake message"
            );
        }
        let to = self.wire_addr(to);
        if self.dark() {
            return Ok(());
        }
        for p in packets {
            self.socket.send_to(p, to)?;
            peer.tx_datagrams.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Encrypt and send one inner packet.
    pub fn send(&self, peer: &Peer, inner: &[u8]) -> Result<(), TransportError> {
        let to = peer.addr().ok_or(TransportError::NoAddress)?;
        let post = ENCAP.with(|s| {
            classify(
                peer.tunn.lock().encapsulate(inner, &mut s.borrow_mut()),
                None,
            )
        });
        match post {
            Post::Send(p) => self.transmit(peer, &p, to),
            Post::Failed(e) => Err(TransportError::WireGuard(e)),
            _ => Ok(()),
        }
    }

    /// Encrypt one inner packet and send it to `to` rather than the peer's
    /// current address: a probe of another path. If the peer answers there,
    /// roaming moves the session onto it.
    pub fn send_via(
        &self,
        peer: &Peer,
        inner: &[u8],
        to: SocketAddr,
    ) -> Result<(), TransportError> {
        let post = ENCAP.with(|s| {
            classify(
                peer.tunn.lock().encapsulate(inner, &mut s.borrow_mut()),
                None,
            )
        });
        match post {
            Post::Send(p) => self.transmit(peer, &p, to),
            Post::Failed(e) => Err(TransportError::WireGuard(e)),
            _ => Ok(()),
        }
    }

    /// How many datagrams one system call may carry (1: no batching here).
    pub fn batch_segments(&self) -> usize {
        if self.batch_off.load(Ordering::Relaxed) {
            1
        } else {
            self.batch.max_gso_segments().clamp(1, self.batch_cap)
        }
    }

    /// Encrypt a batch under one lock, then send it with the lock released, so
    /// a keyframe's hundreds of sends never hold up the receive thread's
    /// decryption of input for the same peer. The datagrams are encrypted end
    /// to end into `out` (reused scratch) and go out in runs of equal size,
    /// each run in one system call where the OS can: UDP segmentation offload
    /// on Linux and Windows, `sendmsg_x` on macOS.
    pub fn send_batch<'a>(
        &self,
        peer: &Peer,
        inner: impl Iterator<Item = &'a [u8]>,
        out: &mut WireBatch,
    ) -> Result<(), TransportError> {
        let to = peer.addr().ok_or(TransportError::NoAddress)?;
        out.lens.clear();
        let mut at = 0;
        {
            let mut tunn = peer.tunn.lock();
            if tunn.time_since_last_handshake().is_none() {
                // No session: boringtun queues each packet and starts a
                // handshake, which is not a data datagram's size.
                drop(tunn);
                for packet in inner {
                    self.send(peer, packet)?;
                }
                return Ok(());
            }
            for packet in inner {
                let end = at + packet.len() + WG_OVERHEAD;
                if out.bytes.len() < end {
                    out.bytes.resize(end.max(2 * out.bytes.len()), 0);
                }
                match tunn.encapsulate(packet, &mut out.bytes[at..end]) {
                    TunnResult::WriteToNetwork(bytes) => {
                        out.lens.push(bytes.len());
                        at += bytes.len();
                    }
                    TunnResult::Err(e) => return Err(TransportError::WireGuard(format!("{e:?}"))),
                    _ => {}
                }
            }
        }
        if self.dark() {
            return Ok(());
        }
        let to = self.wire_addr(to);
        let max = self.batch_segments();
        let mut result = Ok(());
        let (mut i, mut start) = (0, 0);
        while i < out.lens.len() {
            // A run: datagrams of one size, the last of them possibly shorter.
            let size = out.lens[i];
            let mut j = i + 1;
            let mut end = start + size;
            while j < out.lens.len() && j - i < max && out.lens[j] <= size {
                end += out.lens[j];
                j += 1;
                if out.lens[j - 1] < size {
                    break;
                }
            }
            match self.send_run(&out.bytes[start..end], size, to) {
                // Keep going: one refused datagram is FEC's problem, a whole
                // abandoned frame is not.
                Err(e) => result = Err(TransportError::Io(e)),
                Ok(()) => {
                    peer.tx_datagrams
                        .fetch_add((j - i) as u64, Ordering::Relaxed);
                }
            }
            i = j;
            start = end;
        }
        result
    }

    /// Send `bytes`, datagrams of `size` bytes end to end (the last may be
    /// shorter), to `to`.
    fn send_run(&self, bytes: &[u8], size: usize, to: SocketAddr) -> io::Result<()> {
        if bytes.len() <= size {
            return self.socket.send_to(bytes, to).map(|_| ());
        }
        let transmit = quinn_udp::Transmit {
            destination: to,
            ecn: None,
            contents: bytes,
            segment_size: Some(size),
            src_ip: None,
        };
        let Err(e) = self.batch.try_send((&self.socket).into(), &transmit) else {
            return Ok(());
        };
        // One at a time instead. If that works, batching is what failed (a
        // NIC without segmentation offload, say): stop batching.
        for datagram in bytes.chunks(size) {
            self.socket.send_to(datagram, to)?;
        }
        if !self.batch_off.swap(true, Ordering::Relaxed) {
            tracing::warn!(error = %e, "batched send failed; sending one datagram per call \
                from now on");
        }
        Ok(())
    }

    /// Start a handshake now (client side, once the host's address is known).
    /// Without this the first handshake waits for the 25 s keepalive.
    pub fn initiate(&self, peer: &Peer) -> Result<(), TransportError> {
        let to = peer.addr().ok_or(TransportError::NoAddress)?;
        let packets = self.initiation(peer)?;
        self.transmit(peer, &packets, to)
    }

    /// A fresh handshake initiation for `peer`, as the datagrams to send.
    /// Sending the same initiation along several paths races them: the host
    /// answers the first copy it receives (a duplicate fails its replay
    /// check), and the answer's source becomes the peer's address.
    pub fn initiation(&self, peer: &Peer) -> Result<Vec<Vec<u8>>, TransportError> {
        self.initiation_with(peer, false)
    }

    /// A new initiation even while one is in flight. WireGuard retries a
    /// handshake only after REKEY_TIMEOUT (5 s), and only to the peer's one
    /// address: a client racing several paths, whose first initiation was
    /// lost (or met a busy host), re-races sooner with this.
    pub fn fresh_initiation(&self, peer: &Peer) -> Result<Vec<Vec<u8>>, TransportError> {
        self.initiation_with(peer, true)
    }

    fn initiation_with(&self, peer: &Peer, force: bool) -> Result<Vec<Vec<u8>>, TransportError> {
        let post = ENCAP.with(|s| {
            classify(
                peer.tunn
                    .lock()
                    .format_handshake_initiation(&mut s.borrow_mut(), force),
                None,
            )
        });
        match post {
            Post::Send(p) => Ok(p),
            Post::Failed(e) => Err(TransportError::WireGuard(e)),
            _ => Ok(Vec::new()),
        }
    }

    /// Send datagrams from [`Endpoint::initiation`] to `to`.
    pub fn send_initiation(
        &self,
        peer: &Peer,
        packets: &[Vec<u8>],
        to: SocketAddr,
    ) -> Result<(), TransportError> {
        self.transmit(peer, packets, to)
    }

    /// Pump every peer's timers: rekey, keepalive, handshake retry. Call at
    /// least every 250 ms.
    pub fn tick(&self) {
        for peer in self.peers() {
            let Some(to) = peer.addr() else { continue };
            let post =
                ENCAP.with(|s| classify(peer.tunn.lock().update_timers(&mut s.borrow_mut()), None));
            match post {
                Post::Send(p) => {
                    let _ = self.transmit(&peer, &p, to);
                }
                Post::Failed(e) => tracing::debug!(peer = peer.id, error = %e, "timer"),
                _ => {}
            }
        }
    }

    fn route(&self, datagram: &[u8], src: SocketAddr) -> Option<Arc<Peer>> {
        let by_index = |idx: u32| self.peer(idx >> 8);
        let own_public = self.identity.x25519_public();
        match Tunn::parse_incoming_packet(datagram).ok()? {
            Packet::PacketData(p) => by_index(p.receiver_idx),
            Packet::HandshakeResponse(p) => by_index(p.receiver_idx),
            Packet::PacketCookieReply(p) => by_index(p.receiver_idx),
            Packet::PqHandshakeResponse(p) => by_index(p.receiver_idx),
            Packet::PqsHandshakeResponse(p) => by_index(p.receiver_idx),
            Packet::PqsHandshakeInit(ref p) => {
                let hh = parse_pqs_handshake_anon(self.identity.mlkem(), &own_public, p).ok()?;
                self.peer_by_key(&hh.peer_static_public)
            }
            Packet::PqSegment(p) => {
                let mut routes = self.seg_routes.lock();
                if p.seg_idx == 0 {
                    if let Ok(hh) =
                        parse_pqs_segment0_anon(self.identity.mlkem(), &own_public, p.chunk)
                    {
                        let peer = self.peer_by_key(&hh.peer_static_public)?;
                        routes.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(10));
                        routes.insert((src, p.hs_id), (peer.id, Instant::now()));
                        return Some(peer);
                    }
                    return by_index(p.hs_id);
                }
                match routes.get(&(src, p.hs_id)) {
                    Some((id, _)) => self.peer(*id),
                    None => by_index(p.hs_id),
                }
            }
            // Every peer here uses static-KEM auth; a classical or phase-1
            // initiation is from something that is not one of ours.
            Packet::HandshakeInit(_) | Packet::PqHandshakeInit(_) => None,
        }
    }

    /// Receive one datagram, blocking up to `RECV_TIMEOUT`. Call from one
    /// thread only.
    pub fn recv(&self, out: &mut [u8]) -> io::Result<Received> {
        #[cfg(target_os = "macos")]
        if let Some(inbox) = &self.inbox {
            let mut inbox = inbox.lock();
            if inbox.next == inbox.count {
                if let Some(nothing) = inbox.fill(&self.batch, &self.socket)? {
                    return Ok(nothing);
                }
            }
            let (datagram, src) = inbox.take();
            return self.receive(datagram, src, out);
        }
        WIRE.with(|w| {
            let mut wire = w.borrow_mut();
            let (n, src) = match self.socket.recv_from(&mut wire[..]) {
                Ok(v) => v,
                Err(e) => return nothing_received(e),
            };
            self.receive(&wire[..n], src, out)
        })
    }

    /// One received datagram: authenticate and decrypt it, answer it (a
    /// handshake), or pass it on as it is (not WireGuard).
    fn receive(&self, datagram: &[u8], src: SocketAddr, out: &mut [u8]) -> io::Result<Received> {
        let src = unmap(src);
        let n = datagram.len();
        let is_wireguard = n >= 4 && (1..=9).contains(&datagram[0]) && datagram[1..4] == [0, 0, 0];
        if !is_wireguard {
            let len = n.min(out.len());
            out[..len].copy_from_slice(&datagram[..len]);
            return Ok(Received::Foreign(src, len));
        }
        if self.dark() || self.test_loss.as_ref().is_some_and(|t| t.drops(datagram)) {
            return Ok(Received::Control);
        }

        let Some(peer) = self.route(datagram, src) else {
            return Ok(Received::Control);
        };
        let post = if out.len() >= SCRATCH {
            // Straight into the caller's buffer: boringtun decrypts a data
            // packet to its front (and anything it answers with fits too).
            match peer.tunn.lock().decapsulate(Some(src.ip()), datagram, out) {
                TunnResult::WriteToTunnelV4(p, _) | TunnResult::WriteToTunnelV6(p, _) => {
                    Post::Delivered(p.len())
                }
                other => classify(other, None),
            }
        } else {
            ENCAP.with(|s| {
                classify(
                    peer.tunn
                        .lock()
                        .decapsulate(Some(src.ip()), datagram, &mut s.borrow_mut()),
                    Some(out),
                )
            })
        };
        if let Post::Failed(e) = &post {
            tracing::trace!(peer = peer.id, %src, error = %e, "datagram did not authenticate");
            return Ok(Received::Control);
        }
        // Authenticated: roaming and liveness.
        peer.set_addr(src);
        peer.last_rx_ms.store(self.now_ms(), Ordering::Relaxed);
        peer.rx_datagrams.fetch_add(1, Ordering::Relaxed);
        match post {
            Post::Delivered(len) => Ok(Received::Data(peer, len)),
            Post::Send(p) => {
                let _ = self.transmit(&peer, &p, src);
                self.drain(&peer, src);
                Ok(Received::Control)
            }
            _ => Ok(Received::Control),
        }
    }

    fn drain(&self, peer: &Peer, to: SocketAddr) {
        loop {
            let post = ENCAP.with(|s| {
                let mut scratch = s.borrow_mut();
                match peer.tunn.lock().decapsulate(None, &[], &mut scratch) {
                    TunnResult::Done => None,
                    other => Some(classify(other, None)),
                }
            });
            match post {
                None | Some(Post::Failed(_)) => break,
                Some(Post::Send(p)) => {
                    let _ = self.transmit(peer, &p, to);
                }
                Some(_) => {}
            }
        }
    }
}

/// Scratch for [`Endpoint::send_batch`]: a batch's datagrams, encrypted end
/// to end. Keep one per sending thread; it grows to the largest batch.
#[derive(Default)]
pub struct WireBatch {
    bytes: Vec<u8>,
    lens: Vec<usize>,
}

impl WireBatch {
    pub fn new() -> WireBatch {
        WireBatch::default()
    }
}

/// A receive that returned no datagram: what to tell the caller.
fn nothing_received(e: io::Error) -> io::Result<Received> {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Ok(Received::Timeout),
        // Windows reports an ICMP port-unreachable for an earlier send as an
        // error on the NEXT recv. It says nothing about this one.
        io::ErrorKind::ConnectionReset => Ok(Received::Control),
        _ => Err(e),
    }
}

/// Datagrams read at once and handed out one per `recv` (macOS: recvmsg_x
/// takes every datagram queued, up to its batch, in one system call; it
/// blocks, as recv_from does, only while there are none).
#[cfg(target_os = "macos")]
struct Inbox {
    buf: Vec<u8>,
    meta: Vec<quinn_udp::RecvMeta>,
    next: usize,
    count: usize,
}

/// Room for any datagram a peer sends (the tunnel's are at most `PATH_MTU`).
#[cfg(target_os = "macos")]
const INBOX_SLOT: usize = 4096;

/// Datagrams one receive call may take (None: one per recv_from).
#[cfg(target_os = "macos")]
fn recv_batch() -> Option<usize> {
    let n = match std::env::var("PINGPONG_RECV_BATCH") {
        Ok(v) => v.parse().unwrap_or(RECV_BATCH),
        Err(_) => RECV_BATCH,
    };
    (n > 1).then_some(n.min(quinn_udp::BATCH_SIZE))
}

#[cfg(target_os = "macos")]
const RECV_BATCH: usize = quinn_udp::BATCH_SIZE;

#[cfg(target_os = "macos")]
impl Inbox {
    fn new(n: usize) -> Inbox {
        Inbox {
            buf: vec![0; n * INBOX_SLOT],
            meta: vec![quinn_udp::RecvMeta::default(); n],
            next: 0,
            count: 0,
        }
    }

    /// Read what is queued (waiting for the first up to the socket's
    /// timeout). Some(result) when there was nothing.
    fn fill(
        &mut self,
        state: &quinn_udp::UdpSocketState,
        socket: &UdpSocket,
    ) -> io::Result<Option<Received>> {
        self.next = 0;
        self.count = 0;
        let n = self.meta.len();
        let mut chunks = self.buf.chunks_mut(INBOX_SLOT);
        let mut bufs: [std::io::IoSliceMut<'_>; quinn_udp::BATCH_SIZE] =
            std::array::from_fn(|_| std::io::IoSliceMut::new(chunks.next().unwrap_or_default()));
        match state.recv(socket.into(), &mut bufs[..n], &mut self.meta) {
            Ok(n) => {
                self.count = n;
                Ok(None)
            }
            Err(e) => nothing_received(e).map(Some),
        }
    }

    fn take(&mut self) -> (&[u8], SocketAddr) {
        let i = self.next;
        self.next += 1;
        let m = &self.meta[i];
        let len = m.len.min(INBOX_SLOT);
        (&self.buf[i * INBOX_SLOT..i * INBOX_SLOT + len], m.addr)
    }
}

/// Batched sends on `socket`, which stays blocking: `recv` waits on it with a
/// timeout, one datagram at a time.
fn batch_state(socket: &Socket) -> io::Result<quinn_udp::UdpSocketState> {
    let state = quinn_udp::UdpSocketState::new(socket.into())?;
    #[cfg(target_os = "macos")]
    // SAFETY: sendmsg_x has been in macOS since 10.11 (this builds for 14 and
    // later), and quinn-udp falls back to sendmsg if the symbol is missing.
    unsafe {
        state.set_apple_fast_path();
    }
    // Received datagrams are read one per recv; quinn-udp's coalescing of
    // them (GRO) would hand several over as one.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let off: libc::c_int = 0;
        // SAFETY: a valid socket and a c_int option value.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_UDP,
                libc::UDP_GRO,
                &off as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    socket.set_nonblocking(false)?;
    Ok(state)
}

fn bind_dual_stack(port: u16) -> io::Result<Socket> {
    let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_only_v6(false)?;
    s.bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port).into())?;
    Ok(s)
}
