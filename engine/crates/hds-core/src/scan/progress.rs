//! Pacing and speed of the progress events (ALGORITHMS A8, PROTOCOL
//! `scan_progress`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Window of the speed average.
const SPEED_WINDOW: Duration = Duration::from_secs(5);

/// Shortest interval between two progress events: at most 4 per second.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Paces progress events and measures the speed of one scan run.
#[derive(Debug)]
pub(crate) struct Meter {
    started: Instant,
    interval: Duration,
    last_emit: Option<Instant>,
    /// `(time, bytes read by this run)`, oldest first.
    samples: VecDeque<(Instant, u64)>,
}

impl Meter {
    pub(crate) fn new(now: Instant, interval: Duration) -> Meter {
        let mut samples = VecDeque::new();
        samples.push_back((now, 0));
        Meter {
            started: now,
            interval: interval.max(MIN_INTERVAL),
            last_emit: None,
            samples,
        }
    }

    /// Records that this run has read `bytes` bytes so far.
    pub(crate) fn sample(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
        // Keep one sample at or before the start of the window as its base.
        while let (Some(_), Some(&(second, _))) = (self.samples.front(), self.samples.get(1)) {
            if now.saturating_duration_since(second) >= SPEED_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Whether an event may be sent now.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.last_emit
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval)
    }

    pub(crate) fn emitted(&mut self, now: Instant) {
        self.last_emit = Some(now);
    }

    /// Bytes per second over the last 5 seconds (0 before anything was read).
    pub(crate) fn speed(&self, now: Instant) -> u64 {
        let (Some(&(t0, b0)), Some(&(_, b1))) = (self.samples.front(), self.samples.back()) else {
            return 0;
        };
        let millis = now.saturating_duration_since(t0).as_millis();
        if millis == 0 {
            return 0;
        }
        let bytes = u128::from(b1.saturating_sub(b0));
        let speed = bytes.saturating_mul(1000).checked_div(millis).unwrap_or(0);
        u64::try_from(speed).unwrap_or(u64::MAX)
    }

    /// Whole seconds since the run started.
    pub(crate) fn elapsed_seconds(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_secs()
    }
}

/// Remaining bytes over speed, rounded up; `None` at speed 0.
pub(crate) fn eta_seconds(remaining: u64, speed: u64) -> Option<u64> {
    remaining.checked_div(speed).map(|whole| {
        if remaining.is_multiple_of(speed) {
            whole
        } else {
            whole.saturating_add(1)
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arithmetic_side_effects, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn speed_is_averaged_over_the_last_five_seconds() {
        let t0 = Instant::now();
        let mut meter = Meter::new(t0, MIN_INTERVAL);
        assert_eq!(meter.speed(t0), 0);
        meter.sample(t0 + Duration::from_secs(1), 100);
        assert_eq!(meter.speed(t0 + Duration::from_secs(1)), 100);
        meter.sample(t0 + Duration::from_secs(10), 1100);
        meter.sample(t0 + Duration::from_secs(12), 1300);
        // The base is the newest sample at or before t0 + 7 s: t0 + 1 s.
        assert_eq!(meter.speed(t0 + Duration::from_secs(12)), 1200 / 11);
        meter.sample(t0 + Duration::from_secs(16), 1700);
        // Now t0 + 10 s is the base.
        assert_eq!(meter.speed(t0 + Duration::from_secs(16)), 600 / 6);
    }

    #[test]
    fn events_are_paced() {
        let t0 = Instant::now();
        let mut meter = Meter::new(t0, Duration::from_millis(1));
        assert!(meter.due(t0));
        meter.emitted(t0);
        assert!(!meter.due(t0 + Duration::from_millis(249)));
        assert!(meter.due(t0 + Duration::from_millis(250)));
    }

    #[test]
    fn eta_rounds_up() {
        assert_eq!(eta_seconds(10, 0), None);
        assert_eq!(eta_seconds(10, 5), Some(2));
        assert_eq!(eta_seconds(11, 5), Some(3));
        assert_eq!(eta_seconds(0, 5), Some(0));
    }
}
