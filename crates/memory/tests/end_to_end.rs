//! The memory layer as a kernel actually runs it: wired as a component, sweeping on its own
//! timer, sharing one database file with the transcript store, and stopping cleanly.
//!
//! `behavior.rs` and `persistence.rs` both drive the stores directly. Everything here needs a
//! kernel to mean anything — component ordering, the background sweep's lifecycle, and the
//! claim `aik-store` makes in its own module documentation that two durable subsystems share
//! one file rather than one schema.

use std::sync::Arc;
use std::time::Duration;

use aik_api::agent::SessionId;
use aik_api::context::{ContextBudget, ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryQuery, MemoryRecord, MemoryStore};
use aik_api::model::{Message, Role};
use aik_api::permission::{Principal, PrincipalId, PrincipalKind};
use aik_context::RedbContextComponent;
use aik_core::ErrorKind;
use aik_core::clock::{ManualClock, Timestamp};
use aik_core::prelude::*;
use aik_memory::{MemoryComponent, RedbMemoryComponent};
use aik_store::{Db, StoreComponent};
use serde_json::json;

fn config_for(path: &std::path::Path) -> Config {
    Config::builder()
        .layer(json!({
            "components": { "store": { "db": { "path": path } } }
        }))
        .build()
}

fn alice() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
}

fn record(kind: &str, created_at_ms: u64) -> MemoryRecord {
    MemoryRecord::new(
        kind,
        json!({ "n": created_at_ms }),
        Timestamp::from_millis(created_at_ms),
    )
}

