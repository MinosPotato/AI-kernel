//! Reclaiming sessions nobody came back to.
//!
//! Retention is the only thing in this crate that destroys data its owner still has, so what
//! it must get right is narrow and unforgiving: remove exactly the sessions that are past the
//! cutoff, remove all of each one, and remove nothing else. A sweep that took one session too
//! many would be silent data loss; one that took none would leave the store growing for ever
//! while looking healthy.
//!
//! Everything here runs on a [`ManualClock`]. A session's idle time is measured against the
//! same clock that stamped it, so driving that clock is the only way to assert about days of
//! idleness in a test that finishes.

use std::sync::Arc;
use std::time::Duration;

use aik_api::agent::SessionId;
use aik_api::context::ContextBudget;
use aik_context::{ContextComponent, DEFAULT_RETENTION_BATCH, RedbContextComponent};
use aik_core::clock::{Clock, ManualClock, SharedClock, Timestamp};
use aik_core::prelude::*;
use aik_store::StoreComponent;
use serde_json::json;

mod support;

use support::{Backend, say, user};

/// A clock a test drives, starting far enough from zero that a cutoff can be subtracted.
fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(Timestamp::from_millis(1_000_000)))
}

crate::both_backends!(
    a_sweep_removes_sessions_past_the_cutoff_and_nothing_else,
    a_sweep_of_a_store_with_nothing_due_removes_nothing,
    a_swept_session_is_gone_in_full,
    a_sweep_reclaims_every_owners_sessions,
    a_session_that_was_appended_to_again_is_not_stale,
    compacting_a_session_does_not_buy_it_more_time,
);

async fn a_sweep_removes_sessions_past_the_cutoff_and_nothing_else(backend: Backend) {
    let fixture = backend.on_clock(clock());
    let store = fixture.store();
    let cx = user("alice");
    let (old, recent) = (SessionId::new(), SessionId::new());

    store.append(&old, say("last month"), &cx).await.unwrap();
    fixture.advance(10_000);
    store.append(&recent, say("just now"), &cx).await.unwrap();

    // Cutoff between the two: the older session is at or before it, the newer is after.
    let cutoff = Timestamp::from_millis(fixture.now().as_millis() - 5_000);
    assert_eq!(fixture.sweeper().sweep_stale(cutoff).await.unwrap(), 1);

    assert!(store.stats(&old, &cx).await.unwrap().is_none());
    assert!(store.stats(&recent, &cx).await.unwrap().is_some());

    let listed = store.sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, recent);
}

async fn a_sweep_of_a_store_with_nothing_due_removes_nothing(backend: Backend) {
    let fixture = backend.on_clock(clock());
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    store.append(&session, say("hello"), &cx).await.unwrap();

    // A cutoff before everything, and one exactly at the origin: neither is allowed to be a
    // wildcard. `sweep_stale` on an empty store is likewise a no-op rather than an error.
    for cutoff in [Timestamp::from_millis(0), Timestamp::from_millis(999_999)] {
        assert_eq!(fixture.sweeper().sweep_stale(cutoff).await.unwrap(), 0);
    }
    assert_eq!(store.sessions(&cx).await.unwrap().len(), 1);
}

async fn a_swept_session_is_gone_in_full(backend: Backend) {
    let fixture = backend.on_clock(clock());
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    let record = store.append(&session, say("hello"), &cx).await.unwrap();
    store.append(&session, say("again"), &cx).await.unwrap();

    fixture.advance(10_000);
    assert_eq!(
        fixture.sweeper().sweep_stale(fixture.now()).await.unwrap(),
        1
    );

    // Every way of reaching the session agrees it is gone — which is the same set of
    // assertions `clear` has to satisfy, because a sweep *is* a clear the owner did not ask
    // for and must not differ from one.
    assert!(store.stats(&session, &cx).await.unwrap().is_none());
    assert!(
        store
            .get(&session, &record.id, &cx)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.sessions(&cx).await.unwrap().is_empty());
    assert_eq!(
        store
            .window(&session, &ContextBudget::UNLIMITED, &cx)
            .await
            .unwrap()
            .messages
            .len(),
        0
    );

    // And the id is free again: nothing lingers that would refuse a new session under it.
    let fresh = store
        .append(&session, say("new"), &user("bob"))
        .await
        .unwrap();
    assert_eq!(fresh.sequence, 0);
}

