//! Per-stage latency attribution (v1 design §12.1).
//!
//! Telemetry is not optional: an end-to-end latency figure without per-stage
//! attribution cannot be acted on. Report p50/p95/p99, never means alone --
//! tail latency is what is actually felt.

/// A stage's latency samples, in microseconds.
///
/// Sorting on demand is enough: this is summarised once a second, not per
/// frame, so ~60 samples are sorted per report.
pub struct Histogram {
    name: &'static str,
    /// A deque rather than a `Vec`: dropping the oldest sample happens on the
    /// capture and receive threads, and `Vec::remove(0)` would shift the whole
    /// buffer every frame once the cap is reached.
    samples: std::collections::VecDeque<u32>,
}

/// Cap on retained samples, so a caller that forgets to [`Histogram::clear`]
/// leaks nothing. At 60 fps this is ~68 seconds of frames.
const MAX_SAMPLES: usize = 4096;

impl Histogram {
    pub fn new(name: &'static str) -> Histogram {
        Histogram {
            name,
            samples: std::collections::VecDeque::new(),
        }
    }

    pub fn record(&mut self, us: u32) {
        if self.samples.len() >= MAX_SAMPLES {
            // Drop the oldest, same reasoning as every other queue here: stale
            // samples describe a situation that has already passed.
            self.samples.pop_front();
        }
        self.samples.push_back(us);
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Start a fresh window. Called once per report so each line describes the
    /// last second rather than the whole run.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// p50, p95, p99 in microseconds. All zero when empty.
    ///
    /// Nearest-rank, on a copy: `&self` keeps this callable from a reporting
    /// path that only holds a shared borrow, and sorting ~60 values once a
    /// second costs nothing.
    pub fn percentiles(&self) -> (u32, u32, u32) {
        if self.samples.is_empty() {
            return (0, 0, 0);
        }
        let mut sorted: Vec<u32> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        (
            nearest_rank(&sorted, 50.0),
            nearest_rank(&sorted, 95.0),
            nearest_rank(&sorted, 99.0),
        )
    }

    /// One line, in milliseconds, naming the stage.
    ///
    /// Deliberately no mean: an average hides exactly the tail that gets felt.
    pub fn report(&self) -> String {
        let (p50, p95, p99) = self.percentiles();
        format!(
            "{}: n={} p50={:.2} p95={:.2} p99={:.2} ms",
            self.name,
            self.samples.len(),
            p50 as f64 / 1000.0,
            p95 as f64 / 1000.0,
            p99 as f64 / 1000.0,
        )
    }
}

fn nearest_rank(sorted: &[u32], p: f64) -> u32 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_a_known_distribution() {
        let mut h = Histogram::new("test");
        for i in 1..=100u32 {
            h.record(i);
        }
        let (p50, p95, p99) = h.percentiles();
        assert_eq!(p50, 50);
        assert_eq!(p95, 95);
        assert_eq!(p99, 99);
    }

    #[test]
    fn empty_histogram_reports_zeros() {
        assert_eq!(Histogram::new("empty").percentiles(), (0, 0, 0));
    }

    #[test]
    fn report_names_the_stage() {
        let mut h = Histogram::new("encode");
        h.record(5000);
        assert!(h.report().contains("encode"));
    }

    /// A stage that is never cleared must not grow without bound -- these live
    /// for the life of the process on a thread that runs at 60 Hz.
    #[test]
    fn retains_a_bounded_window_of_the_most_recent_samples() {
        let mut h = Histogram::new("bounded");
        for i in 0..(MAX_SAMPLES as u32 + 100) {
            h.record(i);
        }
        assert_eq!(h.count(), MAX_SAMPLES);
        // The window that survived is 100..=MAX_SAMPLES+99, so its median is
        // offset by exactly the 100 samples that were dropped. Checking the
        // median rather than the max is what distinguishes "dropped the oldest"
        // from "stopped recording once full".
        assert_eq!(h.percentiles().0, 100 + MAX_SAMPLES as u32 / 2 - 1);
    }

    #[test]
    fn clearing_starts_a_fresh_window() {
        let mut h = Histogram::new("window");
        h.record(1000);
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.percentiles(), (0, 0, 0));
    }

    /// The report is what a human reads at 3am; milliseconds, not microseconds.
    #[test]
    fn report_is_in_milliseconds() {
        let mut h = Histogram::new("stage");
        h.record(2_500);
        assert!(h.report().contains("2.50"), "{}", h.report());
    }
}
