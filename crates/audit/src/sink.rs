//! The bridge: kernel events in, durable records out.
//!
//! [`ToolRegistry`](aik_api::tool::ToolRegistry) publishes
//! [`AuthorizationDecided`] and [`ToolInvoked`] on the kernel bus and knows nothing about
//! storage. This is the subscriber that gives them a life longer than the process, and it is
//! deliberately the *only* thing that writes to the trail.
//!
//! # What this is, and what it is not
//!
//! It is an asynchronous mirror of the event stream. It is **not** a synchronous audit gate:
//! a tool call is not held up until its record is on disk, and a decision whose record fails
//! to be written is not thereby reversed. Making it one would mean moving the write into
//! `ToolRegistry::invoke` — which would put a disk in the path of every authorization
//! question, and is a change to the enforcement point rather than to the audit trail. The
//! architecture here is the one the kernel already had: audit is a subscriber.
//!
//! What that costs is bounded and, more importantly, *visible*:
//!
//! * **Events the bus dropped** — a subscriber that falls behind a bounded broadcast — are
//!   recorded as an [`AuditGap`] naming how many went. The trail can be incomplete; it cannot
//!   be quietly incomplete.
//! * **Records the store refused** — a full disk, a database that will not open — are logged
//!   at `error` and counted in [`AuditSink::failures`]. They are also, unavoidably, absent
//!   from the trail: a store that cannot be written to cannot be told that it could not be
//!   written to.
//!
//! # Ordering
//!
//! One subscription per event type, selected over. Each type therefore reaches the store in
//! publication order, and two types published in the same instant may be written either way
//! round. Merging them through the firehose would fix the interleaving and cost the typed
//! payloads, which is a bad trade for a property the records already carry: see
//! [`AuditRecord`](aik_api::audit::AuditRecord) for why the timestamp and the correlation id
//! are what order the events, and the sequence number is what orders the trail.
//!
//! # Draining on shutdown
//!
//! Cancellation does not simply abandon the queue. When the component stops, the task drains
//! whatever the bus has already buffered before it exits, so events published during shutdown
//! — the last tool call of a run, most obviously — are on disk rather than lost to a race
//! between the publisher stopping and the subscriber stopping.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aik_api::audit::{AuditEntry, AuditGap, AuditStore, AuthorizationDecided, ToolInvoked};
use aik_core::clock::SharedClock;
use aik_core::event::{EventStream, RecvError};
use aik_core::task::Tasks;

/// Writes published audit events into an [`AuditStore`].
pub struct AuditSink {
    store: Arc<dyn AuditStore>,
    clock: SharedClock,
    /// Records the store refused. Monotonic, and never reset.
    failures: AtomicU64,
    /// Events the bus reports it dropped before this sink caught up.
    missed: AtomicU64,
}

impl std::fmt::Debug for AuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditSink")
            .field("failures", &self.failures())
            .field("missed", &self.missed())
            .finish()
    }
}

impl AuditSink {
    /// Creates a sink that appends to `store`, stamping gaps with `clock`.
    pub fn new(store: Arc<dyn AuditStore>, clock: SharedClock) -> Self {
        Self {
            store,
            clock,
            failures: AtomicU64::new(0),
            missed: AtomicU64::new(0),
        }
    }

    /// How many records this sink failed to write.
    ///
    /// Non-zero means the trail is incomplete in a way the trail itself cannot show, because
    /// the store that would have to hold the evidence is the thing that failed. Exposed so a
    /// test can assert on it and an operator interface can surface it.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// How many published events the bus reports this sink missed.
    ///
    /// Also written into the trail as an [`AuditGap`]; this counter is the in-process view of
    /// the same fact.
    pub fn missed(&self) -> u64 {
        self.missed.load(Ordering::Relaxed)
    }

