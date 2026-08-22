//! [`JobScheduler`]: one engine, wired either way.
//!
//! The volatile and persistent schedulers are not two implementations. They are this one,
//! holding either no [`JobStore`] or a durable one, because everything that could differ
//! between them — when a job is due, what happens to a firing that overruns, who a job runs
//! as — is exactly what must *not* differ. See [`crate::store`] for why a store is a mirror
//! rather than a schedule.
//!
//! # The driver
//!
//! One task, spawned in the component's scope, doing four things and nothing else: sleeping
//! until the earliest due job, waking when the schedule changes, watching the event bus when
//! (and only when) some job is waiting on an event, and stopping when the scope is cancelled.
//! Firings are dispatched into their own tasks, so a slow handler delays nothing but its own
//! job's next occurrence.
//!
//! # Locking
//!
//! The schedule is behind an async mutex that is held across the store write a mutation
//! needs, so that "what is on disk" and "what is in memory" cannot be observed disagreeing;
//! the set of running jobs is behind a plain one, so that a firing's slot can be released
//! from `Drop`. Both are taken in that order, and only ever briefly.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::scheduler::{
    JobFailed, JobHandler, JobId, JobSkipped, JobSpec, RunId, ScheduledJob, Scheduler, SkipReason,
};
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::event::{Envelope, EventBus, EventStream, RecvError};
use aik_core::id::{ComponentId, CorrelationId, EventName};
use aik_core::task::Tasks;
use aik_core::{ComponentContext, Error, Result};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::events::Publisher;
use crate::owner::{authorize, principal_of};
use crate::runner::{Firing, RunGuard, RunSlot, Running, execute};
use crate::state::{JobState, Recovery, first_run, next_run, recover, validate};
use crate::store::JobStore;

/// How far back a missed firing is still worth running, when nothing says otherwise.
///
/// An hour is short enough that a reminder does not arrive from another working day, and long
/// enough that a reboot, an update or a laptop lid does not silently swallow the morning's
/// work. See [`recover`] for why there is a window at all.
pub const DEFAULT_CATCH_UP_WINDOW: Duration = Duration::from_secs(60 * 60);

/// How long the driver waits after failing to record a firing before trying again.
///
/// A firing is claimed by writing the advanced schedule *before* the handler runs, so a store
/// that will not accept the write leaves the job due — and a driver that simply looped would
/// spin on a broken disk at whatever rate the failure returns. Backing off turns that into a
/// slow, visible retry.
const STORE_FAILURE_BACKOFF: Duration = Duration::from_secs(1);

/// What the scheduler needs from the kernel, other than the schedule itself.
///
/// Assembled from a [`ComponentContext`] in the ordinary case, or by hand in a test. It
/// deliberately does *not* hold the context itself: the context reaches the registry, the
/// registry holds the scheduler, and that cycle would keep the whole kernel — including the
/// open database file — alive for as long as the process.
#[derive(Clone)]
pub struct SchedulerRuntime {
    component: ComponentId,
    clock: SharedClock,
    events: EventBus,
    tasks: Tasks,
}

impl std::fmt::Debug for SchedulerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerRuntime")
            .field("component", &self.component)
            .finish_non_exhaustive()
    }
}

impl SchedulerRuntime {
    /// Assembles a runtime explicitly.
    pub fn new(
        component: impl Into<ComponentId>,
        clock: SharedClock,
        events: EventBus,
        tasks: Tasks,
    ) -> Self {
        Self {
            component: component.into(),
            clock,
            events,
            tasks,
        }
    }

    /// Takes everything from a component's context: its identity, the kernel clock, the event
    /// bus and the component's own task scope.
    pub fn from_component(ctx: &ComponentContext) -> Self {
        Self::new(
            ctx.id().clone(),
            ctx.clock().clone(),
            ctx.events().clone(),
            ctx.tasks().clone(),
        )
    }
}

