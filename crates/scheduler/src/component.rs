//! Wires a scheduler into the kernel as a normal component.
//!
//! Two of them, for the two wirings, published under the same default id so that making the
//! schedule durable is a one-line change — exactly the shape
//! [`MemoryComponent`](aik_memory)/[`RedbMemoryComponent`](aik_memory) and the two context
//! components already use.
//!
//! # Why handlers are snapshotted at `start`
//!
//! Both components resolve every `dyn JobHandler` in the registry during
//! [`start`](Component::start) and hand the set to the scheduler. `init` runs for *every*
//! component before any component is started, so by then everything that publishes a handler
//! has published it; and a snapshot means the scheduler never holds the registry, which would
//! mean holding the context that owns the registry that owns the scheduler — a cycle that
//! would keep the kernel, and the open database file, alive for the life of the process.
//!
//! The consequence is worth stating plainly: a handler registered after startup is not seen.
//! Publishing services from `init` is the kernel's rule already, so this asks nothing new.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aik_api::scheduler::{JobHandler, Scheduler};
use aik_core::prelude::*;
use aik_store::Db;
use serde::Deserialize;

use crate::persistent::RedbJobStore;
use crate::scheduler::{DEFAULT_CATCH_UP_WINDOW, JobScheduler, SchedulerRuntime};
use crate::store::JobStore;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "scheduler.jobs";

/// What a scheduler component reads from `components.<id>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SchedulerSettings {
    /// How far back a firing missed while nothing was running is still worth running.
    catch_up_window_ms: u64,
}

impl Default for SchedulerSettings {
    fn default() -> Self {
        Self {
            catch_up_window_ms: DEFAULT_CATCH_UP_WINDOW.as_millis() as u64,
        }
    }
}

/// Resolves the catch-up window: an explicit builder value wins, otherwise configuration,
/// otherwise the default.
fn catch_up_window(ctx: &ComponentContext, explicit: Option<Duration>) -> Result<Duration> {
    if let Some(window) = explicit {
        return Ok(window);
    }
    let settings: SchedulerSettings = ctx.settings().get_or_default("")?;
    Ok(Duration::from_millis(settings.catch_up_window_ms))
}

/// Every `dyn JobHandler` the kernel knows about, keyed by the id a job names.
fn handlers(ctx: &ComponentContext) -> HashMap<ComponentId, Arc<dyn JobHandler>> {
    ctx.registry()
        .list::<dyn JobHandler>()
        .into_iter()
        .collect()
}

/// Registers a [`JobScheduler`] that keeps its schedule in memory.
///
/// Jobs asking to be [`persistent`](aik_api::scheduler::JobSpec::persistent) are refused
/// rather than accepted and forgotten. Use [`RedbSchedulerComponent`] for those.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_scheduler::SchedulerComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(SchedulerComponent::new()).build()
/// # }
/// ```
pub struct SchedulerComponent {
    id: ComponentId,
    default: bool,
    catch_up_window: Option<Duration>,
    scheduler: OnceLock<Arc<JobScheduler>>,
}

impl std::fmt::Debug for SchedulerComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerComponent")
            .field("id", &self.id)
            .field("default", &self.default)
            .field("catch_up_window", &self.catch_up_window)
            .finish()
    }
}

impl Default for SchedulerComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`].
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            catch_up_window: None,
            scheduler: OnceLock::new(),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this becomes the registry's default [`Scheduler`].
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Overrides the catch-up window, ignoring `components.<id>.catch_up_window_ms`.
    #[must_use]
    pub fn with_catch_up_window(mut self, window: Duration) -> Self {
        self.catch_up_window = Some(window);
        self
    }

    /// The scheduler this component built, once `init` has run.
    pub fn scheduler(&self) -> Option<&Arc<JobScheduler>> {
        self.scheduler.get()
    }
}

#[async_trait]
impl Component for SchedulerComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("runs time- and event-triggered jobs, forgetting them at shutdown")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let window = catch_up_window(ctx, self.catch_up_window)?;
        let scheduler = Arc::new(
            JobScheduler::volatile(SchedulerRuntime::from_component(ctx))
                .with_catch_up_window(window),
        );
        self.scheduler
            .set(scheduler.clone())
            .expect("init runs at most once per component");

        let published: Arc<dyn Scheduler> = scheduler;
        if self.default {
            ctx.provide_default::<dyn Scheduler>(published)
        } else {
            ctx.provide::<dyn Scheduler>(published)
        }
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let scheduler = self.scheduler.get().expect("init runs before start");
        scheduler.start(handlers(ctx)).await
    }

    async fn stop(&self, _ctx: &ComponentContext) -> Result<()> {
        if let Some(scheduler) = self.scheduler.get() {
            scheduler.stop();
        }
        Ok(())
    }
}

/// Registers a [`JobScheduler`] whose persistent jobs live in the kernel's shared database.
///
/// The durable counterpart of [`SchedulerComponent`]: same capability, same default id, same
/// registry semantics, and one more guarantee — a job marked
/// [`persistent`](aik_api::scheduler::JobSpec::persistent) is still there after a restart. It
/// depends on the [`aik_store`] component, which must therefore be in the kernel too.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_scheduler::RedbSchedulerComponent;
/// use aik_store::StoreComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(StoreComponent::new())
///     .component(RedbSchedulerComponent::new())
///     .build()
/// # }
/// ```
pub struct RedbSchedulerComponent {
    id: ComponentId,
    database: ComponentId,
    default: bool,
    catch_up_window: Option<Duration>,
    scheduler: OnceLock<Arc<JobScheduler>>,
}

