//! Wires a context store into the kernel as a normal component.
//!
//! Two of them, for the two implementations. They publish the same capabilities under the
//! same default id, because they are alternatives rather than complements: a kernel has one
//! context store, and choosing the durable one is meant to be a one-line change that leaves
//! every dependant, and any `components.context.store` configuration, exactly as it was.

use std::sync::Arc;

use aik_api::context::{ContextStore, TokenCounter};
use aik_core::prelude::*;
use aik_store::Db;

use crate::persistent::RedbContextStore;
use crate::store::{DEFAULT_MAX_RECORDS_PER_SESSION, InMemoryContextStore};
use crate::tokens::HeuristicTokenCounter;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "context.store";

/// Registers a [`ContextStore`] and a [`TokenCounter`] as kernel components.
///
/// Both capabilities are published, because they are useful separately: an agent resolves
/// `dyn ContextStore` to hold its transcript, while anything that merely needs to know what
/// something will cost — a router choosing a model, a UI showing a session's size —
/// resolves `dyn TokenCounter` on its own.
///
/// Unlike a `dyn Tool`, this component *may* safely be resolved from the open kernel
/// registry: a context store is infrastructure, like `dyn MemoryStore`, not a gated
/// capability. What keeps it safe is not being unreachable but that sessions are owned — see
/// [`aik_api::context`](aik_api::context#what-the-model-can-and-cannot-touch).
///
/// ```
/// use aik_context::ContextComponent;
/// use aik_core::prelude::*;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(ContextComponent::new()).build()
/// # }
/// ```
pub struct ContextComponent {
    id: ComponentId,
    default: bool,
    counter: Option<Arc<dyn TokenCounter>>,
    max_records: usize,
}

impl std::fmt::Debug for ContextComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextComponent")
            .field("id", &self.id)
            .field("default", &self.default)
            .field("token_counter_configured", &self.counter.is_some())
            .field("max_records_per_session", &self.max_records)
            .finish()
    }
}

impl Default for ContextComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], with the heuristic
    /// token counter and the default per-session record bound.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            counter: None,
            max_records: DEFAULT_MAX_RECORDS_PER_SESSION,
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether these become the registry's default implementations.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Uses a provider-specific token counter instead of
    /// [`HeuristicTokenCounter`].
    #[must_use]
    pub fn with_token_counter(mut self, counter: Arc<dyn TokenCounter>) -> Self {
        self.counter = Some(counter);
        self
    }

    /// Overrides how many records one session may hold.
    #[must_use]
    pub fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }
}

#[async_trait]
impl Component for ContextComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("in-memory agent context store with budgeted model windows")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let counter: Arc<dyn TokenCounter> = match &self.counter {
            Some(counter) => counter.clone(),
            None => Arc::new(HeuristicTokenCounter::new()),
        };

        // Assembly events go to the kernel's own bus, attributed to this component, so
        // anything watching context cost is an ordinary subscriber.
        let store = InMemoryContextStore::new()
            .with_token_counter(counter.clone())
            .with_clock(ctx.clock().clone())
            .with_events(ctx.events().clone(), self.id.clone())
            .with_max_records(self.max_records);

        let store: Arc<dyn ContextStore> = Arc::new(store);
        if self.default {
            ctx.provide_default::<dyn ContextStore>(store)?;
            ctx.provide_default::<dyn TokenCounter>(counter)
        } else {
            ctx.provide::<dyn ContextStore>(store)?;
            ctx.provide::<dyn TokenCounter>(counter)
        }
    }
}

/// Registers a [`RedbContextStore`] and a [`TokenCounter`] as kernel components.
///
/// The persistent counterpart of [`ContextComponent`]: same capabilities, same default id,
/// same registry semantics. The difference is where the transcript lives, so swapping one
/// for the other changes nothing for anything that resolves `dyn ContextStore`.
///
/// It depends on the [`aik_store`] component, which must therefore be in the kernel too.
/// The kernel orders the two and refuses to start if the database is absent, so a
/// misconfiguration is a startup failure rather than a store that quietly forgets.
///
/// ```
/// use aik_context::RedbContextComponent;
/// use aik_core::prelude::*;
/// use aik_store::StoreComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(StoreComponent::new())
///     .component(RedbContextComponent::new())
///     .build()
/// # }
/// ```
pub struct RedbContextComponent {
    id: ComponentId,
    database: ComponentId,
    default: bool,
    counter: Option<Arc<dyn TokenCounter>>,
    max_records: usize,
}

