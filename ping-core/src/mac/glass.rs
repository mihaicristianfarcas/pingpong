//! The window server's path for the stream's layer, learnt from the
//! drawables' trips to the glass.
//!
//! Full screen with nothing over it, macOS takes the layer straight to the
//! display: a drawable committed now is on the glass at the next refresh,
//! and one drawable at a time waiting for it is enough (the renderer's
//! `wait_for_glass` keeps it to one, so that the next frame drawn is the next
//! shown). Anything that makes the window server composite the layer first
//! puts the glass two to three refreshes away: a screen recording, a window,
//! menu or notification over the stream, a window rather than full screen, an
//! HDR picture. One drawable at a time then shows a frame every third
//! refresh -- 40 of 60 frames a second at 120 Hz -- and only the layer's
//! three keep up.
//!
//! No API says which path the layer is on, so its trips say. A trip is
//! clean when nothing ahead held the drawable up: none was waiting when it
//! was committed, or the refresh before its own showed nothing new. Clean
//! trips last under a refresh and a half straight to the display, two or
//! more composited. Measured with `ping-core/examples/mac-present` on an M4
//! Pro's 120 Hz panel at 3600x2338: 2.8-11.3 ms straight, 17.8-27.4 ms
//! while the screen was recorded.
//!
//! A stream at the display's rate keeps drawables waiting at every refresh
//! once three may wait, so no trip is clean, and a composited path could
//! never learn that the recording is over. It checks itself once a second
//! (a probe): one drawable waits for one fewer ahead of it than usual, which
//! leaves a refresh with nothing new before it on the glass, so its trip is
//! clean -- unless the drawables ahead are a queue of the stream's own (the
//! path went straight, they stayed, each still shown a refresh after the
//! last). Then the next drawable waits for none at all.

/// Clean trips longer than this, in refreshes, are composited ones: between
/// the longest straight trip measured (1.36) and the shortest composited
/// one (2.13).
const COMPOSITED_TRIP: f64 = 1.75;
/// Composited trips in a row before the path counts as composited. One
/// straight trip makes it straight again: a composited path cannot deliver
/// one, while a straight one is late now and then (a busy GPU).
const COMPOSITED_AFTER: u32 = 4;
/// Seconds a composited path goes without a clean trip before the next
/// drawable is a probe, which shows one frame a refresh longer at the
/// display's rate; going straight again takes about this long.
const PROBE_AFTER: f64 = 1.0;

/// How the next drawable goes to the glass when a composited path checks
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// With one fewer waiting ahead of it than usual.
    Gap,
    /// With none waiting ahead of it: a gap probe was held up all the same.
    Drain,
}

/// A drawable on its way to the glass: when it was committed, how many were
/// ahead of it, and whether it is a probe.
#[derive(Debug, Clone, Copy)]
pub struct Trip {
    committed: f64,
    ahead: u32,
    probe: Option<Probe>,
}

/// Drawables on their way to the glass, and the path they take. Times are
/// `CACurrentMediaTime` seconds, as `presentedTime` reports them.
#[derive(Debug, Default)]
pub struct GlassPath {
    /// Committed and not yet on the glass.
    queued: u32,
    /// When the last drawable reached the glass.
    last_on_glass: f64,
    /// Composited clean trips in a row.
    long_trips: u32,
    composited: bool,
    /// When the last clean trip ended.
    last_clean: f64,
    /// A probe is on its way: no other until it is back.
    probe_out: bool,
    /// The last gap probe was held up: the next probe drains.
    drain_next: bool,
    /// What the next drawable committed is, as it was let through to the
    /// glass (`admit`): decided once, since a trip ending between the wait
    /// and the commit can change what `probe` says.
    admitted: Option<Probe>,
}

impl GlassPath {
    /// Drawables committed and not yet on the glass.
    pub fn queued(&self) -> u32 {
        self.queued
    }

    /// The window server composites the layer, as the trips say.
    pub fn composited(&self) -> bool {
        self.composited
    }

    /// The probe the next drawable should be, if one is due: composited, no
    /// probe out, and no clean trip for `PROBE_AFTER` (or a gap probe held
    /// up).
    pub fn probe(&self, now: f64) -> Option<Probe> {
        if !self.composited || self.probe_out {
            None
        } else if self.drain_next {
            Some(Probe::Drain)
        } else {
            (now - self.last_clean > PROBE_AFTER).then_some(Probe::Gap)
        }
    }

    /// The next drawable may go to the glass, as `probe` if it is one (it
    /// waited as that probe waits).
    pub fn admit(&mut self, probe: Option<Probe>) {
        self.admitted = probe;
    }

    /// A drawable committed at `now`, as it was admitted.
    pub fn commit(&mut self, now: f64) -> Trip {
        let probe = self.admitted.take();
        let trip = Trip {
            committed: now,
            ahead: self.queued,
            probe,
        };
        self.queued += 1;
        self.probe_out |= probe.is_some();
        trip
    }

