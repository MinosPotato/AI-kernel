//! Publishing what the scheduler did, and what it did not do.
//!
//! Every firing publishes its lifecycle on the kernel bus rather than only into a log,
//! because unattended work is the kind nobody is watching: a job that stopped running is
//! invisible until something notices, and `tracing` output is not something a UI, a
//! supervisor or an out-of-process bridge can subscribe to. The events are defined in
//! [`aik_api::scheduler`]; what lives here is the small amount of assembly they need.
//!
//! Two rules the assembly enforces, both borrowed from
//! [`audit`](aik_api::audit#what-these-events-must-never-carry):
//!
//! * the job's payload is never copied into an event;
//! * every event carries the firing's correlation id, so the events describing a run and
//!   everything the run itself did join on one key.

use aik_api::permission::Principal;
use aik_api::scheduler::{JobEvent, JobId, RunId};
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::event::{Envelope, Event, EventBus};
use aik_core::id::{ComponentId, CorrelationId};

/// Publishes scheduler events, attributed to the scheduler component.
///
/// Holds an [`EventBus`] and a [`ComponentId`] rather than a
/// [`ComponentContext`](aik_core::ComponentContext), which would reach the registry that
/// holds the scheduler and make the whole kernel context unreclaimable through that cycle —
/// and, with it, the database file the registry keeps open.
#[derive(Clone)]
pub(crate) struct Publisher {
    events: EventBus,
    component: ComponentId,
    clock: SharedClock,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("component", &self.component)
            .finish()
    }
}

impl Publisher {
    /// Creates a publisher attributing everything to `component`.
    pub(crate) fn new(events: EventBus, component: ComponentId, clock: SharedClock) -> Self {
        Self {
            events,
            component,
            clock,
        }
    }

    /// The current time, by the kernel clock.
    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// The component everything published here is attributed to.
    pub(crate) fn component(&self) -> &ComponentId {
        &self.component
    }

    /// Subscribes to every event on the bus, as JSON.
    ///
    /// The only way to watch for an event named at runtime: a typed subscription needs the
    /// Rust type, and a job's [`Trigger::OnEvent`](aik_api::scheduler::Trigger::OnEvent)
    /// names an event the scheduler has never heard of and cannot link against.
    pub(crate) fn subscribe_any(&self) -> aik_core::event::EventStream<serde_json::Value> {
        self.events.subscribe_any()
    }

    /// Publishes one event, attributed to the scheduler and tied to a firing.
    pub(crate) fn publish<E: Event>(&self, event: E, correlation: CorrelationId) {
        let metadata = self
            .events
            .metadata_for::<E>()
            .with_source(self.component.clone())
            .with_correlation(correlation);
        self.events.publish_envelope(Envelope::new(metadata, event));
    }

    /// Assembles the fields every job event carries.
    pub(crate) fn job_event(
        &self,
        job: &JobId,
        handler: &ComponentId,
        owner: &Principal,
        run: RunId,
        correlation: CorrelationId,
    ) -> JobEvent {
        JobEvent {
            job: job.clone(),
            run,
            handler: handler.clone(),
            correlation,
            timestamp: self.clock.now(),
            owner: owner.id.clone(),
            owner_kind: owner.kind,
        }
    }
}