impl std::fmt::Debug for RedbContextComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbContextComponent")
            .field("id", &self.id)
            .field("database", &self.database)
            .field("default", &self.default)
            .field("token_counter_configured", &self.counter.is_some())
            .field("max_records_per_session", &self.max_records)
            .finish()
    }
}

impl Default for RedbContextComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl RedbContextComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], backed by the
    /// kernel's default database.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            database: ComponentId::new(aik_store::DEFAULT_COMPONENT_ID),
            default: true,
            counter: None,
            max_records: DEFAULT_MAX_RECORDS_PER_SESSION,
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Uses a named database rather than the registry's default one.
    ///
    /// Pair this with [`StoreComponent::with_id`](aik_store::StoreComponent::with_id) when a
    /// kernel opens more than one database; the dependency is declared on whichever id is
    /// named here, so the kernel still orders them correctly.
    #[must_use]
    pub fn with_database(mut self, database: impl Into<ComponentId>) -> Self {
        self.database = database.into();
        self
    }

    /// Controls whether these become the registry's default implementations.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Uses a provider-specific token counter instead of [`HeuristicTokenCounter`].
    #[must_use]
    pub fn with_token_counter(mut self, counter: Arc<dyn TokenCounter>) -> Self {
        self.counter = Some(counter);
        self
    }

    /// Overrides how many records one session may hold.
    #[must_use]
    pub fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }
}

#[async_trait]
impl Component for RedbContextComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("persistent agent context store with budgeted model windows")
            .requires(self.database.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let counter: Arc<dyn TokenCounter> = match &self.counter {
            Some(counter) => counter.clone(),
            None => Arc::new(HeuristicTokenCounter::new()),
        };
        // Named rather than default, so that a kernel with two databases still gets the one
        // this component was told to use.
        let db = ctx.service_named::<Db>(&self.database)?;

        let store = RedbContextStore::new(db)?
            .with_token_counter(counter.clone())
            .with_clock(ctx.clock().clone())
            .with_events(ctx.events().clone(), self.id.clone())
            .with_max_records(self.max_records);

        let store: Arc<dyn ContextStore> = Arc::new(store);
        if self.default {
            ctx.provide_default::<dyn ContextStore>(store)?;
            ctx.provide_default::<dyn TokenCounter>(counter)
        } else {
            ctx.provide::<dyn ContextStore>(store)?;
            ctx.provide::<dyn TokenCounter>(counter)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = ContextComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert!(component.counter.is_none());
        assert_eq!(component.max_records, DEFAULT_MAX_RECORDS_PER_SESSION);
    }

    #[test]
    fn builders_accumulate() {
        let component = ContextComponent::new()
            .with_id("context.secondary")
            .as_default(false)
            .with_token_counter(Arc::new(HeuristicTokenCounter::new()))
            .with_max_records(5);
        assert_eq!(component.id, ComponentId::new("context.secondary"));
        assert!(!component.default);
        assert!(component.counter.is_some());
        assert_eq!(component.max_records, 5);
    }

    #[test]
    fn the_persistent_component_depends_on_the_database() {
        let descriptor = RedbContextComponent::new().descriptor();
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
        let descriptor = RedbContextComponent::new()
            .with_database("store.db.transcripts")
            .descriptor();
        let named: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(named, vec![&ComponentId::new("store.db.transcripts")]);
    }

    #[test]
    fn both_components_publish_the_same_capabilities_under_the_same_id() {
        // Swapping one for the other must not require touching anything that resolves a
        // context store, so they are interchangeable by construction.
        assert_eq!(
            ContextComponent::new().descriptor().id,
            RedbContextComponent::new().descriptor().id
        );
    }
}
