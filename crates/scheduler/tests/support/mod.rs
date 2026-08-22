//! Fixtures shared by the scheduler test suites.
//!
//! Every behavioural assertion is written once and run against both wirings, because the
//! persistent scheduler's job is to be indistinguishable from the volatile one except for
//! surviving a restart. A test that only ever ran against one of them would let the two drift
//! — which is exactly the arrangement the memory and context stores already use.
//!
//! # Time
//!
//! The clock the scheduler reads is [`TokioClock`], which reports tokio's own time. Under
//! `#[tokio::test(start_paused = true)]` that makes the whole suite deterministic and
//! instantaneous: advancing tokio's clock advances the scheduler's, so a job due in an hour
//! fires when the test says so and not a moment of real time later. A [`ManualClock`] would
//! move the scheduler's idea of "now" without moving the timer it is sleeping on, which is
//! precisely the pair that has to stay in step.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Principal, PrincipalKind};
use aik_api::scheduler::{JobHandler, JobId, JobSpec, Scheduler};
use aik_core::clock::{Clock, Timestamp};
use aik_core::event::{Event, EventBus, EventStream};
use aik_core::id::{ComponentId, CorrelationId};
use aik_core::task::Tasks;
use aik_core::{Error, Result};
use aik_scheduler::{JobScheduler, JobStore, RedbJobStore, SchedulerRuntime};
use aik_store::Db;
use async_trait::async_trait;
use serde_json::Value;
use tempfile::TempDir;

/// How long a test waits for something it expects to *arrive*.
///
/// Wall-clock generosity costs nothing here: the runtime's clock is paused, so waiting for an
/// event that is coming resolves as fast as the tasks can run, and only a test that is
/// actually wrong ever waits.
pub(crate) const PATIENCE: Duration = Duration::from_secs(30);

/// How many times a spin-wait yields before it gives up.
///
/// Spin-waits are bounded by iterations rather than by a timer, and that is not a stylistic
/// choice: a loop that yields keeps the runtime busy, a paused clock only advances when the
/// runtime is *idle*, and so a `timeout` wrapped around a spin-wait can never fire. Bounding
/// the spin instead is what makes a broken assertion a failing test rather than a hung one.
const SPINS: usize = 100_000;

/// Yields until `condition` holds, failing the test rather than hanging if it never does.
pub(crate) async fn until(what: &str, mut condition: impl AsyncFnMut() -> bool) {
    for _ in 0..SPINS {
        if condition().await {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("gave up waiting for {what}");
}

/// Where the fixture's clock starts.
///
/// Not the epoch: a job scheduled at time zero has a `next_run` that is trivially distinct
/// from "unset", and a suite that only ever ran from zero would not notice code that confused
/// the two.
pub(crate) const START: Timestamp = Timestamp::from_millis(1_700_000_000_000);

/// A clock that reports tokio's time, so that pausing and advancing the runtime moves the
/// scheduler's idea of now with it.
#[derive(Debug)]
pub(crate) struct TokioClock {
    base: Timestamp,
    origin: tokio::time::Instant,
}

impl TokioClock {
    /// Starts a clock at `base`. Must be called inside a tokio runtime.
    pub(crate) fn new(base: Timestamp) -> Self {
        Self {
            base,
            origin: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        self.base.saturating_add(self.origin.elapsed())
    }
}

/// Moves the runtime's clock, and with it the scheduler's.
pub(crate) async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
}

/// Which wiring a suite function is being run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// [`JobScheduler::volatile`]: the schedule is forgotten at shutdown.
    Volatile,
    /// [`JobScheduler::persistent`]: persistent jobs live in a redb database.
    Redb,
}

impl Backend {
    /// Whether this wiring accepts [`JobSpec::persistent`].
    pub(crate) fn persists(self) -> bool {
        self == Self::Redb
    }
}

/// One call a handler received.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    /// Which job was running.
    pub job: JobId,
    /// The principal the firing carried.
    pub principal: Option<Principal>,
    /// The firing's correlation id, shared by its attempts.
    pub correlation: CorrelationId,
    /// The deadline the firing was given, if any.
    pub deadline: Option<Timestamp>,
    /// The job's payload, as the handler saw it.
    pub payload: Value,
}