async fn a_sweep_reclaims_every_owners_sessions(backend: Backend) {
    let fixture = backend.on_clock(clock());
    let store = fixture.store();
    let (hers, his) = (SessionId::new(), SessionId::new());

    store.append(&hers, say("a"), &user("alice")).await.unwrap();
    store.append(&his, say("b"), &user("bob")).await.unwrap();

    fixture.advance(10_000);
    // Owner-blind by contract: housekeeping that could only reclaim one principal's sessions
    // would leave everybody else's on disk for ever.
    assert_eq!(
        fixture.sweeper().sweep_stale(fixture.now()).await.unwrap(),
        2
    );
    assert!(store.sessions(&user("alice")).await.unwrap().is_empty());
    assert!(store.sessions(&user("bob")).await.unwrap().is_empty());
}

async fn a_session_that_was_appended_to_again_is_not_stale(backend: Backend) {
    let fixture = backend.on_clock(clock());
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    store.append(&session, say("long ago"), &cx).await.unwrap();
    fixture.advance(10_000);
    store
        .append(&session, say("but just now"), &cx)
        .await
        .unwrap();

    // The cutoff is after the *first* append and before the second. A store that indexed the
    // session by when it was created rather than by when it was last touched would delete a
    // conversation somebody is in the middle of.
    let cutoff = Timestamp::from_millis(fixture.now().as_millis() - 5_000);
    assert_eq!(fixture.sweeper().sweep_stale(cutoff).await.unwrap(), 0);
    assert_eq!(
        store.stats(&session, &cx).await.unwrap().unwrap().records,
        2
    );
}

async fn compacting_a_session_does_not_buy_it_more_time(backend: Backend) {
    let fixture = backend.on_clock(clock());
    let store = fixture.store();
    let cx = user("alice");
    let session = SessionId::new();

    for body in ["one", "two", "three"] {
        store.append(&session, say(body), &cx).await.unwrap();
    }

    fixture.advance(10_000);
    store.compact(&session, 1, &cx).await.unwrap();

    // Compaction is not activity. If it moved `updated_at`, a session could be kept alive for
    // ever by housekeeping that was supposed to be reclaiming it.
    assert_eq!(
        fixture.sweeper().sweep_stale(fixture.now()).await.unwrap(),
        1
    );
    assert!(store.stats(&session, &cx).await.unwrap().is_none());
}

#[tokio::test]
async fn a_persistent_sweep_reclaims_a_backlog_in_bounded_batches() {
    // A batch of three against seven stale sessions, so completing the sweep provably takes
    // more than one transaction. The contract is that `sweep_stale` loops until nothing is
    // due, not that it stops after a batch.
    let fixture = Backend::Redb.batched(clock(), 3);
    let store = fixture.store();
    let cx = user("alice");

    let sessions: Vec<SessionId> = (0..7).map(|_| SessionId::new()).collect();
    for session in &sessions {
        store.append(session, say("hello"), &cx).await.unwrap();
    }

    fixture.advance(10_000);
    assert_eq!(
        fixture.sweeper().sweep_stale(fixture.now()).await.unwrap(),
        7
    );
    assert!(store.sessions(&cx).await.unwrap().is_empty());
}

