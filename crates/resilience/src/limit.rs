//! Bounding how many calls to a provider are in flight at once.
//!
//! The mechanism is a semaphore, and the interesting part is the three ways acquiring one can
//! end. A caller that is cancelled stops immediately; a caller whose deadline passes fails as
//! a timeout; a caller that simply waited too long fails as a *transient* failure, because a
//! provider that is saturated right now is one whose call is worth making again later.
//!
//! Waiting is bounded for the same reason every other wait in the kernel is: an unbounded
//! queue in front of a slow provider is a request that never returns and a caller that cannot
//! tell "still working" from "never going to finish".

use std::sync::Arc;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::resilience::TransientFailure;
use aik_core::clock::SharedClock;
use aik_core::{Error, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How many calls may be in flight at once.
#[derive(Debug)]
pub(crate) struct ConcurrencyLimit {
    /// `None` when the limit is disabled, which is not the same as a permit count of zero.
    semaphore: Option<Arc<Semaphore>>,
    acquire_timeout: Duration,
    clock: SharedClock,
}

impl ConcurrencyLimit {
    /// Creates a limit of `permits` concurrent calls; `0` means unlimited.
    pub(crate) fn new(permits: usize, acquire_timeout: Duration, clock: SharedClock) -> Self {
        Self {
            semaphore: (permits > 0).then(|| Arc::new(Semaphore::new(permits))),
            acquire_timeout,
            clock,
        }
    }

    /// How many calls may still start without waiting.
    pub(crate) fn available(&self) -> Option<usize> {
        self.semaphore
            .as_ref()
            .map(|semaphore| semaphore.available_permits())
    }

    /// Waits for a slot, honouring cancellation and the caller's deadline.
    ///
    /// The wait is the shorter of the configured timeout and whatever is left of the
    /// context's deadline: a caller with two seconds of budget must not spend thirty waiting
    /// for a slot it could not have used.
    pub(crate) async fn acquire(&self, cx: &ExecutionContext) -> Result<Option<Permit>> {
        let Some(semaphore) = self.semaphore.clone() else {
            return Ok(None);
        };

        // The fast path: a free slot costs no timers and no select.
        if let Ok(permit) = semaphore.clone().try_acquire_owned() {
            return Ok(Some(Permit(permit)));
        }

        let remaining = remaining_budget(cx, &self.clock);
        let wait = match remaining {
            Some(budget) if budget <= Duration::ZERO => {
                return Err(Error::Timeout(Duration::ZERO));
            }
            Some(budget) => budget.min(self.acquire_timeout),
            None => self.acquire_timeout,
        };

        tokio::select! {
            biased;
            () = cx.cancellation.cancelled() => Err(Error::Cancelled),
            permit = semaphore.acquire_owned() => match permit {
                Ok(permit) => Ok(Some(Permit(permit))),
                // Unreachable while this owns the semaphore: nothing closes it.
                Err(_) => Err(Error::other("the provider's concurrency limit was closed")),
            },
            () = tokio::time::sleep(wait) => {
                if remaining.is_some_and(|budget| budget <= wait) {
                    return Err(Error::Timeout(wait));
                }
                Err(TransientFailure::new(format!(
                    "no slot became free within {}ms",
                    wait.as_millis()
                ))
                .wrapped("waiting for a model provider slot"))
            }
        }
    }
}

/// A held slot. Dropping it releases the slot.
#[derive(Debug)]
pub(crate) struct Permit(#[allow(dead_code)] OwnedSemaphorePermit);

/// How much of a context's deadline is left, if it has one.
fn remaining_budget(cx: &ExecutionContext, clock: &SharedClock) -> Option<Duration> {
    cx.deadline
        .map(|deadline| deadline.saturating_since(clock.now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::{SystemClock, Timestamp};

    fn clock() -> SharedClock {
        Arc::new(SystemClock)
    }

    #[tokio::test]
    async fn a_disabled_limit_hands_out_nothing_and_never_waits() {
        let limit = ConcurrencyLimit::new(0, Duration::from_millis(10), clock());
        assert!(limit.available().is_none());
        for _ in 0..100 {
            assert!(
                limit
                    .acquire(&ExecutionContext::new())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn permits_are_bounded_and_returned_on_drop() {
        let limit = ConcurrencyLimit::new(2, Duration::from_millis(50), clock());
        let cx = ExecutionContext::new();

        let one = limit.acquire(&cx).await.unwrap();
        let two = limit.acquire(&cx).await.unwrap();
        assert_eq!(limit.available(), Some(0));

        let error = limit.acquire(&cx).await.unwrap_err();
        assert!(
            aik_api::resilience::transient_failure(&error).is_some(),
            "a saturated provider is worth calling again later: {error}"
        );

        drop(one);
        assert_eq!(limit.available(), Some(1));
        drop(two);
        assert_eq!(limit.available(), Some(2));
    }

    #[tokio::test]
    async fn a_waiter_is_served_when_a_slot_frees_up() {
        let limit = Arc::new(ConcurrencyLimit::new(1, Duration::from_secs(5), clock()));
        let cx = ExecutionContext::new();
        let held = limit.acquire(&cx).await.unwrap();

        let waiter = tokio::spawn({
            let limit = limit.clone();
            async move {
                limit
                    .acquire(&ExecutionContext::new())
                    .await
                    .map(|p| p.is_some())
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        assert!(waiter.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn cancellation_wins_over_a_pending_wait() {
        let limit = ConcurrencyLimit::new(1, Duration::from_secs(30), clock());
        let cx = ExecutionContext::new();
        let _held = limit.acquire(&cx).await.unwrap();

        let waiting = ExecutionContext::new();
        let token = waiting.cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });

        let error = limit.acquire(&waiting).await.unwrap_err();
        assert!(matches!(error, Error::Cancelled), "{error}");
    }

    #[tokio::test]
    async fn an_expired_deadline_costs_nothing_to_refuse() {
        let limit = ConcurrencyLimit::new(1, Duration::from_secs(30), clock());
        let cx = ExecutionContext::new();
        let _held = limit.acquire(&cx).await.unwrap();

        let expired = ExecutionContext::new().with_deadline(Timestamp::from_millis(1));
        let error = limit.acquire(&expired).await.unwrap_err();
        assert!(matches!(error, Error::Timeout(_)), "{error}");
    }

    #[tokio::test]
    async fn a_deadline_shorter_than_the_timeout_is_the_one_that_applies() {
        let limit = ConcurrencyLimit::new(1, Duration::from_secs(30), clock());
        let cx = ExecutionContext::new();
        let _held = limit.acquire(&cx).await.unwrap();

        let deadline = Timestamp::now().saturating_add(Duration::from_millis(30));
        let short = ExecutionContext::new().with_deadline(deadline);

        let started = std::time::Instant::now();
        let error = limit.acquire(&short).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited too long"
        );
        assert!(
            matches!(error, Error::Timeout(_)),
            "a caller out of budget has expired, not been rate limited: {error}"
        );
    }
}
