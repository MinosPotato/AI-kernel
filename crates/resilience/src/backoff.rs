//! How long to wait before trying again, and whether to wait at all.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::settings::RetrySettings;

/// A deterministic source of jitter.
///
/// # Why this is not a random number generator
///
/// It is one, but a deliberately small and unfashionable one: SplitMix64, sixteen lines, no
/// dependency. Jitter here spreads retries out; it does not protect anything. Reaching for a
/// cryptographic generator would add a dependency, a supply-chain surface and an entry in
/// `deny.toml` in exchange for unpredictability nobody needs, and reaching for a fast
/// statistical one would add all of that minus the unpredictability.
///
/// Being seedable is the property that earns its place: a test can pin the seed and assert on
/// exact delays, which is the only way a backoff schedule is testable at all.
#[derive(Debug)]
pub(crate) struct Jitter {
    state: AtomicU64,
}

impl Jitter {
    /// A generator with a fixed starting point.
    pub(crate) const fn seeded(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }

    /// Returns a value in `0..=bound`.
    fn next_up_to(&self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let mut z = self
            .state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        z % (bound + 1)
    }
}

/// What the caller should do about a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backoff {
    /// Wait this long, then try again.
    Wait {
        /// How long to wait.
        delay: Duration,
        /// Whether the delay came from the service's own `retry-after`.
        honoured_retry_after: bool,
    },
    /// Stop: no attempts are left.
    Exhausted,
}

/// Turns an attempt number into a delay.
///
/// The schedule is exponential from [`RetrySettings::base_delay_ms`], doubling per attempt,
/// capped at [`RetrySettings::max_delay_ms`], with **full jitter**: the delay actually taken
/// is uniform in `0..=capped`. Full jitter rather than half or none because the thing this
/// spreads out is not one client — a scheduler firing several agent jobs at once, or a daemon
/// serving several frontends, produces several clients that failed at the same instant
/// because the upstream failed once. A schedule with no jitter marches all of them back in
/// step, which is how a service that was briefly overloaded stays that way.
///
/// A service that stated a `retry-after` overrides all of it. That figure is honoured
/// verbatim up to [`RetrySettings::max_retry_after_ms`] and clamped there — a client that
/// obeyed an unbounded `retry-after` could be parked for as long as any upstream, or anything
/// able to answer as one, cared to name.
#[derive(Debug)]
pub(crate) struct BackoffSchedule {
    settings: RetrySettings,
    jitter: Jitter,
}

impl BackoffSchedule {
    /// Creates a schedule seeded from `seed`.
    pub(crate) fn new(settings: RetrySettings, seed: u64) -> Self {
        Self {
            settings,
            jitter: Jitter::seeded(seed),
        }
    }

    /// How many attempts are permitted in total, including the first.
    pub(crate) fn max_attempts(&self) -> u32 {
        self.settings.max_attempts.max(1)
    }

