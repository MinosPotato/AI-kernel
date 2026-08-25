//! Expiration: the one rule both stores apply identically.
//!
//! A record with `expires_at` in the past is not deleted the instant it expires — nothing
//! is watching the clock that closely — but it must stop being visible immediately, and it
//! must eventually stop taking up space. Those are two different guarantees:
//!
//! * **Visibility** is [`is_live`], checked against the caller's clock on every
//!   [`MemoryStore::query`](aik_api::memory::MemoryStore::query). An expired record simply
//!   is not in the results, however recently the sweep last ran.
//! * **Reclamation** is the periodic sweep [`spawn_expiry_task`] drives, calling into
//!   whichever store implements [`ExpirySweeper`]. It is what actually removes the record
//!   and both of its indexes; without it, an expired record would sit in the database
//!   forever, merely invisible.
//!
//! [`MemoryStore::get`](aik_api::memory::MemoryStore::get) and
//! [`MemoryStore::delete`](aik_api::memory::MemoryStore::delete) deliberately do not apply
//! `is_live`: they address a record by id, not by relevance, and a caller that already holds
//! an id is not the audience the visibility rule protects. Until the sweep reaches it, an
//! expired record can still be fetched or deleted by id — the same way a file can still be
//! `unlink`ed after its retention policy says it should be gone.

use std::sync::Arc;
use std::time::Duration;

use aik_core::Result;
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::task::Tasks;
use async_trait::async_trait;

/// Whether a record with this expiry is still visible at `now`.
///
/// A record with no expiry is always live. One that expires exactly at `now` is treated as
/// already gone, not as gone-next-instant: `query` and a wall clock that reads the same
/// millisecond should never disagree about it.
pub(crate) fn is_live(expires_at: Option<Timestamp>, now: Timestamp) -> bool {
    match expires_at {
        Some(expires_at) => expires_at > now,
        None => true,
    }
}

/// The most records a durable sweep reclaims in one write transaction.
///
/// A sweep is unattended housekeeping that competes for the database's single write slot
/// with work someone is waiting for, so it is deliberately not one transaction however much
/// has expired. Batching bounds three things at once: the memory one sweep allocates, how
/// long it holds the write slot, and how much work is still in flight when the kernel is
/// asked to stop.
///
/// Large enough that a routine sweep is a single batch; small enough that a backlog of
/// millions does not become one transaction, one allocation, or one shutdown that misses its
/// deadline.
pub const DEFAULT_SWEEP_BATCH: usize = 1_024;

/// Something that can reclaim its own expired records.
///
/// Implemented by both stores, not folded into
/// [`MemoryStore`](aik_api::memory::MemoryStore) itself: sweeping is housekeeping, not
/// retrieval, and keeping it a separate trait is what lets the background sweep drive it
/// through a plain `Arc<dyn ExpirySweeper>` without the rest of the crate depending on which
/// backend it is. It is public so tests — and anything else that wants reclamation on its
/// own schedule rather than [`DEFAULT_EXPIRY_SWEEP_INTERVAL`](crate::DEFAULT_EXPIRY_SWEEP_INTERVAL) —
/// can call it directly instead of waiting for the background task.
///
/// # Obligations
///
/// * **Complete.** When it returns `Ok`, every record due at `now` has been removed, along
///   with every index entry naming it. An implementation that reclaims in batches loops until
///   there is nothing left rather than returning after the first one.
/// * **Interruptible between batches.** The returned future may be dropped, and dropping it
///   must leave the store consistent — which a per-batch transaction gives for free and a
///   single transaction over everything does not.
/// * **Restartable.** A sweep that was interrupted is completed by the next one; nothing
///   depends on any single call finishing.
#[async_trait]
pub trait ExpirySweeper: Send + Sync + 'static {
    /// Removes every record whose expiry is at or before `now`, along with its indexes.
    /// Returns how many were removed.
    async fn sweep_expired(&self, now: Timestamp) -> Result<usize>;
}

