//! Retention: forgetting a conversation nobody came back to.
//!
//! Compaction bounds how large one session gets. It does nothing about how *many* sessions
//! there are, and a durable transcript store accumulates those the way any log does: a
//! session per run, kept for ever, most of them two turns long and never opened again.
//! Retention is the answer to that, and it is deliberately a different mechanism from
//! compaction because it answers a different question — not "what in this conversation is
//! still worth keeping" but "is this conversation still a conversation".
//!
//! # The rule
//!
//! A session whose [`ContextStats::updated_at`](aik_api::context::ContextStats::updated_at)
//! is at or before a cutoff is removed in full, exactly as
//! [`ContextStore::clear`](aik_api::context::ContextStore::clear) would remove it. There is
//! no partial state: the header, every record, every index entry, all of it or none of it.
//!
//! `updated_at` moves only on append, so the clock a session is measured against is "when
//! did anyone last say anything", not "when was it last read" and not "when was it last
//! compacted". Housekeeping that reset the clock would guarantee nothing ever expired.
//!
//! # Off unless asked for
//!
//! There is no default retention period, and that is the deliberate choice rather than an
//! omission. Every other bound in this crate refuses to *add* something — an append past the
//! record limit, a window over budget. Retention is the only one that destroys data a user
//! already has, and a transcript store that quietly deleted last month's conversations
//! because nobody configured it otherwise would be a data-loss bug shipped as a default.
//! So [`ContextComponent::with_retention`](crate::ContextComponent::with_retention) and its
//! persistent counterpart take the period explicitly, and a kernel that never calls them
//! keeps every session until something asks for it to be cleared.
//!
//! # Why this is not `aik-memory`'s sweeper
//!
//! The shape is the same on purpose — a cancellable task, a batched sweep, an interruptible
//! future — because the failure modes are the same and one of them has been thought about
//! twice. Three things differ, and each is a consequence of what is being reclaimed:
//!
//! * A memory record carries its own `expires_at` and expires whether or not it is swept, so
//!   `is_live` hides it immediately. A session has no expiry field; the cutoff is computed
//!   by the task from a configured period, and a stale session stays readable until it is
//!   actually removed. That is correct — a session the owner can still name is not a
//!   privacy leak, and a transcript that vanished mid-conversation because a period elapsed
//!   would be worse than one that lingers.
//! * The unit of work is a whole transcript rather than one record, so the batch is far
//!   smaller: see [`DEFAULT_RETENTION_BATCH`].
//! * A memory sweep is driven by a timestamp the caller already has. A retention sweep needs
//!   a period subtracted from now, and a period longer than the clock's own elapsed time
//!   must produce a cutoff that matches nothing rather than wrapping — see
//!   [`retention_cutoff`].

use std::sync::Arc;
use std::time::Duration;

use aik_core::Result;
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::task::Tasks;
use async_trait::async_trait;

/// How often the background retention sweep runs, when nothing configures it.
///
/// An hour, rather than the minute `aik-memory` sweeps expired records on: the thing being
/// reclaimed here is measured in days, so
/// sweeping more often only competes with the conversation somebody is having for the
/// database's single write slot. Nothing observable depends on the interval — a session
/// stays readable right up until it is removed — so the only cost of a long one is disk
/// space that was already going to be reclaimed.
pub const DEFAULT_RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The most sessions a durable sweep removes in one write transaction.
///
/// Much smaller than the memory store's record batch, because a unit here is not one row: a
/// session can hold up to
/// [`DEFAULT_MAX_RECORDS_PER_SESSION`](crate::DEFAULT_MAX_RECORDS_PER_SESSION) records plus
/// as many index entries, so a batch of sixty-four sessions is already up to some hundreds
/// of thousands of removals in one transaction. Bounding the batch bounds the same three
/// things it bounds for memory — the allocation, how long the write slot is held, and how
/// much work is in flight when the kernel is asked to stop — and the right number is
/// therefore two orders of magnitude lower.
pub const DEFAULT_RETENTION_BATCH: usize = 64;

/// Something that can reclaim its own abandoned sessions.
///
/// Implemented by both stores and kept out of
/// [`ContextStore`](aik_api::context::ContextStore) itself, exactly as
/// `aik-memory`'s `ExpirySweeper` is kept out of `MemoryStore`: sweeping is
/// unattended housekeeping, not retrieval, and a separate trait is what lets the component's
/// background task drive whichever backend the kernel published through one
/// `Arc<dyn RetentionSweeper>`.
///
/// It is public so that a test — or anything wanting reclamation on its own schedule rather
/// than [`DEFAULT_RETENTION_SWEEP_INTERVAL`] — can call it directly instead of waiting for a
/// timer.
///
/// # Obligations
///
/// * **Owner-blind.** A sweep reclaims whatever is stale, whoever owns it, and takes no
///   [`Principal`](aik_api::permission::Principal) at all. Retention is a property of the
///   session, and housekeeping that could only reclaim the sessions of whichever principal
///   happened to trigger it would leave everyone else's on disk for ever. This is the one
///   operation in the crate that is not owner-scoped, and it is safe precisely because it
///   *only* deletes: it returns a count, never a session, never an owner, and never a
///   record.
/// * **Complete.** When it returns `Ok`, every session stale at `cutoff` is gone, along with
///   every record and index entry naming it. An implementation that reclaims in batches
///   loops until there is nothing left rather than returning after the first one.
/// * **Interruptible between batches.** The returned future may be dropped, and dropping it
///   must leave the store consistent — which a per-batch transaction gives for free and a
///   single transaction over everything does not.
/// * **Restartable.** A sweep that was interrupted is completed by the next one; nothing
///   depends on any single call finishing.
#[async_trait]
pub trait RetentionSweeper: Send + Sync + 'static {
    /// Removes every session last appended to at or before `cutoff`, and returns how many
    /// were removed.
    async fn sweep_stale(&self, cutoff: Timestamp) -> Result<usize>;
}