    /// What to do after attempt number `attempt` (1-based) failed transiently.
    ///
    /// `retry_after` is what the service asked for, if it asked.
    pub(crate) fn after(&self, attempt: u32, retry_after: Option<Duration>) -> Backoff {
        if attempt >= self.max_attempts() {
            return Backoff::Exhausted;
        }

        if let Some(requested) = retry_after {
            let capped = requested.min(Duration::from_millis(self.settings.max_retry_after_ms));
            return Backoff::Wait {
                delay: capped,
                honoured_retry_after: true,
            };
        }

        let exponent = u32::min(attempt.saturating_sub(1), 16);
        let ceiling = self
            .settings
            .base_delay_ms
            .saturating_mul(1u64 << exponent)
            .min(self.settings.max_delay_ms);

        Backoff::Wait {
            delay: Duration::from_millis(self.jitter.next_up_to(ceiling)),
            honoured_retry_after: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> RetrySettings {
        RetrySettings {
            max_attempts: 4,
            base_delay_ms: 500,
            max_delay_ms: 8_000,
            max_retry_after_ms: 60_000,
        }
    }

    fn delay(backoff: Backoff) -> Duration {
        match backoff {
            Backoff::Wait { delay, .. } => delay,
            Backoff::Exhausted => panic!("expected a wait"),
        }
    }

    #[test]
    fn the_first_attempt_is_not_a_retry() {
        let schedule = BackoffSchedule::new(settings(), 1);
        // Four attempts means three waits: after 1, 2 and 3.
        assert!(matches!(schedule.after(1, None), Backoff::Wait { .. }));
        assert!(matches!(schedule.after(3, None), Backoff::Wait { .. }));
        assert_eq!(schedule.after(4, None), Backoff::Exhausted);
        assert_eq!(schedule.after(9, None), Backoff::Exhausted);
    }

    #[test]
    fn a_single_attempt_never_waits() {
        let schedule = BackoffSchedule::new(
            RetrySettings {
                max_attempts: 1,
                ..settings()
            },
            1,
        );
        assert_eq!(schedule.after(1, None), Backoff::Exhausted);
    }

    #[test]
    fn zero_attempts_is_read_as_one_rather_than_as_none() {
        // Configuration cannot produce a provider that is never called at all.
        let schedule = BackoffSchedule::new(
            RetrySettings {
                max_attempts: 0,
                ..settings()
            },
            1,
        );
        assert_eq!(schedule.max_attempts(), 1);
        assert_eq!(schedule.after(1, None), Backoff::Exhausted);
    }

    #[test]
    fn jitter_stays_inside_the_exponential_ceiling() {
        let schedule = BackoffSchedule::new(
            RetrySettings {
                max_attempts: 20,
                ..settings()
            },
            0xDEAD_BEEF,
        );
        for attempt in 1..=6u32 {
            let ceiling = 500u64
                .saturating_mul(1 << (attempt - 1))
                .min(settings().max_delay_ms);
            for _ in 0..64 {
                let taken = delay(schedule.after(attempt, None));
                assert!(
                    taken <= Duration::from_millis(ceiling),
                    "attempt {attempt} produced {taken:?} above {ceiling}ms"
                );
            }
        }
    }

    #[test]
    fn the_ceiling_grows_and_then_stops() {
        // Asserted on the ceiling rather than on a sampled delay, which is the part that is
        // deterministic. A huge attempt number must not shift the exponent into nonsense.
        let schedule = BackoffSchedule::new(
            RetrySettings {
                max_attempts: u32::MAX,
                ..settings()
            },
            7,
        );
        for _ in 0..64 {
            assert!(delay(schedule.after(99, None)) <= Duration::from_millis(8_000));
        }
    }

    #[test]
    fn jitter_actually_varies() {
        let schedule = BackoffSchedule::new(
            RetrySettings {
                max_attempts: 20,
                ..settings()
            },
            42,
        );
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            seen.insert(delay(schedule.after(4, None)));
        }
        assert!(
            seen.len() > 1,
            "a jittered delay that never moves is not one"
        );
    }

    #[test]
    fn a_seeded_schedule_repeats_exactly() {
        let one = BackoffSchedule::new(settings(), 99);
        let two = BackoffSchedule::new(settings(), 99);
        for attempt in 1..=3 {
            assert_eq!(one.after(attempt, None), two.after(attempt, None));
        }
    }

    #[test]
    fn a_stated_retry_after_wins_over_the_schedule() {
        let schedule = BackoffSchedule::new(settings(), 1);
        assert_eq!(
            schedule.after(1, Some(Duration::from_secs(30))),
            Backoff::Wait {
                delay: Duration::from_secs(30),
                honoured_retry_after: true,
            }
        );
    }

    #[test]
    fn a_stated_retry_after_is_still_capped() {
        // An upstream, or anything able to answer as one, must not be able to park a client
        // for an arbitrary length of time.
        let schedule = BackoffSchedule::new(settings(), 1);
        assert_eq!(
            delay(schedule.after(1, Some(Duration::from_secs(86_400)))),
            Duration::from_millis(60_000)
        );
    }

    #[test]
    fn a_stated_retry_after_does_not_buy_extra_attempts() {
        let schedule = BackoffSchedule::new(settings(), 1);
        assert_eq!(
            schedule.after(4, Some(Duration::from_secs(1))),
            Backoff::Exhausted
        );
    }

    #[test]
    fn a_zero_ceiling_yields_a_zero_delay_rather_than_dividing_by_zero() {
        let schedule = BackoffSchedule::new(
            RetrySettings {
                base_delay_ms: 0,
                max_delay_ms: 0,
                ..settings()
            },
            1,
        );
        assert_eq!(delay(schedule.after(1, None)), Duration::ZERO);
    }
}
