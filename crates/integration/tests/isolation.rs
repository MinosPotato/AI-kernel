//! One principal, four subsystems, one answer.
//!
//! Each subsystem enforces ownership with the same rule — [`Principal::may_act_for`] — and
//! each has its own suite proving it. What none of them can prove alone is that the rule
//! still holds when authority *crosses* a boundary: a job scheduled by Alice runs later,
//! unattended, under a principal the scheduler derives, and then asks the memory store for
//! records. Two independent ownership checks, one derived identity between them.
//!
//! That derivation is the interesting part. A firing does not run *as* its owner; it runs as
//! the system acting for them ([`aik_scheduler::RUN_PRINCIPAL`]). So the memory store is
//! being asked to honour a delegation it never saw created — which either works, or is a
//! scheduled job that silently cannot reach the memories it was scheduled to maintain, or is
//! a scheduled job that can reach everybody's.

use std::sync::Arc;
use std::time::Duration;

use aik_api::memory::{MemoryQuery, MemoryRecord, MemoryStore};
use aik_api::permission::PrincipalId;
use aik_api::scheduler::{JobSpec, Scheduler, Trigger};
use aik_core::clock::Timestamp;
use aik_core::prelude::*;
use aik_core::{ErrorKind, Result};
use serde_json::json;

mod support;
use support::{BoxFuture, HandlerComponent, RecordingHandler, store_config, until, user};

const HANDLER: &str = "jobs.reader";

/// What one firing managed to read, recorded out of the handler.
type Seen = Arc<std::sync::Mutex<Vec<(String, usize)>>>;

/// A kernel whose scheduled job reads memory through the firing's own context.
fn kernel(path: &std::path::Path, handler: Arc<RecordingHandler>) -> Kernel {
    Kernel::builder()
        .config(store_config(path))
        .component(aik_store::StoreComponent::new())
        .component(aik_memory::RedbMemoryComponent::new())
        .component(
            HandlerComponent::new(HANDLER, handler).requiring(aik_memory::DEFAULT_COMPONENT_ID),
        )
        .component(aik_scheduler::RedbSchedulerComponent::new())
        .build()
        .expect("a valid wiring")
}

/// Builds the handler body that queries memory as whoever the firing is.
fn reader(
    memories: Arc<std::sync::Mutex<Option<Arc<dyn MemoryStore>>>>,
    seen: Seen,
) -> RecordingHandler {
    RecordingHandler::running(move |spec, cx| -> BoxFuture {
        let memories = memories.clone();
        let seen = seen.clone();
        Box::pin(async move {
            let store = memories.lock().unwrap().clone().expect("resolved in init");
            let found = store.query(&MemoryQuery::default(), &cx).await?;
            seen.lock()
                .unwrap()
                .push((spec.id.to_string(), found.len()));
            Ok::<(), aik_core::Error>(())
        })
    })
}

/// Resolves the memory store into `slot` during `init`, the way a real subsystem would.
struct Resolver(Arc<std::sync::Mutex<Option<Arc<dyn MemoryStore>>>>);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish()
    }
}

#[async_trait]
impl Component for Resolver {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("jobs.reader.resolver").requires(aik_memory::DEFAULT_COMPONENT_ID)
    }
    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        *self.0.lock().unwrap() = Some(ctx.service::<dyn MemoryStore>()?);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_job_reaches_its_owners_memories_and_only_those() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    let slot: Arc<std::sync::Mutex<Option<Arc<dyn MemoryStore>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handler = Arc::new(reader(slot.clone(), seen.clone()));

    let kernel = Kernel::builder()
        .config(store_config(&path))
        .component(aik_store::StoreComponent::new())
        .component(aik_memory::RedbMemoryComponent::new())
        .component(Resolver(slot.clone()))
        .component(
            HandlerComponent::new(HANDLER, handler.clone())
                .requiring(aik_memory::DEFAULT_COMPONENT_ID),
        )
        .component(aik_scheduler::RedbSchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    // One memory each, stored by two different people.
    let memories = kernel.context().service::<dyn MemoryStore>().unwrap();
    for who in ["alice", "bob"] {
        memories
            .put(
                MemoryRecord::new("fact", json!({ "who": who }), Timestamp::EPOCH),
                &user(who),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        memories
            .query(&MemoryQuery::default(), &user("alice"))
            .await
            .unwrap()
            .len(),
        1,
        "the store itself isolates, which is what makes the next assertion about the seam"
    );

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(
            JobSpec::new(
                "alice-job",
                Trigger::After {
                    delay: Duration::from_millis(20),
                },
                HANDLER,
            ),
            &user("alice"),
        )
        .await
        .unwrap();

    until("alice's job to run", async || {
        !seen.lock().unwrap().is_empty()
    })
    .await;

    let observed = seen.lock().unwrap().clone();
    assert_eq!(observed.len(), 1, "the job fired once");
    assert_eq!(
        observed[0],
        ("alice-job".to_owned(), 1),
        "a firing sees exactly its owner's records: not zero, which would mean delegation was \
         lost, and not two, which would mean it was a master key"
    );

    // And the identity that achieved that is a delegate, not an impersonation.
    let principal = handler.firings()[0]
        .principal
        .clone()
        .expect("a firing is never anonymous");
    assert_eq!(principal.id, PrincipalId::new(aik_scheduler::RUN_PRINCIPAL));
    assert_ne!(principal.id, PrincipalId::new("alice"));
    assert!(principal.may_act_for(&PrincipalId::new("alice")));
    assert!(!principal.may_act_for(&PrincipalId::new("bob")));

    drop(memories);
    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_principal_cannot_reach_anothers_jobs_or_memories_in_the_same_kernel() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let kernel = kernel(&path, Arc::new(RecordingHandler::new()));
    kernel.start().await.unwrap();

    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
    let memories = kernel.context().service::<dyn MemoryStore>().unwrap();

    scheduler
        .schedule(
            JobSpec::new(
                "alice-job",
                Trigger::After {
                    delay: Duration::from_secs(3_600),
                },
                HANDLER,
            ),
            &user("alice"),
        )
        .await
        .unwrap();
    let record = MemoryRecord::new("fact", json!({ "who": "alice" }), Timestamp::EPOCH);
    let id = record.id;
    memories.put(record, &user("alice")).await.unwrap();

    // Enumeration hides rather than errors, in both subsystems, so that a refusal never
    // confirms what exists.
    assert!(scheduler.list(&user("mallory")).await.unwrap().is_empty());
    assert!(
        memories
            .query(&MemoryQuery::default(), &user("mallory"))
            .await
            .unwrap()
            .is_empty()
    );

    // Naming one names it, and is refused identically by both.
    assert_eq!(
        scheduler
            .cancel(
                &aik_api::scheduler::JobId::new("alice-job"),
                &user("mallory")
            )
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Permission
    );
    assert_eq!(
        memories
            .get(&id, &user("mallory"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Permission
    );

    drop(scheduler);
    drop(memories);
    kernel.shutdown().await.unwrap();
}