/// What a [`TestHandler`] does when it is called.
#[derive(Debug, Clone, Copy)]
enum Behaviour {
    /// Return immediately.
    Succeed,
    /// Fail the first `n` calls, then succeed.
    FailFirst(usize),
    /// Fail every call.
    AlwaysFail,
    /// Block until released or cancelled.
    Hold,
    /// Sleep, for testing deadlines.
    Sleep(Duration),
}

/// A job handler that records what it was asked to do and can be told how to behave.
///
/// One type rather than five near-identical ones, because every test needs the recording and
/// only differs in what the handler does with the call.
#[derive(Debug)]
pub(crate) struct TestHandler {
    behaviour: Behaviour,
    ignore_cancellation: bool,
    calls: Mutex<Vec<Call>>,
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    observed_cancellation: AtomicBool,
    release: tokio::sync::Notify,
}

impl Default for TestHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHandler {
    /// A handler that records the call and succeeds.
    pub(crate) fn new() -> Self {
        Self {
            behaviour: Behaviour::Succeed,
            ignore_cancellation: false,
            calls: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            observed_cancellation: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
        }
    }

    /// Fails the first `attempts` calls and succeeds afterwards.
    pub(crate) fn failing_first(mut self, attempts: usize) -> Self {
        self.behaviour = Behaviour::FailFirst(attempts);
        self
    }

    /// Fails every call.
    pub(crate) fn always_failing(mut self) -> Self {
        self.behaviour = Behaviour::AlwaysFail;
        self
    }

    /// Blocks until [`TestHandler::release`] is called, or until cancelled.
    pub(crate) fn holding(mut self) -> Self {
        self.behaviour = Behaviour::Hold;
        self
    }

    /// Sleeps for `duration`, for exercising a deadline.
    pub(crate) fn sleeping(mut self, duration: Duration) -> Self {
        self.behaviour = Behaviour::Sleep(duration);
        self
    }

    /// Makes a holding handler deaf to cancellation, so that a run has to be waited out
    /// rather than interrupted.
    pub(crate) fn deaf_to_cancellation(mut self) -> Self {
        self.ignore_cancellation = true;
        self
    }

    /// Lets a holding handler finish.
    pub(crate) fn release(&self) {
        self.release.notify_waiters();
    }

    /// Every call so far.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("no test panics here").clone()
    }

    /// How many calls so far.
    pub(crate) fn count(&self) -> usize {
        self.calls.lock().expect("no test panics here").len()
    }

    /// The most calls that were ever in flight at once.
    pub(crate) fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }

    /// Whether a call ever saw its context cancelled.
    pub(crate) fn observed_cancellation(&self) -> bool {
        self.observed_cancellation.load(Ordering::SeqCst)
    }

    /// Waits until the handler has been called at least `count` times.
    pub(crate) async fn wait_for_calls(&self, count: usize) {
        until(&format!("{count} calls to the handler"), async || {
            self.count() >= count
        })
        .await;
    }
}

#[async_trait]
impl JobHandler for TestHandler {
    async fn run(&self, job: &JobSpec, cx: &ExecutionContext) -> Result<()> {
        let attempt = {
            let mut calls = self.calls.lock().expect("no test panics here");
            calls.push(Call {
                job: job.id.clone(),
                principal: cx.principal.clone(),
                correlation: cx.correlation,
                deadline: cx.deadline,
                payload: job.payload.clone(),
            });
            calls.len()
        };

        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_in_flight.fetch_max(in_flight, Ordering::SeqCst);
        let outcome = self.act(attempt, cx).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        outcome
    }
}

impl TestHandler {
    async fn act(&self, attempt: usize, cx: &ExecutionContext) -> Result<()> {
        match self.behaviour {
            Behaviour::Succeed => Ok(()),
            Behaviour::AlwaysFail => Err(Error::other("this handler always fails")),
            Behaviour::FailFirst(attempts) if attempt <= attempts => {
                Err(Error::other(format!("attempt {attempt} fails")))
            }
            Behaviour::FailFirst(_) => Ok(()),
            Behaviour::Sleep(duration) => {
                tokio::time::sleep(duration).await;
                Ok(())
            }
            Behaviour::Hold if self.ignore_cancellation => {
                self.release.notified().await;
                Ok(())
            }
            Behaviour::Hold => {
                tokio::select! {
                    () = self.release.notified() => Ok(()),
                    () = cx.cancelled() => {
                        self.observed_cancellation.store(true, Ordering::SeqCst);
                        Err(Error::Cancelled)
                    }
                }
            }
        }
    }
}