/// Waits for `condition` to hold, or gives up. Used for the background sweep, which runs on
/// its own timer: the test cannot know which tick will be the one that reclaims.
async fn eventually(what: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = Duration::from_secs(5);
    let result = tokio::time::timeout(deadline, async {
        loop {
            if condition().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(result.is_ok(), "timed out waiting for {what}");
}

#[tokio::test]
async fn the_in_memory_component_publishes_a_store() {
    let kernel = Kernel::builder()
        .component(MemoryComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let store = kernel.context().service::<dyn MemoryStore>().unwrap();
    let stored = record("fact", 1);
    store.put(stored.clone(), &alice()).await.unwrap();
    assert!(store.get(&stored.id, &alice()).await.unwrap().is_some());

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_persistent_component_refuses_to_start_without_a_database() {
    // Better a startup failure attributed to the missing dependency than a kernel that comes
    // up with a memory store nobody notices is absent.
    let error = Kernel::builder()
        .component(RedbMemoryComponent::new())
        .build()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Wiring);
    assert!(
        error.to_string().contains("store.db"),
        "the failure should name the database component, got `{error}`"
    );
}

#[tokio::test]
async fn the_persistent_component_writes_to_the_kernel_s_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(RedbMemoryComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let store = ctx.service::<dyn MemoryStore>().unwrap();
    let stored = record("preference", 1);
    store.put(stored.clone(), &alice()).await.unwrap();

    // The memories are in the kernel's database, not a second file of the memory store's
    // own: one file to secure, back up or delete.
    let db = ctx.service::<Db>().unwrap();
    assert_eq!(db.path(), path);

    // Everything holding the database has to go before it can be reopened: shutting the
    // kernel down stops its components but does not drop the registry that owns the handle.
    kernel.shutdown().await.unwrap();
    drop((db, store, ctx, kernel));

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(RedbMemoryComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let store = kernel.context().service::<dyn MemoryStore>().unwrap();
    assert_eq!(
        store.get(&stored.id, &alice()).await.unwrap(),
        Some(MemoryRecord {
            owner: PrincipalId::new("alice"),
            ..stored
        }),
        "the record survived the restart, owner and all"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_background_sweep_reclaims_without_being_asked() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_000)));

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .clock(clock.clone())
        .component(StoreComponent::new())
        .component(RedbMemoryComponent::new().with_expiry_interval(Duration::from_millis(5)))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let store = kernel.context().service::<dyn MemoryStore>().unwrap();
    let mut expiring = record("fact", 1);
    expiring.expires_at = Some(Timestamp::from_millis(1_500));
    let permanent = record("preference", 2);
    store.put(expiring.clone(), &alice()).await.unwrap();
    store.put(permanent.clone(), &alice()).await.unwrap();

    // Nothing is due yet, so no tick may remove anything.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(store.get(&expiring.id, &alice()).await.unwrap().is_some());

    clock.set(Timestamp::from_millis(2_000));

    // This is the only assertion that the component's `start` actually spawned the task: no
    // test calls `sweep_expired` here, so if the timer is not running the record stays.
    let probe = store.clone();
    let id = expiring.id;
    eventually("the expiry sweep to reclaim the record", async || {
        probe.get(&id, &alice()).await.unwrap().is_none()
    })
    .await;

    assert!(
        store.get(&permanent.id, &alice()).await.unwrap().is_some(),
        "a sweep must not reclaim a record that never expires"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_stops_the_sweep_rather_than_waiting_out_its_interval() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    // An interval far longer than the shutdown timeout: if cancellation were only observed
    // when the timer fires, this shutdown would block for an hour and then time out.
    let kernel = Kernel::builder()
        .config(config_for(&path))
        .shutdown_timeout(Duration::from_secs(5))
        .component(StoreComponent::new())
        .component(RedbMemoryComponent::new().with_expiry_interval(Duration::from_secs(3600)))
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    assert!(
        kernel.context().tasks().running() > 0,
        "the sweep is running"
    );

    let started = std::time::Instant::now();
    kernel.shutdown().await.unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown waited on the sweep interval instead of cancelling it"
    );
    assert_eq!(
        kernel.context().tasks().running(),
        0,
        "the sweep task outlived the kernel that owns it"
    );
}

#[tokio::test]
async fn the_transcript_and_the_memories_share_one_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let session = SessionId::new();

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(RedbContextComponent::new())
        .component(RedbMemoryComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    let context_store = ctx.service::<dyn ContextStore>().unwrap();
    let memory_store = ctx.service::<dyn MemoryStore>().unwrap();

    // Both subsystems resolve the *same* handle. redb takes an exclusive lock per file, so
    // two independently opened databases would not be a performance question — the second
    // one simply would not open.
    let context_db = ctx
        .service_named::<Db>(&ComponentId::new("store.db"))
        .unwrap();
    assert!(Arc::ptr_eq(&context_db, &ctx.service::<Db>().unwrap()));
    assert_eq!(context_db.path(), path);

    let turn = context_store
        .append(
            &session,
            ContextEntry::new(Message::text(
                Role::User,
                "remember that I prefer dark mode",
            )),
            &alice(),
        )
        .await
        .unwrap();
    let memory = record("preference", 1);
    memory_store.put(memory.clone(), &alice()).await.unwrap();

    // Interleaved writes from the two subsystems, to make sure neither serialises the other
    // into an error and neither lands in the other's tables.
    for index in 0..8u64 {
        context_store
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, format!("turn {index}"))),
                &alice(),
            )
            .await
            .unwrap();
        memory_store
            .put(record("fact", index), &alice())
            .await
            .unwrap();
    }

    kernel.shutdown().await.unwrap();
    drop((context_store, memory_store, context_db, ctx, kernel));

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(RedbContextComponent::new())
        .component(RedbMemoryComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let ctx = kernel.context();

    let context_store = ctx.service::<dyn ContextStore>().unwrap();
    let stats = context_store
        .stats(&session, &alice())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.records, 9, "the transcript came back whole");
    assert_eq!(
        context_store
            .get(&session, &turn.id, &alice())
            .await
            .unwrap(),
        Some(turn)
    );
    let window = context_store
        .window(&session, &ContextBudget::UNLIMITED, &alice())
        .await
        .unwrap();
    assert_eq!(window.usage.included_records, 9);

    let memory_store = ctx.service::<dyn MemoryStore>().unwrap();
    let remembered = memory_store
        .query(&MemoryQuery::default(), &alice())
        .await
        .unwrap();
    assert_eq!(remembered.len(), 9, "the memories came back whole");
    assert!(
        memory_store
            .get(&memory.id, &alice())
            .await
            .unwrap()
            .is_some(),
        "and neither subsystem's rows were mistaken for the other's"
    );

    // One schema version covering both subsystems' tables, which is the point of sharing a
    // file rather than sharing a schema.
    assert_eq!(
        ctx.service::<Db>().unwrap().schema_version().unwrap(),
        aik_store::SCHEMA_VERSION
    );

    kernel.shutdown().await.unwrap();
}
