//! Wires an audit trail into the kernel as a normal component.
//!
//! Two of them, for the two backends, published under the same default id so that choosing
//! the durable one is a one-line change — the same shape `aik-context`, `aik-memory` and
//! `aik-scheduler` use. Both subscribe to the audit events during
//! [`init`](Component::init) and drive the sink from [`start`](Component::start).
//!
//! # Why the subscription is taken in `init`
//!
//! The kernel initialises every component before it starts any. Subscribing in `init`
//! therefore guarantees the trail contains everything published from the first component's
//! `start` onward; subscribing in `start` would leave a window whose width is decided by
//! component ordering, and "which authorization decisions are audited" is not a thing that
//! should depend on the order somebody happened to register components in.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aik_api::audit::{AuditStore, AuthorizationDecided, ToolInvoked};
use aik_core::event::EventStream;
use aik_core::prelude::*;
use aik_store::Db;

use crate::persistent::RedbAuditStore;
use crate::retention::{
    AuditRetentionSweeper, DEFAULT_RETENTION_SWEEP_INTERVAL, spawn_retention_task,
};
use crate::settings::AuditSettings;
use crate::sink::{AuditSink, spawn_audit_task};
use crate::store::InMemoryAuditStore;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "audit.store";

/// The subscriptions one audit component holds between `init` and `start`.
///
/// A `Mutex<Option<..>>` rather than a `OnceLock` because `start` takes them *out*: an
/// `EventStream` is not clonable, and a component whose `start` ran twice must not end up with
/// two tasks racing for one queue.
type Subscriptions = Mutex<Option<(EventStream<AuthorizationDecided>, EventStream<ToolInvoked>)>>;

/// Everything the two components share, so that the volatile and durable wiring cannot drift.
struct Wiring {
    id: ComponentId,
    default: bool,
    retention: Option<Duration>,
    retention_interval: Option<Duration>,
    subscriptions: Subscriptions,
}

impl Wiring {
    fn new(id: &str) -> Self {
        Self {
            id: ComponentId::new(id),
            default: true,
            retention: None,
            retention_interval: None,
            subscriptions: Mutex::new(None),
        }
    }

    /// Subscribes and publishes the store. Called from `init`, once.
    fn init(&self, ctx: &ComponentContext, store: Arc<dyn AuditStore>) -> Result<()> {
        *self.subscriptions.lock().expect("the subscription lock") = Some((
            ctx.subscribe::<AuthorizationDecided>(),
            ctx.subscribe::<ToolInvoked>(),
        ));

        if self.default {
            ctx.provide_default::<dyn AuditStore>(store)
        } else {
            ctx.provide::<dyn AuditStore>(store)
        }
    }

    /// Starts the sink, and the retention sweep if one is configured. Called from `start`.
    fn start(
        &self,
        ctx: &ComponentContext,
        store: Arc<dyn AuditStore>,
        sweeper: Arc<dyn AuditRetentionSweeper>,
    ) -> Result<Arc<AuditSink>> {
        let (decisions, invocations) = self
            .subscriptions
            .lock()
            .expect("the subscription lock")
            .take()
            .ok_or_else(|| {
                Error::other("the audit component was started without having been initialised")
            })?;

        let sink = Arc::new(AuditSink::new(store, ctx.clock().clone()));
        spawn_audit_task(ctx.tasks(), sink.clone(), decisions, invocations);

        let settings: AuditSettings = ctx.settings().get_or_default("")?;
        let section = self.id.as_str();
        // The builder wins where it was used, so a deployment that pins retention in code is
        // not silently widened by a configuration file — and where it was not, the file is
        // the only place an operator can express the policy at all.
        let retention = match self.retention {
            Some(retention) => Some(retention),
            None => settings.retention(section)?,
        };
        let interval = match self.retention_interval {
            Some(interval) => Some(interval),
            None => settings.sweep_interval(section)?,
        }
        .unwrap_or(DEFAULT_RETENTION_SWEEP_INTERVAL);

        if let Some(retention) = retention {
            tracing::info!(
                days = retention.as_secs() / (24 * 60 * 60),
                "audit retention is enabled; records older than this will be removed"
            );
            spawn_retention_task(
                ctx.tasks(),
                ctx.clock().clone(),
                sweeper,
                interval,
                retention,
            );
        }

        Ok(sink)
    }
}