/// A [`Scheduler`] that runs jobs from one process.
pub struct JobScheduler {
    publisher: Publisher,
    tasks: Tasks,
    store: Option<Arc<dyn JobStore>>,
    catch_up_window: Duration,
    schedule: Mutex<BTreeMap<JobId, JobState>>,
    running: Running,
    handlers: OnceLock<HashMap<ComponentId, Arc<dyn JobHandler>>>,
    wake: Notify,
}

impl std::fmt::Debug for JobScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobScheduler")
            .field("persistent", &self.store.is_some())
            .field("catch_up_window", &self.catch_up_window)
            .field("started", &self.handlers.get().is_some())
            .finish_non_exhaustive()
    }
}

impl JobScheduler {
    /// A scheduler that keeps its schedule in memory, and refuses persistent jobs.
    pub fn volatile(runtime: SchedulerRuntime) -> Self {
        Self::build(runtime, None)
    }

    /// A scheduler that mirrors its persistent jobs into `store`.
    pub fn persistent(runtime: SchedulerRuntime, store: Arc<dyn JobStore>) -> Self {
        Self::build(runtime, Some(store))
    }

    fn build(runtime: SchedulerRuntime, store: Option<Arc<dyn JobStore>>) -> Self {
        Self {
            publisher: Publisher::new(runtime.events, runtime.component, runtime.clock),
            tasks: runtime.tasks,
            store,
            catch_up_window: DEFAULT_CATCH_UP_WINDOW,
            schedule: Mutex::new(BTreeMap::new()),
            running: Running::default(),
            handlers: OnceLock::new(),
            wake: Notify::new(),
        }
    }

    /// Overrides how far back a missed firing is still worth running.
    #[must_use]
    pub fn with_catch_up_window(mut self, window: Duration) -> Self {
        self.catch_up_window = window;
        self
    }

    /// Whether this scheduler can honour [`JobSpec::persistent`].
    pub fn is_persistent(&self) -> bool {
        self.store.is_some()
    }

    /// Reads the schedule back, recovers what was missed, and starts the driver.
    ///
    /// `handlers` is the set of job handlers this scheduler will ever use, snapshotted from
    /// the kernel registry by the component that owns it. A snapshot rather than a live
    /// lookup for two reasons: every component has finished `init` — and therefore has
    /// published everything it publishes — before any component is started, so the snapshot is
    /// complete; and holding the registry would mean holding the context that owns it, which
    /// is the cycle [`SchedulerRuntime`] exists to avoid.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read, which fails the component and therefore the
    /// kernel's startup. That is deliberate: a scheduler that started anyway would be a
    /// system that looks healthy while its durable jobs quietly do not exist.
    pub async fn start(
        self: &Arc<Self>,
        handlers: HashMap<ComponentId, Arc<dyn JobHandler>>,
    ) -> Result<()> {
        self.handlers
            .set(handlers)
            .map_err(|_| Error::Lifecycle("the scheduler has already been started".into()))?;

        self.recover_persisted().await?;

        let driver = Arc::clone(self);
        self.tasks
            .spawn_cancellable("scheduler.driver", move |token| driver.drive(token));
        Ok(())
    }

    /// Stops the driver and asks every firing in flight to stop.
    ///
    /// Does not wait for them: the kernel's own shutdown waits for the whole task scope, with
    /// one deadline shared by every component, which is the only place that decision can be
    /// made coherently.
    pub fn stop(&self) {
        self.tasks.cancel();
    }

    /// Loads persisted jobs and decides what to do about anything that came due while nothing
    /// was running.
    async fn recover_persisted(&self) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let loaded = store.load().await?;
        let now = self.publisher.now();
        let mut schedule = self.schedule.lock().await;

