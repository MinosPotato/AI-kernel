//! End-to-end tests of kernel assembly, lifecycle and wiring.
//!
//! These exercise the kernel the way a frontend would: build it, start it, use it through
//! a [`KernelContext`], shut it down.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aik_core::clock::{ManualClock, Timestamp};
use aik_core::prelude::*;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

/// Records the order in which lifecycle phases run across all components.
#[derive(Debug, Default, Clone)]
struct Journal(Arc<Mutex<Vec<String>>>);

impl Journal {
    fn record(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// A component that does nothing but write to the journal.
struct Recorder {
    descriptor: ComponentDescriptor,
    journal: Journal,
}

impl Recorder {
    fn new(id: &str, requires: &[&str], journal: &Journal) -> Self {
        let descriptor = requires
            .iter()
            .fold(ComponentDescriptor::new(id), |acc, dep| acc.requires(*dep));
        Self {
            descriptor,
            journal: journal.clone(),
        }
    }
}

#[async_trait]
impl Component for Recorder {
    fn descriptor(&self) -> ComponentDescriptor {
        self.descriptor.clone()
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        self.journal.record(format!("init:{}", ctx.id()));
        Ok(())
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        self.journal.record(format!("start:{}", ctx.id()));
        Ok(())
    }

    async fn stop(&self, ctx: &ComponentContext) -> Result<()> {
        self.journal.record(format!("stop:{}", ctx.id()));
        Ok(())
    }
}

/// A component that fails in a chosen phase.
struct Faulty {
    id: &'static str,
    phase: &'static str,
    journal: Journal,
}

#[async_trait]
impl Component for Faulty {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id)
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        self.journal.record(format!("init:{}", ctx.id()));
        if self.phase == "init" {
            return Err(Error::other("init exploded"));
        }
        Ok(())
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        self.journal.record(format!("start:{}", ctx.id()));
        if self.phase == "start" {
            return Err(Error::other("start exploded"));
        }
        Ok(())
    }

    async fn stop(&self, ctx: &ComponentContext) -> Result<()> {
        self.journal.record(format!("stop:{}", ctx.id()));
        Ok(())
    }
}

/// A component that spawns a background task on `start`, which only marks `finished` once
/// it actually observes cancellation and returns — not merely once it is asked to stop.
struct TaskSpawner {
    id: &'static str,
    finished: Arc<AtomicBool>,
}

#[async_trait]
impl Component for TaskSpawner {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id)
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let finished = self.finished.clone();
        ctx.tasks()
            .spawn_cancellable("worker", move |token| async move {
                token.cancelled().await;
                finished.store(true, Ordering::SeqCst);
            });
        Ok(())
    }
}

// A capability and two competing implementations of it.
trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

struct FixedGreeter(&'static str);

impl Greeter for FixedGreeter {
    fn greet(&self) -> String {
        self.0.to_owned()
    }
}

/// Publishes a `Greeter` under its own component id.
struct GreeterComponent {
    id: &'static str,
    greeting: &'static str,
    default: bool,
}

#[async_trait]
impl Component for GreeterComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id)
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let service = Arc::new(FixedGreeter(self.greeting));
        if self.default {
            ctx.provide_default::<dyn Greeter>(service)
        } else {
            ctx.provide::<dyn Greeter>(service)
        }
    }
}