impl std::fmt::Debug for RedbSchedulerComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbSchedulerComponent")
            .field("id", &self.id)
            .field("database", &self.database)
            .field("default", &self.default)
            .field("catch_up_window", &self.catch_up_window)
            .finish()
    }
}

impl Default for RedbSchedulerComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl RedbSchedulerComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], backed by the kernel's
    /// default database.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            database: ComponentId::new(aik_store::DEFAULT_COMPONENT_ID),
            default: true,
            catch_up_window: None,
            scheduler: OnceLock::new(),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Uses a named database rather than the registry's default one.
    #[must_use]
    pub fn with_database(mut self, database: impl Into<ComponentId>) -> Self {
        self.database = database.into();
        self
    }

    /// Controls whether this becomes the registry's default [`Scheduler`].
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Overrides the catch-up window, ignoring `components.<id>.catch_up_window_ms`.
    #[must_use]
    pub fn with_catch_up_window(mut self, window: Duration) -> Self {
        self.catch_up_window = Some(window);
        self
    }

    /// The scheduler this component built, once `init` has run.
    pub fn scheduler(&self) -> Option<&Arc<JobScheduler>> {
        self.scheduler.get()
    }
}

#[async_trait]
impl Component for RedbSchedulerComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("runs time- and event-triggered jobs, keeping persistent ones on disk")
            .requires(self.database.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let window = catch_up_window(ctx, self.catch_up_window)?;
        let db = ctx.service_named::<Db>(&self.database)?;
        let store: Arc<dyn JobStore> = Arc::new(RedbJobStore::new(db)?);
        let scheduler = Arc::new(
            JobScheduler::persistent(SchedulerRuntime::from_component(ctx), store)
                .with_catch_up_window(window),
        );
        self.scheduler
            .set(scheduler.clone())
            .expect("init runs at most once per component");

        let published: Arc<dyn Scheduler> = scheduler;
        if self.default {
            ctx.provide_default::<dyn Scheduler>(published)
        } else {
            ctx.provide::<dyn Scheduler>(published)
        }
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let scheduler = self.scheduler.get().expect("init runs before start");
        scheduler.start(handlers(ctx)).await
    }

    async fn stop(&self, _ctx: &ComponentContext) -> Result<()> {
        if let Some(scheduler) = self.scheduler.get() {
            scheduler.stop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = SchedulerComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert_eq!(component.catch_up_window, None);
    }

    #[test]
    fn builders_accumulate() {
        let component = SchedulerComponent::new()
            .with_id("scheduler.secondary")
            .as_default(false)
            .with_catch_up_window(Duration::from_secs(5));
        assert_eq!(component.id, ComponentId::new("scheduler.secondary"));
        assert!(!component.default);
        assert_eq!(component.catch_up_window, Some(Duration::from_secs(5)));
    }

    #[test]
    fn the_persistent_component_depends_on_the_database() {
        let descriptor = RedbSchedulerComponent::new().descriptor();
        let required: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(
            required,
            vec![&ComponentId::new(aik_store::DEFAULT_COMPONENT_ID)],
            "the kernel must open the database before the scheduler that writes to it"
        );
    }

    #[test]
    fn a_named_database_is_what_the_dependency_points_at() {
        let descriptor = RedbSchedulerComponent::new()
            .with_database("store.db.jobs")
            .descriptor();
        let named: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(named, vec![&ComponentId::new("store.db.jobs")]);
    }

    #[test]
    fn both_components_publish_the_same_capability_under_the_same_id() {
        assert_eq!(
            SchedulerComponent::new().descriptor().id,
            RedbSchedulerComponent::new().descriptor().id
        );
    }

    #[test]
    fn the_volatile_component_needs_no_database() {
        assert!(
            SchedulerComponent::new()
                .descriptor()
                .dependencies
                .is_empty(),
            "a scheduler that stores nothing must not force a database into the kernel"
        );
    }
}