        for mut state in loaded {
            // A job scheduled since this process started is more recent than anything on
            // disk, and must not be overwritten by the row it itself replaced.
            if schedule.contains_key(&state.spec.id) {
                continue;
            }

            match recover(&state, now, self.catch_up_window) {
                // Overdue needs no special handling: leaving `next_run` in the past is exactly
                // what makes the driver fire it once, on its first pass, and then advance the
                // schedule past every occurrence that elapsed.
                Recovery::Pending | Recovery::Overdue(_) => {
                    schedule.insert(state.spec.id.clone(), state);
                }
                Recovery::Expired { missed, next } => {
                    self.publish_skipped(&state, missed, SkipReason::Missed);
                    match next {
                        Some(next) => {
                            state.next_run = Some(next);
                            store.put(&state).await?;
                            schedule.insert(state.spec.id.clone(), state);
                        }
                        None => store.remove(&state.spec.id).await?,
                    }
                }
            }
        }
        Ok(())
    }

    /// The driver: sleep, wake, fire, repeat.
    async fn drive(self: Arc<Self>, token: CancellationToken) {
        let mut firehose: Option<EventStream<Value>> = None;

        loop {
            let (delay, wants_events) = self.horizon().await;
            match (wants_events, firehose.is_some()) {
                // Subscribed only while some job is actually waiting on an event: a firehose
                // subscriber makes the bus serialise every event in the system to JSON, and a
                // scheduler with no event-triggered jobs has no business imposing that.
                (true, false) => firehose = Some(self.publisher.subscribe_any()),
                (false, true) => firehose = None,
                _ => {}
            }

            let woke = tokio::select! {
                () = token.cancelled() => Woke::Cancelled,
                () = self.wake.notified() => Woke::Rescheduled,
                () = sleep_for(delay) => Woke::Due,
                event = next_event(firehose.as_mut()) => match event {
                    Some(envelope) => Woke::Event(Box::new(envelope)),
                    None => Woke::BusClosed,
                },
            };

            match woke {
                Woke::Cancelled => break,
                Woke::Rescheduled => {}
                Woke::Due => {
                    if self.fire_due().await {
                        tokio::select! {
                            () = token.cancelled() => break,
                            () = tokio::time::sleep(STORE_FAILURE_BACKOFF) => {}
                        }
                    }
                }
                Woke::Event(envelope) => {
                    // A job triggered by an event the scheduler itself published would
                    // retrigger on its own completion, for ever. Ignoring its own output is
                    // the cheapest way to make that impossible; chaining a job off another
                    // job's outcome needs loop detection, and is better added deliberately
                    // than discovered at three in the morning.
                    if envelope.metadata.source.as_ref() != Some(self.publisher.component()) {
                        self.fire_for_event(&envelope.metadata.name).await;
                    }
                }
                Woke::BusClosed => firehose = None,
            }
        }

        tracing::debug!("scheduler driver stopped");
    }

    /// How long until the earliest due job, and whether anything is waiting on an event.
    async fn horizon(&self) -> (Option<Duration>, bool) {
        let now = self.publisher.now();
        let schedule = self.schedule.lock().await;

        let mut earliest: Option<Timestamp> = None;
        let mut wants_events = false;
        for state in schedule.values() {
            if state.is_event_driven() {
                wants_events = true;
                continue;
            }
            if let Some(next) = state.next_run {
                earliest = Some(earliest.map_or(next, |current: Timestamp| current.min(next)));
            }
        }

        (
            earliest.map(|next| next.saturating_since(now)),
            wants_events,
        )
    }

    /// Fires every job whose time has come. Returns whether the store refused a claim.
    async fn fire_due(&self) -> bool {
        let now = self.publisher.now();
        let due: Vec<JobId> = {
            let schedule = self.schedule.lock().await;
            schedule
                .values()
                .filter(|state| !state.is_event_driven())
                .filter(|state| state.next_run.is_some_and(|next| next <= now))
                .map(|state| state.spec.id.clone())
                .collect()
        };

        let mut store_failed = false;
        for id in due {
            store_failed |= matches!(self.claim(&id, now).await, Claim::StoreFailed);
        }
        store_failed
    }

    /// Claims one due firing: advances the schedule durably, then dispatches it.
    ///
    /// The order is the whole of the at-most-once guarantee. The advanced schedule is
    /// committed *before* the handler is called, so a process that dies mid-firing comes back
    /// to a job whose next occurrence is in the future — the firing is lost rather than
    /// repeated. For work whose side effects the kernel cannot see (a message sent, a model
    /// called and paid for) losing one is the recoverable failure and repeating one is not.
    async fn claim(&self, id: &JobId, now: Timestamp) -> Claim {
        let mut schedule = self.schedule.lock().await;

        let Some(state) = schedule.get(id) else {
            return Claim::Gone;
        };
        let Some(scheduled_for) = state.next_run.filter(|next| *next <= now) else {
            return Claim::Gone;
        };

        let mut advanced = state.clone();
        advanced.last_run = Some(scheduled_for);
        advanced.next_run = next_run(&advanced.spec.trigger, scheduled_for, now);

        if advanced.spec.persistent
            && let Some(store) = &self.store
        {
            let written = match advanced.next_run {
                Some(_) => store.put(&advanced).await,
                // A one-shot job is gone the moment it is claimed rather than after it has
                // run, so that a crash mid-firing cannot bring it back.
                None => store.remove(id).await,
            };
            if let Err(error) = written {
                tracing::error!(
                    job = %id,
                    %error,
                    "could not record a firing; the job stays due and will be retried"
                );
                return Claim::StoreFailed;
            }
        }

        match advanced.next_run {
            Some(_) => {
                schedule.insert(id.clone(), advanced.clone());
            }
            None => {
                schedule.remove(id);
            }
        }

        if self.is_running(id) {
            drop(schedule);
            self.publish_skipped(&advanced, scheduled_for, SkipReason::AlreadyRunning);
            return Claim::Skipped;
        }

        self.dispatch(&advanced, scheduled_for);
        Claim::Dispatched
    }

    /// Fires every job waiting on `event`.
    ///
    /// An event-triggered job has no schedule to advance, so nothing is written: `last_run` is
    /// updated in memory and is deliberately not durable for these jobs. Persisting it would
    /// mean a write transaction for every matching event on the bus, to record a field nothing
    /// makes a decision from.
    async fn fire_for_event(&self, event: &EventName) {
        let now = self.publisher.now();
        let waiting: Vec<JobId> = {
            let schedule = self.schedule.lock().await;
            schedule
                .values()
                .filter(|state| state.awaited_event() == Some(event))
                .map(|state| state.spec.id.clone())
                .collect()
        };

        for id in waiting {
            let fired = {
                let mut schedule = self.schedule.lock().await;
                let Some(state) = schedule.get_mut(&id) else {
                    continue;
                };
                state.last_run = Some(now);
                state.clone()
            };

            if self.is_running(&id) {
                self.publish_skipped(&fired, now, SkipReason::AlreadyRunning);
            } else {
                self.dispatch(&fired, now);
            }
        }
    }

    /// Starts one firing in its own task.
    fn dispatch(&self, state: &JobState, scheduled_for: Timestamp) {
        let run = RunId::new();

        let Some(handlers) = self.handlers.get() else {
            tracing::error!(job = %state.spec.id, "a firing was dispatched before the scheduler started");
            return;
        };
        let Some(handler) = handlers.get(&state.spec.handler).cloned() else {
            self.publish_missing_handler(state, run);
            return;
        };

        let token = self.tasks.cancellation_token().child_token();
        let slot = RunGuard::claim(
            Arc::clone(&self.running),
            state.spec.id.clone(),
            RunSlot {
                run,
                token: token.clone(),
                owner: state.owner.id.clone(),
            },
        );
        let firing = Firing {
            spec: state.spec.clone(),
            owner: state.owner.clone(),
            run,
            scheduled_for,
            token,
        };
        let publisher = self.publisher.clone();

        self.tasks.spawn(
            format!("scheduler.run.{}", state.spec.id),
            execute(publisher, handler, firing, slot),
        );
    }

    fn is_running(&self, id: &JobId) -> bool {
        self.run_slot(id).is_some()
    }

    /// The firing occupying a job's exclusion slot, if there is one.
    fn run_slot(&self, id: &JobId) -> Option<RunSlot> {
        self.running
            .lock()
            .expect("the running-job lock is never held across a panic")
            .get(id)
            .cloned()
    }

    fn publish_skipped(&self, state: &JobState, scheduled_for: Timestamp, reason: SkipReason) {
        let correlation = CorrelationId::new();
        tracing::info!(job = %state.spec.id, ?reason, "a scheduled firing was skipped");
        self.publisher.publish(
            JobSkipped {
                event: self.publisher.job_event(
                    &state.spec.id,
                    &state.spec.handler,
                    &state.owner,
                    RunId::new(),
                    correlation,
                ),
                scheduled_for,
                reason,
            },
            correlation,
        );
    }

    /// Reports a firing whose handler is not registered.
    ///
    /// A failure with no [`JobStarted`](aik_api::scheduler::JobStarted) before it, because
    /// nothing started. It is never retried: the handler set is fixed when the scheduler
    /// starts, so waiting and asking again cannot produce a different answer.
    fn publish_missing_handler(&self, state: &JobState, run: RunId) {
        let correlation = CorrelationId::new();
        let error = Error::not_found("job handler", &state.spec.handler);
        tracing::error!(
            job = %state.spec.id,
            handler = %state.spec.handler,
            "a scheduled job names a handler that is not registered"
        );
        self.publisher.publish(
            JobFailed {
                event: self.publisher.job_event(
                    &state.spec.id,
                    &state.spec.handler,
                    &state.owner,
                    run,
                    correlation,
                ),
                attempt: 0,
                duration_ms: 0,
                kind: format!("{:?}", error.kind()).to_lowercase(),
                error: error.to_string(),
                will_retry: false,
            },
            correlation,
        );
    }
}

