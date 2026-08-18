//! Handles onto the running kernel.
//!
//! [`KernelContext`] is the kernel's public face: configuration, events, the service
//! registry, task spawning, time and shutdown. It is cheap to clone and can be handed to
//! anything — a UI thread, a CLI command, a background service.
//!
//! [`ComponentContext`] is a `KernelContext` bound to one component. It adds the
//! component's identity, so published events are attributed, spawned tasks live in that
//! component's cancellation scope, and configuration is scoped to `components.<id>`.
//!
//! Passing a context explicitly is what keeps the kernel free of global mutable state:
//! there is no ambient "current kernel", so two kernels can run in one process (which is
//! exactly what the test suite does).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::clock::{SharedClock, Timestamp};
use crate::config::Config;
use crate::error::Result;
use crate::event::{Envelope, Event, EventBus, EventStream};
use crate::id::{ComponentId, CorrelationId};
use crate::registry::Registry;
use crate::task::Tasks;

pub(crate) struct ContextInner {
    pub(crate) config: Config,
    pub(crate) events: EventBus,
    pub(crate) registry: Registry,
    pub(crate) tasks: Tasks,
    pub(crate) clock: SharedClock,
    /// Signals "please shut down"; distinct from task cancellation, which is "stop now".
    pub(crate) shutdown: CancellationToken,
}

/// A cheaply cloneable handle onto a kernel.
#[derive(Clone)]
pub struct KernelContext {
    inner: Arc<ContextInner>,
}

impl std::fmt::Debug for KernelContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelContext")
            .field("registry", &self.inner.registry)
            .field("events", &self.inner.events)
            .field("running_tasks", &self.inner.tasks.running())
            .finish()
    }
}

impl KernelContext {
    pub(crate) fn new(inner: Arc<ContextInner>) -> Self {
        Self { inner }
    }

    /// The kernel's configuration snapshot.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// The event bus.
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// The service registry.
    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// The root task scope.
    pub fn tasks(&self) -> &Tasks {
        &self.inner.tasks
    }

    /// The kernel clock.
    pub fn clock(&self) -> &SharedClock {
        &self.inner.clock
    }

    /// The current time according to the kernel clock.
    pub fn now(&self) -> Timestamp {
        self.inner.clock.now()
    }

    /// Publishes an event with no source attribution.
    pub fn publish<E: Event>(&self, event: E) -> usize {
        self.inner.events.publish(event)
    }

    /// Subscribes to an event type.
    pub fn subscribe<E: Event>(&self) -> EventStream<E> {
        self.inner.events.subscribe::<E>()
    }

    /// Resolves the default service for a capability.
    ///
    /// `ctx.service::<dyn ModelProvider>()?` is the normal way for one part of the system
    /// to reach another.
    pub fn service<T>(&self) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.inner.registry.resolve::<T>()
    }

    /// Resolves a named service for a capability.
    pub fn service_named<T>(&self, id: &ComponentId) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.inner.registry.get::<T>(id)
    }

    /// Asks the kernel to shut down.
    ///
    /// This only raises the signal. Whoever is driving the kernel — usually
    /// [`Kernel::run`](crate::kernel::Kernel::run) — performs the orderly shutdown.
    pub fn request_shutdown(&self) {
        tracing::debug!("shutdown requested");
        self.inner.shutdown.cancel();
    }

    /// Returns true once shutdown has been requested.
    pub fn is_shutdown_requested(&self) -> bool {
        self.inner.shutdown.is_cancelled()
    }

    /// Waits until shutdown is requested.
    pub async fn shutdown_requested(&self) {
        self.inner.shutdown.cancelled().await;
    }

    /// Binds this context to a component identity.
    pub fn for_component(&self, id: impl Into<ComponentId>) -> ComponentContext {
        ComponentContext {
            id: id.into(),
            tasks: self.inner.tasks.child(),
            kernel: self.clone(),
        }
    }
}

/// A [`KernelContext`] bound to one component.
#[derive(Clone, Debug)]
pub struct ComponentContext {
    id: ComponentId,
    tasks: Tasks,
    kernel: KernelContext,
}

impl ComponentContext {
    /// This component's id.
    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    /// The unscoped kernel context.
    pub fn kernel(&self) -> &KernelContext {
        &self.kernel
    }

    /// The whole configuration.
    ///
    /// Prefer [`ComponentContext::settings`] for this component's own settings.
    pub fn config(&self) -> &Config {
        self.kernel.config()
    }

    /// This component's configuration section, `components.<id>`.
    ///
    /// Returns an empty section if the component is not configured, so a component can
    /// always fall back to its defaults.
    pub fn settings(&self) -> Config {
        self.kernel
            .config()
            .section(&format!("components.{}", self.id))
    }

    /// The event bus.
    pub fn events(&self) -> &EventBus {
        self.kernel.events()
    }

    /// The service registry.
    pub fn registry(&self) -> &Registry {
        self.kernel.registry()
    }

    /// This component's task scope.
    ///
    /// Tasks spawned here are cancelled when the component stops, and when the kernel
    /// shuts down.
    pub fn tasks(&self) -> &Tasks {
        &self.tasks
    }

    /// This component's cancellation token.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.tasks.cancellation_token()
    }

    /// The kernel clock.
    pub fn clock(&self) -> &SharedClock {
        self.kernel.clock()
    }

    /// The current time according to the kernel clock.
    pub fn now(&self) -> Timestamp {
        self.kernel.now()
    }

    /// Publishes an event attributed to this component.
    pub fn publish<E: Event>(&self, event: E) -> usize {
        let metadata = self
            .kernel
            .events()
            .metadata_for::<E>()
            .with_source(self.id.clone());
        self.kernel
            .events()
            .publish_envelope(Envelope::new(metadata, event))
    }

    /// Publishes an event attributed to this component and tied to a logical operation.
    pub fn publish_correlated<E: Event>(&self, event: E, correlation: CorrelationId) -> usize {
        let metadata = self
            .kernel
            .events()
            .metadata_for::<E>()
            .with_source(self.id.clone())
            .with_correlation(correlation);
        self.kernel
            .events()
            .publish_envelope(Envelope::new(metadata, event))
    }

    /// Subscribes to an event type.
    pub fn subscribe<E: Event>(&self) -> EventStream<E> {
        self.kernel.subscribe::<E>()
    }

    /// Publishes a service under this component's name.
    ///
    /// Call this from [`Component::init`](crate::component::Component::init): by the time
    /// any component starts, every service is registered.
    pub fn provide<T>(&self, service: Arc<T>) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.registry().register::<T>(self.id.clone(), service)
    }

    /// Publishes a service under this component's name and makes it the default.
    pub fn provide_default<T>(&self, service: Arc<T>) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.registry()
            .register_default::<T>(self.id.clone(), service)
    }

    /// Resolves the default service for a capability.
    pub fn service<T>(&self) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.kernel.service::<T>()
    }

    /// Resolves a named service for a capability.
    pub fn service_named<T>(&self, id: &ComponentId) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.kernel.service_named::<T>(id)
    }

    /// Asks the kernel to shut down.
    pub fn request_shutdown(&self) {
        self.kernel.request_shutdown();
    }
}
