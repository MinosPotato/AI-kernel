//! The kernel: assembly, startup and shutdown.
//!
//! ```
//! use aik_core::prelude::*;
//!
//! # struct Nothing;
//! # #[async_trait]
//! # impl Component for Nothing {
//! #     fn descriptor(&self) -> ComponentDescriptor { ComponentDescriptor::new("demo") }
//! # }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let kernel = Kernel::builder()
//!     .config(Config::builder().env("AIK_").build())
//!     .component(Nothing)
//!     .build()?;
//!
//! kernel.start().await?;
//! // ... the system is running; frontends hold `kernel.context()` ...
//! kernel.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::clock::{SharedClock, SystemClock};
use crate::component::{
    Component, ComponentDescriptor, ComponentState, ComponentStateChanged, Health,
};
use crate::config::Config;
use crate::context::{ComponentContext, ContextInner, KernelContext};
use crate::error::{Error, LifecyclePhase, Result};
use crate::event::{DEFAULT_EVENT_CAPACITY, Event, EventBus};
use crate::graph::resolve_order;
use crate::id::{ComponentId, PluginId};
use crate::plugin::{KERNEL_ABI_VERSION, Plugin, PluginMetadata, PluginRegistrar};
use crate::registry::Registry;
use crate::task::Tasks;

/// How long shutdown waits for background tasks, unless configured otherwise.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the kernel is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelState {
    /// Assembled but not started.
    Created,
    /// Running component `init` and `start`.
    Starting,
    /// All components are running.
    Running,
    /// Stopping components.
    ShuttingDown,
    /// Everything is stopped.
    Stopped,
    /// Startup failed; anything that had started has been rolled back.
    Failed,
}

/// Published whenever the kernel changes state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelStateChanged {
    /// The new state.
    pub state: KernelState,
}

impl Event for KernelStateChanged {
    const NAME: &'static str = "kernel.state_changed";
}

/// Kernel settings, read from the `kernel` section of the configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct KernelSettings {
    /// Per-event-type broadcast capacity.
    event_capacity: usize,
    /// How long shutdown waits for background tasks.
    shutdown_timeout_ms: u64,
}

impl Default for KernelSettings {
    fn default() -> Self {
        Self {
            event_capacity: DEFAULT_EVENT_CAPACITY,
            shutdown_timeout_ms: DEFAULT_SHUTDOWN_TIMEOUT.as_millis() as u64,
        }
    }
}

struct Managed {
    component: Arc<dyn Component>,
    descriptor: ComponentDescriptor,
    ctx: ComponentContext,
    state: Mutex<ComponentState>,
}

impl Managed {
    fn state(&self) -> ComponentState {
        *self.state.lock().expect("component state lock poisoned")
    }

    fn set_state(&self, state: ComponentState) {
        *self.state.lock().expect("component state lock poisoned") = state;
        self.ctx.publish(ComponentStateChanged {
            component: self.descriptor.id.clone(),
            state,
        });
    }
}

/// An assembled system: components, wiring and lifecycle.
///
/// The kernel owns everything. Frontends hold a [`KernelContext`] instead, obtained from
/// [`Kernel::context`].
pub struct Kernel {
    ctx: KernelContext,
    /// Components in dependency order: dependencies first.
    components: Vec<Managed>,
    plugins: Vec<PluginMetadata>,
    shutdown_timeout: Duration,
    state: watch::Sender<KernelState>,
    /// Serialises `start` and `shutdown` against each other.
    lifecycle: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("state", &self.state())
            .field("components", &self.components.len())
            .field("plugins", &self.plugins.len())
            .finish()
    }
}

impl Kernel {
    /// Starts assembling a kernel.
    pub fn builder() -> KernelBuilder {
        KernelBuilder::new()
    }

    /// A handle onto this kernel, for frontends and background work.
    pub fn context(&self) -> KernelContext {
        self.ctx.clone()
    }

    /// The current lifecycle state.
    pub fn state(&self) -> KernelState {
        *self.state.borrow()
    }

    /// Observes lifecycle state changes.
    ///
    /// A UI can render boot progress from this without subscribing to events.
    pub fn watch_state(&self) -> watch::Receiver<KernelState> {
        self.state.subscribe()
    }

