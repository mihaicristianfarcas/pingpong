//! How much later than its best the console's video arrives: a queue
//! growing on the way, which is what a picture running seconds behind on a
//! slow link is.
//!
//! Each frame carries the console's time (RTP, 90 kHz) and arrives at
//! Ping's. Their difference is the one-way delay plus an offset between
//! the two clocks that nothing here knows; its lowest over the last
//! [`BASELINE`] stands for an empty queue, and how far a frame's is above
//! it is the time it spent queued (or waiting for a retransmission). The
//! two clocks drift by parts per million, which the window's end forgets.
//! Said once a second as the most of the second, in the debug log.

use std::time::{Duration, Instant};

/// How long the lowest delay is remembered: long enough to have seen the
/// queue empty, short enough to forget the clocks' drift.
const BASELINE: Duration = Duration::from_secs(10);

/// The lowest delay of each second of [`BASELINE`].
const SECONDS: usize = BASELINE.as_secs() as usize;

#[derive(Debug, Default)]
pub struct Lateness {
    /// A fixed point on Ping's clock, which delays are counted from.
    epoch: Option<Instant>,
    /// Each second's lowest delay (µs, the console's time against Ping's),
    /// newest last.
    lows: Vec<i64>,
    /// This second's lowest, and its highest above the baseline.
    low: Option<i64>,
    worst: Option<Duration>,
    second_started: Option<Instant>,
}

impl Lateness {
    pub fn new() -> Lateness {
        Lateness::default()
    }

    /// A frame of the console's time `rtp_time` (90 kHz) arrived `at`.
    pub fn frame(&mut self, rtp_time: u64, at: Instant) {
        let epoch = *self.epoch.get_or_insert(at);
        let here = at.saturating_duration_since(epoch).as_micros() as i64;
        let there = (rtp_time as i128 * 1_000_000 / 90_000) as i64;
        let delay = here - there;
        self.low = Some(self.low.map_or(delay, |l| l.min(delay)));
        let floor = self
            .lows
            .iter()
            .copied()
            .chain(self.low)
            .min()
            .unwrap_or(delay);
        let late = Duration::from_micros((delay - floor).max(0) as u64);
        self.worst = Some(self.worst.map_or(late, |w| w.max(late)));
        let started = *self.second_started.get_or_insert(at);
        if at.saturating_duration_since(started) >= Duration::from_secs(1) {
            if self.lows.len() == SECONDS {
                self.lows.remove(0);
            }
            self.lows.extend(self.low.take());
            self.second_started = Some(at);
        }
    }

    /// The most a frame was late since the last call; `None` without
    /// frames.
    pub fn take_worst(&mut self) -> Option<Duration> {
        self.worst.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);

    #[test]
    fn frames_on_time_are_not_late() {
        let mut l = Lateness::new();
        let t0 = Instant::now();
        // The console's clock starts anywhere; the link takes 30 ms.
        for n in 0..120u64 {
            l.frame(
                7_000_000 + n * 1500,
                t0 + Duration::from_millis(30) + FRAME * n as u32,
            );
        }
        assert!(l.take_worst().unwrap() < Duration::from_millis(1));
        assert_eq!(l.take_worst(), None);
    }

    #[test]
    fn a_queue_growing_on_the_way_is_how_late_frames_come() {
        let mut l = Lateness::new();
        let t0 = Instant::now();
        for n in 0..60u64 {
            l.frame(n * 1500, t0 + FRAME * n as u32);
        }
        l.take_worst();
        // The picture moves: each frame queues 20 ms more than the last.
        for n in 60..120u64 {
            let queued = Duration::from_millis(20 * (n - 59));
            l.frame(n * 1500, t0 + FRAME * n as u32 + queued);
        }
        let worst = l.take_worst().unwrap();
        assert!(
            worst >= Duration::from_millis(1190) && worst <= Duration::from_millis(1210),
            "{worst:?}"
        );
    }
}
