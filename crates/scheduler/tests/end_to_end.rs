//! The scheduler as a component in a real kernel.
//!
//! Everything else in this suite drives [`JobScheduler`](aik_scheduler::JobScheduler)
//! directly. These assert the parts that only exist once the kernel is holding it: that a
//! handler published by some other component is found, that shutdown reaches a firing in
//! flight and is waited for, that a durable schedule outlives the kernel that wrote it, and
//! that dropping the kernel actually lets go of the database file.

use std::sync::Arc;
use std::time::Duration;

use aik_api::scheduler::{JobCancelled, JobHandler, JobId, JobSpec, Scheduler, Trigger};
use aik_core::prelude::*;
use aik_core::{Config, ErrorKind};
use aik_scheduler::{RedbSchedulerComponent, SchedulerComponent};
use aik_store::StoreComponent;
use serde_json::json;

mod support;
use support::{PATIENCE, START, TestHandler, TokioClock, advance, anonymous, expect, user};

const HANDLER_COMPONENT: &str = "jobs.test";

/// A component whose whole job is to publish one [`JobHandler`], the way any subsystem
/// contributing scheduled work would.
struct HandlerComponent {
    handler: Arc<TestHandler>,
}

impl std::fmt::Debug for HandlerComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerComponent").finish()
    }
}

#[async_trait]
impl Component for HandlerComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(HANDLER_COMPONENT)
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        ctx.provide::<dyn JobHandler>(self.handler.clone())
    }
}

fn clock() -> aik_core::clock::SharedClock {
    Arc::new(TokioClock::new(START))
}

fn spec(id: &str, seconds: u64) -> JobSpec {
    JobSpec::new(
        id,
        Trigger::After {
            delay: Duration::from_secs(seconds),
        },
        HANDLER_COMPONENT,
    )
}