/// The cutoff a sweep at `now` uses for a retention period of `retention`.
///
/// Saturating, and the saturation is the point: a two-week retention against a
/// [`ManualClock`](aik_core::clock::ManualClock) that has only ever reported ten seconds
/// must produce a cutoff no session can be at or before, not a wrapped one that is *after*
/// every session and reclaims the lot. Returning a zero timestamp gives exactly
/// that, since a session's `updated_at` comes from the same clock and cannot precede its
/// origin.
pub(crate) fn retention_cutoff(now: Timestamp, retention: Duration) -> Timestamp {
    let millis = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
    Timestamp::from_millis(now.as_millis().saturating_sub(millis))
}

/// Runs `sweeper` on a timer until the component's scope is cancelled.
///
/// A failed sweep is logged and retried at the next tick rather than propagated: one bad
/// tick should not stop every future one from reclaiming space, and there is nothing here
/// for a caller to react to.
///
/// Cancellation is raced against the sweep and not only against the sleep, for the reason
/// `aik-memory`'s expiry task gives at greater length: a task
/// that observes cancellation only between ticks stops promptly when it is asleep — almost
/// always — and not at all when it is mid-sweep, which is exactly the case where it has the
/// most left to do. Abandoning a sweep is safe because [`RetentionSweeper`] reclaims in
/// batches, so a dropped future leaves every completed batch committed, at most one in
/// flight, and the remainder simply due again at the next start-up.
pub(crate) fn spawn_retention_task(
    tasks: &Tasks,
    clock: SharedClock,
    sweeper: Arc<dyn RetentionSweeper>,
    period: Duration,
    retention: Duration,
) {
    tasks.spawn_cancellable("context.retention-sweep", move |token| async move {
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(period) => {}
            }

            let cutoff = retention_cutoff(clock.now(), retention);
            tokio::select! {
                () = token.cancelled() => break,
                result = sweeper.sweep_stale(cutoff) => {
                    if let Err(error) = result {
                        tracing::error!(
                            %error,
                            "context retention sweep failed; will retry next tick"
                        );
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
    fn a_cutoff_is_the_configured_period_before_now() {
        let cutoff = retention_cutoff(Timestamp::from_millis(10_000), Duration::from_secs(3));
        assert_eq!(cutoff, Timestamp::from_millis(7_000));
    }

    #[test]
    fn a_retention_longer_than_the_clock_reclaims_nothing_rather_than_everything() {
        // The failure this excludes is a wrap: a cutoff *after* every session, which would
        // make a generous retention period delete the whole store on its first tick.
        let cutoff = retention_cutoff(Timestamp::from_millis(10), Duration::from_secs(60 * 60));
        assert_eq!(cutoff, Timestamp::from_millis(0));
    }

    #[test]
    fn an_absurd_retention_period_saturates_rather_than_truncating() {
        let cutoff = retention_cutoff(Timestamp::from_millis(u64::MAX), Duration::MAX);
        assert_eq!(cutoff, Timestamp::from_millis(0));
    }

    /// A sweeper whose `sweep_stale` never returns, so the only way the task spawned around
    /// it can finish is by observing cancellation *during* a sweep rather than between ticks.
    struct NeverFinishes;

    #[async_trait]
    impl RetentionSweeper for NeverFinishes {
        async fn sweep_stale(&self, _cutoff: Timestamp) -> Result<usize> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_is_raced_against_an_in_progress_sweep_not_only_the_sleep() {
        use aik_core::clock::SystemClock;

        let tasks = Tasks::new();
        let clock: SharedClock = Arc::new(SystemClock);
        spawn_retention_task(
            &tasks,
            clock,
            Arc::new(NeverFinishes),
            Duration::from_millis(1),
            Duration::from_secs(1),
        );

        // Long enough that the task has woken from its first (1ms) sleep and is inside the
        // sweep, which then hangs for ever.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = tasks.shutdown(Duration::from_millis(500)).await;
        assert!(
            result.is_ok(),
            "shutdown timed out waiting for a sweep that should have been abandoned, not \
             awaited: {result:?}"
        );
    }
}
