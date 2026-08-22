//! Event-triggered scheduling, driven by a real kernel's event bus.
//!
//! The scheduler's own suite publishes events onto a bus it constructed itself. That proves
//! the matching logic and nothing about the wiring: in a real kernel the events come from
//! *other components*, carry a `source` the publisher stamped, and reach the scheduler
//! through a firehose subscription it takes out lazily. Three things there can only be
//! wrong once a kernel is holding it — whether another component's event arrives at all,
//! whether the scheduler's own events feed back into it, and whether a job fires under its
//! owner's authority when the trigger was somebody else's event.

use std::sync::Arc;
use std::time::Duration;

use aik_api::permission::PrincipalId;
use aik_api::scheduler::{JobCompleted, JobSpec, Scheduler, Trigger};
use aik_core::event::Event;
use aik_core::id::EventName;
use aik_core::prelude::*;
use serde::{Deserialize, Serialize};

mod support;
use support::{HandlerComponent, RecordingHandler, until, user};

const HANDLER: &str = "jobs.reactive";

/// An event some other subsystem publishes, which a job can be attached to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkArrived {
    /// Carried so the test can tell one publication from another.
    batch: u32,
}

impl Event for WorkArrived {
    const NAME: &'static str = "integration.work_arrived";
}

/// A component that publishes [`WorkArrived`] when asked, the way a real subsystem would —
/// through its own [`ComponentContext`], so the envelope carries *its* id as the source.
struct Publisher(Arc<std::sync::Mutex<Option<ComponentContext>>>);

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher").finish()
    }
}

#[async_trait]
impl Component for Publisher {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("work.source")
    }
    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        *self.0.lock().unwrap() = Some(ctx.clone());
        Ok(())
    }
}

/// A kernel with a volatile scheduler, a handler and a publishing component.
///
/// Volatile deliberately: an event trigger has no schedule position to persist, so this is
/// the wiring that isolates event delivery from everything the database does.
fn kernel(
    handler: Arc<RecordingHandler>,
    publisher: Arc<std::sync::Mutex<Option<ComponentContext>>>,
) -> Kernel {
    Kernel::builder()
        .component(Publisher(publisher))
        .component(HandlerComponent::new(HANDLER, handler))
        .component(aik_scheduler::SchedulerComponent::new())
        .build()
        .expect("a valid wiring")
}

fn on(event: &str) -> Trigger {
    Trigger::OnEvent {
        event: EventName::new(event),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_fires_on_an_event_another_component_published() {
    let handler = Arc::new(RecordingHandler::new());
    let publisher = Arc::new(std::sync::Mutex::new(None));
    let kernel = kernel(handler.clone(), publisher.clone());
    kernel.start().await.unwrap();

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(
            JobSpec::new("reactive", on(WorkArrived::NAME), HANDLER),
            &user("alice"),
        )
        .await
        .unwrap();

    let ctx = publisher.lock().unwrap().clone().expect("init ran");
    // The scheduler subscribes to the firehose only once some job needs it, and it takes
    // that subscription out on its driver task rather than inside `schedule`. Publishing
    // until it lands is what makes this test about delivery rather than about that race.
    until("the event-triggered job to run", async || {
        if handler.count() > 0 {
            return true;
        }
        ctx.publish(WorkArrived { batch: 1 });
        false
    })
    .await;

    let firing = &handler.firings()[0];
    assert_eq!(firing.job, "reactive");
    let principal = firing
        .principal
        .clone()
        .expect("a firing is never anonymous");
    assert!(
        principal.may_act_for(&PrincipalId::new("alice")),
        "an event nobody owns still fires the job under the authority of whoever scheduled it"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_the_job_did_not_name_leaves_it_alone() {
    let handler = Arc::new(RecordingHandler::new());
    let publisher = Arc::new(std::sync::Mutex::new(None));
    let kernel = kernel(handler.clone(), publisher.clone());
    kernel.start().await.unwrap();

    kernel
        .context()
        .service::<dyn Scheduler>()
        .unwrap()
        .schedule(
            JobSpec::new("reactive", on("integration.something_else"), HANDLER),
            &user("alice"),
        )
        .await
        .unwrap();

    let ctx = publisher.lock().unwrap().clone().expect("init ran");
    for batch in 0..32 {
        ctx.publish(WorkArrived { batch });
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(handler.count(), 0, "only the named event fires the job");
    kernel.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scheduler_does_not_feed_its_own_events_back_into_itself() {
    let handler = Arc::new(RecordingHandler::new());
    let publisher = Arc::new(std::sync::Mutex::new(None));
    let kernel = kernel(handler.clone(), publisher.clone());
    kernel.start().await.unwrap();
    let scheduler = kernel.context().service::<dyn Scheduler>().unwrap();

    // A job triggered by the very event its own completion publishes. Left unguarded this
    // is not a slow leak but an unbounded loop, and it is reachable by configuration alone.
    scheduler
        .schedule(
            JobSpec::new("ouroboros", on(JobCompleted::NAME), HANDLER),
            &user("alice"),
        )
        .await
        .unwrap();
    // Something that will actually complete, and so publish the triggering event.
    scheduler
        .schedule(
            JobSpec::new(
                "real",
                Trigger::After {
                    delay: Duration::from_millis(20),
                },
                HANDLER,
            ),
            &user("alice"),
        )
        .await
        .unwrap();

    until("the one-shot job to run", async || handler.count() > 0).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        handler.count(),
        1,
        "the scheduler's own output must not retrigger it; a second firing here is the first \
         turn of a loop that never ends"
    );
    assert_eq!(handler.firings()[0].job, "real");

    drop(scheduler);
    kernel.shutdown().await.unwrap();
}
