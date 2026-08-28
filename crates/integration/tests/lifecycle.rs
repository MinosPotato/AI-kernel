//! The whole durable stack in one kernel: start, write, stop, reopen, recover.
//!
//! Every durable subsystem has its own end-to-end suite, and each of those builds a kernel
//! holding *that* subsystem. None of them answers the question this file exists for: whether
//! the four of them together still start, still share one database file, still let go of it,
//! and still find their own data afterwards.
//!
//! That is not a formality. The store, the transcript, the memory and the schedule share one
//! redb file and one schema version; the scheduler and the memory sweeper share one task
//! tree; all four share one registry that hands out `Arc<Db>`. Any of those could hold the
//! file open past shutdown, and the symptom would be a second kernel that cannot start.

use std::sync::Arc;
use std::time::Duration;

use aik_api::agent::SessionId;
use aik_api::context::{ContextEntry, ContextStore};
use aik_api::memory::{MemoryQuery, MemoryRecord, MemoryStore};
use aik_api::model::{Message, Role};
use aik_api::scheduler::{JobSpec, Scheduler, Trigger};
use aik_core::clock::Timestamp;
use aik_core::prelude::*;
use serde_json::json;

mod support;
use support::{HandlerComponent, RecordingHandler, store_config, until, user};

const HANDLER: &str = "jobs.integration";

/// Every durable subsystem the kernel has, over one shared database.
fn kernel(path: &std::path::Path, handler: Arc<RecordingHandler>) -> Kernel {
    Kernel::builder()
        .config(store_config(path))
        .component(aik_store::StoreComponent::new())
        .component(aik_context::RedbContextComponent::new())
        .component(aik_memory::RedbMemoryComponent::new())
        .component(HandlerComponent::new(HANDLER, handler))
        .component(aik_scheduler::RedbSchedulerComponent::new())
        .build()
        .expect("the full durable stack is a valid wiring")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_full_durable_stack_starts_writes_stops_and_recovers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let alice = user("alice");
    let session = SessionId::new();

    {
        let kernel = kernel(&path, Arc::new(RecordingHandler::new()));
        kernel.start().await.expect("the kernel starts");

        let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
        scheduler
            .schedule(
                JobSpec::new(
                    "nightly",
                    Trigger::Every {
                        interval: Duration::from_secs(3_600),
                    },
                    HANDLER,
                )
                .persistent(true),
                &alice,
            )
            .await
            .expect("a durable job is accepted");

        let memories = kernel.context().service::<dyn MemoryStore>().unwrap();
        memories
            .put(
                MemoryRecord::new("fact", json!({ "n": 1 }), Timestamp::EPOCH),
                &alice,
            )
            .await
            .expect("a memory record is stored");

        let transcript = kernel.context().service::<dyn ContextStore>().unwrap();
        transcript
            .append(
                &session,
                ContextEntry::new(Message::text(Role::User, "hello")),
                &alice,
            )
            .await
            .expect("a transcript entry is appended");

        kernel.shutdown().await.expect("the kernel shuts down");
        // Resolved services hold `Arc<Db>` transitively, so releasing redb's exclusive lock
        // means dropping them as well as the kernel -- see `aik_store::StoreComponent`. The
        // next `start` is what proves every one of them let go.
    }

    let kernel = kernel(&path, Arc::new(RecordingHandler::new()));
    kernel
        .start()
        .await
        .expect("a second kernel opens the same database file");

    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
    let jobs = scheduler.list(&alice).await.unwrap();
    assert_eq!(jobs.len(), 1, "the durable job survived");
    assert_eq!(jobs[0].spec.id.as_str(), "nightly");

    let memories = kernel.context().service::<dyn MemoryStore>().unwrap();
    assert_eq!(
        memories
            .query(&MemoryQuery::default(), &alice)
            .await
            .unwrap()
            .len(),
        1,
        "the memory record survived"
    );

    let transcript = kernel.context().service::<dyn ContextStore>().unwrap();
    assert!(
        transcript.stats(&session, &alice).await.unwrap().is_some(),
        "the transcript session survived"
    );

    drop(scheduler);
    drop(memories);
    drop(transcript);
    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_job_fires_after_the_restart_that_recovered_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    {
        let kernel = kernel(&path, Arc::new(RecordingHandler::new()));
        kernel.start().await.unwrap();
        kernel
            .context()
            .service::<dyn Scheduler>()
            .unwrap()
            .schedule(
                JobSpec::new(
                    "soon",
                    Trigger::After {
                        delay: Duration::from_millis(80),
                    },
                    HANDLER,
                )
                .persistent(true),
                &user("alice"),
            )
            .await
            .unwrap();
        // Down before it was ever due, so the firing can only come from recovery.
        kernel.shutdown().await.unwrap();
    }

    let handler = Arc::new(RecordingHandler::new());
    let kernel = kernel(&path, handler.clone());
    kernel.start().await.unwrap();

    until("the recovered job to fire", async || handler.count() > 0).await;
    assert_eq!(
        handler.firings()[0].job,
        "soon",
        "a job recovered from disk is the same job"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stack_records_one_schema_version_for_every_subsystem() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    let kernel = kernel(&path, Arc::new(RecordingHandler::new()));
    kernel.start().await.unwrap();

    let db = kernel.context().service::<aik_store::Db>().unwrap();
    assert_eq!(
        db.schema_version().unwrap(),
        aik_store::SCHEMA_VERSION,
        "every subsystem in one file agrees on one schema version"
    );

    drop(db);
    kernel.shutdown().await.unwrap();
}