#[async_trait]
impl Scheduler for JobScheduler {
    /// Schedules a job, replacing any job already using that id.
    ///
    /// A replacement keeps the owner and the `last_run` the id already had, and recomputes
    /// `next_run` from the new trigger as though the job had just been scheduled. It
    /// deliberately does **not** disturb a firing already in flight: replacing a job says
    /// something about when it runs next, not about the run happening now — and a handler
    /// reprogramming its own schedule from inside its own firing would otherwise be asking to
    /// be cancelled. The next firing still cannot overlap the old one, because exclusion is
    /// keyed by job id rather than by definition.
    async fn schedule(&self, spec: JobSpec, cx: &ExecutionContext) -> Result<()> {
        // Fails closed rather than accepting a job the driver is no longer there to run.
        if self.tasks.is_cancelled() {
            return Err(Error::Cancelled);
        }
        validate(&spec, self.is_persistent())?;

        let principal = principal_of(cx);
        let now = self.publisher.now();
        let mut schedule = self.schedule.lock().await;

        let previous = schedule.get(&spec.id);
        let owner = match previous {
            Some(previous) => {
                authorize(&spec.id, &previous.owner.id, &principal)?;
                previous.owner.clone()
            }
            None => principal,
        };
        let was_persistent = previous.is_some_and(|previous| previous.spec.persistent);
        let last_run = previous.and_then(|previous| previous.last_run);

        let state = JobState {
            next_run: first_run(&spec.trigger, now),
            last_run,
            owner,
            spec,
        };

        // Durable before in-memory, so a caller told the job is scheduled is never told so
        // about a job that is not on disk.
        if let Some(store) = &self.store {
            if state.spec.persistent {
                store.put(&state).await?;
            } else if was_persistent {
                // Replacing a persistent job with a volatile one has to take the old row with
                // it; leaving it would resurrect a job the caller replaced.
                store.remove(&state.spec.id).await?;
            }
        }

        tracing::debug!(job = %state.spec.id, owner = %state.owner.id, next_run = ?state.next_run, "job scheduled");
        schedule.insert(state.spec.id.clone(), state);
        drop(schedule);

        self.wake.notify_one();
        Ok(())
    }

