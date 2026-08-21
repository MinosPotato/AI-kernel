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

/// Something that can reclaim its own expired records.
///
/// Implemented by both stores, not folded into
/// [`MemoryStore`](aik_api::memory::MemoryStore) itself: sweeping is housekeeping, not
/// retrieval, and keeping it a separate trait is what lets [`spawn_expiry_task`] drive it
/// through a plain `Arc<dyn ExpirySweeper>` without the rest of the crate depending on which
/// backend it is. It is public so tests — and anything else that wants reclamation on its
/// own schedule rather than [`DEFAULT_EXPIRY_SWEEP_INTERVAL`](crate::DEFAULT_EXPIRY_SWEEP_INTERVAL) —
/// can call it directly instead of waiting for the background task.
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
/// space, and there is nothing here for a caller to react to. Cancellation is checked
/// between ticks, so shutdown does not wait out a full sleep before the task actually stops.
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
                () = tokio::time::sleep(period) => {
                    if let Err(error) = sweeper.sweep_expired(clock.now()).await {
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
}
