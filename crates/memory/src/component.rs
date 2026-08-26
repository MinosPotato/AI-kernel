//! Wires a memory store into the kernel as a normal component.
//!
//! Two of them, for the two implementations, published under the same default id so that
//! choosing the durable one is a one-line change — exactly the same shape as
//! [`ContextComponent`](aik_context) and [`RedbContextComponent`](aik_context) use for the
//! transcript store. Both also spawn the background sweep [`crate::expiry`] describes: a
//! task, owned by this component's scope, that reclaims expired records on a timer and is
//! cancelled cleanly when the component stops.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aik_api::memory::MemoryStore;
use aik_api::model::{Embedder, ModelId};
use aik_core::prelude::*;
use aik_store::Db;

use crate::expiry::{ExpirySweeper, spawn_expiry_task};
use crate::persistent::RedbMemoryStore;
use crate::store::InMemoryMemoryStore;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "memory.store";

/// How often the background sweep looks for expired records, when none is configured.
///
/// Expiration is already enforced on every read, so this interval
/// only decides how promptly space is reclaimed, not how promptly a caller stops seeing an
/// expired record. A minute is frequent enough that a database's dead weight never grows far
/// past what expired since the last sweep, without running a write transaction so often that
/// it competes with real traffic.
pub const DEFAULT_EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Which component supplies embeddings, and which model it is asked for.
///
/// The component is named rather than resolved as the registry's default `dyn Embedder`,
/// for the same reason [`MemoryToolsComponent`](crate::MemoryToolsComponent) names the store
/// it binds to: a named dependency is one the kernel can *order*, so the provider is started
/// before the store that calls it, whatever order the two were added in. A default resolved
/// during `init` would depend on that order instead.
#[derive(Debug, Clone)]
struct EmbedderChoice {
    component: ComponentId,
    model: ModelId,
}

/// Registers an [`InMemoryMemoryStore`] as a kernel component.
///
/// ```
/// use aik_memory::MemoryComponent;
/// use aik_core::prelude::*;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(MemoryComponent::new()).build()
/// # }
/// ```
pub struct MemoryComponent {
    id: ComponentId,
    default: bool,
    expiry_interval: Duration,
    embedder: Option<EmbedderChoice>,
    store: OnceLock<Arc<InMemoryMemoryStore>>,
}

impl std::fmt::Debug for MemoryComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryComponent")
            .field("id", &self.id)
            .field("default", &self.default)
            .field("expiry_interval", &self.expiry_interval)
            .field("embedder", &self.embedder)
            .finish()
    }
}

impl Default for MemoryComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], sweeping every
    /// [`DEFAULT_EXPIRY_SWEEP_INTERVAL`].
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            expiry_interval: DEFAULT_EXPIRY_SWEEP_INTERVAL,
            embedder: None,
            store: OnceLock::new(),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this becomes the registry's default [`MemoryStore`].
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Overrides how often the expiry sweep runs.
    #[must_use]
    pub fn with_expiry_interval(mut self, interval: Duration) -> Self {
        self.expiry_interval = interval;
        self
    }

    /// Turns on semantic search, embedding with `model` through the `dyn Embedder` published
    /// by `component`.
    ///
    /// The component becomes a declared dependency, so a kernel missing it fails to build
    /// rather than starting a store that cannot embed. See
    /// [`RedbMemoryStore::with_embedder`](crate::RedbMemoryStore::with_embedder) for what
    /// this changes about writes.
    #[must_use]
    pub fn with_embedder(
        mut self,
        component: impl Into<ComponentId>,
        model: impl Into<ModelId>,
    ) -> Self {
        self.embedder = Some(EmbedderChoice {
            component: component.into(),
            model: model.into(),
        });
        self
    }
}

#[async_trait]
impl Component for MemoryComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        let descriptor = ComponentDescriptor::new(self.id.clone())
            .described("in-memory record store with a periodic expiry sweep");
        match &self.embedder {
            Some(choice) => descriptor.requires(choice.component.clone()),
            None => descriptor,
        }
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let mut store = InMemoryMemoryStore::new().with_clock(ctx.clock().clone());
        if let Some(choice) = &self.embedder {
            let embedder = ctx.service_named::<dyn Embedder>(&choice.component)?;
            store = store.with_embedder(embedder, choice.model.clone());
        }
        let store = Arc::new(store);
        // Set before publishing: `start` looks the concrete store up here to drive the
        // sweep, and must never find the slot empty for a component that finished `init`.
        self.store
            .set(store.clone())
            .expect("init runs at most once per component");

        let published: Arc<dyn MemoryStore> = store;
        if self.default {
            ctx.provide_default::<dyn MemoryStore>(published)
        } else {
            ctx.provide::<dyn MemoryStore>(published)
        }
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let store = self.store.get().expect("init runs before start").clone();
        let sweeper: Arc<dyn ExpirySweeper> = store;
        spawn_expiry_task(
            ctx.tasks(),
            ctx.clock().clone(),
            sweeper,
            self.expiry_interval,
        );
        Ok(())
    }
}