    /// Cancels a job, returning whether there was one to cancel.
    ///
    /// Two things live under one id and both are cancelled here: the entry in the schedule,
    /// removed durably if it was persistent, and the firing in flight, if there is one.
    ///
    /// Both are checked, not just the first. A one-shot job leaves the schedule the moment it
    /// is claimed — that is what makes it fire at most once — so between then and its handler
    /// returning it is running but not scheduled, and a `cancel` that only consulted the
    /// schedule would report "no such job" about a job the caller can watch running.
    ///
    /// The firing is stopped *cooperatively*: its context is cancelled and any pending retry
    /// is abandoned, but nothing is aborted. A handler that ignores cancellation runs to
    /// completion and its outcome is still published.
    ///
    /// The two can have different owners — a job that left the schedule while still running
    /// frees its id for somebody else — so both are authorised before either is touched, and
    /// a refusal leaves the schedule exactly as it was.
    async fn cancel(&self, id: &JobId, cx: &ExecutionContext) -> Result<bool> {
        let principal = principal_of(cx);
        let mut schedule = self.schedule.lock().await;

        let scheduled = schedule.get(id).cloned();
        // The slot itself is left in place; its guard releases it when the run ends. Removing
        // it here would let a job rescheduled under the same id start a second firing while
        // the first was still winding down.
        let in_flight = self.run_slot(id);

        // Both authorisations before either mutation, because the entry and the firing need
        // not have the same owner: a job that has left the schedule while still running frees
        // its id, so somebody else may hold the entry while the firing is somebody else's. A
        // refusal has to leave the schedule exactly as it was, not half-cancelled.
        if let Some(existing) = &scheduled {
            authorize(id, &existing.owner.id, &principal)?;
        }
        if let Some(slot) = &in_flight {
            // Authorised against the firing's own record of its owner, which is the same
            // owner the schedule held when it was claimed — and is still available once the
            // schedule no longer holds it.
            authorize(id, &slot.owner, &principal)?;
        }

        if let Some(existing) = &scheduled {
            if existing.spec.persistent
                && let Some(store) = &self.store
            {
                store.remove(id).await?;
            }
            schedule.remove(id);
        }
        drop(schedule);

        if let Some(slot) = &in_flight {
            slot.token.cancel();
        }

        if scheduled.is_none() && in_flight.is_none() {
            return Ok(false);
        }

        tracing::debug!(job = %id, "job cancelled");
        self.wake.notify_one();
        Ok(true)
    }