    /// Appends one entry, reporting rather than propagating a failure.
    ///
    /// There is nothing for a caller to do with an error here — the event has already
    /// happened and the tool has already run — so the only useful responses are to say so
    /// loudly and to keep counting.
    pub async fn record(&self, entry: AuditEntry) {
        let kind = entry.kind();
        if let Err(error) = self.store.append(entry).await {
            self.failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                %error,
                kind = kind.as_str(),
                "an audit record could not be written; the trail is incomplete"
            );
        }
    }

    /// Records that `missed` events were dropped before this sink could read them.
    pub async fn record_gap(&self, missed: u64) {
        self.missed.fetch_add(missed, Ordering::Relaxed);
        tracing::error!(
            missed,
            "the audit sink fell behind the event bus; recording a gap in the trail"
        );
        self.record(AuditEntry::Gap(AuditGap {
            timestamp: self.clock.now(),
            missed,
        }))
        .await;
    }

    /// Handles one result from a subscription, whatever it turned out to be.
    ///
    /// Returns false when the bus has closed and there is nothing further to wait for.
    async fn accept<T>(
        &self,
        result: Result<aik_core::event::Envelope<T>, RecvError>,
        into: impl FnOnce(T) -> AuditEntry,
    ) -> bool {
        match result {
            Ok(envelope) => {
                self.record(into(envelope.payload)).await;
                true
            }
            Err(RecvError::Lagged { count }) => {
                self.record_gap(count).await;
                true
            }
            Err(RecvError::Closed) => false,
        }
    }

    /// Writes whatever both subscriptions have already buffered, then returns.
    ///
    /// Used on shutdown. Takes only what is there: it does not wait for more, because by this
    /// point the kernel is stopping and more may never come.
    pub(crate) async fn drain(
        &self,
        decisions: &mut EventStream<AuthorizationDecided>,
        invocations: &mut EventStream<ToolInvoked>,
    ) {
        while let Some(result) = decisions.try_recv() {
            // Stopping on a closed stream matters as much as stopping on an empty one:
            // `try_recv` reports a closed channel every time it is asked, so a loop that only
            // ended on `None` would spin at full speed for as long as the process lived.
            if !self.accept(result, AuditEntry::Authorization).await {
                break;
            }
        }
        while let Some(result) = invocations.try_recv() {
            if !self.accept(result, AuditEntry::Invocation).await {
                break;
            }
        }
    }
}

