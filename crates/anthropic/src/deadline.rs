//! Turns a configured request timeout and an [`ExecutionContext`] deadline into a single
//! point in time, and races work against it.
//!
//! Deliberately a copy of the Ollama provider's module of the same name rather than a shared
//! one. Sixty lines of timeout arithmetic are not worth a crate that both providers must
//! agree on: how long a request may run, and whether a retry may extend it, is exactly the
//! kind of decision a provider should be able to change without asking another provider's
//! permission.

use std::future::Future;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_core::clock::SharedClock;
use aik_core::{Error, Result};
use tokio_util::sync::CancellationToken;

/// When a request must finish by, and how far away that was when it started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    instant: tokio::time::Instant,
    budget: Duration,
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
        let budget = deadline.saturating_since(now);
        Self {
            instant: tokio::time::Instant::now() + budget,
            budget,
        }
    }

    pub(crate) fn instant(&self) -> tokio::time::Instant {
        self.instant
    }

    /// The whole budget, for the timeout error's message.
    pub(crate) fn budget(&self) -> Duration {
        self.budget
    }
}

/// Runs `future` to completion, unless `cancellation` fires or `deadline` passes first.
///
/// Cancellation is checked ahead of the deadline (`biased`), so a request that is both
/// cancelled and overdue reports [`Error::Cancelled`] — the more specific, actionable reason.
pub(crate) async fn race<T>(
    future: impl Future<Output = Result<T>>,
    cancellation: &CancellationToken,
    deadline: Deadline,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep_until(deadline.instant) => Err(Error::Timeout(deadline.budget)),
        result = future => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::{ManualClock, Timestamp};
    use std::sync::Arc;

    fn clock() -> SharedClock {
        Arc::new(ManualClock::new(Timestamp::from_millis(0)))
    }

    #[tokio::test]
    async fn a_shorter_context_deadline_wins_over_the_default() {
        let deadline = Deadline::compute(
            &clock(),
            Duration::from_secs(10),
            &ExecutionContext::new().with_deadline(Timestamp::from_millis(1_000)),
        );
        assert_eq!(deadline.budget(), Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn a_context_deadline_never_extends_the_default() {
        let deadline = Deadline::compute(
            &clock(),
            Duration::from_secs(10),
            &ExecutionContext::new().with_deadline(Timestamp::from_millis(60_000)),
        );
        assert_eq!(deadline.budget(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn race_reports_cancellation_over_timeout() {
        let deadline =
            Deadline::compute(&clock(), Duration::from_millis(5), &ExecutionContext::new());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result: Result<()> = race(std::future::pending(), &cancellation, deadline).await;

        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    }

    #[tokio::test]
    async fn race_times_out_when_nothing_else_happens() {
        let deadline =
            Deadline::compute(&clock(), Duration::from_millis(5), &ExecutionContext::new());
        let result: Result<()> =
            race(std::future::pending(), &CancellationToken::new(), deadline).await;

        assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    }

    #[tokio::test]
    async fn race_returns_the_future_when_it_wins() {
        let deadline =
            Deadline::compute(&clock(), Duration::from_secs(10), &ExecutionContext::new());
        let result = race(async { Ok(42) }, &CancellationToken::new(), deadline).await;
        assert_eq!(result.unwrap(), 42);
    }
}