    /// Metadata for every plugin that contributed to this kernel.
    pub fn plugins(&self) -> &[PluginMetadata] {
        &self.plugins
    }

    /// Component ids in startup order.
    pub fn component_ids(&self) -> Vec<ComponentId> {
        self.components
            .iter()
            .map(|managed| managed.descriptor.id.clone())
            .collect()
    }

    /// The descriptor of every component, in startup order.
    pub fn descriptors(&self) -> Vec<ComponentDescriptor> {
        self.components
            .iter()
            .map(|managed| managed.descriptor.clone())
            .collect()
    }

    /// Every component's current state, in startup order.
    pub fn component_states(&self) -> Vec<(ComponentId, ComponentState)> {
        self.components
            .iter()
            .map(|managed| (managed.descriptor.id.clone(), managed.state()))
            .collect()
    }

    /// Asks every component how it is doing, in startup order.
    pub async fn health(&self) -> Vec<(ComponentId, Health)> {
        let mut report = Vec::with_capacity(self.components.len());
        for managed in &self.components {
            report.push((
                managed.descriptor.id.clone(),
                managed.component.health().await,
            ));
        }
        report
    }

    /// Initialises and starts every component in dependency order.
    ///
    /// All components are initialised before any is started, so a component may rely on
    /// its dependencies' services being registered without relying on them being active.
    ///
    /// If any phase fails, everything already brought up is stopped in reverse order and
    /// the original error is returned: a failed start never leaves half a system running.
    pub async fn start(&self) -> Result<()> {
        let _guard = self.lifecycle.lock().await;

        match self.state() {
            KernelState::Created => {}
            other => {
                return Err(Error::Lifecycle(format!(
                    "cannot start a kernel in state `{other:?}`"
                )));
            }
        }

        self.transition(KernelState::Starting);
        tracing::info!(components = self.components.len(), "starting kernel");

        // Highest index reached, so rollback knows how far to unwind.
        let mut reached = 0usize;

        for (index, managed) in self.components.iter().enumerate() {
            if let Err(error) = self.run_phase(managed, LifecyclePhase::Init).await {
                self.rollback(reached).await;
                self.transition(KernelState::Failed);
                return Err(error);
            }
            managed.set_state(ComponentState::Initialized);
            reached = index + 1;
        }

        for managed in &self.components {
            if let Err(error) = self.run_phase(managed, LifecyclePhase::Start).await {
                self.rollback(reached).await;
                self.transition(KernelState::Failed);
                return Err(error);
            }
            managed.set_state(ComponentState::Running);
        }

        self.transition(KernelState::Running);
        tracing::info!("kernel running");
        Ok(())
    }

