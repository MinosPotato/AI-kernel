//! Turns a configured request timeout and an [`ExecutionContext`] deadline into a single
//! point in time, and races work against it.

use std::future::Future;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_core::clock::SharedClock;
use aik_core::{Error, Result};
use tokio_util::sync::CancellationToken;

/// When a request must finish by, and how far away that is right now.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    instant: tokio::time::Instant,
    remaining: Duration,
}

impl Deadline {
    /// Computes the earlier of the provider's configured timeout and the caller's own
    /// deadline, both measured from the injected clock so tests can control time.
    pub(crate) fn compute(
        clock: &SharedClock,
        default_timeout: Duration,
        cx: &ExecutionContext,
    ) -> Self {
        let now = clock.now();
        let default_deadline = now.saturating_add(default_timeout);
        let deadline = match cx.deadline {
            Some(requested) if requested < default_deadline => requested,
            _ => default_deadline,
        };
        let remaining = deadline.saturating_since(now);
        Self {
            instant: tokio::time::Instant::now() + remaining,
            remaining,
        }
    }

    pub(crate) fn instant(&self) -> tokio::time::Instant {
        self.instant
    }
}

/// Runs `future` to completion, unless `cancellation` fires or `deadline` passes first.
///
/// Cancellation is checked ahead of the deadline (`biased`), so a request that is both
/// cancelled and overdue reports [`Error::Cancelled`] — the more specific, actionable
/// reason.
pub(crate) async fn race<T>(
    future: impl Future<Output = Result<T>>,
    cancellation: &CancellationToken,
    deadline: Deadline,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep_until(deadline.instant) => Err(Error::Timeout(deadline.remaining)),
        result = future => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::{ManualClock, Timestamp};
    use std::sync::Arc;

    #[tokio::test]
    async fn a_shorter_context_deadline_wins_over_the_default() {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));

        // No context deadline: the configured default applies.
        let default_only =
            Deadline::compute(&clock, Duration::from_secs(10), &ExecutionContext::new());
        assert_eq!(default_only.remaining, Duration::from_secs(10));

        // A context deadline shorter than the default wins.
        let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(1_000));
        let shorter = Deadline::compute(&clock, Duration::from_secs(10), &cx);
        assert_eq!(shorter.remaining, Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn a_context_deadline_never_extends_the_default() {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(60_000));
        let deadline = Deadline::compute(&clock, Duration::from_secs(10), &cx);
        assert_eq!(deadline.remaining, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn race_reports_cancellation_over_timeout() {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let deadline =
            Deadline::compute(&clock, Duration::from_millis(5), &ExecutionContext::new());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result: Result<()> = race(std::future::pending(), &cancellation, deadline).await;

        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    }

    #[tokio::test]
    async fn race_times_out_when_nothing_else_happens() {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let deadline =
            Deadline::compute(&clock, Duration::from_millis(5), &ExecutionContext::new());
        let cancellation = CancellationToken::new();

        let result: Result<()> = race(std::future::pending(), &cancellation, deadline).await;

        assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    }

    #[tokio::test]
    async fn race_returns_the_future_when_it_wins() {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let deadline = Deadline::compute(&clock, Duration::from_secs(10), &ExecutionContext::new());
        let cancellation = CancellationToken::new();

        let result = race(async { Ok(42) }, &cancellation, deadline).await;

        assert_eq!(result.unwrap(), 42);
    }
}
