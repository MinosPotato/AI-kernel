//! [`InMemoryAuditStore`]: the reference [`AuditStore`], gone at the next restart.
//!
//! It exists for the same reason every other subsystem here has a volatile backend: a kernel
//! assembled with `--ephemeral` must still audit, because a run that keeps nothing on disk is
//! not a run that may act unaccountably. It is also the implementation the conformance suite
//! measures the durable one against, so that "persistent" cannot come to mean "subtly
//! different".
//!
//! # Why a `Vec` and not a map
//!
//! The trail is append-only and its key is a dense, ascending sequence, so the natural
//! structure is a vector in sequence order. Retention removes a prefix — everything at or
//! before a cutoff — which is one retain, and nothing else ever removes anything.

use std::sync::Mutex;

use aik_api::audit::{AuditEntry, AuditQuery, AuditRecord, AuditStore, RetentionApplied};
use aik_api::execution::ExecutionContext;
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::{Error, Result};
use async_trait::async_trait;

use crate::retention::{AuditRetentionSweeper, survives_retention};

/// An [`AuditStore`] that keeps the trail in memory.
pub struct InMemoryAuditStore {
    /// Every record held, in sequence order.
    ///
    /// Guarded by a plain mutex rather than an async one: every operation is a short
    /// in-memory scan with no await inside it, so the lock is never held across a suspension
    /// point and an async lock would buy nothing but a dependency on being in a runtime.
    records: Mutex<Vec<AuditRecord>>,
    /// Sequence numbers already handed out, including ones retention has since removed.
    ///
    /// Held separately from the vector's length precisely because retention shortens the
    /// vector and must not renumber what is left: a sequence number is an identity, and
    /// reusing one would make two different records indistinguishable in an exported trail.
    issued: Mutex<u64>,
    /// What a retention marker is stamped with.
    clock: SharedClock,
}

impl std::fmt::Debug for InMemoryAuditStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryAuditStore")
            .field("records", &self.records.lock().map(|records| records.len()))
            .finish()
    }
}

impl Default for InMemoryAuditStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryAuditStore {
    /// Creates an empty trail.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            issued: Mutex::new(0),
            clock: std::sync::Arc::new(SystemClock),
        }
    }

    /// Overrides the clock a retention marker is stamped with. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Every record currently held, oldest first.
    ///
    /// Unfiltered and unauthorized, and therefore deliberately not part of [`AuditStore`]:
    /// it is for tests and for a caller that already owns the whole store.
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records.lock().expect("the audit trail lock").clone()
    }
}

#[async_trait]
impl AuditStore for InMemoryAuditStore {
    async fn append(&self, entry: AuditEntry) -> Result<u64> {
        // Both locks are taken in the same order everywhere in this file — `issued` then
        // `records` — so two concurrent appends cannot deadlock against each other.
        let mut issued = self.issued.lock().expect("the audit sequence lock");
        let sequence = issued
            .checked_add(1)
            .ok_or_else(|| Error::other("the audit trail has run out of sequence numbers"))?;
        self.records
            .lock()
            .expect("the audit trail lock")
            .push(AuditRecord { sequence, entry });
        *issued = sequence;
        Ok(sequence)
    }

    async fn query(&self, query: &AuditQuery, cx: &ExecutionContext) -> Result<Vec<AuditRecord>> {
        let reader = cx.principal_or_system();
        let records = self.records.lock().expect("the audit trail lock");
        Ok(records
            .iter()
            .rev()
            // Visibility first, and separately from the query: a filter narrows what a
            // reader may see and can never widen it, which is only structurally true if the
            // two are different predicates applied in this order.
            .filter(|record| record.visible_to(&reader))
            .filter(|record| query.matches(record))
            .take(query.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect())
    }

    async fn last_sequence(&self) -> Result<u64> {
        Ok(*self.issued.lock().expect("the audit sequence lock"))
    }
}

#[async_trait]
impl AuditRetentionSweeper for InMemoryAuditStore {
    async fn sweep_older_than(&self, cutoff: Timestamp) -> Result<usize> {
        let removed = {
            let mut records = self.records.lock().expect("the audit trail lock");
            let before = records.len();
            records.retain(|record| survives_retention(record, cutoff));
            before - records.len()
        };

        if removed > 0 {
            self.append(AuditEntry::Retention(RetentionApplied {
                timestamp: self.clock.now(),
                cutoff,
                removed: removed as u64,
            }))
            .await?;
        }
        Ok(removed)
    }

    async fn count_older_than(&self, cutoff: Timestamp) -> Result<usize> {
        let records = self.records.lock().expect("the audit trail lock");
        Ok(records
            .iter()
            .filter(|record| !survives_retention(record, cutoff))
            .count())
    }
}
