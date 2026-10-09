//! Rumble: the console sends a pattern (motor strengths, how long, a pause,
//! how many repeats), not motor states as Pong does. This turns patterns
//! into the motor states Ping's controllers play, at the times they change.
//!
//! The platforms hold a motor state until the next one or until a second
//! of silence (`ping_core::pad::RUMBLE_TIMEOUT`), so a running state is
//! said again every [`REFRESH`].

use std::time::{Duration, Instant};

use crate::input::{Vibration, MAX_PADS};

/// A running motor state is said again this often, under the platforms'
/// one-second timeout (Pong repeats every 200 ms for the same reason).
pub const REFRESH: Duration = Duration::from_millis(200);

/// Strengths for one controller's two big motors, 0..=65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Motors {
    /// The left, low-frequency motor.
    pub low: u16,
    /// The right, high-frequency motor.
    pub high: u16,
}

impl Motors {
    /// The four motors of a report on two: the triggers' motors, which the
    /// platforms cannot drive, are folded into the right motor at half
    /// strength so a trigger effect is still felt.
    fn from_vibration(v: &Vibration) -> Motors {
        let scale = |pct: u8| (pct.min(100) as u32 * 65535 / 100) as u16;
        let triggers = v.left_trigger.max(v.right_trigger) / 2;
        Motors {
            low: scale(v.left),
            high: scale(v.right.max(triggers)),
        }
    }

    pub fn is_off(&self) -> bool {
        self.low == 0 && self.high == 0
    }
}

#[derive(Debug, Clone, Copy)]
struct Pattern {
    motors: Motors,
    start: Instant,
    on: Duration,
    period: Duration,
    /// Pulses in all (the first and its repeats).
    pulses: u32,
}

impl Pattern {
    fn motors_at(&self, now: Instant) -> Option<Motors> {
        let t = now.checked_duration_since(self.start)?;
        let period = self.period.max(Duration::from_millis(1));
        let pulse = (t.as_nanos() / period.as_nanos()) as u32;
        if pulse >= self.pulses {
            return None;
        }
        let into = t - period * pulse;
        Some(if into < self.on {
            self.motors
        } else {
            Motors::default()
        })
    }
}

/// Every controller's rumble.
#[derive(Debug, Default)]
pub struct Rumble {
    patterns: [Option<Pattern>; MAX_PADS as usize],
    /// What each controller was last told, and when.
    said: [(Motors, Option<Instant>); MAX_PADS as usize],
}

impl Rumble {
    pub fn new() -> Rumble {
        Rumble::default()
    }

    /// A vibration report arrived: it replaces whatever that controller was
    /// playing.
    pub fn start(&mut self, v: &Vibration, now: Instant) {
        let Some(slot) = self.patterns.get_mut(v.index as usize) else {
            return;
        };
        let on = Duration::from_millis(v.duration_ms as u64);
        *slot = Some(Pattern {
            motors: Motors::from_vibration(v),
            start: now,
            on,
            period: on + Duration::from_millis(v.delay_ms as u64),
            pulses: v.repeat as u32 + 1,
        });
    }

    /// The motor states to send now, as (controller, motors): changes, and
    /// running states due to be said again.
    pub fn poll(&mut self, now: Instant, mut send: impl FnMut(u8, Motors)) {
        for i in 0..MAX_PADS as usize {
            let motors = match self.patterns[i].and_then(|p| p.motors_at(now)) {
                Some(m) => m,
                None => {
                    self.patterns[i] = None;
                    Motors::default()
                }
            };
            let (last, at) = self.said[i];
            let due = at.is_none_or(|t| now.duration_since(t) >= REFRESH);
            if motors != last || (!motors.is_off() && due) {
                self.said[i] = (motors, Some(now));
                send(i as u8, motors);
            }
        }
    }

    /// When [`Rumble::poll`] next has something to say, if anything is
    /// playing.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        let playing =
            self.patterns.iter().any(Option::is_some) || self.said.iter().any(|(m, _)| !m.is_off());
        // Pulses are tens of milliseconds at the shortest: polling at the
        // refresh rate's tenth finds every edge within a few milliseconds.
        playing.then(|| now + REFRESH / 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vib(left: u8, right: u8, duration_ms: u16, delay_ms: u16, repeat: u8) -> Vibration {
        Vibration {
            index: 0,
            left,
            right,
            duration_ms,
            delay_ms,
            repeat,
            ..Default::default()
        }
    }

    fn polled(r: &mut Rumble, now: Instant) -> Vec<(u8, Motors)> {
        let mut out = Vec::new();
        r.poll(now, |i, m| out.push((i, m)));
        out
    }

    #[test]
    fn a_pulse_starts_and_stops() {
        let t0 = Instant::now();
        let mut r = Rumble::new();
        r.start(&vib(100, 50, 100, 0, 0), t0);
        let on = polled(&mut r, t0);
        assert_eq!(
            on,
            vec![(
                0,
                Motors {
                    low: 65535,
                    high: 32767
                }
            )]
        );
        assert!(polled(&mut r, t0 + Duration::from_millis(50)).is_empty());
        assert_eq!(
            polled(&mut r, t0 + Duration::from_millis(100)),
            vec![(0, Motors::default())]
        );
        assert!(polled(&mut r, t0 + Duration::from_millis(400)).is_empty());
        assert_eq!(r.next_change(t0 + Duration::from_millis(400)), None);
    }

    #[test]
    fn repeats_pause_between_pulses() {
        let t0 = Instant::now();
        let mut r = Rumble::new();
        r.start(&vib(40, 0, 50, 30, 1), t0);
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(polled(&mut r, at(0)).len(), 1);
        assert_eq!(polled(&mut r, at(60)), vec![(0, Motors::default())]);
        assert!(!polled(&mut r, at(85))[0].1.is_off(), "second pulse");
        assert!(polled(&mut r, at(135))[0].1.is_off(), "done after it");
        assert!(polled(&mut r, at(300)).is_empty());
    }

    #[test]
    fn a_long_rumble_is_said_again_before_the_platform_gives_up() {
        let t0 = Instant::now();
        let mut r = Rumble::new();
        r.start(&vib(60, 60, 3000, 0, 0), t0);
        assert_eq!(polled(&mut r, t0).len(), 1);
        assert!(polled(&mut r, t0 + REFRESH / 2).is_empty());
        assert_eq!(polled(&mut r, t0 + REFRESH).len(), 1);
    }

    #[test]
    fn trigger_motors_are_felt_in_the_right_motor() {
        let v = Vibration {
            right_trigger: 100,
            duration_ms: 100,
            ..Default::default()
        };
        let m = Motors::from_vibration(&v);
        assert_eq!(m.low, 0);
        assert_eq!(m.high, 32767);
    }

    #[test]
    fn a_report_for_a_fifth_controller_is_ignored() {
        let mut r = Rumble::new();
        let v = Vibration {
            index: 4,
            left: 100,
            duration_ms: 100,
            ..Default::default()
        };
        r.start(&v, Instant::now());
        assert!(polled(&mut r, Instant::now()).is_empty());
    }
}
