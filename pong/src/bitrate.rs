//! Adaptive bitrate and FEC. The client reports once a second what arrived;
//! the host lowers the encoder's bitrate when the path cannot carry the
//! stream and climbs back toward what the client asked for once it can.
//!
//! Random loss is FEC's job, not the bitrate's: at a lower bitrate a lossy
//! Wi-Fi link loses the same share of packets, just with worse pictures. So
//! the rate reacts to congestion -- frames FEC could not rebuild, heavy loss,
//! or queueing delay (round trips well above their recent minimum) -- and
//! otherwise stays.
//!
//! Loss alone is ambiguous, so a cut it causes is a test: if the loss does
//! not fall with the rate, the link loses that much anyway. The cut is undone,
//! that loss becomes the link's baseline (only loss beyond it is congestion),
//! and FEC is raised to cover it ([`BitrateController::fec`]).
//!
//! Under heavy congestion it cuts straight to what the path actually carried
//! (the client's received rate), as WebRTC's does: a queue that has grown to
//! seconds cannot wait for 10%-a-second steps.

use std::collections::VecDeque;

use pingpong_proto::control::LossReport;
use pingpong_proto::fec::FecPolicy;

/// Reports after a cut before climbing again.
const HOLD_REPORTS: u32 = 3;
/// Clean reports needed before each step up.
const CLEAN_TO_CLIMB: u32 = 1;
/// Each step up: this share of the requested rate.
const CLIMB_STEP: f64 = 0.08;
const SEVERE_CUT: f64 = 0.70;
const MILD_CUT: f64 = 0.90;
/// Changes smaller than this are not worth reconfiguring the encoder for.
const MIN_CHANGE: f64 = 0.02;
/// Round-trip samples the baseline minimum is taken over (~30 s).
const RTT_WINDOW: usize = 30;
/// Queueing: RTT above twice the baseline plus this.
const QUEUE_SLACK_US: u32 = 15_000;
/// Severe queueing: RTT above four times the baseline plus this.
const SEVERE_QUEUE_SLACK_US: u32 = 100_000;
/// Encoder bitrate per received wire rate when cutting to it: FEC parity and
/// headers ride on top of the encoder's output, and the queue must drain.
const RATE_CUT: f64 = 0.65;
/// Reports watched after a loss-only cut.
const PROBE_REPORTS: u32 = 3;
/// Loss that stays above this share of what it was before a cut did not
/// follow the rate. Loose, because a second's loss is noisy and the report
/// that set off the cut is likely a high one; a cut that cures congestion
/// takes the loss to about nothing.
const LOSS_STAYED: f64 = 0.5;
/// Link loss below this is noise: default FEC, and lost frames still count
/// as congestion.
const LOSSY_LINK: f64 = 0.01;
/// Loss above this is never taken for the link's own: the stream is broken
/// at any rate, and cutting is the only thing left to try.
const MAX_LINK_LOSS: f64 = 0.25;

/// Watching whether loss follows a cut.
struct Probe {
    loss_before: f64,
    kbps_before: u32,
    reports: u32,
    loss_sum: f64,
}

pub struct BitrateController {
    requested_kbps: u32,
    floor_kbps: u32,
    current_kbps: u32,
    /// False: the rate stays as asked; loss is only measured, for FEC.
    adapt: bool,
    clean: u32,
    hold: u32,
    rtts: VecDeque<u32>,
    /// Packet loss the link has whatever the rate.
    link_loss: f64,
    probe: Option<Probe>,
    /// The rate before a run of loss-only cuts, which a probe that clears
    /// the link restores.
    loss_cut_from: Option<u32>,
    fec: FecPolicy,
}

impl BitrateController {
    pub fn new(requested_kbps: u32, adapt: bool) -> BitrateController {
        BitrateController {
            requested_kbps,
            floor_kbps: (requested_kbps / 8).max(1_500).min(requested_kbps),
            current_kbps: requested_kbps,
            adapt,
            clean: 0,
            hold: 0,
            rtts: VecDeque::with_capacity(RTT_WINDOW),
            link_loss: 0.0,
            probe: None,
            loss_cut_from: None,
            fec: FecPolicy::DEFAULT,
        }
    }

    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// The loss the link has whatever the rate (0..1).
    pub fn link_loss(&self) -> f64 {
        self.link_loss
    }

    /// Parity for the link: the default until it proves lossy, then enough
    /// for about three times its loss, since Wi-Fi loses packets in bursts.
    pub fn fec(&self) -> FecPolicy {
        self.fec
    }