/// A scheduler, plus everything that has to stay alive for it to keep working.
pub(crate) struct Fixture {
    scheduler: Option<Arc<JobScheduler>>,
    db: Option<Arc<Db>>,
    tasks: Tasks,
    events: EventBus,
    clock: Arc<TokioClock>,
    handlers: HashMap<ComponentId, Arc<TestHandler>>,
    catch_up_window: Option<Duration>,
    backend: Backend,
    directory: Option<TempDir>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Fixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fixture")
            .field("backend", &self.backend)
            .field("path", &self.path)
            .finish()
    }
}

impl Fixture {
    /// Prepares an unstarted fixture. Handlers are added before [`Fixture::start`], the same
    /// way the kernel registers them before any component starts.
    pub(crate) fn open(backend: Backend) -> Self {
        let (directory, path) = match backend {
            Backend::Volatile => (None, None),
            Backend::Redb => {
                let directory = tempfile::tempdir().expect("a temporary directory");
                let path = directory.path().join("aik.redb");
                (Some(directory), Some(path))
            }
        };

        Self {
            scheduler: None,
            db: None,
            tasks: Tasks::new(),
            events: EventBus::default(),
            clock: Arc::new(TokioClock::new(START)),
            handlers: HashMap::new(),
            catch_up_window: None,
            backend,
            directory,
            path,
        }
    }

    /// Registers a handler under an id a job can name.
    pub(crate) fn with_handler(mut self, id: &str, handler: TestHandler) -> Self {
        self.handlers
            .insert(ComponentId::new(id), Arc::new(handler));
        self
    }

    /// Overrides how far back a missed firing is still worth running.
    pub(crate) fn with_catch_up_window(mut self, window: Duration) -> Self {
        self.catch_up_window = Some(window);
        self
    }

    /// Builds the scheduler and starts its driver.
    pub(crate) async fn start(mut self) -> Self {
        self.boot().await.expect("the scheduler starts");
        self
    }

    async fn boot(&mut self) -> Result<()> {
        let runtime = SchedulerRuntime::new(
            "scheduler.jobs",
            self.clock.clone(),
            self.events.clone(),
            self.tasks.clone(),
        );
        let mut scheduler = match self.backend {
            Backend::Volatile => JobScheduler::volatile(runtime),
            Backend::Redb => {
                let db = Arc::new(Db::open(self.path()).expect("the database opens"));
                self.db = Some(db.clone());
                let store: Arc<dyn JobStore> =
                    Arc::new(RedbJobStore::new(db).expect("the schedule table is created"));
                JobScheduler::persistent(runtime, store)
            }
        };
        if let Some(window) = self.catch_up_window {
            scheduler = scheduler.with_catch_up_window(window);
        }

        let scheduler = Arc::new(scheduler);
        let handlers: HashMap<ComponentId, Arc<dyn JobHandler>> = self
            .handlers
            .iter()
            .map(|(id, handler)| (id.clone(), handler.clone() as Arc<dyn JobHandler>))
            .collect();
        scheduler.start(handlers).await?;
        self.scheduler = Some(scheduler);
        Ok(())
    }

    /// The scheduler under test, as the contract.
    pub(crate) fn scheduler(&self) -> Arc<dyn Scheduler> {
        self.concrete()
    }

    /// The scheduler under test, concretely.
    pub(crate) fn concrete(&self) -> Arc<JobScheduler> {
        self.scheduler
            .clone()
            .expect("the fixture has been started")
    }

    /// A handler registered with [`Fixture::with_handler`].
    pub(crate) fn handler(&self, id: &str) -> Arc<TestHandler> {
        self.handlers
            .get(&ComponentId::new(id))
            .cloned()
            .unwrap_or_else(|| panic!("no handler is registered as `{id}`"))
    }

