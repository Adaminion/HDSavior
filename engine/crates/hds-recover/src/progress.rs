//! Progress events: at most four per second, with the speed averaged over
//! the last five seconds (PROTOCOL conventions).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Shortest time between two progress events.
pub(crate) const INTERVAL: Duration = Duration::from_millis(250);

/// The speed is averaged over this window.
const WINDOW: Duration = Duration::from_secs(5);

/// Speed and elapsed time at one progress event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rates {
    /// Bytes per second over the last five seconds.
    pub speed_bps: u64,
    /// Whole seconds since the job started.
    pub elapsed_seconds: u64,
}

/// Decides when a progress event is due and what speed it shows.
#[derive(Debug, Clone)]
pub(crate) struct Meter {
    started: Instant,
    last_event: Option<Instant>,
    /// `(time, bytes done)` at recent events; the first one is at or before
    /// the start of the window.
    samples: VecDeque<(Instant, u64)>,
}

impl Meter {
    pub(crate) fn new(now: Instant) -> Meter {
        let mut samples = VecDeque::new();
        samples.push_back((now, 0));
        Meter {
            started: now,
            last_event: None,
            samples,
        }
    }

    /// A meter whose byte count starts at `bytes` (a resumed job), so that
    /// the bytes done before do not count as speed.
    pub(crate) fn starting_at(now: Instant, bytes: u64) -> Meter {
        let mut meter = Meter::new(now);
        meter.samples.clear();
        meter.samples.push_back((now, bytes));
        meter
    }

    /// Whether an event may be sent now.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.last_event
            .is_none_or(|last| now.saturating_duration_since(last) >= INTERVAL)
    }

    /// Records an event at `now` with `bytes_done` and returns its rates.
    pub(crate) fn event(&mut self, now: Instant, bytes_done: u64) -> Rates {
        self.last_event = Some(now);
        self.samples.push_back((now, bytes_done));
        // Keep one sample at or before the window start as the anchor.
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|&(t, _)| now.saturating_duration_since(t) >= WINDOW)
        {
            self.samples.pop_front();
        }
        let speed_bps = match self.samples.front() {
            Some(&(t, bytes)) => {
                let millis = now.saturating_duration_since(t).as_millis();
                let delta = u128::from(bytes_done.saturating_sub(bytes));
                delta
                    .saturating_mul(1000)
                    .checked_div(millis)
                    .map_or(0, |speed| u64::try_from(speed).unwrap_or(u64::MAX))
            }
            None => 0,
        };
        Rates {
            speed_bps,
            elapsed_seconds: now.saturating_duration_since(self.started).as_secs(),
        }
    }
}

/// Seconds until `remaining` bytes are done at `speed_bps`, rounded up;
/// `None` at speed 0.
pub(crate) fn eta_seconds(remaining: u64, speed_bps: u64) -> Option<u64> {
    if speed_bps == 0 {
        None
    } else {
        Some(remaining.div_ceil(speed_bps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_spaced_and_speed_is_a_five_second_average() {
        let t0 = Instant::now();
        let mut meter = Meter::new(t0);
        assert!(meter.due(t0));
        let first = meter.event(t0, 0);
        assert_eq!(
            first,
            Rates {
                speed_bps: 0,
                elapsed_seconds: 0
            }
        );
        assert!(!meter.due(t0 + Duration::from_millis(249)));
        assert!(meter.due(t0 + Duration::from_millis(250)));
        // 100 bytes per second for 10 seconds, then nothing.
        for s in 1..=10u64 {
            let rates = meter.event(t0 + Duration::from_secs(s), 100 * s);
            assert_eq!(rates.speed_bps, 100, "second {s}");
            assert_eq!(rates.elapsed_seconds, s);
        }
        let idle = meter.event(t0 + Duration::from_secs(13), 1000);
        assert_eq!(idle.speed_bps, 40, "200 bytes in the last 5 s");
        let still = meter.event(t0 + Duration::from_secs(20), 1000);
        assert_eq!(still.speed_bps, 0);
        assert_eq!(eta_seconds(1000, 0), None);
        assert_eq!(eta_seconds(1000, 300), Some(4));
        assert_eq!(eta_seconds(0, 300), Some(0));
    }

    #[test]
    fn a_resumed_count_is_not_speed() {
        let t0 = Instant::now();
        let mut meter = Meter::starting_at(t0, 1 << 40);
        let rates = meter.event(t0 + Duration::from_secs(2), (1 << 40) + 2000);
        assert_eq!(rates.speed_bps, 1000);
    }
}
