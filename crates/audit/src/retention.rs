//! Retention: bounding a trail that otherwise grows for ever.
//!
//! An audit trail is the one collection in this system that is written to on every single
//! authorization question — several per tool call — and never read in the normal course of
//! things. Left alone it grows without limit, and "the disk filled up" is a poor way for an
//! audited system to stop working.
//!
//! # The rule
//!
//! A record whose timestamp is at or before a cutoff is removed, with two exceptions and one
//! obligation:
//!
//! * A [gap](aik_api::audit::AuditGap) is never removed. It is the evidence that the trail is
//!   already incomplete, and sweeping it would erase precisely what makes the remaining trail
//!   honest.
//! * A [retention marker](aik_api::audit::RetentionApplied) is never removed, for the same
//!   reason: it is the record of a previous truncation.
//! * A sweep that removed anything **writes a marker saying so** — when it ran, what cutoff
//!   it applied, how many records went. An append-only log that can be silently shortened is
//!   not append-only; one that records its own shortening is.
//!
//! Both exceptions are cheap: a gap is written only when the bus drops events, and a marker
//! only when a sweep removed something, so neither accumulates in proportion to traffic.
//!
//! # Off unless asked for
//!
//! There is no default retention period, and that is deliberate, exactly as it is for the
//! transcript store. Retention here destroys the record of what a system was allowed to do —
//! the thing an operator is least able to reconstruct and most likely to need long after the
//! fact. A default that quietly discarded last month's authority decisions would be a
//! compliance bug shipped as a convenience, so a period is configured explicitly or nothing
//! is ever removed.
//!
//! # Owner-blind, and why that is safe
//!
//! A sweep takes no [`Principal`](aik_api::permission::Principal) and reclaims whatever is
//! stale, whoever it concerns — the same choice `aik-memory` and `aik-context` make, for the
//! same reason: housekeeping that could only reclaim the records of whichever principal
//! happened to trigger it would leave everyone else's on disk for ever. It is safe because
//! the operation only ever *deletes*: it returns a count, never a record, never a principal,
//! never a resource. It is not reachable from a tool, so no model can call it.

use std::sync::Arc;
use std::time::Duration;

use aik_api::audit::AuditRecord;
use aik_core::Result;
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::task::Tasks;
use async_trait::async_trait;

/// How often the background retention sweep runs, when nothing configures it.
///
/// An hour, matching the transcript store's: what is being reclaimed is measured in days, so
/// sweeping more often only competes with real traffic for the database's single write slot,
/// and nothing observable depends on the interval — a record stays readable right up until it
/// is removed.
pub const DEFAULT_RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The most records a durable sweep removes in one write transaction.
///
/// One record here is one row plus its three index entries, so a batch of a thousand is four
/// thousand removals in one transaction. That bounds the same three things it bounds
/// everywhere else in this workspace: the allocation, how long the write slot is held, and
/// how much work is in flight when the kernel is asked to stop.
pub const DEFAULT_RETENTION_BATCH: usize = 1_024;

/// Something that can reclaim its own stale audit records.
///
/// Kept out of [`AuditStore`](aik_api::audit::AuditStore) on purpose, and it is the single
/// most important separation in this crate: `AuditStore` is append-only, so holding an
/// `Arc<dyn AuditStore>` gives no way whatever to remove a record. Reclamation is a second,
/// narrower capability, held only by the component that runs the sweep and by the operator
/// command that prunes explicitly.
///
/// # Obligations
///
/// * **Bounded by the cutoff.** Only records at or before `cutoff` are removed. A record
///   [about the trail itself](aik_api::audit::AuditEntryKind::is_about_the_trail) is never
///   removed at all.
/// * **Self-recording.** A sweep that removed anything appends a
///   [`RetentionApplied`](aik_api::audit::RetentionApplied) record before it returns.
/// * **Complete.** When it returns `Ok`, every sweepable record at or before `cutoff` is
///   gone. An implementation that reclaims in batches loops until there is nothing left.
/// * **Interruptible between batches.** The returned future may be dropped, and dropping it
///   must leave the store consistent — which a per-batch transaction gives and a single
///   transaction over everything does not.
/// * **Restartable.** A sweep that was interrupted is finished by the next one.
#[async_trait]
pub trait AuditRetentionSweeper: Send + Sync + 'static {
    /// Removes every sweepable record at or before `cutoff`, and returns how many went.
    async fn sweep_older_than(&self, cutoff: Timestamp) -> Result<usize>;

    /// How many records [`sweep_older_than`](Self::sweep_older_than) would remove at
    /// `cutoff`, without removing any of them.
    ///
    /// What makes a destructive operation previewable, which for this particular collection
    /// is worth a method of its own: an operator who mistypes a retention period should find
    /// out from a number rather than from a trail that is no longer there.
    ///
    /// Owner-blind and unauthorized on exactly the same terms as the sweep, and safe for the
    /// same reason: it returns a count and nothing else — never a record, never a principal,
    /// never a resource.
    async fn count_older_than(&self, cutoff: Timestamp) -> Result<usize>;
}