    /// Stops every component in reverse dependency order and waits for background tasks.
    ///
    /// Idempotent: shutting down an already-stopped kernel succeeds without doing
    /// anything. Every component is given the chance to stop even if an earlier one
    /// failed; the first error is returned once all of them have been tried.
    pub async fn shutdown(&self) -> Result<()> {
        let _guard = self.lifecycle.lock().await;

        match self.state() {
            KernelState::Stopped => return Ok(()),
            KernelState::Created => {
                self.transition(KernelState::Stopped);
                return Ok(());
            }
            _ => {}
        }

        self.transition(KernelState::ShuttingDown);
        tracing::info!("shutting down kernel");

        let mut first_error = self.stop_range(self.components.len()).await;

        if let Err(error) = self.ctx.tasks().shutdown(self.shutdown_timeout).await {
            tracing::warn!(%error, "background tasks did not finish in time");
            first_error = first_error.or(Some(error));
        }

        self.transition(KernelState::Stopped);
        tracing::info!("kernel stopped");

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Starts the kernel, waits for a shutdown request, then shuts down.
    ///
    /// The shutdown signal is raised by [`KernelContext::request_shutdown`] — from a
    /// component, a CLI command, a UI action or a signal handler installed by the host
    /// process. The kernel itself installs no signal handlers, because that is
    /// platform-specific and belongs to whoever owns `main`.
    pub async fn run(&self) -> Result<()> {
        self.start().await?;
        self.ctx.shutdown_requested().await;
        self.shutdown().await
    }

    async fn run_phase(&self, managed: &Managed, phase: LifecyclePhase) -> Result<()> {
        let id = &managed.descriptor.id;
        tracing::debug!(component = %id, %phase, "component lifecycle");

        let outcome = match phase {
            LifecyclePhase::Init => managed.component.init(&managed.ctx).await,
            LifecyclePhase::Start => managed.component.start(&managed.ctx).await,
            LifecyclePhase::Stop => managed.component.stop(&managed.ctx).await,
        };

        outcome.map_err(|error| {
            tracing::error!(component = %id, %phase, %error, "component lifecycle failed");
            managed.set_state(ComponentState::Failed);
            Error::Component {
                component: id.clone(),
                phase,
                source: Box::new(error),
            }
        })
    }

    /// Stops the first `count` components, in reverse order. Returns the first failure.
    async fn stop_range(&self, count: usize) -> Option<Error> {
        let mut first_error = None;

        for managed in self.components[..count].iter().rev() {
            // Never started, already stopped, or failed on the way up: nothing to unwind,
            // and calling `stop` on a component whose `init`/`start` failed would hand it
            // state it never got to build.
            if matches!(
                managed.state(),
                ComponentState::Registered | ComponentState::Stopped | ComponentState::Failed
            ) {
                managed.ctx.tasks().cancel();
                continue;
            }

            match self.run_phase(managed, LifecyclePhase::Stop).await {
                Ok(()) => managed.set_state(ComponentState::Stopped),
                Err(error) => first_error = first_error.or(Some(error)),
            }

            // Whatever the component did or did not clean up, its tasks stop here.
            managed.ctx.tasks().cancel();
        }

        first_error
    }

    /// Unwinds a failed startup. Errors are logged; the original failure is what matters.
    ///
    /// Mirrors [`Kernel::shutdown`]'s own last step: [`Tasks::cancel`] (which `stop_range`
    /// already calls per component) only *signals* cancellation, it does not wait, so
    /// without this a component whose `start` spawned a background task would have that
    /// task merely told to stop rather than actually waited on before `start` returns —
    /// the caller would treat the kernel as fully torn down while cleanup work might still
    /// be mid-flight. `Tasks::shutdown` is safe to call again if the caller goes on to call
    /// [`Kernel::shutdown`] explicitly afterwards; both cancelling an already-cancelled
    /// token and closing an already-closed tracker are no-ops.
    async fn rollback(&self, count: usize) {
        if count == 0 {
            return;
        }
        tracing::warn!(components = count, "rolling back a failed startup");
        if let Some(error) = self.stop_range(count).await {
            tracing::error!(%error, "a component also failed while rolling back");
        }
        if let Err(error) = self.ctx.tasks().shutdown(self.shutdown_timeout).await {
            tracing::warn!(%error, "background tasks did not finish rolling back in time");
        }
    }

    fn transition(&self, state: KernelState) {
        // `send` refuses to update when nobody is watching; `send_replace` always does.
        self.state.send_replace(state);
        self.ctx.publish(KernelStateChanged { state });
    }
}

/// Assembles a [`Kernel`].
#[derive(Debug, Default)]
pub struct KernelBuilder {
    config: Option<Config>,
    clock: Option<SharedClock>,
    components: Vec<Arc<dyn Component>>,
    plugins: Vec<Box<dyn Plugin>>,
    event_capacity: Option<usize>,
    shutdown_timeout: Option<Duration>,
}

impl std::fmt::Debug for Box<dyn Plugin> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("id", &self.metadata().id)
            .finish()
    }
}