/// Registers a [`RedbMemoryStore`] as a kernel component.
///
/// The persistent counterpart of [`MemoryComponent`]: same capability, same default id, same
/// registry semantics. It depends on the [`aik_store`] component, which must therefore be in
/// the kernel too.
///
/// ```
/// use aik_memory::RedbMemoryComponent;
/// use aik_core::prelude::*;
/// use aik_store::StoreComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(StoreComponent::new())
///     .component(RedbMemoryComponent::new())
///     .build()
/// # }
/// ```
pub struct RedbMemoryComponent {
    id: ComponentId,
    database: ComponentId,
    default: bool,
    expiry_interval: Duration,
    embedder: Option<EmbedderChoice>,
    store: OnceLock<Arc<RedbMemoryStore>>,
}

impl std::fmt::Debug for RedbMemoryComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbMemoryComponent")
            .field("id", &self.id)
            .field("database", &self.database)
            .field("default", &self.default)
            .field("expiry_interval", &self.expiry_interval)
            .field("embedder", &self.embedder)
            .finish()
    }
}

impl Default for RedbMemoryComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl RedbMemoryComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], backed by the
    /// kernel's default database, sweeping every [`DEFAULT_EXPIRY_SWEEP_INTERVAL`].
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            database: ComponentId::new(aik_store::DEFAULT_COMPONENT_ID),
            default: true,
            expiry_interval: DEFAULT_EXPIRY_SWEEP_INTERVAL,
            embedder: None,
            store: OnceLock::new(),
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

    /// Controls whether this becomes the registry's default [`MemoryStore`].
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Overrides how often the expiry sweep runs.
    #[must_use]
    pub fn with_expiry_interval(mut self, interval: Duration) -> Self {
        self.expiry_interval = interval;
        self
    }

    /// Turns on semantic search, embedding with `model` through the `dyn Embedder` published
    /// by `component`.
    ///
    /// See [`MemoryComponent::with_embedder`], and
    /// [`RedbMemoryStore::with_embedder`](crate::RedbMemoryStore::with_embedder) for what it
    /// changes about writes and about records written before it was turned on.
    #[must_use]
    pub fn with_embedder(
        mut self,
        component: impl Into<ComponentId>,
        model: impl Into<ModelId>,
    ) -> Self {
        self.embedder = Some(EmbedderChoice {
            component: component.into(),
            model: model.into(),
        });
        self
    }
}

#[async_trait]
impl Component for RedbMemoryComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        let descriptor = ComponentDescriptor::new(self.id.clone())
            .described("persistent record store with a periodic expiry sweep")
            .requires(self.database.clone());
        match &self.embedder {
            Some(choice) => descriptor.requires(choice.component.clone()),
            None => descriptor,
        }
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let db = ctx.service_named::<Db>(&self.database)?;
        let mut store = RedbMemoryStore::new(db)?.with_clock(ctx.clock().clone());
        if let Some(choice) = &self.embedder {
            let embedder = ctx.service_named::<dyn Embedder>(&choice.component)?;
            store = store.with_embedder(embedder, choice.model.clone());
        }
        let store = Arc::new(store);
        self.store
            .set(store.clone())
            .expect("init runs at most once per component");

        let published: Arc<dyn MemoryStore> = store;
        if self.default {
            ctx.provide_default::<dyn MemoryStore>(published)
        } else {
            ctx.provide::<dyn MemoryStore>(published)
        }
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let store = self.store.get().expect("init runs before start").clone();
        let sweeper: Arc<dyn ExpirySweeper> = store;
        spawn_expiry_task(
            ctx.tasks(),
            ctx.clock().clone(),
            sweeper,
            self.expiry_interval,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = MemoryComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert_eq!(component.expiry_interval, DEFAULT_EXPIRY_SWEEP_INTERVAL);
    }

    #[test]
    fn builders_accumulate() {
        let component = MemoryComponent::new()
            .with_id("memory.secondary")
            .as_default(false)
            .with_expiry_interval(Duration::from_secs(5));
        assert_eq!(component.id, ComponentId::new("memory.secondary"));
        assert!(!component.default);
        assert_eq!(component.expiry_interval, Duration::from_secs(5));
    }

    #[test]
    fn the_persistent_component_depends_on_the_database() {
        let descriptor = RedbMemoryComponent::new().descriptor();
        assert_eq!(descriptor.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        let required: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(
            required,
            vec![&ComponentId::new(aik_store::DEFAULT_COMPONENT_ID)],
            "the kernel must start the database before the store that writes to it"
        );
    }

    #[test]
    fn a_named_database_is_what_the_dependency_points_at() {
        let descriptor = RedbMemoryComponent::new()
            .with_database("store.db.memories")
            .descriptor();
        let named: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(named, vec![&ComponentId::new("store.db.memories")]);
    }

    #[test]
    fn both_components_publish_the_same_capability_under_the_same_id() {
        assert_eq!(
            MemoryComponent::new().descriptor().id,
            RedbMemoryComponent::new().descriptor().id
        );
    }
}
