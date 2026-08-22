//! The component as a kernel actually runs it: events published on the bus, records on disk.
//!
//! `behavior.rs` proves the store keeps what it is handed. This proves the *sink* hands it
//! everything — including events published while the kernel is shutting down, which is the
//! case a naive subscriber loses.

use std::sync::Arc;
use std::time::Duration;

use aik_api::audit::{
    AuditEntryKind, AuditQuery, AuditStore, AuthorizationDecided, AuthorizationOutcome,
    AuthorizationPhase, InvocationOutcome, ToolInvoked,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, Principal, PrincipalId, PrincipalKind, ResourceId};
use aik_api::tool::ToolName;
use aik_audit::{AuditComponent, RedbAuditComponent};
use aik_core::ErrorKind;
use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use aik_core::prelude::*;
use aik_store::StoreComponent;
use serde_json::json;

/// How long a test waits for the sink to catch up before failing rather than hanging.
const PATIENCE: Duration = Duration::from_secs(10);

fn decision(principal: &str, tool: &str) -> AuthorizationDecided {
    AuthorizationDecided {
        correlation: CorrelationId::new(),
        timestamp: Timestamp::from_millis(10),
        tool: ToolName::new(tool),
        principal: PrincipalId::new(principal),
        principal_kind: PrincipalKind::Agent,
        on_behalf_of: Some(PrincipalId::new("alice")),
        action: ActionId::new("fs.read"),
        resource: Some(ResourceId::new("/tmp/notes.txt")),
        phase: AuthorizationPhase::Resource,
        duration_ms: 1,
        approval_wait_ms: None,
        outcome: AuthorizationOutcome::Allowed,
    }
}

fn invocation(principal: &str, tool: &str) -> ToolInvoked {
    ToolInvoked {
        correlation: CorrelationId::new(),
        timestamp: Timestamp::from_millis(11),
        tool: ToolName::new(tool),
        principal: PrincipalId::new(principal),
        principal_kind: PrincipalKind::Agent,
        on_behalf_of: Some(PrincipalId::new("alice")),
        duration_ms: 2,
        authorization_duration_ms: Some(1),
        execution_duration_ms: Some(1),
        outcome: InvocationOutcome::Succeeded,
    }
}

/// A context reading as the human the agent acted for.
fn alice() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
}

/// Waits until the trail holds at least `count` records, or gives up.
async fn wait_for(store: &Arc<dyn AuditStore>, count: usize) -> Vec<aik_api::audit::AuditRecord> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let found = store.query(&AuditQuery::default(), &alice()).await.unwrap();
        if found.len() >= count {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the audit sink never wrote {count} records; it wrote {}",
            found.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn published_events_become_records_without_anyone_asking() {
    let kernel = Kernel::builder()
        .component(AuditComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let ctx = kernel.context();

    ctx.publish(decision("assistant", "fs.read"));
    ctx.publish(invocation("assistant", "fs.read"));

    let store = ctx.service::<dyn AuditStore>().unwrap();
    let found = wait_for(&store, 2).await;

    // Both kinds are there. Deliberately *not* asserted in a fixed order: the sink selects
    // over one subscription per event type, so which of two events published in the same
    // instant is written first is not defined across types. What orders the events
    // themselves is their timestamps and their correlation id, and both are in the record.
    let kinds: Vec<AuditEntryKind> = found.iter().map(|record| record.entry.kind()).collect();
    assert!(kinds.contains(&AuditEntryKind::Authorization), "{kinds:?}");
    assert!(kinds.contains(&AuditEntryKind::Invocation), "{kinds:?}");

    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn events_of_one_kind_are_recorded_in_the_order_they_were_published() {
    // The ordering that *is* guaranteed, and the one an operator reads a trail by: within a
    // single event type the bus is a queue, so sequence order is publication order.
    let kernel = Kernel::builder()
        .component(AuditComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let ctx = kernel.context();

    for at in 0..6u64 {
        ctx.publish(ToolInvoked {
            timestamp: Timestamp::from_millis(at),
            ..invocation("assistant", "fs.read")
        });
    }

    let store = ctx.service::<dyn AuditStore>().unwrap();
    let found = wait_for(&store, 6).await;

    // Newest first, so the timestamps count down and the sequence numbers count down with
    // them: a record written out of order would break one of the two.
    let stamps: Vec<u64> = found
        .iter()
        .map(|record| record.entry.timestamp().as_millis())
        .collect();
    assert_eq!(stamps, vec![5, 4, 3, 2, 1, 0]);
    let sequences: Vec<u64> = found.iter().map(|record| record.sequence).collect();
    assert_eq!(sequences, vec![6, 5, 4, 3, 2, 1]);

    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn events_published_just_before_shutdown_are_still_written() {
    // The race this excludes: the sink task is cancelled while events it was told about are
    // still in the bus's buffer, so the last tool call of a run is missing from the trail —
    // which is the call a person reviewing the trail is most likely to be looking for.
    let kernel = Kernel::builder()
        .component(AuditComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let ctx = kernel.context();
    let store = ctx.service::<dyn AuditStore>().unwrap();

    for _ in 0..8 {
        ctx.publish(invocation("assistant", "fs.read"));
    }
    kernel.shutdown().await.unwrap();

    let found = store.query(&AuditQuery::default(), &alice()).await.unwrap();
    assert_eq!(found.len(), 8, "the sink drained its queue before stopping");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_durable_trail_outlives_the_kernel_that_wrote_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let config = Config::builder()
        .layer(json!({
            "components": { "store": { "db": { "path": path.to_str().unwrap() } } }
        }))
        .build();

    {
        let kernel = Kernel::builder()
            .config(config.clone())
            .component(StoreComponent::new())
            .component(RedbAuditComponent::new())
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        kernel.context().publish(invocation("assistant", "fs.read"));
        let store = kernel.context().service::<dyn AuditStore>().unwrap();
        wait_for(&store, 1).await;
        kernel.shutdown().await.unwrap();
        // Dropping the kernel is what releases redb's exclusive lock on the file.
        drop(store);
        drop(kernel);
    }

    let kernel = Kernel::builder()
        .config(config)
        .component(StoreComponent::new())
        .component(RedbAuditComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let store = kernel.context().service::<dyn AuditStore>().unwrap();
    let found = store.query(&AuditQuery::default(), &alice()).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].entry.tool().unwrap().as_str(), "fs.read");
    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_retention_period_is_read_from_the_component_section() {
    let config = Config::builder()
        .layer(json!({
            "components": { "audit": { "store": { "retention_days": 30 } } }
        }))
        .build();

    let kernel = Kernel::builder()
        .config(config)
        .component(AuditComponent::new())
        .build()
        .unwrap();

    // Nothing to assert about the sweep itself without waiting an hour; what matters here is
    // that a configured period is accepted rather than rejected or ignored, and that the
    // component starts with a sweep task in its scope.
    kernel.start().await.unwrap();
    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nonsensical_retention_period_stops_the_kernel_rather_than_emptying_the_trail() {
    let config = Config::builder()
        .layer(json!({
            "components": { "audit": { "store": { "retention_days": 0 } } }
        }))
        .build();

    let kernel = Kernel::builder()
        .config(config)
        .component(AuditComponent::new())
        .build()
        .unwrap();

    // The kernel reports it as a failed component lifecycle; the configuration error that
    // caused it is the cause underneath, which is what names the offending key.
    let error = kernel.start().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lifecycle);
    let mut chain = String::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(cause) = source {
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    assert!(chain.contains("retention_days"), "{chain}");
}
