//! Judging the link: loss accounting for the host's bitrate controller, and
//! Moonlight-style connection warnings.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use pingpong_proto::control::LossReport;
use pingpong_proto::header::Header;

/// Moonlight-style connection warnings, judged once a second from what the
/// loss report saw and the second's lowest round trip.
#[derive(Default)]
pub(super) struct NetQuality {
    bad: u32,
    good: u32,
    shown: bool,
    min_rtt_us: u32,
}

impl NetQuality {
    /// Returns Some(show) when the warning should appear or go.
    pub(super) fn judge(&mut self, r: &LossReport) -> Option<bool> {
        if r.rtt_us > 0 {
            self.min_rtt_us = if self.min_rtt_us == 0 {
                r.rtt_us
            } else {
                self.min_rtt_us.min(r.rtt_us)
            };
        }
        let loss = if r.expected > 0 {
            1.0 - r.received.min(r.expected) as f64 / r.expected as f64
        } else {
            0.0
        };
        let slow = self.min_rtt_us > 0
            && r.rtt_us > 3 * self.min_rtt_us
            && r.rtt_us > self.min_rtt_us + 40_000;
        if r.frames_lost >= 2 || loss > 0.05 || slow {
            self.bad += 1;
            self.good = 0;
        } else {
            self.good += 1;
            self.bad = 0;
        }
        if !self.shown && self.bad >= 2 {
            self.shown = true;
            return Some(true);
        }
        if self.shown && self.good >= 3 {
            self.shown = false;
            return Some(false);
        }
        None
    }
}

/// Loss accounting for the host's bitrate controller: which (frame, block)
/// pairs have been seen, and how many shards each should have had.
/// Video datagrams expected and received, per FEC block. A block is settled
/// into the report once it is old enough for all of it to have arrived, so a
/// frame that straddles two reports does not look lost in the first.
pub(super) struct LossCounter {
    blocks: VecDeque<BlockCount>,
    pub(super) report: LossReport,
    /// Video bytes on the wire since the last report, and since when.
    bytes: u64,
    since: Instant,
}

struct BlockCount {
    key: (u32, u8),
    expected: u32,
    received: u32,
    first: Instant,
}

/// Late enough that a block's stragglers have arrived (or never will).
const SETTLE: Duration = Duration::from_millis(150);

impl LossCounter {
    pub(super) fn new() -> LossCounter {
        LossCounter {
            blocks: VecDeque::new(),
            report: LossReport::default(),
            bytes: 0,
            since: Instant::now(),
        }
    }

    pub(super) fn packet(&mut self, h: &Header, now: Instant) {
        self.bytes += h.total_len as u64;
        let key = (h.frame_id, h.fec_block_idx);
        match self.blocks.iter_mut().rev().find(|b| b.key == key) {
            Some(b) => b.received += 1,
            None => {
                if self.blocks.len() >= 512 {
                    self.settle_one();
                }
                self.blocks.push_back(BlockCount {
                    key,
                    expected: h.data_shards as u32 + h.parity_shards as u32,
                    received: 1,
                    first: now,
                });
            }
        }
    }

    fn settle_one(&mut self) {
        if let Some(b) = self.blocks.pop_front() {
            self.report.expected += b.expected;
            self.report.received += b.received.min(b.expected);
        }
    }

    /// The report's received rate, and start the next interval.
    pub(super) fn take_rate(&mut self, now: Instant) {
        let ms = now.duration_since(self.since).as_millis().max(1) as u64;
        self.report.received_kbps = (self.bytes * 8 / ms).min(u32::MAX as u64) as u32;
        self.bytes = 0;
        self.since = now;
    }

    /// Move every block old enough into the report.
    pub(super) fn settle(&mut self, now: Instant) {
        while self
            .blocks
            .front()
            .is_some_and(|b| now.duration_since(b.first) >= SETTLE)
        {
            self.settle_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::header::Kind;

    #[test]
    fn a_connection_warning_needs_two_bad_seconds_and_clears_after_three_good() {
        use pingpong_proto::control::LossReport;
        let good = LossReport {
            received: 1000,
            expected: 1000,
            rtt_us: 10_000,
            ..Default::default()
        };
        let lossy = LossReport {
            received: 900,
            expected: 1000,
            rtt_us: 10_000,
            ..Default::default()
        };
        let laggy = LossReport {
            rtt_us: 90_000,
            ..good
        };
        let mut q = super::NetQuality::default();
        assert_eq!(q.judge(&good), None);
        assert_eq!(q.judge(&lossy), None, "one bad second is a blip");
        assert_eq!(q.judge(&good), None);
        assert_eq!(q.judge(&lossy), None);
        assert_eq!(q.judge(&laggy), Some(true), "loss, then a queue");
        assert_eq!(q.judge(&good), None);
        assert_eq!(q.judge(&good), None);
        assert_eq!(q.judge(&good), Some(false));
    }

    fn header(frame_id: u32, data: u8, parity: u8) -> Header {
        Header {
            keyframe: false,
            recovery: false,
            lan_shards: false,
            frame_end: false,
            kind: Kind::Video,
            total_len: 0,
            fragment_idx: 0,
            data_shards: data,
            parity_shards: parity,
            fec_block_idx: 0,
            frame_len: 0,
            capture_ts_us: 0,
            frame_id,
        }
    }

    #[test]
    fn a_frame_straddling_two_reports_is_not_loss() {
        let mut c = LossCounter::new();
        let t0 = Instant::now();
        // Frame 1 (1 data + 2 parity): first packet just before a report...
        c.packet(&header(1, 1, 2), t0);
        c.settle(t0 + Duration::from_millis(5));
        assert_eq!(
            (c.report.received, c.report.expected),
            (0, 0),
            "not settled yet"
        );
        // ...the rest just after.
        c.packet(&header(1, 1, 2), t0 + Duration::from_millis(6));
        c.packet(&header(1, 1, 2), t0 + Duration::from_millis(7));
        c.settle(t0 + Duration::from_millis(400));
        assert_eq!((c.report.received, c.report.expected), (3, 3));
    }

    #[test]
    fn a_missing_shard_is_counted_once_settled() {
        let mut c = LossCounter::new();
        let t0 = Instant::now();
        c.packet(&header(7, 4, 2), t0);
        for _ in 0..4 {
            c.packet(&header(7, 4, 2), t0);
        }
        c.settle(t0 + Duration::from_millis(200));
        assert_eq!((c.report.received, c.report.expected), (5, 6));
    }
}
