//! Elapsed time and remaining-time estimate for a compression run.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Throughput is measured over this recent window, so the estimate follows
/// changes in speed (for example, a stretch of already recorded images that
/// are skipped quickly) instead of averaging the whole run.
const RATE_WINDOW: Duration = Duration::from_secs(20);
/// No estimate is offered until images have been processed for this long.
const MIN_SPAN: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    completed: usize,
}

#[derive(Debug)]
pub struct RunClock {
    started: Instant,
    stopped: Option<Instant>,
    first: Option<Sample>,
    recent: VecDeque<Sample>,
    total: usize,
}

impl RunClock {
    pub fn start(now: Instant) -> Self {
        Self {
            started: now,
            stopped: None,
            first: None,
            recent: VecDeque::new(),
            total: 0,
        }
    }

    /// Records the progress reported at `now`.
    pub fn record(&mut self, now: Instant, completed: usize, total: usize) {
        if self.stopped.is_some() {
            return;
        }
        self.total = total;
        let sample = Sample { at: now, completed };
        self.first.get_or_insert(sample);
        self.recent.push_back(sample);
        // Keep the newest sample at or beyond the window edge, so the window
        // covers the whole span instead of shrinking below it.
        while self.recent.len() > 2
            && now.saturating_duration_since(self.recent[1].at) >= RATE_WINDOW
        {
            self.recent.pop_front();
        }
    }

    /// Freezes the elapsed time; a stopped clock gives no estimate.
    pub fn stop(&mut self, now: Instant) {
        self.stopped.get_or_insert(now);
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.stopped
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }

    /// Estimated time until every image is processed, rounded up to whole
    /// seconds, or `None` while there is not enough progress to measure.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        if self.stopped.is_some() {
            return None;
        }
        let last = *self.recent.back()?;
        let left = self
            .total
            .checked_sub(last.completed)
            .filter(|left| *left > 0)?;
        // Prefer the recent rate; early in the run, or while one slow image
        // keeps the window flat, fall back to the average since the start.
        let rate = rate_between(*self.recent.front()?, last)
            .or_else(|| rate_between(self.first?, last))?;
        let estimate = Duration::try_from_secs_f64(left as f64 / rate).ok()?;
        // Count down between progress reports, but never claim to be done.
        let estimate = estimate
            .saturating_sub(now.saturating_duration_since(last.at))
            .max(Duration::from_secs(1));
        Some(Duration::from_secs(
            estimate.as_secs() + u64::from(estimate.subsec_nanos() > 0),
        ))
    }
}

/// Images per second between two samples, if they are far enough apart and
/// at least one image was completed in between.
fn rate_between(from: Sample, to: Sample) -> Option<f64> {
    let span = to.at.saturating_duration_since(from.at);
    let done = to.completed.checked_sub(from.completed)?;
    (span >= MIN_SPAN && done > 0).then(|| done as f64 / span.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::RunClock;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn no_estimate_before_enough_progress() {
        let start = Instant::now();
        let mut clock = RunClock::start(start);
        assert_eq!(clock.remaining(start), None);
        clock.record(start, 0, 100);
        clock.record(start + seconds(1), 10, 100);
        assert_eq!(clock.remaining(start + seconds(1)), None);
    }

    #[test]
    fn steady_progress_counts_down_between_reports() {
        let start = Instant::now();
        let mut clock = RunClock::start(start);
        clock.record(start, 0, 100);
        clock.record(start + seconds(10), 20, 100);
        // 2 images per second and 80 left.
        assert_eq!(clock.remaining(start + seconds(10)), Some(seconds(40)));
        assert_eq!(clock.remaining(start + seconds(15)), Some(seconds(35)));
        assert_eq!(clock.remaining(start + seconds(90)), Some(seconds(1)));
    }

    #[test]
    fn estimate_follows_the_recent_rate() {
        let start = Instant::now();
        let mut clock = RunClock::start(start);
        clock.record(start, 0, 100);
        clock.record(start + seconds(10), 50, 100);
        clock.record(start + seconds(20), 52, 100);
        clock.record(start + seconds(30), 54, 100);
        clock.record(start + seconds(40), 56, 100);
        // The last 20 seconds completed 4 images: 0.2 per second, 44 left.
        assert_eq!(clock.remaining(start + seconds(40)), Some(seconds(220)));
    }

    #[test]
    fn a_flat_window_falls_back_to_the_run_average() {
        let start = Instant::now();
        let mut clock = RunClock::start(start);
        clock.record(start, 0, 30);
        clock.record(start + seconds(10), 10, 30);
        clock.record(start + seconds(35), 10, 30);
        clock.record(start + seconds(40), 10, 30);
        // Nothing finished in the window; the average is 10 images in 40 s.
        assert_eq!(clock.remaining(start + seconds(40)), Some(seconds(80)));
    }

    #[test]
    fn a_finished_or_stopped_run_has_no_estimate() {
        let start = Instant::now();
        let mut clock = RunClock::start(start);
        clock.record(start, 0, 10);
        clock.record(start + seconds(5), 10, 10);
        assert_eq!(clock.remaining(start + seconds(5)), None);

        clock.stop(start + seconds(6));
        assert_eq!(clock.elapsed(start + seconds(60)), seconds(6));
        clock.record(start + seconds(7), 0, 10);
        assert_eq!(clock.remaining(start + seconds(7)), None);
    }
}