    fn update_fec(&mut self) {
        let p = self.link_loss;
        let wanted = if p < LOSSY_LINK {
            20.0
        } else {
            (20.0 + 300.0 * p).min(60.0)
        };
        // Steps of 5 points, changed only once the estimate is well past the
        // current one: a loss hovering at a boundary does not flap it.
        if (wanted - self.fec.percent as f64).abs() < 3.5 {
            return;
        }
        let percent = (wanted / 5.0).round() * 5.0;
        self.fec = FecPolicy {
            percent: percent as u8,
            // 2 at the default, 8 at 60%: small frames get a burst's worth.
            min_parity: (2.0 + (percent - 20.0) / 7.5).ceil() as u8,
        };
    }

    /// Feed one report; returns the new bitrate when it should change.
    pub fn on_report(&mut self, r: &LossReport) -> Option<u32> {
        if r.expected == 0 {
            // Nothing was sent (a still desktop): no information.
            return None;
        }
        let loss = 1.0 - (r.received.min(r.expected) as f64 / r.expected as f64);
        let (queueing, queue_severe) = self.queueing(r.rtt_us);
        if !self.adapt {
            self.learn_link_loss(loss);
            return None;
        }
        self.hold = self.hold.saturating_sub(1);

        if let Some(p) = self.probe.as_mut() {
            if !queueing && loss <= MAX_LINK_LOSS {
                p.reports += 1;
                p.loss_sum += loss;
                if p.reports < PROBE_REPORTS {
                    return None;
                }
                let p = self.probe.take().expect("probing");
                let after = p.loss_sum / p.reports as f64;
                if after >= p.loss_before * LOSS_STAYED && after < MAX_LINK_LOSS {
                    // The loss did not follow the rate: it is the link's.
                    // Undo the cuts and let FEC carry it. (At least "lossy":
                    // bursts can lose frames at little average loss.)
                    self.link_loss = self.link_loss.max(after).max(LOSSY_LINK);
                    self.update_fec();
                    self.clean = 0;
                    self.hold = 0;
                    let back = self
                        .loss_cut_from
                        .take()
                        .unwrap_or(p.kbps_before)
                        .max(p.kbps_before);
                    return self.set(back);
                }
                // It did: that was congestion. Judge this report as usual.
            } else {
                // A queue is building, or loss no link has: congestion after all.
                self.probe = None;
                self.loss_cut_from = None;
            }
        }

        // Only loss beyond the link's own is congestion; and on a lossy link
        // frames FEC could not rebuild say more about FEC than the rate.
        let excess = loss - self.link_loss;
        let frames_lost = if self.link_loss >= LOSSY_LINK {
            0
        } else {
            r.frames_lost
        };
        let severe = frames_lost >= 2 || excess > 0.10 || queue_severe;
        let mild = frames_lost == 1 || queueing || excess > 0.05;

        let target = if severe || mild {
            self.clean = 0;
            self.hold = HOLD_REPORTS;
            let cut = self.current_kbps as f64 * if severe { SEVERE_CUT } else { MILD_CUT };
            // What got through is the path's capacity only when the path is
            // full: a queue, or loss no link has of its own. Otherwise the
            // stream may simply be sending less than its rate (a still scene).
            let full = queue_severe || loss > MAX_LINK_LOSS;
            let target = if full && r.received_kbps > 0 {
                cut.min(r.received_kbps as f64 * RATE_CUT)
            } else {
                cut
            };
            if queueing {
                self.loss_cut_from = None;
            } else if self.current_kbps > self.floor_kbps {
                self.loss_cut_from.get_or_insert(self.current_kbps);
                self.probe = Some(Probe {
                    loss_before: loss,
                    kbps_before: self.current_kbps,
                    reports: 0,
                    loss_sum: 0.0,
                });
            }
            target
        } else {
            self.learn_link_loss(loss);
            self.clean += 1;
            if self.hold == 0
                && self.clean >= CLEAN_TO_CLIMB
                && self.current_kbps < self.requested_kbps
            {
                self.clean = 0;
                let next = self.current_kbps as f64 + self.requested_kbps as f64 * CLIMB_STEP;
                if self.loss_cut_from.is_some_and(|from| next >= from as f64) {
                    self.loss_cut_from = None;
                }
                next
            } else {
                return None;
            }
        };
        let next = (target.round() as u32).clamp(self.floor_kbps, self.requested_kbps);
        let change =
            (next as f64 - self.current_kbps as f64).abs() / self.current_kbps.max(1) as f64;
        if change < MIN_CHANGE && next != self.requested_kbps {
            return None;
        }
        self.set(next)
    }

    fn set(&mut self, kbps: u32) -> Option<u32> {
        if kbps == self.current_kbps {
            return None;
        }
        self.current_kbps = kbps;
        Some(kbps)
    }