    /// Lists the jobs `cx` may act for, ordered by id.
    async fn list(&self, cx: &ExecutionContext) -> Result<Vec<ScheduledJob>> {
        let principal = principal_of(cx);
        let schedule = self.schedule.lock().await;
        Ok(schedule
            .values()
            .filter(|state| principal.may_act_for(&state.owner.id))
            .map(JobState::to_scheduled)
            .collect())
    }
}

/// What ended one turn of the driver loop.
///
/// Separate from the `select!` so that the branch that has to *mutate* the firehose
/// subscription can do it after the borrows the select holds have been released.
enum Woke {
    Cancelled,
    Rescheduled,
    Due,
    Event(Box<Envelope<Value>>),
    BusClosed,
}

/// The outcome of trying to claim one due firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// The firing was handed to a task.
    Dispatched,
    /// A previous firing of the same job was still running.
    Skipped,
    /// The job was cancelled or replaced between being found due and being claimed.
    Gone,
    /// The schedule could not be advanced durably, so nothing was run.
    StoreFailed,
}

/// Sleeps for `delay`, or for ever when there is nothing due.
async fn sleep_for(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

/// The next event off the firehose, skipping lag, or never when nothing is subscribed.
///
/// Lag means the scheduler missed events, and therefore missed firings of any job waiting on
/// them. There is nothing to be done about it after the fact — the events are gone — so it is
/// reported loudly and the subscription carries on rather than being torn down.
async fn next_event(stream: Option<&mut EventStream<Value>>) -> Option<Envelope<Value>> {
    let Some(stream) = stream else {
        return std::future::pending().await;
    };
    loop {
        match stream.recv().await {
            Ok(envelope) => return Some(envelope),
            Err(RecvError::Lagged { count }) => {
                tracing::error!(
                    missed = count,
                    "the scheduler fell behind the event bus; event-triggered jobs missed firings"
                );
            }
            Err(RecvError::Closed) => return None,
        }
    }
}