    /// The drawable of `trip` reached the glass at `at` (0: it never did),
    /// on a display that refreshes every `refresh` seconds at its fastest.
    pub fn presented(&mut self, trip: Trip, at: f64, refresh: f64) {
        self.queued = self.queued.saturating_sub(1);
        if trip.probe.is_some() {
            self.probe_out = false;
        }
        if at <= 0.0 || refresh <= 0.0 {
            return;
        }
        let clean = trip.ahead == 0 || at - self.last_on_glass > 1.5 * refresh;
        self.last_on_glass = self.last_on_glass.max(at);
        let refreshes = (at - trip.committed) / refresh;
        tracing::trace!(
            trip_ms = format_args!("{:.2}", (at - trip.committed) * 1e3),
            ahead = trip.ahead,
            clean,
            probe = ?trip.probe,
            "on the glass"
        );
        if !clean {
            if trip.probe == Some(Probe::Gap) {
                self.drain_next = true;
            }
            return;
        }
        self.last_clean = at;
        self.drain_next = false;
        if refreshes > COMPOSITED_TRIP {
            self.long_trips += 1;
            if self.long_trips >= COMPOSITED_AFTER && !self.composited {
                self.composited = true;
                tracing::info!(
                    trip_ms = format_args!("{:.1}", (at - trip.committed) * 1e3),
                    "the stream is composited (in a window, recorded, or something over \
                     it): frames may queue for the glass"
                );
            }
        } else {
            self.long_trips = 0;
            if self.composited {
                self.composited = false;
                tracing::info!(
                    trip_ms = format_args!("{:.1}", (at - trip.committed) * 1e3),
                    "the stream goes straight to the display again"
                );
            }
        }
    }

    /// Stop counting the drawables still out, a probe among them: one the
    /// window server dropped unshown may never report back.
    pub fn forget_queued(&mut self) {
        self.queued = 0;
        self.probe_out = false;
    }

