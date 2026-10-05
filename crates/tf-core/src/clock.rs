use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ids::Nanos;

/// Source of "now". Engine and strategy code must take time from a `Clock`
/// (or from event timestamps), never from `SystemTime`/`Instant` directly;
/// that is what makes live, replay and backtest runs interchangeable.
pub trait Clock: Send + Sync {
    fn now(&self) -> Nanos;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Nanos {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as Nanos)
            .unwrap_or(0)
    }
}

/// Simulated clock, advanced by the replay driver as events are delivered.
/// Never moves backwards.
#[derive(Debug, Default)]
pub struct SimClock(AtomicU64);

impl SimClock {
    pub fn new(start: Nanos) -> Self {
        SimClock(AtomicU64::new(start))
    }

    pub fn advance_to(&self, t: Nanos) {
        self.0.fetch_max(t, Ordering::Relaxed);
    }
}

impl Clock for SimClock {
    fn now(&self) -> Nanos {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_clock_is_monotonic() {
        let c = SimClock::new(100);
        c.advance_to(150);
        c.advance_to(120);
        assert_eq!(c.now(), 150);
    }

    #[test]
    fn system_clock_is_after_2025() {
        assert!(SystemClock.now() > 1_735_689_600_000_000_000);
    }
}