/// Runs `sink` against both subscriptions until the component's scope is cancelled.
///
/// The two streams are selected over rather than joined, so neither can starve the other, and
/// cancellation is checked in the same `select!` so a stop request is observed between events
/// rather than only when one arrives. On cancellation the queues are drained before the task
/// finishes; see [the module documentation](self#draining-on-shutdown).
pub(crate) fn spawn_audit_task(
    tasks: &Tasks,
    sink: Arc<AuditSink>,
    mut decisions: EventStream<AuthorizationDecided>,
    mut invocations: EventStream<ToolInvoked>,
) {
    tasks.spawn_cancellable("audit.sink", move |token| async move {
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                result = decisions.recv() => {
                    if !sink.accept(result, AuditEntry::Authorization).await {
                        break;
                    }
                }
                result = invocations.recv() => {
                    if !sink.accept(result, AuditEntry::Invocation).await {
                        break;
                    }
                }
            }
        }
        sink.drain(&mut decisions, &mut invocations).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::audit::{AuditEntryKind, AuditQuery, InvocationOutcome};
    use aik_api::execution::ExecutionContext;
    use aik_api::permission::{PrincipalId, PrincipalKind};
    use aik_api::tool::ToolName;
    use aik_core::clock::{SystemClock, Timestamp};
    use aik_core::{Error, Result};
    use async_trait::async_trait;

    use crate::InMemoryAuditStore;

    fn invocation() -> ToolInvoked {
        ToolInvoked {
            correlation: aik_core::id::CorrelationId::new(),
            timestamp: Timestamp::from_millis(1),
            tool: ToolName::new("demo.tool"),
            principal: PrincipalId::new("agent"),
            principal_kind: PrincipalKind::Agent,
            on_behalf_of: None,
            duration_ms: 1,
            authorization_duration_ms: None,
            execution_duration_ms: None,
            outcome: InvocationOutcome::Succeeded,
        }
    }

    /// A store that refuses everything, to prove a failing store is counted rather than
    /// panicking the task that feeds it.
    #[derive(Debug)]
    struct Refuses;

    #[async_trait]
    impl AuditStore for Refuses {
        async fn append(&self, _entry: AuditEntry) -> Result<u64> {
            Err(Error::other("the disk is full"))
        }

        async fn query(
            &self,
            _query: &AuditQuery,
            _cx: &ExecutionContext,
        ) -> Result<Vec<aik_api::audit::AuditRecord>> {
            Ok(Vec::new())
        }

        async fn last_sequence(&self) -> Result<u64> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn a_refused_append_is_counted_rather_than_propagated() {
        let sink = AuditSink::new(Arc::new(Refuses), Arc::new(SystemClock));
        sink.record(AuditEntry::Invocation(invocation())).await;
        sink.record(AuditEntry::Invocation(invocation())).await;
        assert_eq!(sink.failures(), 2);
    }

    #[tokio::test]
    async fn draining_a_closed_bus_finishes_instead_of_spinning() {
        // The regression this pins: `try_recv` reports a closed channel every time it is
        // asked, so a drain that stopped only on `None` never stopped at all — it burned a
        // core for as long as the process lived, after the kernel had already shut down.
        let bus = aik_core::event::EventBus::new(8, Arc::new(SystemClock));
        let mut decisions = bus.subscribe::<AuthorizationDecided>();
        let mut invocations = bus.subscribe::<ToolInvoked>();
        bus.publish(invocation());
        drop(bus);

        let store = Arc::new(InMemoryAuditStore::new());
        let sink = AuditSink::new(store.clone(), Arc::new(SystemClock));

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sink.drain(&mut decisions, &mut invocations),
        )
        .await
        .expect("draining a closed bus must terminate");

        assert_eq!(
            store.records().len(),
            1,
            "what was already buffered is still written before the drain gives up"
        );
    }

    #[tokio::test]
    async fn an_event_the_bus_dropped_becomes_a_gap_in_the_trail() {
        // Driven through the real bus rather than by calling `record_gap`: the path that
        // matters is the one from `RecvError::Lagged` to a stored record, and a test that
        // called the last step directly would pass even if nothing ever reached it.
        let bus = aik_core::event::EventBus::new(2, Arc::new(SystemClock));
        let mut decisions = bus.subscribe::<AuthorizationDecided>();
        let mut invocations = bus.subscribe::<ToolInvoked>();
        for _ in 0..5 {
            bus.publish(invocation());
        }

        let store = Arc::new(InMemoryAuditStore::new());
        let sink = AuditSink::new(store.clone(), Arc::new(SystemClock));
        sink.drain(&mut decisions, &mut invocations).await;

        assert_eq!(
            sink.missed(),
            3,
            "a bus of capacity 2 handed 5 events drops 3"
        );
        let records = store.records();
        let gaps: Vec<&AuditEntry> = records
            .iter()
            .map(|record| &record.entry)
            .filter(|entry| entry.kind() == AuditEntryKind::Gap)
            .collect();
        assert_eq!(gaps.len(), 1, "the loss is recorded exactly once");
        assert!(matches!(gaps[0], AuditEntry::Gap(gap) if gap.missed == 3));

        // And the events that did survive are still recorded, after the gap that precedes
        // them: a trail that dropped the survivors as well would lose more than the bus did.
        assert_eq!(records.len(), 3);
    }

    #[tokio::test]
    async fn a_gap_is_both_counted_and_written_into_the_trail() {
        let store = Arc::new(InMemoryAuditStore::new());
        let sink = AuditSink::new(store.clone(), Arc::new(SystemClock));

        sink.record_gap(9).await;

        assert_eq!(sink.missed(), 9);
        let records = store.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].entry.kind(), AuditEntryKind::Gap);
        assert!(matches!(
            &records[0].entry,
            AuditEntry::Gap(gap) if gap.missed == 9
        ));
    }
}
