//! A keyboard's auto-repeat timing, as the client sends it with a session
//! (`control::SessionStart::repeat_delay_ms`) and the host repeats held
//! keys by (`pingpong_input::repeat`).

use std::time::Duration;

/// How a held key repeats: after `delay`, every `interval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepeatRate {
    pub delay: Duration,
    pub interval: Duration,
}

impl RepeatRate {
    /// Windows' defaults, and Sunshine's delay: half a second, then about
    /// thirty a second (`SPI_GETKEYBOARDSPEED` 31).
    pub const DEFAULT: RepeatRate = RepeatRate {
        delay: Duration::from_millis(500),
        interval: Duration::from_millis(33),
    };

    /// From a client's request, in milliseconds; `None` for 0 (the client
    /// did not say) or for timings no keyboard has.
    pub fn from_millis(delay_ms: u16, interval_ms: u16) -> Option<RepeatRate> {
        if !(100..=2000).contains(&delay_ms) || !(10..=500).contains(&interval_ms) {
            return None;
        }
        Some(RepeatRate {
            delay: Duration::from_millis(delay_ms as u64),
            interval: Duration::from_millis(interval_ms as u64),
        })
    }

    /// As milliseconds, for the wire.
    pub fn to_millis(self) -> (u16, u16) {
        (
            self.delay.as_millis().min(u16::MAX as u128) as u16,
            self.interval.as_millis().min(u16::MAX as u128) as u16,
        )
    }

    /// Windows' keyboard settings (`SPI_GETKEYBOARDDELAY` 0..=3: 250 ms to
    /// 1 s; `SPI_GETKEYBOARDSPEED` 0..=31: about 2.5 to 30 repeats a second,
    /// in even steps).
    pub fn from_windows(delay: u32, speed: u32) -> RepeatRate {
        let per_second = 2.5 + speed.min(31) as f64 * (27.5 / 31.0);
        RepeatRate {
            delay: Duration::from_millis(250 * (delay.min(3) as u64 + 1)),
            interval: Duration::from_secs_f64(1.0 / per_second),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_settings_span_a_quarter_second_to_a_second_and_2_5_to_30_a_second() {
        let slowest = RepeatRate::from_windows(3, 0);
        assert_eq!(slowest.delay, Duration::from_millis(1000));
        assert_eq!(slowest.interval.as_millis(), 400);
        let fastest = RepeatRate::from_windows(0, 31);
        assert_eq!(fastest.delay, Duration::from_millis(250));
        assert_eq!(fastest.interval.as_millis(), 33);
        // Out of range is clamped, not wrapped.
        assert_eq!(
            RepeatRate::from_windows(9, 99),
            RepeatRate::from_windows(3, 31)
        );
    }

    #[test]
    fn a_client_rate_outside_any_keyboard_is_refused() {
        assert_eq!(RepeatRate::from_millis(0, 0), None);
        assert_eq!(RepeatRate::from_millis(225, 2), None);
        assert_eq!(RepeatRate::from_millis(5000, 30), None);
        let r = RepeatRate::from_millis(225, 30).unwrap();
        assert_eq!(r.to_millis(), (225, 30));
    }
}
