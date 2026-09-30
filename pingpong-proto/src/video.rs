//! Video frames above the packet layer: the per-frame prefix, and the client's
//! decision about which reassembled frames may reach the decoder.
//!
//! The decision is Moonlight's (`VideoDepacketizer.c`): once a frame is lost,
//! nothing more is decoded until a frame arrives that does not depend on it --
//! an IDR, or the recovery frame the host encodes after invalidating the lost
//! frames as references. Decoding past a loss is what produced v2's smeared
//! blocks and regions that never healed: H.264 happily decodes a P-frame whose
//! reference is missing, and every later frame inherits the damage.

/// Bytes prepended to every encoded frame before packetization.
pub const FRAME_PREFIX_LEN: usize = 4;
const PREFIX_VERSION: u8 = 1;

/// The prefix carries the host's processing latency (capture to hand-off to
/// the network), so the client can show it without synchronised clocks.
pub fn write_prefix(host_latency_us: u32) -> [u8; FRAME_PREFIX_LEN] {
    let units = (host_latency_us / 100).min(u16::MAX as u32) as u16;
    let b = units.to_le_bytes();
    [PREFIX_VERSION, 0, b[0], b[1]]
}

/// Split a reassembled frame into (host latency µs, bitstream).
pub fn split_prefix(frame: &[u8]) -> Option<(u32, &[u8])> {
    if frame.len() < FRAME_PREFIX_LEN || frame[0] != PREFIX_VERSION {
        return None;
    }
    let units = u16::from_le_bytes([frame[2], frame[3]]) as u32;
    Some((units * 100, &frame[FRAME_PREFIX_LEN..]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Idr,
    P,
    /// First frame after reference invalidation.
    Recovery,
}

impl FrameType {
    pub fn from_flags(keyframe: bool, recovery: bool) -> FrameType {
        if keyframe {
            FrameType::Idr
        } else if recovery {
            FrameType::Recovery
        } else {
            FrameType::P
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Decode,
    Drop,
}

/// Something the client must ask the host for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Idr,
    Invalidate { first: u32, last: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitIdr,
    Streaming,
    AwaitRecovery { first_lost: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GateStats {
    pub decoded: u64,
    /// Frames received but withheld from the decoder while recovering.
    pub dropped: u64,
    /// Frames that never arrived.
    pub lost: u64,
    /// Loss events (each starts one recovery).
    pub losses: u64,
    pub idr_requests: u64,
    pub rfi_requests: u64,
}

fn ahead(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

pub struct FrameGate {
    state: State,
    next: Option<u32>,
    latest: u32,
    last_request_us: u64,
    wait_started_us: u64,
    /// How long to wait for a request's effect before repeating it.
    retry_us: u64,
    /// How long a reference-invalidation recovery may take before giving up
    /// on it and asking for an IDR.
    escalate_us: u64,
    stats: GateStats,
}

impl FrameGate {
    /// A gate waiting for the session's first IDR. The host opens every
    /// session with one, so no request is made until `retry` has passed.
    pub fn new(now_us: u64) -> FrameGate {
        let mut g = FrameGate {
            state: State::AwaitIdr,
            next: None,
            latest: 0,
            last_request_us: now_us,
            wait_started_us: now_us,
            retry_us: 0,
            escalate_us: 0,
            stats: GateStats::default(),
        };
        g.set_rtt(0);
        g
    }

    /// Scale retries to the path: one round trip plus margin, never tighter
    /// than 50 ms (so a busy LAN does not repeat itself) and an IDR escalation
    /// no sooner than 300 ms.
    pub fn set_rtt(&mut self, rtt_us: u64) {
        self.retry_us = (2 * rtt_us + 20_000).max(50_000);
        self.escalate_us = (6 * rtt_us).max(300_000);
    }

    pub fn stats(&self) -> GateStats {
        self.stats
    }

    pub fn is_recovering(&self) -> bool {
        self.state != State::Streaming
    }

    fn request(&mut self, r: Request, now_us: u64) -> Option<Request> {
        self.last_request_us = now_us;
        match r {
            Request::Idr => self.stats.idr_requests += 1,
            Request::Invalidate { .. } => self.stats.rfi_requests += 1,
        }
        Some(r)
    }

    /// A frame was reassembled. Decide whether the decoder may have it.
    pub fn on_frame(&mut self, id: u32, ty: FrameType, now_us: u64) -> (Verdict, Option<Request>) {
        let gap = match self.next {
            None => 0,
            Some(n) => ahead(id, n),
        };
        if gap < 0 {
            // Late or duplicate: its successor was already handled.
            self.stats.dropped += 1;
            return (Verdict::Drop, None);
        }
        self.next = Some(id.wrapping_add(1));
        self.latest = id;
        if gap > 0 {
            self.stats.lost += gap as u64;
        }

        if ty == FrameType::Idr {
            self.state = State::Streaming;
            self.stats.decoded += 1;
            return (Verdict::Decode, None);
        }

        match self.state {
            State::AwaitIdr => {
                self.stats.dropped += 1;
                (Verdict::Drop, None)
            }
            State::Streaming if gap == 0 => {
                self.stats.decoded += 1;
                (Verdict::Decode, None)
            }
            State::Streaming => {
                let first_lost = id.wrapping_sub(gap as u32);
                self.state = State::AwaitRecovery { first_lost };
                self.wait_started_us = now_us;
                self.stats.losses += 1;
                self.stats.dropped += 1;
                // This frame is withheld too: it predicts from the loss.
                let r = self.request(
                    Request::Invalidate {
                        first: first_lost,
                        last: id,
                    },
                    now_us,
                );
                (Verdict::Drop, r)
            }
            // Trusted even after a further gap: a recovery frame references only
            // frames older than the invalidated range, which starts at our
            // first loss and runs to the last frame encoded before the request
            // arrived -- so every frame in the gap is one it does not use.
            State::AwaitRecovery { .. } if ty == FrameType::Recovery => {
                self.state = State::Streaming;
                self.stats.decoded += 1;
                (Verdict::Decode, None)
            }
            State::AwaitRecovery { first_lost } => {
                self.stats.dropped += 1;
                // A further loss while waiting (possibly of the recovery frame
                // itself): widen the request right away rather than waiting
                // for the retry timer.
                let r = if gap > 0 {
                    self.request(
                        Request::Invalidate {
                            first: first_lost,
                            last: id,
                        },
                        now_us,
                    )
                } else {
                    None
                };
                (Verdict::Drop, r)
            }
        }
    }

    /// Timer: repeat or escalate a request whose effect has not arrived. Call
    /// every few milliseconds, and whenever a frame arrives.
    pub fn poll(&mut self, now_us: u64) -> Option<Request> {
        let since_request = now_us.saturating_sub(self.last_request_us);
        match self.state {
            State::Streaming => None,
            State::AwaitIdr => {
                // IDRs are big: never ask faster than every 200 ms, or a lossy
                // link drowns in them.
                if since_request >= self.retry_us.max(200_000) {
                    self.request(Request::Idr, now_us)
                } else {
                    None
                }
            }
            State::AwaitRecovery { first_lost } => {
                if now_us.saturating_sub(self.wait_started_us) >= self.escalate_us {
                    self.state = State::AwaitIdr;
                    self.request(Request::Idr, now_us)
                } else if since_request >= self.retry_us {
                    self.request(
                        Request::Invalidate {
                            first: first_lost,
                            last: self.latest,
                        },
                        now_us,
                    )
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FrameType::*;

    const MS: u64 = 1000;

    fn streaming_from(first: u32) -> FrameGate {
        let mut g = FrameGate::new(0);
        assert_eq!(g.on_frame(first, Idr, 0), (Verdict::Decode, None));
        g
    }

    #[test]
    fn prefix_round_trips() {
        let mut frame = write_prefix(6_789).to_vec();
        frame.extend_from_slice(b"bits");
        assert_eq!(split_prefix(&frame), Some((6_700, &b"bits"[..])));
        assert_eq!(split_prefix(&[9, 0, 0, 0]), None);
        assert_eq!(split_prefix(&[1, 0]), None);
    }

    #[test]
    fn nothing_decodes_before_the_first_idr() {
        let mut g = FrameGate::new(0);
        assert_eq!(g.on_frame(5, P, 0).0, Verdict::Drop);
        assert_eq!(g.on_frame(6, Recovery, 0).0, Verdict::Drop);
        assert_eq!(g.on_frame(7, Idr, 0).0, Verdict::Decode);
        assert_eq!(g.on_frame(8, P, 0).0, Verdict::Decode);
    }

    #[test]
    fn a_missing_first_idr_is_requested_but_not_immediately() {
        let mut g = FrameGate::new(0);
        assert_eq!(
            g.poll(10 * MS),
            None,
            "the host opens with an IDR; don't ask for a second"
        );
        assert_eq!(g.poll(250 * MS), Some(Request::Idr));
        assert_eq!(g.poll(260 * MS), None, "rate limited");
    }

    #[test]
    fn a_gap_stops_decoding_and_asks_for_invalidation() {
        let mut g = streaming_from(10);
        assert_eq!(g.on_frame(11, P, 1).0, Verdict::Decode);
        // 12 and 13 lost.
        assert_eq!(
            g.on_frame(14, P, 2),
            (
                Verdict::Drop,
                Some(Request::Invalidate {
                    first: 12,
                    last: 14
                })
            )
        );
        // Frames predicting from the loss are withheld...
        assert_eq!(g.on_frame(15, P, 3), (Verdict::Drop, None));
        // ...until the recovery frame.
        assert_eq!(g.on_frame(16, Recovery, 4), (Verdict::Decode, None));
        assert_eq!(g.on_frame(17, P, 5), (Verdict::Decode, None));
        let s = g.stats();
        assert_eq!((s.lost, s.losses, s.dropped), (2, 1, 2));
    }

    #[test]
    fn an_idr_ends_any_wait() {
        let mut g = streaming_from(0);
        g.on_frame(3, P, 0);
        assert!(g.is_recovering());
        assert_eq!(g.on_frame(4, Idr, 1).0, Verdict::Decode);
        assert!(!g.is_recovering());
    }

    #[test]
    fn losing_the_recovery_frame_widens_the_request() {
        let mut g = streaming_from(0);
        g.on_frame(2, P, 0); // 1 lost
                             // Recovery would have been 5; it was lost too.
        assert_eq!(
            g.on_frame(6, P, 10 * MS),
            (
                Verdict::Drop,
                Some(Request::Invalidate { first: 1, last: 6 })
            )
        );
    }

    #[test]
    fn an_unanswered_invalidation_is_repeated_then_escalated_to_idr() {
        let mut g = streaming_from(0);
        g.set_rtt(5 * MS);
        g.on_frame(2, P, 0);
        assert_eq!(g.poll(10 * MS), None);
        assert_eq!(
            g.poll(51 * MS),
            Some(Request::Invalidate { first: 1, last: 2 })
        );
        assert_eq!(g.poll(52 * MS), None);
        assert_eq!(g.poll(301 * MS), Some(Request::Idr));
        assert_eq!(
            g.on_frame(3, P, 302 * MS).0,
            Verdict::Drop,
            "now only an IDR will do"
        );
        assert_eq!(g.on_frame(4, Recovery, 303 * MS).0, Verdict::Drop);
        assert_eq!(g.on_frame(5, Idr, 304 * MS).0, Verdict::Decode);
    }

    #[test]
    fn late_and_duplicate_frames_are_dropped() {
        let mut g = streaming_from(100);
        g.on_frame(101, P, 0);
        assert_eq!(g.on_frame(101, P, 0).0, Verdict::Drop);
        assert_eq!(g.on_frame(99, P, 0).0, Verdict::Drop);
    }

    #[test]
    fn frame_ids_wrap() {
        let mut g = streaming_from(u32::MAX - 1);
        assert_eq!(g.on_frame(u32::MAX, P, 0).0, Verdict::Decode);
        assert_eq!(g.on_frame(0, P, 0).0, Verdict::Decode);
        assert_eq!(
            g.on_frame(2, P, 0),
            (
                Verdict::Drop,
                Some(Request::Invalidate { first: 1, last: 2 })
            )
        );
    }

    #[test]
    fn a_recovery_frame_after_a_further_gap_still_recovers() {
        // 3 was encoded before the host saw the request, so it was invalidated
        // along with 1..=2 and the recovery frame 4 does not reference it.
        let mut g = streaming_from(0);
        g.on_frame(2, P, 0); // wait on 1
        assert_eq!(g.on_frame(4, Recovery, 0).0, Verdict::Decode);
    }

    #[test]
    fn a_recovery_frame_while_streaming_after_a_gap_is_a_new_loss() {
        let mut g = streaming_from(0);
        assert_eq!(g.on_frame(2, Recovery, 0).0, Verdict::Drop);
        assert!(g.is_recovering());
    }
}