    /// Subscribes to a scheduler event.
    pub(crate) fn watch<E: Event>(&self) -> EventStream<E> {
        self.events.subscribe::<E>()
    }

    /// The event bus, for publishing something a job is waiting on.
    pub(crate) fn events(&self) -> &EventBus {
        &self.events
    }

    /// The clock the scheduler reads.
    pub(crate) fn clock(&self) -> &Arc<TokioClock> {
        &self.clock
    }

    /// Where the database lives, for a persistent fixture.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_deref().expect("a persistent fixture")
    }

    /// The open database, for a test that puts another durable subsystem in the same file.
    ///
    /// Handed out rather than reopened deliberately: redb holds an exclusive lock, so sharing
    /// a file means sharing the handle, which is the whole arrangement `aik-store` exists to
    /// support.
    pub(crate) fn db(&self) -> Arc<Db> {
        self.db.clone().expect("a started persistent fixture")
    }

    /// Stops the scheduler and waits for its tasks, releasing redb's lock on the file.
    ///
    /// Waiting rather than merely cancelling is the point: the driver task holds a handle to
    /// the scheduler, which holds the store, which holds the database. A reopen that succeeds
    /// is proof that every one of them was actually released.
    pub(crate) async fn close(&mut self) {
        self.tasks
            .shutdown(PATIENCE)
            .await
            .expect("the scheduler's tasks stop");
        self.scheduler = None;
        // Dropped last and deliberately: redb's exclusive lock is released only when every
        // handle is gone, so a reopen that succeeds proves the scheduler let go of it.
        self.db = None;
    }

    /// Closes the scheduler and starts a new one over the same database.
    ///
    /// This is what a restart is: a new task scope, a new scheduler, the same file and the
    /// same clock — because a restart does not rewind time.
    pub(crate) async fn restart(&mut self) {
        self.try_restart().await.expect("the scheduler restarts");
    }

    /// The same, for a test that needs to see a restart *fail* rather than assume it works.
    pub(crate) async fn try_restart(&mut self) -> Result<()> {
        assert!(
            self.backend.persists(),
            "only a persistent fixture can be restarted"
        );
        self.close().await;
        self.tasks = Tasks::new();
        self.boot().await
    }
}

/// A context acting as a named user.
pub(crate) fn user(id: &str) -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::User))
}

/// A context acting as an agent working for `owner`.
pub(crate) fn agent_for(id: &str, owner: &str) -> ExecutionContext {
    ExecutionContext::new()
        .with_principal(Principal::new(id, PrincipalKind::Agent).on_behalf_of(owner))
}

/// A context naming no principal at all, which is the system acting for itself.
pub(crate) fn anonymous() -> ExecutionContext {
    ExecutionContext::new()
}

/// Waits for the next event of a type, failing the test rather than hanging.
pub(crate) async fn expect<E: Event>(stream: &mut EventStream<E>) -> E {
    tokio::time::timeout(PATIENCE, stream.recv())
        .await
        .unwrap_or_else(|_| panic!("no `{}` arrived", E::NAME))
        .unwrap_or_else(|error| panic!("the `{}` subscription failed: {error}", E::NAME))
        .payload
}

/// Asserts that no event of a type arrives while the runtime settles.
pub(crate) async fn expect_none<E: Event>(stream: &mut EventStream<E>) {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    if let Some(Ok(envelope)) = stream.try_recv() {
        panic!(
            "an unexpected `{}` arrived: {:?}",
            E::NAME,
            envelope.payload
        );
    }
}

/// Runs one suite function against both wirings.
///
/// Each name becomes a module with a `volatile` and a `redb` test in it, so a failure names
/// the assertion *and* the wiring that broke it.
#[macro_export]
macro_rules! both_backends {
    ($($name:ident),+ $(,)?) => {
        $(
            mod $name {
                #[tokio::test(start_paused = true)]
                async fn volatile() {
                    super::$name($crate::support::Backend::Volatile).await;
                }

                #[tokio::test(start_paused = true)]
                async fn redb() {
                    super::$name($crate::support::Backend::Redb).await;
                }
            }
        )+
    };
}