impl std::fmt::Debug for Wiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wiring")
            .field("id", &self.id)
            .field("default", &self.default)
            .field("retention", &self.retention)
            .field("retention_interval", &self.retention_interval)
            .finish()
    }
}

/// Registers an [`InMemoryAuditStore`] as a kernel component.
///
/// The trail lives as long as the process. That is the right pairing for an `--ephemeral`
/// run — a run that writes nothing to disk must not write its audit trail there either — and
/// it still means the run is auditable while it happens.
///
/// ```
/// use aik_audit::AuditComponent;
/// use aik_core::prelude::*;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(AuditComponent::new()).build()
/// # }
/// ```
#[derive(Debug)]
pub struct AuditComponent {
    wiring: Wiring,
    store: OnceLock<Arc<InMemoryAuditStore>>,
    sink: OnceLock<Arc<AuditSink>>,
}

impl Default for AuditComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], keeping everything it
    /// records for as long as the process lives.
    pub fn new() -> Self {
        Self {
            wiring: Wiring::new(DEFAULT_COMPONENT_ID),
            store: OnceLock::new(),
            sink: OnceLock::new(),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.wiring.id = id.into();
        self
    }

    /// Controls whether this becomes the registry's default [`AuditStore`].
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.wiring.default = default;
        self
    }

    /// Removes records older than `retention`, on a timer.
    ///
    /// Off unless called. See [`crate::retention`] for why an audit trail in particular has
    /// no default retention period.
    #[must_use]
    pub fn with_retention(mut self, retention: Duration) -> Self {
        self.wiring.retention = Some(retention);
        self
    }

    /// Overrides how often the retention sweep runs.
    ///
    /// Does nothing on its own: with no [`with_retention`](Self::with_retention) and no
    /// configured period there is no sweep to schedule.
    #[must_use]
    pub fn with_retention_interval(mut self, interval: Duration) -> Self {
        self.wiring.retention_interval = Some(interval);
        self
    }

    /// The sink this component drives, once it has started.
    ///
    /// `None` before `start`. Exposed so a test — or a frontend that wants to report a
    /// broken trail out loud — can read [`AuditSink::failures`].
    pub fn sink(&self) -> Option<&Arc<AuditSink>> {
        self.sink.get()
    }
}

#[async_trait]
impl Component for AuditComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.wiring.id.clone())
            .described("in-memory audit trail of authorization decisions and tool invocations")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let store = Arc::new(InMemoryAuditStore::new().with_clock(ctx.clock().clone()));
        self.store
            .set(store.clone())
            .expect("init runs at most once per component");
        self.wiring.init(ctx, store)
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let store = self.store.get().expect("init runs before start").clone();
        let sweeper: Arc<dyn AuditRetentionSweeper> = store.clone();
        let sink = self.wiring.start(ctx, store, sweeper)?;
        let _ = self.sink.set(sink);
        Ok(())
    }
}

/// Registers a [`RedbAuditStore`] as a kernel component.
///
/// The persistent counterpart of [`AuditComponent`]: same capability, same default id, same
/// registry semantics, and one more guarantee — the record of what this system was allowed to
/// do survives the process that did it. It depends on the [`aik_store`] component, which must
/// therefore be in the kernel too.
///
/// ```
/// use aik_audit::RedbAuditComponent;
/// use aik_core::prelude::*;
/// use aik_store::StoreComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(StoreComponent::new())
///     .component(RedbAuditComponent::new())
///     .build()
/// # }
/// ```
#[derive(Debug)]
pub struct RedbAuditComponent {
    wiring: Wiring,
    database: ComponentId,
    store: OnceLock<Arc<RedbAuditStore>>,
    sink: OnceLock<Arc<AuditSink>>,
}