/// Consumes a `Greeter` at start time, proving services exist before anything starts.
struct GreeterConsumer {
    seen: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Component for GreeterConsumer {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("consumer").requires("greeter")
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let greeter = ctx.service::<dyn Greeter>()?;
        *self.seen.lock().unwrap() = Some(greeter.greet());
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Tick {
    seq: u64,
}

impl Event for Tick {
    const NAME: &'static str = "test.tick";
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn components_start_in_dependency_order_and_stop_in_reverse() {
    let journal = Journal::default();
    let kernel = Kernel::builder()
        .component(Recorder::new("app", &["db"], &journal))
        .component(Recorder::new("db", &["config"], &journal))
        .component(Recorder::new("config", &[], &journal))
        .build()
        .unwrap();

    kernel.start().await.unwrap();
    kernel.shutdown().await.unwrap();

    assert_eq!(
        journal.entries(),
        [
            // Every component is initialised before any is started.
            "init:config",
            "init:db",
            "init:app",
            "start:config",
            "start:db",
            "start:app",
            // Shutdown unwinds in reverse.
            "stop:app",
            "stop:db",
            "stop:config",
        ]
    );
}

#[tokio::test]
async fn the_kernel_reports_its_state_throughout() {
    let kernel = Kernel::builder()
        .component(Recorder::new("a", &[], &Journal::default()))
        .build()
        .unwrap();

    assert_eq!(kernel.state(), KernelState::Created);
    kernel.start().await.unwrap();
    assert_eq!(kernel.state(), KernelState::Running);
    kernel.shutdown().await.unwrap();
    assert_eq!(kernel.state(), KernelState::Stopped);

    assert_eq!(
        kernel.component_states(),
        [(ComponentId::new("a"), ComponentState::Stopped)]
    );
}

#[tokio::test]
async fn shutdown_is_idempotent_and_starting_twice_is_refused() {
    let kernel = Kernel::builder().build().unwrap();

    kernel.start().await.unwrap();
    let error = kernel.start().await.unwrap_err();
    assert!(matches!(error, Error::Lifecycle(_)), "{error}");

    kernel.shutdown().await.unwrap();
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutting_down_a_kernel_that_never_started_is_fine() {
    let kernel = Kernel::builder().build().unwrap();
    kernel.shutdown().await.unwrap();
    assert_eq!(kernel.state(), KernelState::Stopped);
}

#[tokio::test]
async fn a_failed_start_rolls_back_what_was_already_running() {
    let journal = Journal::default();
    let kernel = Kernel::builder()
        .component(Recorder::new("a", &[], &journal))
        .component(Faulty {
            id: "b",
            phase: "start",
            journal: journal.clone(),
        })
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();
    match &error {
        Error::Component { component, .. } => assert_eq!(component.as_str(), "b"),
        other => panic!("expected a component failure, got {other}"),
    }

    assert_eq!(kernel.state(), KernelState::Failed);
    assert_eq!(
        journal.entries(),
        [
            "init:a", "init:b", "start:a", "start:b",
            // `a` came up, so it is taken back down. `b` failed on the way up and is not
            // asked to stop.
            "stop:a",
        ]
    );
    assert_eq!(
        kernel.component_states(),
        [
            (ComponentId::new("a"), ComponentState::Stopped),
            (ComponentId::new("b"), ComponentState::Failed),
        ]
    );
}

#[tokio::test]
async fn a_failed_start_waits_for_tasks_the_rolled_back_components_spawned() {
    // `a` came up and spawned a background task; `b` then failed to start, so `a` is
    // rolled back. `Tasks::cancel` alone only *signals* the task to stop -- proving this
    // requires a task that marks itself finished only after it actually observes that
    // signal and returns, not one that is merely told to.
    let finished = Arc::new(AtomicBool::new(false));
    let kernel = Kernel::builder()
        .component(TaskSpawner {
            id: "a",
            finished: finished.clone(),
        })
        .component(Faulty {
            id: "b",
            phase: "start",
            journal: Journal::default(),
        })
        .build()
        .unwrap();

    kernel.start().await.unwrap_err();

    assert!(
        finished.load(Ordering::SeqCst),
        "rollback must wait for a background task to actually finish, not just cancel it",
    );
}

#[tokio::test]
async fn a_failed_init_stops_the_components_already_initialised() {
    let journal = Journal::default();
    let kernel = Kernel::builder()
        .component(Recorder::new("a", &[], &journal))
        .component(Faulty {
            id: "b",
            phase: "init",
            journal: journal.clone(),
        })
        .build()
        .unwrap();

    kernel.start().await.unwrap_err();

    // Nothing was started, so nothing is left half-running.
    assert_eq!(journal.entries(), ["init:a", "init:b", "stop:a"]);
}

#[tokio::test]
async fn run_starts_waits_for_a_shutdown_request_then_stops() {
    let journal = Journal::default();
    let kernel = Arc::new(
        Kernel::builder()
            .component(Recorder::new("a", &[], &journal))
            .build()
            .unwrap(),
    );

    let ctx = kernel.context();
    let running = tokio::spawn({
        let kernel = kernel.clone();
        async move { kernel.run().await }
    });

    // Wait until the kernel is actually up before asking it to stop.
    let mut states = kernel.watch_state();
    while *states.borrow_and_update() != KernelState::Running {
        states.changed().await.unwrap();
    }

    ctx.request_shutdown();
    running.await.unwrap().unwrap();

    assert_eq!(journal.entries(), ["init:a", "start:a", "stop:a"]);
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

#[tokio::test]
async fn services_registered_during_init_are_available_at_start() {
    let seen = Arc::new(Mutex::new(None));
    let kernel = Kernel::builder()
        .component(GreeterComponent {
            id: "greeter",
            greeting: "hello",
            default: false,
        })
        .component(GreeterConsumer { seen: seen.clone() })
        .build()
        .unwrap();

    kernel.start().await.unwrap();
    assert_eq!(seen.lock().unwrap().as_deref(), Some("hello"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn implementations_are_swappable_without_touching_the_consumer() {
    async fn greeting_from(greeting: &'static str) -> String {
        let seen = Arc::new(Mutex::new(None));
        let kernel = Kernel::builder()
            .component(GreeterComponent {
                id: "greeter",
                greeting,
                default: false,
            })
            .component(GreeterConsumer { seen: seen.clone() })
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        kernel.shutdown().await.unwrap();
        let value = seen.lock().unwrap().clone();
        value.unwrap()
    }

    assert_eq!(greeting_from("hello").await, "hello");
    assert_eq!(greeting_from("bonjour").await, "bonjour");
}

#[tokio::test]
async fn competing_implementations_are_resolved_by_the_declared_default() {
    let kernel = Kernel::builder()
        .component(GreeterComponent {
            id: "a-greeter",
            greeting: "from a",
            default: false,
        })
        .component(GreeterComponent {
            id: "z-greeter",
            greeting: "from z",
            default: true,
        })
        .build()
        .unwrap();

    kernel.start().await.unwrap();
    let ctx = kernel.context();

    assert_eq!(ctx.service::<dyn Greeter>().unwrap().greet(), "from z");
    assert_eq!(
        ctx.service_named::<dyn Greeter>(&ComponentId::new("a-greeter"))
            .unwrap()
            .greet(),
        "from a"
    );
    // Discovery: both are enumerable without knowing either concrete type.
    assert_eq!(ctx.registry().list::<dyn Greeter>().len(), 2);

    kernel.shutdown().await.unwrap();
}

#[test]
fn unsound_wiring_is_rejected_at_build_time() {
    let journal = Journal::default();

    let missing = Kernel::builder()
        .component(Recorder::new("app", &["ghost"], &journal))
        .build()
        .unwrap_err();
    assert!(
        matches!(missing, Error::MissingDependency { .. }),
        "{missing}"
    );

    let cycle = Kernel::builder()
        .component(Recorder::new("a", &["b"], &journal))
        .component(Recorder::new("b", &["a"], &journal))
        .build()
        .unwrap_err();
    assert!(matches!(cycle, Error::DependencyCycle(_)), "{cycle}");

    let duplicate = Kernel::builder()
        .component(Recorder::new("a", &[], &journal))
        .component(Recorder::new("a", &[], &journal))
        .build()
        .unwrap_err();
    assert!(
        matches!(duplicate, Error::AlreadyExists { .. }),
        "{duplicate}"
    );
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Reads its own settings and publishes what it found.
struct Configurable(Arc<Mutex<u32>>);

#[async_trait]
impl Component for Configurable {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("configurable")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        *self.0.lock().unwrap() = ctx.settings().get_or_default::<u32>("retries")?;
        Ok(())
    }
}

#[tokio::test]
async fn components_read_their_own_configuration_section() {
    let retries = Arc::new(Mutex::new(0));
    let kernel = Kernel::builder()
        .config(
            Config::builder()
                .layer(json!({ "components": { "configurable": { "retries": 7 } } }))
                .build(),
        )
        .component(Configurable(retries.clone()))
        .build()
        .unwrap();

    kernel.start().await.unwrap();
    assert_eq!(*retries.lock().unwrap(), 7);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn unconfigured_components_fall_back_to_their_defaults() {
    let retries = Arc::new(Mutex::new(99));
    let kernel = Kernel::builder()
        .component(Configurable(retries.clone()))
        .build()
        .unwrap();

    kernel.start().await.unwrap();
    assert_eq!(*retries.lock().unwrap(), 0);
    kernel.shutdown().await.unwrap();
}

#[test]
fn kernel_settings_come_from_configuration() {
    let kernel = Kernel::builder()
        .config(
            Config::builder()
                .layer(json!({ "kernel": { "event_capacity": 4 } }))
                .build(),
        )
        .build()
        .unwrap();
    assert_eq!(kernel.context().events().capacity(), 4);

    let error = Kernel::builder()
        .config(
            Config::builder()
                .layer(json!({ "kernel": { "event_capacity": 0 } }))
                .build(),
        )
        .build()
        .unwrap_err();
    assert!(matches!(error, Error::Config { .. }), "{error}");
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Emits ticks from a background task until cancelled.
struct Ticker;

#[async_trait]
impl Component for Ticker {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("ticker")
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let ctx = ctx.clone();
        ctx.clone()
            .tasks()
            .spawn_cancellable("tick-loop", move |token| async move {
                let mut seq = 0;
                while !token.is_cancelled() {
                    ctx.publish(Tick { seq });
                    seq += 1;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });
        Ok(())
    }
}

#[tokio::test]
async fn events_published_by_a_component_carry_its_identity() {
    let kernel = Kernel::builder().component(Ticker).build().unwrap();
    let mut ticks = kernel.context().subscribe::<Tick>();

    kernel.start().await.unwrap();

    let tick = ticks.recv().await.unwrap();
    assert_eq!(tick.metadata.source, Some(ComponentId::new("ticker")));
    assert_eq!(tick.metadata.name.as_str(), "test.tick");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_is_observable_over_the_firehose() {
    let kernel = Kernel::builder()
        .component(Recorder::new("a", &[], &Journal::default()))
        .build()
        .unwrap();

    // A bridge that knows none of the kernel's event types.
    let firehose = kernel.context().events().subscribe_any();

    kernel.start().await.unwrap();
    kernel.shutdown().await.unwrap();

    let observed: Vec<(String, serde_json::Value)> = firehose
        .into_stream()
        .map(|envelope| (envelope.metadata.name.to_string(), envelope.payload))
        .take(4)
        .collect()
        .await;

    assert_eq!(observed[0].0, "kernel.state_changed");
    assert_eq!(observed[0].1, json!({ "state": "starting" }));
    assert_eq!(observed[1].0, "kernel.component_state_changed");
    assert_eq!(
        observed[1].1,
        json!({ "component": "a", "state": "initialized" })
    );
}

#[tokio::test]
async fn background_tasks_stop_when_the_kernel_does() {
    let kernel = Kernel::builder().component(Ticker).build().unwrap();
    kernel.start().await.unwrap();

    let ctx = kernel.context();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(ctx.tasks().running() > 0);

    kernel.shutdown().await.unwrap();
    assert_eq!(ctx.tasks().running(), 0);
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

struct DemoPlugin {
    journal: Journal,
}

impl Plugin for DemoPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new("demo", "0.1.0").described("two demo components")
    }

    fn register(&self, registrar: &mut PluginRegistrar<'_>) -> Result<()> {
        let prefix = registrar.plugin_id().to_string();
        registrar
            .component(Recorder::new(
                &format!("{prefix}.leader"),
                &[],
                &self.journal,
            ))
            .component(Recorder::new(
                &format!("{prefix}.follower"),
                &[&format!("{prefix}.leader")],
                &self.journal,
            ));
        Ok(())
    }
}

#[tokio::test]
async fn plugins_contribute_components_that_participate_normally() {
    let journal = Journal::default();
    let kernel = Kernel::builder()
        .plugin(DemoPlugin {
            journal: journal.clone(),
        })
        .build()
        .unwrap();

    assert_eq!(kernel.plugins().len(), 1);
    assert_eq!(kernel.plugins()[0].id.as_str(), "demo");
    assert_eq!(
        kernel.component_ids(),
        [
            ComponentId::new("demo.leader"),
            ComponentId::new("demo.follower")
        ]
    );

    kernel.start().await.unwrap();
    kernel.shutdown().await.unwrap();
    assert_eq!(journal.entries().first().unwrap(), "init:demo.leader");
}

#[test]
fn plugins_built_against_another_abi_are_refused() {
    struct Ancient;

    impl Plugin for Ancient {
        fn metadata(&self) -> PluginMetadata {
            let mut metadata = PluginMetadata::new("ancient", "0.0.1");
            metadata.abi_version = 0;
            metadata
        }

        fn register(&self, _: &mut PluginRegistrar<'_>) -> Result<()> {
            Ok(())
        }
    }

    let error = Kernel::builder().plugin(Ancient).build().unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error}");
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_clock_is_injectable() {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(5_000)));
    let kernel = Kernel::builder()
        .clock(clock.clone())
        .component(Ticker)
        .build()
        .unwrap();

    let mut ticks = kernel.context().subscribe::<Tick>();
    kernel.start().await.unwrap();

    assert_eq!(
        ticks.recv().await.unwrap().metadata.timestamp,
        Timestamp::from_millis(5_000)
    );
    assert_eq!(kernel.context().now(), Timestamp::from_millis(5_000));

    kernel.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_kernels_in_one_process_do_not_share_anything() {
    let counter = Arc::new(AtomicUsize::new(0));

    struct Counting(Arc<AtomicUsize>);

    #[async_trait]
    impl Component for Counting {
        fn descriptor(&self) -> ComponentDescriptor {
            ComponentDescriptor::new("counting")
        }

        async fn start(&self, _: &ComponentContext) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let first = Kernel::builder()
        .component(Counting(counter.clone()))
        .build()
        .unwrap();
    let second = Kernel::builder()
        .component(Counting(counter.clone()))
        .build()
        .unwrap();

    first.start().await.unwrap();
    assert_eq!(second.state(), KernelState::Created);

    // Events published in one kernel are invisible in the other.
    let mut ticks = second.context().subscribe::<Tick>();
    first.context().publish(Tick { seq: 1 });
    assert!(ticks.try_recv().is_none());

    second.start().await.unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    first.shutdown().await.unwrap();
    assert_eq!(second.state(), KernelState::Running);
    second.shutdown().await.unwrap();
}
