//! Injectable time.
//!
//! Time is reached through [`Clock`] rather than through [`std::time::SystemTime`]
//! directly, so that scheduling, timeouts and anything else time-dependent can be driven
//! deterministically in tests. The kernel puts a clock in the context; components should
//! use it instead of reading the system clock themselves.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A wall-clock instant, in milliseconds since the Unix epoch.
///
/// Milliseconds are enough resolution for scheduling and event ordering, and the compact
/// representation serialises to a plain number, which keeps event payloads small and
/// readable when they cross a process boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The Unix epoch.
    pub const EPOCH: Self = Self(0);

    /// Reads the system clock.
    ///
    /// Prefer [`Clock::now`] inside components, so time can be controlled in tests.
    pub fn now() -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self(u64::try_from(millis).unwrap_or(u64::MAX))
    }

    /// Creates a timestamp from milliseconds since the Unix epoch.
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Returns milliseconds since the Unix epoch.
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Returns the corresponding [`SystemTime`].
    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0)
    }

    /// Returns this instant advanced by `duration`, saturating at the representable maximum.
    pub const fn saturating_add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration.as_millis() as u64))
    }

    /// Returns how long after `earlier` this instant is, or zero if it is not later.
    pub const fn saturating_since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

/// A source of wall-clock time.
pub trait Clock: Send + Sync + std::fmt::Debug + 'static {
    /// Returns the current instant.
    fn now(&self) -> Timestamp;
}

/// The real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock that only moves when told to, for tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    /// Creates a clock stopped at `start`.
    pub fn new(start: Timestamp) -> Self {
        Self {
            millis: AtomicU64::new(start.as_millis()),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, duration: Duration) {
        self.millis
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
    }

    /// Moves the clock to an absolute instant.
    pub fn set(&self, at: Timestamp) {
        self.millis.store(at.as_millis(), Ordering::Relaxed);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.millis.load(Ordering::Relaxed))
    }
}

/// A shared clock handle, as stored in the kernel context.
pub type SharedClock = Arc<dyn Clock>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_only_moves_when_told() {
        let clock = ManualClock::new(Timestamp::from_millis(1_000));
        assert_eq!(clock.now().as_millis(), 1_000);
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.now().as_millis(), 1_500);
        clock.set(Timestamp::EPOCH);
        assert_eq!(clock.now(), Timestamp::EPOCH);
    }

    #[test]
    fn timestamps_serialise_as_numbers() {
        let json = serde_json::to_string(&Timestamp::from_millis(42)).unwrap();
        assert_eq!(json, "42");
    }

    #[test]
    fn durations_between_timestamps_saturate() {
        let early = Timestamp::from_millis(100);
        let late = Timestamp::from_millis(400);
        assert_eq!(late.saturating_since(early), Duration::from_millis(300));
        assert_eq!(early.saturating_since(late), Duration::ZERO);
    }
}
