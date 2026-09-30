//! One monotonic microsecond clock per process.
//!
//! Every timestamp that crosses a module boundary -- `capture_ts_us` on the
//! host, `decoded_at_us` on the client -- has to be on the same epoch as the
//! timestamps it is subtracted from, or the difference is noise. A shared epoch
//! makes that automatic instead of a convention each crate has to remember.
//!
//! # This does NOT make the two machines comparable
//!
//! The epoch starts at first use, so the host's `capture_ts_us` and the
//! client's `now_us()` are offsets from two unrelated instants on two unrelated
//! machines. Subtracting one from the other measures nothing.
//!
//! Every stage the telemetry attributes (v1 design §12.1) is therefore
//! measured *within* one process. The network leg and the end-to-end figure
//! need something that sees both machines: the client's clock-offset estimate
//! from the ping exchange (`ping_core::stats`), or a camera filming both
//! screens -- the only method that can see display latency at all.

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Microseconds since this process's epoch.
///
/// Wraps every ~71 minutes. Subtract with [`u32::wrapping_sub`] and a stream
/// that outlives the wrap still attributes correctly, since no stage this is
/// used for is anywhere near that long.
pub fn now_us() -> u32 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u32
}

/// Elapsed microseconds between two [`now_us`] readings, wrap-safe.
pub fn since(earlier_us: u32) -> u32 {
    now_us().wrapping_sub(earlier_us)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_monotonically() {
        let a = now_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now_us();
        assert!(b > a, "{b} !> {a}");
        assert!(b - a >= 1_000, "expected at least 1 ms, got {}", b - a);
    }

    /// A stage measured across the 71-minute wrap must not report ~71 minutes.
    #[test]
    fn differences_survive_the_u32_wrap() {
        let before_wrap = u32::MAX - 500;
        let after_wrap = 500u32;
        assert_eq!(after_wrap.wrapping_sub(before_wrap), 1001);
    }
}
