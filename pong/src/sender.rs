//! Encoded frames → FEC packets → the tunnel, paced.
//!
//! Pacing is Apollo's (`stream.cpp`, videoBroadcastThread): packets go out in
//! groups no larger than 1 ms's worth of the pacing rate, and a group waits
//! until its share of time has come. A frame therefore leaves as a short,
//! even stream instead of one burst -- the burst is what overflows a Wi-Fi
//! queue or a router buffer and loses more packets than FEC can repair.
//!
//! On the local network, to a client that understands them, frames go in
//! `LAN_PAYLOAD_LEN` shards: a sixth fewer datagrams, as Moonlight's larger
//! LAN packets.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use pingpong_proto::fec::FecPolicy;
use pingpong_proto::packetize::{Datagrams, Packetizer};
use pingpong_proto::video::FrameType;
use pingpong_transport::{Endpoint, Peer, WireBatch};

/// Who a session's video goes to: its client first, then anyone watching
/// (an agent's session). The session thread changes it as watchers come
/// and go; the sender reads it once a frame.
pub type Recipients = Arc<parking_lot::RwLock<Vec<Arc<Peer>>>>;

/// One encoded frame, with the pingpong frame prefix already in `data`.
pub struct OutFrame {
    pub id: u32,
    pub kind: FrameType,
    pub capture_ts_us: u32,
    pub data: Vec<u8>,
}

/// Counters the stats line reads and resets.
#[derive(Default)]
pub struct SendStats {
    pub frames: AtomicU64,
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub failed: AtomicU64,
    pub pace_wait_us: AtomicU64,
}

/// The pacing arithmetic, separate from the clock so it can be tested.
pub struct Pace {
    /// Bytes on the wire per millisecond at the pacing rate.
    per_ms: usize,
    /// When the next frame's first group may go: groups of the previous frame
    /// that were paced into the future still count.
    next_start: Option<Instant>,
}

/// Datagrams per group at most.
const GROUP_MAX: usize = 64;
/// A datagram's bytes on the wire beyond the inner packet: WireGuard, UDP
/// and IPv4 headers.
pub const WIRE_OVERHEAD: usize = 32 + 8 + 20;

impl Pace {
    pub fn new(pace_mbps: u32) -> Pace {
        Pace {
            per_ms: (pace_mbps as usize * 1_000_000 / 8 / 1000).max(1),
            next_start: None,
        }
    }

    /// Datagrams per group: 1 ms's worth at the pacing rate, at most
    /// `GROUP_MAX`.
    pub fn group_len(&self, datagram_bytes: usize) -> usize {
        (self.per_ms / datagram_bytes.max(1)).clamp(1, GROUP_MAX)
    }

    /// When a group may be sent, `bytes_before` into a frame that started at
    /// `frame_start`.
    pub fn due(&self, frame_start: Instant, bytes_before: usize) -> Instant {
        frame_start + Duration::from_micros((bytes_before as u64 * 1000) / self.per_ms as u64)
    }

    pub fn frame_start(&mut self, now: Instant) -> Instant {
        match self.next_start {
            Some(t) if t > now => t,
            _ => now,
        }
    }

    pub fn frame_done(&mut self, frame_start: Instant, bytes: usize) {
        self.next_start = Some(self.due(frame_start, bytes));
    }
}

/// An address on this network (or one nearby), where the path's MTU is
/// Ethernet's: private and link-local IPv4, link-local and unique-local
/// IPv6 -- but not Tailscale's addresses (100.64/10, fd7a:115c:a1e0::/48),
/// whose interface MTU is 1280.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let tailscale = s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0;
            (v6.is_unicast_link_local() || v6.is_unique_local()) && !tailscale
        }
    }
}

/// Everything the send thread works with, handed over when it starts.
pub struct SendLoop {
    pub endpoint: Arc<Endpoint>,
    pub recipients: Recipients,
    pub frames: Receiver<OutFrame>,
    /// The pacing rate: at most 1 ms's worth of it leaves in one group.
    pub pace_mbps: u32,
    /// The FEC policy the session thread picks, as `FecPolicy::to_bits`.
    pub fec: Arc<AtomicU16>,
    /// Whether the client may be sent `LAN_PAYLOAD_LEN` shards.
    pub lan_shards: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
    pub stats: Arc<SendStats>,
}