    /// Track the link's loss from reports that showed no congestion: quickly
    /// down (the link got better), slowly up.
    fn learn_link_loss(&mut self, loss: f64) {
        let weight = if loss < self.link_loss { 0.2 } else { 0.05 };
        self.link_loss += weight * (loss - self.link_loss);
        if self.link_loss < LOSSY_LINK / 2.0 {
            self.link_loss = self.link_loss.min(loss);
        }
        self.update_fec();
    }

    /// (queueing, severe queueing).
    fn queueing(&mut self, rtt_us: u32) -> (bool, bool) {
        if rtt_us == 0 {
            return (false, false);
        }
        if self.rtts.len() == RTT_WINDOW {
            self.rtts.pop_front();
        }
        self.rtts.push_back(rtt_us);
        let base = self.rtts.iter().copied().min().unwrap_or(rtt_us);
        let known = self.rtts.len() >= 3;
        (
            known && rtt_us > base * 2 + QUEUE_SLACK_US,
            known && rtt_us > base * 4 + SEVERE_QUEUE_SLACK_US,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(received: u32, expected: u32, frames_lost: u16, rtt_ms: u32) -> LossReport {
        LossReport {
            received,
            expected,
            frames_lost,
            frames_ok: 60,
            rtt_us: rtt_ms * 1000,
            received_kbps: 0,
        }
    }

    #[test]
    fn a_clean_path_keeps_the_requested_rate() {
        let mut c = BitrateController::new(50_000, true);
        for _ in 0..20 {
            assert_eq!(c.on_report(&report(4000, 4000, 0, 3)), None);
        }
        assert_eq!(c.current_kbps(), 50_000);
        assert_eq!(c.fec(), FecPolicy::DEFAULT);
    }

    #[test]
    fn random_loss_that_fec_absorbs_changes_nothing() {
        let mut c = BitrateController::new(50_000, true);
        for _ in 0..20 {
            assert_eq!(
                c.on_report(&report(3900, 4000, 0, 3)),
                None,
                "2.5% loss, every frame rebuilt"
            );
        }
    }

    #[test]
    fn congestion_cuts_then_the_rate_climbs_back() {
        let mut c = BitrateController::new(50_000, true);
        assert_eq!(c.on_report(&report(3000, 4000, 5, 3)), Some(35_000));
        // The loss went with the rate: congestion. Held for a few reports,
        // then up 4 Mbit/s every clean report.
        let mut rates = vec![];
        for _ in 0..40 {
            if let Some(r) = c.on_report(&report(2800, 2800, 0, 3)) {
                rates.push(r);
            }
        }
        assert_eq!(rates.first(), Some(&39_000));
        assert_eq!(rates.last(), Some(&50_000), "back to what was asked for");
        assert!(rates.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(c.fec(), FecPolicy::DEFAULT, "not a lossy link");
    }

    #[test]
    fn congestion_that_a_cut_does_not_cure_is_cut_again() {
        let mut c = BitrateController::new(50_000, true);
        assert_eq!(c.on_report(&report(3000, 4000, 5, 3)), Some(35_000));
        assert_eq!(c.on_report(&report(2400, 2800, 4, 3)), None, "watching");
        assert_eq!(c.on_report(&report(2700, 2800, 1, 3)), None, "watching");
        // Loss fell to a third of what it was: it followed the rate, and
        // what is left still needs a cut.
        assert_eq!(c.on_report(&report(2600, 2800, 2, 3)), Some(24_500));
    }

    #[test]
    fn a_lossy_link_keeps_its_rate_and_gets_more_fec() {
        let mut c = BitrateController::new(50_000, true);
        // 10% loss in bursts FEC cannot always cover, whatever the rate.
        assert_eq!(c.on_report(&report(3600, 4000, 3, 3)), Some(35_000));
        assert_eq!(c.on_report(&report(2520, 2800, 3, 3)), None);
        assert_eq!(c.on_report(&report(2520, 2800, 2, 3)), None);
        assert_eq!(
            c.on_report(&report(2520, 2800, 3, 3)),
            Some(50_000),
            "the cut did not help: undone"
        );
        for _ in 0..30 {
            assert_eq!(c.on_report(&report(3600, 4000, 2, 3)), None);
        }
        assert!((c.link_loss() - 0.10).abs() < 0.01, "{}", c.link_loss());
        assert_eq!(
            c.fec(),
            FecPolicy {
                percent: 50,
                min_parity: 6
            }
        );
    }

    #[test]
    fn congestion_on_a_lossy_link_still_cuts() {
        let mut c = BitrateController::new(50_000, true);
        c.on_report(&report(3600, 4000, 3, 3));
        for _ in 0..3 {
            c.on_report(&report(2520, 2800, 3, 3));
        }
        assert_eq!(c.current_kbps(), 50_000);
        // Loss doubles: the part beyond the link's own is congestion.
        assert_eq!(c.on_report(&report(3200, 4000, 9, 3)), Some(45_000));
    }

    #[test]
    fn a_link_that_heals_gets_default_fec_back() {
        let mut c = BitrateController::new(50_000, true);
        c.on_report(&report(3600, 4000, 3, 3));
        for _ in 0..3 {
            c.on_report(&report(2520, 2800, 3, 3));
        }
        assert_ne!(c.fec(), FecPolicy::DEFAULT);
        for _ in 0..20 {
            c.on_report(&report(4000, 4000, 0, 3));
        }
        assert_eq!(c.fec(), FecPolicy::DEFAULT);
    }

    #[test]
    fn a_fixed_rate_still_learns_the_link_for_fec() {
        let mut c = BitrateController::new(50_000, false);
        for _ in 0..60 {
            assert_eq!(c.on_report(&report(3800, 4000, 4, 90)), None);
        }
        assert_eq!(c.current_kbps(), 50_000);
        assert!(c.fec().percent > 30, "{:?}", c.fec());
    }

    #[test]
    fn a_run_of_cuts_is_undone_whole() {
        let mut c = BitrateController::new(20_000, true);
        // Burst loss at the start of a stream: two frames lost.
        assert_eq!(c.on_report(&report(3800, 4000, 2, 3)), Some(14_000));
        // The loss eases (noise, not the cut), so this probe says congestion,
        c.on_report(&report(3950, 4000, 0, 3));
        c.on_report(&report(3950, 4000, 0, 3));
        // and the next lost frame cuts again.
        assert_eq!(c.on_report(&report(3920, 4000, 1, 3)), Some(12_600));
        for _ in 0..2 {
            assert_eq!(c.on_report(&report(3900, 4000, 1, 3)), None);
        }
        // This time the loss stayed: the link's. Back to where the run began.
        assert_eq!(c.on_report(&report(3900, 4000, 1, 3)), Some(20_000));
    }

    #[test]
    fn a_collapsing_path_is_not_left_to_a_probe() {
        let mut c = BitrateController::new(20_000, true);
        assert_eq!(c.on_report(&report(3700, 4000, 2, 3)), Some(14_000));
        // Mid-probe the path collapses: cut again at once, to what got through.
        let r = LossReport {
            received_kbps: 4_000,
            ..report(1000, 3000, 20, 3)
        };
        assert_eq!(c.on_report(&r), Some(2_600));
    }

    #[test]
    fn a_stream_sending_less_than_its_rate_is_not_cut_to_what_it_sends() {
        let mut c = BitrateController::new(20_000, true);
        // A still scene: 2 Mbit/s on the wire, and a lossy second.
        let r = LossReport {
            received_kbps: 2_000,
            ..report(3500, 4000, 3, 3)
        };
        assert_eq!(c.on_report(&r), Some(14_000));
    }

    #[test]
    fn it_never_goes_below_the_floor() {
        let mut c = BitrateController::new(20_000, true);
        for _ in 0..30 {
            c.on_report(&report(100, 1000, 20, 3));
        }
        assert_eq!(c.current_kbps(), 2_500, "90% loss is never the link's own");
    }

    #[test]
    fn queueing_delay_counts_as_congestion() {
        let mut c = BitrateController::new(50_000, true);
        for _ in 0..5 {
            assert_eq!(c.on_report(&report(4000, 4000, 0, 4)), None);
        }
        // RTT balloons from 4 ms to 60: a queue is building somewhere.
        assert_eq!(c.on_report(&report(4000, 4000, 0, 60)), Some(45_000));
    }

    #[test]
    fn a_long_queue_cuts_straight_to_what_got_through() {
        let mut c = BitrateController::new(40_000, true);
        for _ in 0..5 {
            c.on_report(&report(4000, 4000, 0, 3));
        }
        // A throttle: the queue grows to seconds, 12 Mbit/s gets through.
        let r = LossReport {
            rtt_us: 2_000_000,
            received_kbps: 12_000,
            ..report(1000, 1000, 0, 0)
        };
        assert_eq!(c.on_report(&r), Some(7_800));
    }

    #[test]
    fn an_idle_stream_says_nothing() {
        let mut c = BitrateController::new(50_000, true);
        assert_eq!(c.on_report(&report(0, 0, 0, 3)), None);
    }
}
