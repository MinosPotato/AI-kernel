//! A complete miniature system, exercising every kernel mechanism at once.
//!
//! Three components, wired only through capabilities and events:
//!
//! * `sensor` publishes a `Sensor` service and emits readings from a background task.
//! * `analyser` depends on `sensor`, consumes the service and reacts to the events.
//! * `bridge` observes the whole system over the JSON firehose, knowing no event types —
//!   which is how a Quickshell socket or a chat frontend will eventually watch the system.
//!
//! Run with:
//!
//! ```text
//! cargo run -p aik --example minimal
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aik::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

// --- A capability -----------------------------------------------------------------
// Consumers depend on this trait. Any implementation can be swapped in without either
// the kernel or the consumer changing.

trait Sensor: Send + Sync {
    fn read(&self) -> u64;
}

// --- An event ---------------------------------------------------------------------
// Serialisable, with a stable name, so out-of-process bridges can observe it.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reading {
    value: u64,
}

impl Event for Reading {
    const NAME: &'static str = "demo.reading";
}

// --- A component that provides the capability --------------------------------------

#[derive(Default)]
struct CountingSensor {
    counter: AtomicU64,
}

impl Sensor for CountingSensor {
    fn read(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

struct SensorComponent {
    sensor: Arc<CountingSensor>,
}

#[async_trait]
impl Component for SensorComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("demo.sensor").described("emits periodic readings")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        // Published during init, so it exists before anything starts.
        ctx.provide_default::<dyn Sensor>(self.sensor.clone())
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        // Interval comes from `components.demo.sensor.interval_ms`, with a default.
        let interval =
            Duration::from_millis(ctx.settings().get_optional("interval_ms")?.unwrap_or(100));
        let sensor = self.sensor.clone();
        let ctx = ctx.clone();

        // Spawned in this component's cancellation scope: it stops when the component or
        // the kernel does, and shutdown waits for it.
        ctx.clone()
            .tasks()
            .spawn_cancellable("sensor-loop", move |token| async move {
                loop {
                    tokio::select! {
                        () = token.cancelled() => break,
                        () = tokio::time::sleep(interval) => {
                            ctx.publish(Reading { value: sensor.read() });
                        }
                    }
                }
                println!("[sensor]   loop stopped cleanly");
            });

        Ok(())
    }
}

// --- A component that consumes it ---------------------------------------------------

struct Analyser {
    stop_after: u64,
}

#[async_trait]
impl Component for Analyser {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("demo.analyser").requires("demo.sensor")
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        // Resolved by capability. The analyser has no idea `CountingSensor` exists.
        let sensor = ctx.service::<dyn Sensor>()?;
        println!("[analyser] first direct read: {}", sensor.read());

        let mut readings = ctx.subscribe::<Reading>();
        let ctx = ctx.clone();
        let stop_after = self.stop_after;

        ctx.clone().tasks().spawn("analyser-loop", async move {
            while let Ok(reading) = readings.recv().await {
                println!(
                    "[analyser] reading {} from `{}`",
                    reading.payload.value,
                    reading.metadata.source.as_ref().expect("attributed"),
                );
                if reading.payload.value >= stop_after {
                    // Any component can ask the whole system to wind down.
                    ctx.request_shutdown();
                    break;
                }
            }
        });

        Ok(())
    }

    async fn stop(&self, _: &ComponentContext) -> Result<()> {
        println!("[analyser] stopping");
        Ok(())
    }
}

// --- A bridge that knows nothing ----------------------------------------------------

struct Bridge;

#[async_trait]
impl Component for Bridge {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new("demo.bridge").described("mirrors every event as JSON")
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let mut everything = ctx.events().subscribe_any();

        // A subscription never ends on its own, so this task must be cancellable —
        // otherwise shutdown would wait for it until the timeout expires.
        ctx.tasks()
            .spawn_until_cancelled("bridge-loop", async move {
                while let Ok(envelope) = everything.recv().await {
                    println!("[bridge]   {} {}", envelope.metadata.name, envelope.payload);
                }
            });

        Ok(())
    }
}

// --- A plugin that contributes the lot ----------------------------------------------

struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new("demo", env!("CARGO_PKG_VERSION")).described("the example system")
    }

    fn register(&self, registrar: &mut PluginRegistrar<'_>) -> Result<()> {
        registrar
            .component(SensorComponent {
                sensor: Arc::new(CountingSensor::default()),
            })
            .component(Analyser { stop_after: 4 })
            .component(Bridge);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::builder()
        .layer(json!({
            "kernel": { "shutdown_timeout_ms": 2000 },
            "components": { "demo.sensor": { "interval_ms": 150 } }
        }))
        // Anything in the environment wins, e.g. AIK_COMPONENTS__DEMO_SENSOR__INTERVAL_MS.
        .env("AIK_")
        .build();

    let kernel = Kernel::builder()
        .config(config)
        .plugin(DemoPlugin)
        .build()?;

    println!("start order: {:?}", kernel.component_ids());

    // `run` starts everything, waits for a shutdown request, then stops in reverse order.
    kernel.run().await?;

    println!("final states: {:?}", kernel.component_states());
    Ok(())
}