    /// The window changed (into or out of full screen, to another display),
    /// and with it the path: straight until the trips say otherwise. Going
    /// full screen is itself composited, and a stream at the display's rate
    /// would otherwise wait for a probe to go straight after it.
    pub fn forget_path(&mut self) {
        if self.composited {
            tracing::info!("the window changed: the stream's path is judged afresh");
        }
        self.composited = false;
        self.long_trips = 0;
        self.drain_next = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ120: f64 = 1.0 / 120.0;

    /// Let a drawable through as `probe` and commit it at `at`.
    fn commit_as(p: &mut GlassPath, at: f64, probe: Option<Probe>) -> Trip {
        p.admit(probe);
        p.commit(at)
    }

    /// Commit a drawable at `at` ms and see it on the glass `trip` ms later.
    fn trip(p: &mut GlassPath, at: f64, trip: f64) {
        let t = p.commit(at / 1e3);
        p.presented(t, (at + trip) / 1e3, HZ120);
    }

    #[test]
    fn a_path_is_straight_until_trips_say_otherwise() {
        let mut p = GlassPath::default();
        assert!(!p.composited());
        for i in 0..60 {
            trip(&mut p, 1000.0 + i as f64 * 16.7, 6.0);
        }
        assert!(!p.composited());
    }

    #[test]
    fn four_long_clean_trips_make_the_path_composited() {
        let mut p = GlassPath::default();
        for i in 0..3 {
            trip(&mut p, 1000.0 + i as f64 * 25.0, 24.0);
        }
        assert!(!p.composited());
        trip(&mut p, 1075.0, 24.0);
        assert!(p.composited());
    }

    #[test]
    fn a_straight_trip_now_and_then_keeps_a_late_one_from_counting() {
        let mut p = GlassPath::default();
        for i in 0..40 {
            let late = if i % 3 == 0 { 6.0 } else { 20.0 };
            trip(&mut p, 1000.0 + i as f64 * 25.0, late);
        }
        assert!(!p.composited());
    }

    #[test]
    fn one_short_clean_trip_makes_the_path_straight_again() {
        let mut p = GlassPath::default();
        for i in 0..4 {
            trip(&mut p, 1000.0 + i as f64 * 25.0, 24.0);
        }
        assert!(p.composited());
        trip(&mut p, 1200.0, 5.0);
        assert!(!p.composited());
    }

    #[test]
    fn trips_held_up_by_drawables_ahead_are_not_judged() {
        // Straight to the display at its full rate, with two drawables
        // waiting ahead of each since a burst: every trip is the queue's two
        // refreshes and the path's 6 ms, none of it the path's own.
        let mut p = GlassPath::default();
        let mut out = std::collections::VecDeque::new();
        out.push_back(p.commit(0.9995));
        out.push_back(p.commit(0.9996));
        for k in 0..120 {
            let now = 1.0 + k as f64 * HZ120;
            out.push_back(p.commit(now));
            let oldest = out.pop_front().unwrap();
            p.presented(oldest, now + 0.006, HZ120);
        }
        assert!(!p.composited());
    }

    #[test]
    fn a_trip_after_a_free_refresh_is_judged_even_with_one_ahead() {
        let mut p = GlassPath::default();
        // 60 fps on a composited path: each still out when the next is
        // committed, but a refresh with nothing new between them.
        let mut ahead = None;
        for i in 0..6 {
            let at = 1000.0 + i as f64 * 16.7;
            let t = p.commit(at / 1e3);
            if let Some((t, on)) = ahead.take() {
                p.presented(t, on, HZ120);
            }
            ahead = Some((t, (at + 24.0) / 1e3));
        }
        assert!(p.composited());
    }

    /// A path made composited by four long clean trips, the last on the
    /// glass at 1.099 s.
    fn composited() -> GlassPath {
        let mut p = GlassPath::default();
        for i in 0..4 {
            trip(&mut p, 1000.0 + i as f64 * 25.0, 24.0);
        }
        assert!(p.composited());
        p
    }

    #[test]
    fn a_composited_path_without_clean_trips_asks_for_a_gap_probe() {
        let mut p = composited();
        assert_eq!(p.probe(1.2), None);
        assert_eq!(p.probe(2.2), Some(Probe::Gap));
        // The probe's trip: still composited, and the clock starts again.
        let t = commit_as(&mut p, 2.2, Some(Probe::Gap));
        p.presented(t, 2.222, HZ120);
        assert!(p.composited());
        assert_eq!(p.probe(2.3), None);
    }

    #[test]
    fn one_probe_at_a_time() {
        let mut p = composited();
        let due = p.probe(2.2);
        let probe = commit_as(&mut p, 2.2, due);
        assert_eq!(p.probe(2.21), None);
        p.presented(probe, 2.222, HZ120);
        assert_eq!(p.probe(2.23), None);
    }

    #[test]
    fn a_gap_probe_held_up_all_the_same_is_followed_by_a_drain() {
        let mut p = composited();
        // Two out ahead of the probe, each a refresh after the one before
        // it on the glass: a queue, so the probe's trip says nothing.
        let a = p.commit(2.180);
        let b = p.commit(2.198);
        let probe = commit_as(&mut p, 2.2, Some(Probe::Gap));
        p.presented(a, 2.2040, HZ120);
        p.presented(b, 2.2123, HZ120);
        p.presented(probe, 2.2206, HZ120);
        assert!(p.composited());
        assert_eq!(p.probe(2.221), Some(Probe::Drain));
        // The drain: nothing ahead, straight to the display after all.
        let drain = commit_as(&mut p, 2.23, Some(Probe::Drain));
        p.presented(drain, 2.236, HZ120);
        assert!(!p.composited());
        assert_eq!(p.probe(9.0), None);
    }

    #[test]
    fn a_drawable_is_the_probe_it_was_let_through_as() {
        let mut p = composited();
        let a = p.commit(2.180);
        let b = p.commit(2.198);
        let probe = commit_as(&mut p, 2.2, Some(Probe::Gap));
        let c = p.commit(2.208);
        p.presented(a, 2.2040, HZ120);
        p.presented(b, 2.2123, HZ120);
        // Let through as an ordinary drawable, with others still out...
        p.admit(None);
        // ...when the gap probe comes back held up, which makes a drain due.
        p.presented(probe, 2.2206, HZ120);
        assert_eq!(p.probe(2.221), Some(Probe::Drain));
        // It did not wait for an empty path, so it is no drain: the drain is
        // still due for the next one.
        let ordinary = p.commit(2.222);
        assert_eq!(p.probe(2.223), Some(Probe::Drain));
        p.presented(c, 2.2289, HZ120);
        p.presented(ordinary, 2.2372, HZ120);
        assert_eq!(p.probe(2.24), Some(Probe::Drain));
    }

    #[test]
    fn a_probe_that_never_reports_back_does_not_stop_the_next() {
        let mut p = composited();
        let _lost = commit_as(&mut p, 2.2, Some(Probe::Gap));
        assert_eq!(p.probe(3.5), None);
        p.forget_queued();
        assert_eq!(p.queued(), 0);
        assert_eq!(p.probe(3.5), Some(Probe::Gap));
    }

    #[test]
    fn a_straight_path_never_asks_for_a_probe() {
        let mut p = GlassPath::default();
        trip(&mut p, 1000.0, 6.0);
        assert_eq!(p.probe(100.0), None);
    }

    #[test]
    fn a_new_window_is_straight_until_its_trips_say_otherwise() {
        let mut p = composited();
        p.forget_path();
        assert!(!p.composited());
        assert_eq!(p.probe(9.0), None);
        for i in 0..4 {
            trip(&mut p, 2000.0 + i as f64 * 25.0, 24.0);
        }
        assert!(p.composited());
    }

    #[test]
    fn a_drawable_never_shown_frees_its_place_and_says_nothing() {
        let mut p = GlassPath::default();
        let t = p.commit(1.0);
        assert_eq!(p.queued(), 1);
        p.presented(t, 0.0, HZ120);
        assert_eq!(p.queued(), 0);
        assert!(!p.composited());
    }
}