impl Default for RedbAuditComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl RedbAuditComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], backed by the kernel's
    /// default database, keeping every record until something removes it.
    pub fn new() -> Self {
        Self {
            wiring: Wiring::new(DEFAULT_COMPONENT_ID),
            database: ComponentId::new(aik_store::DEFAULT_COMPONENT_ID),
            store: OnceLock::new(),
            sink: OnceLock::new(),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.wiring.id = id.into();
        self
    }

    /// Uses a named database rather than the registry's default one.
    #[must_use]
    pub fn with_database(mut self, database: impl Into<ComponentId>) -> Self {
        self.database = database.into();
        self
    }

    /// Controls whether this becomes the registry's default [`AuditStore`].
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.wiring.default = default;
        self
    }

    /// Removes records older than `retention`, on a timer.
    ///
    /// Off unless called or configured. See [`crate::retention`].
    #[must_use]
    pub fn with_retention(mut self, retention: Duration) -> Self {
        self.wiring.retention = Some(retention);
        self
    }

    /// Overrides how often the retention sweep runs.
    #[must_use]
    pub fn with_retention_interval(mut self, interval: Duration) -> Self {
        self.wiring.retention_interval = Some(interval);
        self
    }

    /// The sink this component drives, once it has started.
    pub fn sink(&self) -> Option<&Arc<AuditSink>> {
        self.sink.get()
    }
}

#[async_trait]
impl Component for RedbAuditComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.wiring.id.clone())
            .described("durable audit trail of authorization decisions and tool invocations")
            .requires(self.database.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let db = ctx.service_named::<Db>(&self.database)?;
        let store = Arc::new(RedbAuditStore::new(db)?.with_clock(ctx.clock().clone()));
        self.store
            .set(store.clone())
            .expect("init runs at most once per component");
        self.wiring.init(ctx, store)
    }

    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let store = self.store.get().expect("init runs before start").clone();
        let sweeper: Arc<dyn AuditRetentionSweeper> = store.clone();
        let sink = self.wiring.start(ctx, store, sweeper)?;
        let _ = self.sink.set(sink);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = AuditComponent::new();
        assert_eq!(component.wiring.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.wiring.default);
        assert_eq!(
            component.wiring.retention, None,
            "an audit trail must not discard records nobody asked it to discard"
        );
    }

    #[test]
    fn builders_accumulate() {
        let component = AuditComponent::new()
            .with_id("audit.secondary")
            .as_default(false)
            .with_retention(Duration::from_secs(60))
            .with_retention_interval(Duration::from_secs(5));
        assert_eq!(component.wiring.id, ComponentId::new("audit.secondary"));
        assert!(!component.wiring.default);
        assert_eq!(component.wiring.retention, Some(Duration::from_secs(60)));
        assert_eq!(
            component.wiring.retention_interval,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn the_persistent_component_depends_on_the_database() {
        let descriptor = RedbAuditComponent::new().descriptor();
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
            "the kernel must start the database before the trail that writes to it"
        );
    }

    #[test]
    fn a_named_database_is_what_the_dependency_points_at() {
        let descriptor = RedbAuditComponent::new()
            .with_database("store.db.audit")
            .descriptor();
        let named: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(named, vec![&ComponentId::new("store.db.audit")]);
    }

    #[test]
    fn both_components_publish_the_same_capability_under_the_same_id() {
        assert_eq!(
            AuditComponent::new().descriptor().id,
            RedbAuditComponent::new().descriptor().id
        );
    }

    #[test]
    fn the_persistent_component_defaults_to_keeping_everything_too() {
        assert_eq!(RedbAuditComponent::new().wiring.retention, None);
    }
}
