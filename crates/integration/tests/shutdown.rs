//! Stopping a kernel while its subsystems are busy with the database they share.
//!
//! Shutdown is where the task tree, the component order and the shared `Db` all have to
//! agree at once, and each of them is tested alone elsewhere. What is only true of the
//! combination: the scheduler's run tasks, the memory store's expiry sweeper and whatever a
//! handler is doing all live in *one* task tree with one deadline, and all of them are
//! holding `Arc<Db>` when the kernel is asked to stop.
//!
//! The failure this guards against is not a crash. It is a shutdown that returns having
//! merely *signalled* cancellation, leaving work running against a database the caller
//! believes is finished with — which looks like a clean exit and is not one.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aik_api::memory::{MemoryRecord, MemoryStore};
use aik_api::scheduler::{JobCancelled, JobSpec, Scheduler, Trigger};
use aik_core::clock::Timestamp;
use aik_core::prelude::*;
use aik_core::{ErrorKind, Result};
use serde_json::json;

mod support;
use support::{BoxFuture, HandlerComponent, RecordingHandler, store_config, until, user};

const HANDLER: &str = "jobs.churn";

/// The shutdown deadline these tests give the kernel.
///
/// Long enough that a correct shutdown never approaches it, short enough that a broken one
/// fails the test in seconds rather than looking like a hang.
const DEADLINE: Duration = Duration::from_secs(10);

/// Resolves the memory store during `init`.
struct Resolver(Arc<std::sync::Mutex<Option<Arc<dyn MemoryStore>>>>);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish()
    }
}

#[async_trait]
impl Component for Resolver {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("jobs.churn.resolver").requires(aik_memory::DEFAULT_COMPONENT_ID)
    }
    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        *self.0.lock().unwrap() = Some(ctx.service::<dyn MemoryStore>()?);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_reaches_a_firing_that_is_writing_to_the_shared_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    let slot: Arc<std::sync::Mutex<Option<Arc<dyn MemoryStore>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let writes = Arc::new(AtomicUsize::new(0));

    // A handler that will not stop on its own: only cancellation ends this loop, so a
    // shutdown that returns is a shutdown that actually reached it.
    let handler = {
        let slot = slot.clone();
        let writes = writes.clone();
        Arc::new(RecordingHandler::running(move |_spec, cx| -> BoxFuture {
            let slot = slot.clone();
            let writes = writes.clone();
            Box::pin(async move {
                let memories = slot.lock().unwrap().clone().expect("resolved in init");
                while !cx.cancellation.is_cancelled() {
                    memories
                        .put(
                            MemoryRecord::new("churn", json!({ "n": 1 }), Timestamp::EPOCH),
                            &cx,
                        )
                        .await?;
                    writes.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                }
                Ok::<(), aik_core::Error>(())
            })
        }))
    };

    let kernel = Kernel::builder()
        .config(store_config(&path))
        .shutdown_timeout(DEADLINE)
        .component(aik_store::StoreComponent::new())
        .component(aik_context::RedbContextComponent::new())
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

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(
            JobSpec::new(
                "churn",
                Trigger::After {
                    delay: Duration::from_millis(20),
                },
                HANDLER,
            )
            .persistent(true),
            &user("alice"),
        )
        .await
        .unwrap();

    until("the firing to be genuinely mid-write", async || {
        writes.load(Ordering::SeqCst) > 20
    })
    .await;

    let started = Instant::now();
    kernel
        .shutdown()
        .await
        .expect("a busy schedule still stops within the deadline");
    let elapsed = started.elapsed();

    assert!(
        elapsed < DEADLINE,
        "shutdown took {elapsed:?}, which means it waited out the deadline rather than \
         cancelling the work"
    );
    assert!(
        writes.load(Ordering::SeqCst) > 0,
        "the test only means something if the job really was writing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_publishes_the_cancellation_of_a_firing_in_flight() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let handler = Arc::new(RecordingHandler::holding());

    let kernel = Kernel::builder()
        .config(store_config(&path))
        .shutdown_timeout(DEADLINE)
        .component(aik_store::StoreComponent::new())
        .component(aik_memory::RedbMemoryComponent::new())
        .component(HandlerComponent::new(HANDLER, handler.clone()))
        .component(aik_scheduler::RedbSchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut cancelled = kernel.context().subscribe::<JobCancelled>();

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(
            JobSpec::new(
                "held",
                Trigger::After {
                    delay: Duration::from_millis(20),
                },
                HANDLER,
            ),
            &user("alice"),
        )
        .await
        .unwrap();

    until("the firing to reach the handler", async || {
        handler.count() > 0
    })
    .await;

    kernel.shutdown().await.expect("shutdown completes");

    assert!(
        handler.noticed_cancellation(),
        "a handler blocked on its context has to be woken by shutdown, not waited out"
    );
    let event = tokio::time::timeout(Duration::from_secs(5), cancelled.recv())
        .await
        .expect("a JobCancelled arrives")
        .expect("the subscription is intact");
    assert_eq!(event.payload.event.job.as_str(), "held");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_kernel_refuses_new_scheduled_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");

    let kernel = Kernel::builder()
        .config(store_config(&path))
        .component(aik_store::StoreComponent::new())
        .component(HandlerComponent::new(
            HANDLER,
            Arc::new(RecordingHandler::new()),
        ))
        .component(aik_scheduler::RedbSchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
    kernel.shutdown().await.unwrap();

    // The service handle outlives the kernel, so this is reachable, and accepting a job
    // nothing is left to run would be exactly the silent failure the scheduler exists to
    // avoid.
    let error = scheduler
        .schedule(
            JobSpec::new(
                "too-late",
                Trigger::After {
                    delay: Duration::from_millis(1),
                },
                HANDLER,
            ),
            &user("alice"),
        )
        .await
        .expect_err("a stopped kernel has nothing to run a job with");
    assert_eq!(error.kind(), ErrorKind::Cancelled);
}