#[tokio::test]
async fn retention_survives_a_restart() {
    let fixture_clock = clock();
    let mut fixture = Backend::Redb.on_clock(fixture_clock);
    let cx = user("alice");
    let (old, recent) = (SessionId::new(), SessionId::new());

    fixture
        .store()
        .append(&old, say("last month"), &cx)
        .await
        .unwrap();
    fixture.advance(10_000);
    fixture
        .store()
        .append(&recent, say("just now"), &cx)
        .await
        .unwrap();

    // The sweep happens in one process and the assertion in the next, so what is being
    // checked is the file rather than a handle's memory of it.
    let cutoff = Timestamp::from_millis(fixture.now().as_millis() - 5_000);
    assert_eq!(fixture.sweeper().sweep_stale(cutoff).await.unwrap(), 1);
    fixture.reopen();

    let listed = fixture.store().sessions(&cx).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, recent);
    assert!(fixture.store().stats(&old, &cx).await.unwrap().is_none());

    // And a second sweep at the same cutoff finds nothing left to do, which is what proves
    // the index entry went with the session rather than surviving it.
    assert_eq!(fixture.sweeper().sweep_stale(cutoff).await.unwrap(), 0);
}

#[tokio::test]
async fn a_component_with_no_retention_configured_never_sweeps() {
    // The shipped default. A store that expired conversations because nobody said otherwise
    // would be data loss delivered as housekeeping, so the absence of a sweep is asserted
    // rather than assumed.
    let component = ContextComponent::new();
    let debug = format!("{component:?}");
    assert!(
        debug.contains("retention: None"),
        "retention must be off unless configured: {debug}"
    );
    assert!(
        format!("{:?}", RedbContextComponent::new()).contains("retention: None"),
        "and off for the durable backend too, which is the one it would matter on"
    );
}

#[tokio::test]
async fn a_configured_kernel_sweeps_stale_sessions_and_stops_cleanly() {
    use aik_api::context::ContextStore;
    use aik_api::execution::ExecutionContext;

    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("aik.redb");
    let clock = clock();
    let shared: SharedClock = clock.clone();

    let kernel = Kernel::builder()
        .config(
            Config::builder()
                .layer(json!({ "components": { "store": { "db": { "path": path } } } }))
                .build(),
        )
        .clock(shared)
        .component(StoreComponent::new())
        .component(
            RedbContextComponent::new()
                .with_retention(Duration::from_secs(5))
                .with_retention_interval(Duration::from_millis(5)),
        )
        .build()
        .expect("a kernel");
    kernel.start().await.expect("started");

    let store = kernel
        .context()
        .service::<dyn ContextStore>()
        .expect("the context store is published");
    let cx = ExecutionContext::new().with_principal(aik_api::permission::Principal::new(
        "alice",
        aik_api::permission::PrincipalKind::User,
    ));
    let session = SessionId::new();
    store.append(&session, say("hello"), &cx).await.unwrap();

    // Ten seconds of idleness against a five-second retention, on a clock the test owns.
    clock.set(Timestamp::from_millis(clock.now().as_millis() + 10_000));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if store.stats(&session, &cx).await.unwrap().is_none() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the configured retention sweep never reclaimed the stale session"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Shutdown must stop the sweep and release the database, not merely stop scheduling it.
    // The resolved handle is dropped too: redb's lock belongs to the `Arc<Db>`, so a test
    // still holding a store would prove nothing about whether the kernel let go.
    kernel.shutdown().await.expect("the kernel stops cleanly");
    drop(store);
    drop(kernel);
    aik_store::Db::open(&path).expect("the file is unlocked after shutdown");
}

#[test]
fn the_shipped_batch_is_far_smaller_than_a_sessions_record_bound() {
    // The number itself is a judgement call, but its relationship to the record bound is not:
    // a batch is a transaction, and one session can hold `DEFAULT_MAX_RECORDS_PER_SESSION`
    // records, so a batch sized like the memory store's would be millions of removals at once.
    const { assert!(DEFAULT_RETENTION_BATCH < aik_context::DEFAULT_MAX_RECORDS_PER_SESSION) };
}
