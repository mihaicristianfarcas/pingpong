//! Selects which captured frames to encode, given a target framerate.
//! See v1 design §7.2 and §7.2.1.
//!
//! CRITICAL: capture delivery is present-driven, NOT clock-driven. A static
//! desktop presents nothing and frames stop arriving entirely. This type is
//! therefore purely reactive -- it is asked about each arriving frame and never
//! drives a clock of its own. Do not rewrite it around a fixed-rate loop.

pub const MIN_TARGET_FPS: u32 = 30;
pub const MAX_TARGET_FPS: u32 = 240;

pub struct Pacer {
    target_fps: u32,
    interval_us: u32,
    /// Slack on the schedule. A frame arriving slightly ahead of when it is due
    /// still counts as that frame.
    ///
    /// Without this, a target equal to the display's refresh rate loses about a
    /// third of all frames, which is what the first end-to-end run measured:
    /// 33-40 fps encoded against 60 captured. WGC delivers at ~16667 us with
    /// jitter of a few hundred microseconds either way, so roughly half the
    /// gaps fall a hair under a strict 16666 us threshold. Each near-miss was
    /// dropped AND pushed the reference forward, so the next frame had to wait
    /// a further full interval.
    ///
    /// A quarter of an interval is wide enough to absorb capture jitter and
    /// narrow enough that a source running at twice the target still alternates
    /// rather than passing everything through.
    tolerance_us: u32,
    /// When the next frame is due, on the `capture_ts_us` timebase.
    next_due_us: Option<u32>,
}

impl Pacer {
    pub fn new(target_fps: u32) -> Self {
        let target_fps = target_fps.clamp(MIN_TARGET_FPS, MAX_TARGET_FPS);
        let interval_us = 1_000_000 / target_fps;
        Pacer {
            target_fps,
            interval_us,
            tolerance_us: interval_us / 4,
            next_due_us: None,
        }
    }

    pub fn target_fps(&self) -> u32 {
        self.target_fps
    }

    /// Should the frame captured at `now_us` be encoded?
    ///
    /// Uses wrapping arithmetic: `now_us` shares the `capture_ts_us` timebase,
    /// which wraps every ~71.6 minutes (v1 design §5.1). Distances are compared as
    /// `i32` after a wrapping subtraction, which is correct for any real gap
    /// under ~35 minutes and needs no special case at the wrap.
    pub fn should_encode(&mut self, now_us: u32) -> bool {
        let Some(due) = self.next_due_us else {
            self.next_due_us = Some(now_us.wrapping_add(self.interval_us));
            return true;
        };

        // Negative means the frame is early; positive means it is late.
        let offset = now_us.wrapping_sub(due) as i32;
        if offset < -(self.tolerance_us as i32) {
            return false;
        }

        self.next_due_us = Some(if offset > self.interval_us as i32 {
            // More than a whole interval late: the source stalled, which for a
            // present-driven capture means the desktop simply had nothing to
            // show. Restart the schedule from now instead of trying to catch up
            // in a burst -- those frames no longer exist to be encoded.
            now_us.wrapping_add(self.interval_us)
        } else {
            // Advance by exactly one interval rather than to `now_us`, so
            // jitter does not accumulate into permanent drift.
            due.wrapping_add(self.interval_us)
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_always_encodes() {
        let mut p = Pacer::new(60);
        assert!(p.should_encode(0));
    }

    #[test]
    fn drops_frames_arriving_faster_than_target() {
        // 120 Hz arrival, 60 fps target -> every other frame.
        let mut p = Pacer::new(60);
        let step = 1_000_000 / 120;
        let decisions: Vec<bool> = (0..8).map(|i| p.should_encode(i * step)).collect();
        assert_eq!(
            decisions,
            vec![true, false, true, false, true, false, true, false]
        );
    }

    #[test]
    fn passes_everything_through_when_target_meets_arrival() {
        let mut p = Pacer::new(120);
        let step = 1_000_000 / 120;
        for i in 0..10 {
            assert!(p.should_encode(i * step), "frame {i} should encode");
        }
    }

    /// THE regression test for the bug the first end-to-end run exposed.
    ///
    /// A 60 Hz display feeding a 60 fps target must encode essentially every
    /// frame. The original pacer dropped a third of them, because real capture
    /// timestamps jitter either side of the nominal interval and a strict
    /// `elapsed >= interval` test rejects every gap that lands a hair short --
    /// then pushes its reference forward so the following frame waits a whole
    /// extra interval.
    #[test]
    fn keeps_nearly_every_frame_when_the_target_equals_the_refresh_rate() {
        let mut p = Pacer::new(60);
        // Nominal 16667 us with +-300 us of jitter, which is well inside what
        // WGC actually delivers.
        let jitter = [0i32, -280, 190, -310, 120, -150, 260, -90, 300, -220];
        let mut t: u32 = 0;
        let mut encoded = 0;
        for i in 0..600 {
            if p.should_encode(t) {
                encoded += 1;
            }
            t = t.wrapping_add((16_667 + jitter[i % jitter.len()]) as u32);
        }
        assert!(
            encoded >= 594,
            "expected ~600 of 600 frames encoded at a matched rate, got {encoded}"
        );
    }

    /// The flip side: slack must not be so generous that a source running at
    /// twice the target passes everything through.
    #[test]
    fn still_halves_a_source_running_at_double_the_target() {
        let mut p = Pacer::new(60);
        let mut encoded = 0;
        for i in 0..600u32 {
            if p.should_encode(i * (1_000_000 / 120)) {
                encoded += 1;
            }
        }
        assert!(
            (295..=305).contains(&encoded),
            "expected ~300 of 600 at double rate, got {encoded}"
        );
    }

    #[test]
    fn sparse_arrival_always_encodes() {
        // A static desktop presents nothing, so frames arrive far apart
        // (v1 design §7.2.1). Every one must be encoded.
        let mut p = Pacer::new(60);
        assert!(p.should_encode(0));
        assert!(p.should_encode(5_000_000)); // 5 s later
        assert!(p.should_encode(30_000_000)); // 25 s later
    }

    #[test]
    fn irregular_arrival_does_not_stall() {
        // Deliberately jittery arrival: the pacer must never enter a state
        // where it stops encoding entirely.
        let mut p = Pacer::new(60);
        let mut t = 0u32;
        let mut encoded = 0;
        for jitter in [1000u32, 40_000, 2_000, 33_000, 500, 90_000, 16_000] {
            t = t.wrapping_add(jitter);
            if p.should_encode(t) {
                encoded += 1;
            }
        }
        assert!(
            encoded >= 3,
            "must keep encoding under jitter, got {encoded}"
        );
    }

    #[test]
    fn survives_timestamp_wrap() {
        // capture_ts_us wraps every ~71.6 minutes (v1 design §5.1).
        let mut p = Pacer::new(60);
        assert!(p.should_encode(u32::MAX - 1000));
        // Wrapped past the end: delta is 1000 + 15_667 via wrapping_sub.
        assert!(p.should_encode(15_667));
    }

    #[test]
    fn clamps_target_fps_to_valid_range() {
        assert_eq!(Pacer::new(0).target_fps(), 30);
        assert_eq!(Pacer::new(10).target_fps(), 30);
        assert_eq!(Pacer::new(1000).target_fps(), 240);
        assert_eq!(Pacer::new(144).target_fps(), 144);
    }
}
