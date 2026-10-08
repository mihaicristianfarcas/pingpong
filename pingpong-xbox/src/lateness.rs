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
//! Said once a second as the most of the second, in the debug log, and
//! judged with the frames given up into the corner note a stream shows on
//! a poor connection ([`LinkJudge`]).

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

/// A queue this long, a second at a time, says the link carries less than
/// the console sends: on a link that keeps up, a frame is late by its time
/// on the wire and its retransmissions' (tens of milliseconds).
const BEHIND: Duration = Duration::from_millis(200);
/// Under this, the queue has drained.
const CAUGHT_UP: Duration = Duration::from_millis(100);

/// What the corner note says about the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkWarning {
    /// The picture runs this far behind: more is sent than the link
    /// carries.
    Behind(Duration),
    /// Frames are given up for keyframes, or none came in a second: the
    /// link loses packets that retransmissions do not bring back, or
    /// carries nothing.
    Lossy,
}

/// The corner note, judged once a second as Pong's stream judges its own
/// (Moonlight's rule): shown after two bad seconds in a row, gone after
/// three good ones.
#[derive(Debug, Default)]
pub struct LinkJudge {
    bad: u32,
    good: u32,
    shown: Option<LinkWarning>,
}

impl LinkJudge {
    /// A second of the stream ended: the most a frame was late in it
    /// (`None`: no frame came), and how many times frames were given up.
    /// Returns the note when it changes: a new one, or `None` to take it
    /// away.
    pub fn second(&mut self, late: Option<Duration>, given_up: u64) -> Option<Option<LinkWarning>> {
        let now = match late {
            // A picture standing still, waiting for a keyframe that does
            // not come, is the worst second of all, not a quiet one.
            None => Some(LinkWarning::Lossy),
            Some(late) if late >= BEHIND || (self.shown.is_some() && late >= CAUGHT_UP) => {
                Some(LinkWarning::Behind(late))
            }
            Some(_) if given_up > 0 => Some(LinkWarning::Lossy),
            Some(_) => None,
        };
        match now {
            Some(w) => {
                self.bad += 1;
                self.good = 0;
                if self.bad >= 2 && self.shown != Some(w) {
                    self.shown = Some(w);
                    return Some(Some(w));
                }
            }
            None => {
                self.good += 1;
                self.bad = 0;
                if self.good >= 3 && self.shown.is_some() {
                    self.shown = None;
                    return Some(None);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Option<Duration> {
        Some(Duration::from_millis(n))
    }

    #[test]
    fn the_note_comes_after_two_bad_seconds_and_goes_after_three_good() {
        let mut j = LinkJudge::default();
        assert_eq!(j.second(ms(30), 0), None);
        assert_eq!(j.second(ms(400), 0), None);
        assert_eq!(
            j.second(ms(600), 0),
            Some(Some(LinkWarning::Behind(Duration::from_millis(600))))
        );
        // Still behind, by more: the note says so.
        assert_eq!(
            j.second(ms(900), 0),
            Some(Some(LinkWarning::Behind(Duration::from_millis(900))))
        );
        assert_eq!(j.second(ms(20), 0), None);
        assert_eq!(j.second(ms(20), 0), None);
        assert_eq!(j.second(ms(20), 0), Some(None));
        assert_eq!(j.second(ms(20), 0), None);
    }

    #[test]
    fn frames_given_up_two_seconds_running_are_a_poor_connection() {
        let mut j = LinkJudge::default();
        assert_eq!(j.second(ms(30), 1), None);
        assert_eq!(j.second(ms(30), 2), Some(Some(LinkWarning::Lossy)));
        assert_eq!(j.second(ms(30), 1), None, "said once");
        // One bad second among good ones says nothing.
        let mut j = LinkJudge::default();
        for _ in 0..5 {
            assert_eq!(j.second(ms(30), 0), None);
            assert_eq!(j.second(ms(30), 1), None);
        }
    }

    #[test]
    fn a_picture_standing_still_keeps_the_note() {
        let mut j = LinkJudge::default();
        j.second(ms(30), 1);
        assert_eq!(j.second(None, 0), Some(Some(LinkWarning::Lossy)));
        // Seconds without a frame (a keyframe asked for, not come) are not
        // good ones: the note stays.
        for _ in 0..5 {
            assert_eq!(j.second(None, 0), None);
        }
        assert_eq!(j.second(ms(30), 0), None);
        assert_eq!(j.second(ms(30), 0), None);
        assert_eq!(j.second(ms(30), 0), Some(None));
    }

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