/// Runs `sweeper` on a timer until the component's scope is cancelled.
///
/// A failed sweep is logged and retried at the next tick rather than propagated: one bad
/// tick — a transient I/O error, say — should not stop every future one from reclaiming
/// space, and there is nothing here for a caller to react to.
///
/// # Why cancellation is raced against the sweep and not only the sleep
///
/// Shutdown gives every background task one shared deadline
/// ([`Kernel::shutdown`](aik_core::Kernel::shutdown)). A task that observes cancellation only
/// between ticks stops promptly when it is asleep — which is almost always — and not at all
/// when it is mid-sweep, which is exactly the case where it has the most left to do. So both
/// halves of the loop are racing the token, and a sweep in progress is abandoned rather than
/// waited out.
///
/// Abandoning it is safe because [`ExpirySweeper`] reclaims in batches: dropping the future
/// leaves every completed batch committed and at most one in flight, and whatever was missed
/// is simply due again at the next start-up. The alternative — one transaction covering an
/// arbitrary backlog — could not be abandoned at all, and would put an unbounded operation
/// inside a bounded shutdown.
pub(crate) fn spawn_expiry_task(
    tasks: &Tasks,
    clock: SharedClock,
    sweeper: Arc<dyn ExpirySweeper>,
    period: Duration,
) {
    tasks.spawn_cancellable("memory.expiry-sweep", move |token| async move {
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(period) => {}
            }

            tokio::select! {
                () = token.cancelled() => break,
                result = sweeper.sweep_expired(clock.now()) => {
                    if let Err(error) = result {
                        tracing::error!(%error, "memory expiry sweep failed; will retry next tick");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_expiry_is_always_live() {
        assert!(is_live(None, Timestamp::from_millis(u64::MAX)));
    }

    #[test]
    fn expiry_in_the_future_is_live() {
        let expires_at = Timestamp::from_millis(2_000);
        assert!(is_live(Some(expires_at), Timestamp::from_millis(1_000)));
    }

    #[test]
    fn expiry_at_or_before_now_is_not_live() {
        let expires_at = Timestamp::from_millis(1_000);
        assert!(!is_live(Some(expires_at), Timestamp::from_millis(1_000)));
        assert!(!is_live(Some(expires_at), Timestamp::from_millis(1_001)));
    }

    /// A sweeper whose `sweep_expired` never returns on its own, so the only way the task
    /// spawned around it can ever finish is by observing cancellation *during* the sweep —
    /// not merely between ticks. `shutdown_stops_the_sweep_rather_than_waiting_out_its_interval`
    /// in `aik-memory`'s `end_to_end.rs` proves the sleep half of the race; this proves the
    /// other half, which that test's long interval never actually exercises (it cancels while
    /// the loop is asleep, not while a sweep is in progress).
    struct NeverFinishes;

    #[async_trait]
    impl ExpirySweeper for NeverFinishes {
        async fn sweep_expired(&self, _now: Timestamp) -> Result<usize> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_is_raced_against_an_in_progress_sweep_not_only_the_sleep() {
        use aik_core::clock::SystemClock;

        let tasks = Tasks::new();
        let clock: SharedClock = Arc::new(SystemClock);
        spawn_expiry_task(
            &tasks,
            clock,
            Arc::new(NeverFinishes),
            Duration::from_millis(1),
        );

        // Long enough that the task has woken from its first (1ms) sleep and is inside the
        // sweep, which then hangs for ever.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // If cancellation were checked only between ticks, this would time out: the fake
        // sweep never returns on its own, so the task would still be awaiting it.
        let result = tasks.shutdown(Duration::from_millis(500)).await;
        assert!(
            result.is_ok(),
            "shutdown timed out waiting for a sweep that should have been abandoned, not \
             awaited: {result:?}"
        );
    }
}