/// Whether `record` survives a sweep at `cutoff`.
///
/// One function, used by both backends, so that "what retention spares" cannot come to mean
/// two things.
pub(crate) fn survives_retention(record: &AuditRecord, cutoff: Timestamp) -> bool {
    record.entry.timestamp() > cutoff || record.entry.kind().is_about_the_trail()
}

/// The cutoff a sweep at `now` uses for a retention period of `retention`.
///
/// Saturating, and the saturation is the point: a ninety-day retention against a
/// [`ManualClock`](aik_core::clock::ManualClock) that has only ever reported ten seconds must
/// produce a cutoff no record is at or before, not a wrapped one that is *after* every record
/// and reclaims the entire trail on the first tick.
pub(crate) fn retention_cutoff(now: Timestamp, retention: Duration) -> Timestamp {
    let millis = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
    Timestamp::from_millis(now.as_millis().saturating_sub(millis))
}

/// Runs `sweeper` on a timer until the component's scope is cancelled.
///
/// A failed sweep is logged and retried at the next tick rather than propagated: one bad tick
/// must not stop every future one from reclaiming space, and there is nothing here for a
/// caller to react to.
///
/// Cancellation is raced against the sweep itself and not only against the sleep. A task that
/// observes cancellation only between ticks stops promptly when it is asleep — almost always
/// — and not at all when it is mid-sweep, which is exactly the case where it has the most
/// left to do. Abandoning a sweep is safe because [`AuditRetentionSweeper`] reclaims in
/// batches: a dropped future leaves every completed batch committed, at most one in flight,
/// and the remainder simply due again at the next start-up.
pub(crate) fn spawn_retention_task(
    tasks: &Tasks,
    clock: SharedClock,
    sweeper: Arc<dyn AuditRetentionSweeper>,
    period: Duration,
    retention: Duration,
) {
    tasks.spawn_cancellable("audit.retention-sweep", move |token| async move {
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(period) => {}
            }

            let cutoff = retention_cutoff(clock.now(), retention);
            tokio::select! {
                () = token.cancelled() => break,
                result = sweeper.sweep_older_than(cutoff) => {
                    match result {
                        Ok(0) => {}
                        Ok(removed) => tracing::info!(
                            removed,
                            cutoff = cutoff.as_millis(),
                            "audit retention removed records"
                        ),
                        Err(error) => tracing::error!(
                            %error,
                            "audit retention sweep failed; will retry next tick"
                        ),
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::audit::{AuditEntry, AuditGap, AuditRecord, RetentionApplied};
    use aik_core::clock::SystemClock;

    fn gap(at: u64) -> AuditRecord {
        AuditRecord {
            sequence: 1,
            entry: AuditEntry::Gap(AuditGap {
                timestamp: Timestamp::from_millis(at),
                missed: 1,
            }),
        }
    }

    fn marker(at: u64) -> AuditRecord {
        AuditRecord {
            sequence: 2,
            entry: AuditEntry::Retention(RetentionApplied {
                timestamp: Timestamp::from_millis(at),
                cutoff: Timestamp::from_millis(at),
                removed: 1,
            }),
        }
    }

    #[test]
    fn a_cutoff_is_the_configured_period_before_now() {
        let cutoff = retention_cutoff(Timestamp::from_millis(10_000), Duration::from_secs(3));
        assert_eq!(cutoff, Timestamp::from_millis(7_000));
    }

    #[test]
    fn a_retention_longer_than_the_clock_reclaims_nothing_rather_than_everything() {
        let cutoff = retention_cutoff(Timestamp::from_millis(10), Duration::from_secs(60 * 60));
        assert_eq!(cutoff, Timestamp::from_millis(0));
    }

    #[test]
    fn an_absurd_retention_period_saturates_rather_than_truncating() {
        let cutoff = retention_cutoff(Timestamp::from_millis(u64::MAX), Duration::MAX);
        assert_eq!(cutoff, Timestamp::from_millis(0));
    }

    #[test]
    fn a_record_about_the_trail_survives_any_cutoff() {
        let cutoff = Timestamp::from_millis(u64::MAX);
        assert!(survives_retention(&gap(1), cutoff));
        assert!(survives_retention(&marker(1), cutoff));
    }

    /// A sweeper whose sweep never returns, so the only way the task spawned around it can
    /// finish is by observing cancellation *during* a sweep rather than between ticks.
    struct NeverFinishes;

    #[async_trait]
    impl AuditRetentionSweeper for NeverFinishes {
        async fn sweep_older_than(&self, _cutoff: Timestamp) -> Result<usize> {
            std::future::pending().await
        }

        async fn count_older_than(&self, _cutoff: Timestamp) -> Result<usize> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn cancellation_is_raced_against_an_in_progress_sweep_not_only_the_sleep() {
        let tasks = Tasks::new();
        spawn_retention_task(
            &tasks,
            Arc::new(SystemClock),
            Arc::new(NeverFinishes),
            Duration::from_millis(1),
            Duration::from_secs(1),
        );

        // Long enough that the task has woken from its first sleep and is inside the sweep,
        // which then hangs for ever.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = tasks.shutdown(Duration::from_millis(500)).await;
        assert!(
            result.is_ok(),
            "shutdown timed out waiting for a sweep that should have been abandoned: {result:?}"
        );
    }
}