#[tokio::test(start_paused = true)]
async fn a_handler_published_by_another_component_is_found_and_run() {
    let handler = Arc::new(TestHandler::new());
    let kernel = Kernel::builder()
        .clock(clock())
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(SchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
    scheduler
        .schedule(spec("job", 60), &anonymous())
        .await
        .unwrap();

    advance(Duration::from_secs(60)).await;
    handler.wait_for_calls(1).await;

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_a_firing_in_flight_and_waits_for_it() {
    let handler = Arc::new(TestHandler::new().holding());
    let kernel = Kernel::builder()
        .clock(clock())
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(SchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();
    let mut cancelled = kernel.context().subscribe::<JobCancelled>();

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(spec("slow", 1), &anonymous())
        .await
        .unwrap();
    advance(Duration::from_secs(1)).await;
    handler.wait_for_calls(1).await;

    // The handler is inside `run` and will stay there until it is told to stop, so a shutdown
    // that returns without a timeout is a shutdown that actually reached it.
    kernel.shutdown().await.unwrap();

    assert!(handler.observed_cancellation());
    expect(&mut cancelled).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_driver_so_nothing_new_fires() {
    let handler = Arc::new(TestHandler::new());
    let kernel = Kernel::builder()
        .clock(clock())
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(SchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
    scheduler
        .schedule(spec("later", 600), &anonymous())
        .await
        .unwrap();
    kernel.shutdown().await.unwrap();

    advance(Duration::from_secs(3_600)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(handler.count(), 0);

    let error = scheduler
        .schedule(spec("post-mortem", 1), &anonymous())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn a_persistent_scheduler_needs_a_database_in_the_kernel() {
    let error = Kernel::builder()
        .clock(clock())
        .component(RedbSchedulerComponent::new())
        .build()
        .expect_err("a durable scheduler without a database is a wiring mistake");
    assert_eq!(error.kind(), ErrorKind::Wiring);
}

#[tokio::test(start_paused = true)]
async fn a_persistent_job_outlives_the_kernel_that_scheduled_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let config = |path: &std::path::Path| {
        Config::builder()
            .layer(json!({
                "components": { "store": { "db": { "path": path } } }
            }))
            .build()
    };

    {
        let kernel = Kernel::builder()
            .clock(clock())
            .config(config(&path))
            .component(StoreComponent::new())
            .component(HandlerComponent {
                handler: Arc::new(TestHandler::new()),
            })
            .component(RedbSchedulerComponent::new())
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        kernel
            .context()
            .service::<dyn Scheduler>()
            .unwrap()
            .schedule(spec("nightly", 3_600).persistent(true), &user("alice"))
            .await
            .unwrap();
        kernel.shutdown().await.unwrap();
        // Dropping the kernel is what releases redb's exclusive lock -- and it can only do
        // that if nothing registered in it holds a handle back to the registry. The next
        // `Db::open` is the assertion.
    }

    let handler = Arc::new(TestHandler::new());
    let kernel = Kernel::builder()
        .clock(clock())
        .config(config(&path))
        .component(StoreComponent::new())
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(RedbSchedulerComponent::new())
        .build()
        .expect("the second kernel opens the same database");
    kernel.start().await.unwrap();

    let listed = kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .list(&user("alice"))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].spec.id, JobId::new("nightly"));

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_catch_up_window_can_be_set_in_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.redb");
    let config = Config::builder()
        .layer(json!({
            "components": {
                "store": { "db": { "path": path } },
                "scheduler": { "jobs": { "catch_up_window_ms": 1_000 } },
            }
        }))
        .build();

    // A job due well inside the default window, but well outside the configured one.
    {
        let kernel = Kernel::builder()
            .clock(clock())
            .config(config.clone())
            .component(StoreComponent::new())
            .component(HandlerComponent {
                handler: Arc::new(TestHandler::new()),
            })
            .component(RedbSchedulerComponent::new())
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        kernel
            .context()
            .service::<dyn Scheduler>()
            .unwrap()
            .schedule(spec("reminder", 60).persistent(true), &anonymous())
            .await
            .unwrap();
        kernel.shutdown().await.unwrap();
    }

    advance(Duration::from_secs(600)).await;

    let handler = Arc::new(TestHandler::new());
    let kernel = Kernel::builder()
        .clock(clock())
        .config(config)
        .component(StoreComponent::new())
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(RedbSchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        handler.count(),
        0,
        "a one-second catch-up window does not reach a firing missed ten minutes ago"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_job_scheduled_during_another_components_init_is_kept() {
    /// A component that schedules its own maintenance while wiring itself up, which is when a
    /// subsystem knows what work it needs and before anything is running.
    struct EagerComponent;

    #[async_trait]
    impl Component for EagerComponent {
        fn descriptor(&self) -> ComponentDescriptor {
            ComponentDescriptor::new("jobs.eager").requires(aik_scheduler::DEFAULT_COMPONENT_ID)
        }

        async fn init(&self, ctx: &ComponentContext) -> Result<()> {
            ctx.service::<dyn Scheduler>()?
                .schedule(spec("eager", 60), &anonymous())
                .await
        }
    }

    let handler = Arc::new(TestHandler::new());
    let kernel = Kernel::builder()
        .clock(clock())
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(SchedulerComponent::new())
        .component(EagerComponent)
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    advance(Duration::from_secs(60)).await;
    handler.wait_for_calls(1).await;

    kernel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_kernel_shuts_down_within_its_deadline_with_a_busy_schedule() {
    let handler = Arc::new(TestHandler::new());
    let kernel = Kernel::builder()
        .clock(clock())
        .shutdown_timeout(PATIENCE)
        .component(HandlerComponent {
            handler: handler.clone(),
        })
        .component(SchedulerComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();
    for index in 0..32 {
        scheduler
            .schedule(
                JobSpec::new(
                    format!("job-{index}"),
                    Trigger::Every {
                        interval: Duration::from_millis(10),
                    },
                    HANDLER_COMPONENT,
                ),
                &anonymous(),
            )
            .await
            .unwrap();
    }
    advance(Duration::from_millis(100)).await;

    kernel
        .shutdown()
        .await
        .expect("a schedule with work in it still stops on time");
}