impl KernelBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the configuration snapshot. Defaults to empty.
    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Overrides the clock. Defaults to [`SystemClock`].
    #[must_use]
    pub fn clock(mut self, clock: SharedClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Overrides the event bus capacity, ignoring `kernel.event_capacity`.
    #[must_use]
    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = Some(capacity);
        self
    }

    /// Overrides the shutdown timeout, ignoring `kernel.shutdown_timeout_ms`.
    #[must_use]
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
    }

    /// Adds a component.
    #[must_use]
    pub fn component(mut self, component: impl Component) -> Self {
        self.components.push(Arc::new(component));
        self
    }

    /// Adds an already-shared component.
    ///
    /// Use this when the caller needs to keep a handle to the component, e.g. to inspect
    /// it from tests or to drive it from a frontend.
    #[must_use]
    pub fn shared_component(mut self, component: Arc<dyn Component>) -> Self {
        self.components.push(component);
        self
    }

    /// Adds a plugin, whose components are collected during [`KernelBuilder::build`].
    #[must_use]
    pub fn plugin(mut self, plugin: impl Plugin) -> Self {
        self.plugins.push(Box::new(plugin));
        self
    }

    /// Adds an already-boxed plugin, as a dynamic loader would produce.
    #[must_use]
    pub fn boxed_plugin(mut self, plugin: Box<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Validates the wiring and assembles the kernel.
    ///
    /// Everything that can be checked statically is checked here rather than at startup:
    /// plugin ABI compatibility, duplicate plugin and component ids, unmet required
    /// dependencies and dependency cycles. A kernel that builds is a kernel whose wiring
    /// is sound.
    pub fn build(mut self) -> Result<Kernel> {
        let config = self.config.unwrap_or_default();
        let settings = config.get_or_default::<KernelSettings>("kernel")?;

        let plugins = collect_plugins(&mut self.plugins, &mut self.components)?;
        let descriptors: Vec<ComponentDescriptor> = self
            .components
            .iter()
            .map(|component| component.descriptor())
            .collect();
        reject_duplicate_components(&descriptors)?;

        let order = resolve_order(&descriptors)?;

        let clock = self.clock.unwrap_or_else(|| Arc::new(SystemClock));
        let capacity = self.event_capacity.unwrap_or(settings.event_capacity);
        if capacity == 0 {
            return Err(Error::config(
                "kernel.event_capacity",
                "must be greater than zero",
            ));
        }

        let ctx = KernelContext::new(Arc::new(ContextInner {
            config,
            events: EventBus::new(capacity, clock.clone()),
            registry: Registry::new(),
            tasks: Tasks::new(),
            clock,
            shutdown: CancellationToken::new(),
        }));

        // Index components by id so they can be laid out in dependency order.
        let mut by_id: Vec<Option<(Arc<dyn Component>, ComponentDescriptor)>> = self
            .components
            .into_iter()
            .zip(descriptors)
            .map(Some)
            .collect();
        let positions: std::collections::HashMap<ComponentId, usize> = by_id
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref()
                    .map(|(_, descriptor)| (descriptor.id.clone(), index))
            })
            .collect();

        let components = order
            .into_iter()
            .map(|id| {
                let index = positions[&id];
                let (component, descriptor) = by_id[index].take().expect("each id appears once");
                Managed {
                    ctx: ctx.for_component(descriptor.id.clone()),
                    state: Mutex::new(ComponentState::Registered),
                    component,
                    descriptor,
                }
            })
            .collect();

        Ok(Kernel {
            ctx,
            components,
            plugins,
            shutdown_timeout: self
                .shutdown_timeout
                .unwrap_or_else(|| Duration::from_millis(settings.shutdown_timeout_ms)),
            state: watch::Sender::new(KernelState::Created),
            lifecycle: tokio::sync::Mutex::new(()),
        })
    }
}

fn collect_plugins(
    plugins: &mut Vec<Box<dyn Plugin>>,
    components: &mut Vec<Arc<dyn Component>>,
) -> Result<Vec<PluginMetadata>> {
    let mut seen: HashSet<PluginId> = HashSet::new();
    let mut collected = Vec::with_capacity(plugins.len());

    for plugin in plugins.iter() {
        let metadata = plugin.metadata();

        if metadata.abi_version != KERNEL_ABI_VERSION {
            return Err(Error::Unsupported(format!(
                "plugin `{}` was built against kernel ABI {}, but this kernel is ABI {}",
                metadata.id, metadata.abi_version, KERNEL_ABI_VERSION
            )));
        }
        if !seen.insert(metadata.id.clone()) {
            return Err(Error::already_exists("plugin", &metadata.id));
        }

        let mut registrar = PluginRegistrar::new(metadata.id.clone(), components);
        plugin.register(&mut registrar)?;
        collected.push(metadata);
    }

    Ok(collected)
}

fn reject_duplicate_components(descriptors: &[ComponentDescriptor]) -> Result<()> {
    let mut seen = HashSet::with_capacity(descriptors.len());
    for descriptor in descriptors {
        if !seen.insert(descriptor.id.clone()) {
            return Err(Error::already_exists("component", &descriptor.id));
        }
    }
    Ok(())
}