/// The send thread: packetizes each encoded frame and paces it onto the
/// tunnel, to the client and then to each watcher, until `stop`.
pub fn run(send: SendLoop) {
    let SendLoop {
        endpoint,
        recipients,
        frames,
        pace_mbps,
        fec,
        lan_shards,
        stop,
        stats,
    } = send;
    let mut packetizer = Packetizer::new();
    let mut fec_bits = FecPolicy::DEFAULT.to_bits();
    let mut pace = Pace::new(pace_mbps);
    let mut lan_now = false;
    let mut packets = Datagrams::new();
    let mut scratch = WireBatch::new();
    tracing::info!(
        per_call = endpoint.batch_segments(),
        "video datagrams per send call"
    );
    while !stop.load(Ordering::Relaxed) {
        let frame = match frames.recv_timeout(Duration::from_millis(100)) {
            Ok(f) => f,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let bits = fec.load(Ordering::Relaxed);
        if bits != fec_bits {
            fec_bits = bits;
            packetizer.set_fec(FecPolicy::from_bits(bits));
        }
        let to: Vec<Arc<Peer>> = recipients.read().clone();
        // One client, on the local network, that understands them (the
        // session says so): LAN-sized shards.
        let lan = lan_shards.load(Ordering::Relaxed)
            && to.len() == 1
            && to[0].addr().is_some_and(|a| is_lan(a.ip()));
        if lan != lan_now {
            lan_now = lan;
            tracing::info!(lan, "video shard size for the local network");
            packetizer.set_lan_shards(lan);
        }
        if let Err(e) = packetizer.packetize_into(
            &frame.data,
            frame.id,
            frame.capture_ts_us,
            frame.kind == FrameType::Idr,
            frame.kind == FrameType::Recovery,
            &mut packets,
        ) {
            tracing::warn!(error = ?e, len = frame.data.len(), "frame could not be packetized");
            continue;
        }

        let start = pace.frame_start(Instant::now());
        let group_len = pace.group_len(packets.get(0).len() + WIRE_OVERHEAD);
        let (mut sent, mut sent_bytes) = (0, 0);
        let mut first = 0;
        while first < packets.len() {
            let group = first..(first + group_len).min(packets.len());
            first = group.end;
            let due = pace.due(start, sent_bytes);
            let now = Instant::now();
            if due > now {
                let wait = due - now;
                stats
                    .pace_wait_us
                    .fetch_add(wait.as_micros() as u64, Ordering::Relaxed);
                std::thread::sleep(wait);
            }
            // Each group to every recipient in turn: a watcher costs the
            // link as much again, paced the same way.
            for peer in &to {
                match endpoint.send_batch(peer, packets.range(group.clone()), &mut scratch) {
                    Ok(()) => {}
                    Err(e) => {
                        stats.failed.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(error = %e, "send");
                    }
                }
            }
            let bytes: usize = packets
                .range(group.clone())
                .map(|d| d.len() + WIRE_OVERHEAD)
                .sum();
            sent += group.len() * to.len().max(1);
            sent_bytes += bytes * to.len().max(1);
        }
        pace.frame_done(start, sent_bytes);
        stats.frames.fetch_add(1, Ordering::Relaxed);
        stats.packets.fetch_add(sent as u64, Ordering::Relaxed);
        stats
            .bytes
            .fetch_add(packets.bytes() as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_hundred_megabits_is_about_seventy_datagrams_a_millisecond() {
        let pace = Pace::new(800);
        assert_eq!(pace.group_len(1260), 64);
        assert_eq!(Pace::new(400).group_len(1260), 39);
        assert_eq!(Pace::new(400).group_len(1480), 33);
    }

    #[test]
    fn a_frame_is_spread_at_the_pacing_rate() {
        let pace = Pace::new(100); // 12 500 bytes a millisecond
        let t0 = Instant::now();
        assert_eq!(pace.due(t0, 0), t0);
        assert_eq!(pace.due(t0, 12_500), t0 + Duration::from_millis(1));
        assert_eq!(pace.due(t0, 43_750), t0 + Duration::from_micros(3500));
    }

    #[test]
    fn a_frame_arriving_early_waits_for_the_previous_frames_tail() {
        let mut pace = Pace::new(100);
        let t0 = Instant::now();
        pace.frame_done(t0, 62_500); // occupies 5 ms
        assert_eq!(
            pace.frame_start(t0 + Duration::from_millis(1)),
            t0 + Duration::from_millis(5)
        );
        assert_eq!(
            pace.frame_start(t0 + Duration::from_millis(9)),
            t0 + Duration::from_millis(9)
        );
    }

    #[test]
    fn the_local_network_is_private_addresses_but_not_tailscale() {
        for lan in [
            "192.168.1.15",
            "10.1.2.3",
            "172.16.0.9",
            "169.254.3.4",
            "fe80::1",
            "fd12:3456::1",
        ] {
            assert!(is_lan(lan.parse().unwrap()), "{lan}");
        }
        for far in [
            "100.101.102.103",
            "203.0.113.7",
            "8.8.8.8",
            "fd7a:115c:a1e0::1",
            "2001:db8::1",
            "127.0.0.1",
        ] {
            assert!(!is_lan(far.parse().unwrap()), "{far}");
        }
    }
}
