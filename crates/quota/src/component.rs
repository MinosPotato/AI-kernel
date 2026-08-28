//! Wires a [`LimitedQuotaGuard`] into the kernel as a normal component.
//!
//! Two of them, for the two backends, published under the same default id so that choosing
//! the durable one is a one-line change — the same shape `aik-context`, `aik-memory`,
//! `aik-scheduler` and `aik-audit` use.
//!
//! # Why the document is passed in rather than read here
//!
//! A malformed ceiling should stop a deployment from starting, with the rule that is wrong
//! named, rather than surfacing at the first model turn somebody takes. That is easiest when
//! the document is parsed by whatever assembles the kernel, before any component exists —
//! which is exactly what `aik-runtime` does with the policy document, and for the same
//! reason. A component that read its own section would be validated at `init`, which is
//! later, and would put the ceilings somewhere other than where the policy they complement
//! lives.

use std::sync::Arc;

use aik_api::quota::QuotaGuard;
use aik_core::prelude::*;
use aik_store::Db;

use crate::document::QuotaDocument;
use crate::guard::LimitedQuotaGuard;
use crate::ledger::{InMemoryUsageLedger, UsageLedger};
use crate::persistent::RedbUsageLedger;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "quota.guard";

/// What the two components share, so the volatile and durable wiring cannot drift.
#[derive(Debug)]
struct Wiring {
    id: ComponentId,
    default: bool,
    document: QuotaDocument,
}

impl Wiring {
    fn new(document: QuotaDocument) -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            document,
        }
    }

    fn init(&self, ctx: &ComponentContext, ledger: Arc<dyn UsageLedger>) -> Result<()> {
        let guard =
            LimitedQuotaGuard::new(self.document.clone(), ledger)?.with_clock(ctx.clock().clone());
        if self.document.is_empty() {
            tracing::debug!("no spend ceilings are configured; the quota guard is inert");
        } else {
            tracing::info!(
                rules = self.document.limits.len(),
                "spend ceilings are in force"
            );
        }

        let guard: Arc<dyn QuotaGuard> = Arc::new(guard);
        if self.default {
            ctx.provide_default::<dyn QuotaGuard>(guard)
        } else {
            ctx.provide::<dyn QuotaGuard>(guard)
        }
    }
}

/// Registers a quota guard whose counters live as long as the process.
///
/// The right pairing for an `--ephemeral` deployment: a run that writes nothing to disk must
/// not write its ledger there either, and the ceilings are still enforced while it runs.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_quota::{QuotaComponent, QuotaDocument};
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(QuotaComponent::new(QuotaDocument::empty()))
///     .build()
/// # }
/// ```
#[derive(Debug)]
pub struct QuotaComponent {
    wiring: Wiring,
}

impl QuotaComponent {
    /// Creates a component enforcing `document`, registered under [`DEFAULT_COMPONENT_ID`]
    /// as the registry's default `dyn QuotaGuard`.
    pub fn new(document: QuotaDocument) -> Self {
        Self {
            wiring: Wiring::new(document),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.wiring.id = id.into();
        self
    }

    /// Controls whether this becomes the registry's default `dyn QuotaGuard`.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.wiring.default = default;
        self
    }
}

#[async_trait]
impl Component for QuotaComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.wiring.id.clone())
            .described("in-memory cumulative spend ceilings")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        self.wiring.init(ctx, Arc::new(InMemoryUsageLedger::new()))
    }
}

/// Registers a quota guard whose counters outlive the process.
///
/// The persistent counterpart of [`QuotaComponent`]: same capability, same default id, same
/// registry semantics, and one more guarantee — a budget cannot be reset by restarting the
/// thing it constrains. It depends on the [`aik_store`] component, which must therefore be in
/// the kernel too.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_quota::{QuotaDocument, RedbQuotaComponent};
/// use aik_store::StoreComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(StoreComponent::new())
///     .component(RedbQuotaComponent::new(QuotaDocument::empty()))
///     .build()
/// # }
/// ```
#[derive(Debug)]
pub struct RedbQuotaComponent {
    wiring: Wiring,
    database: ComponentId,
}

impl RedbQuotaComponent {
    /// Creates a component enforcing `document` over the kernel's default database.
    pub fn new(document: QuotaDocument) -> Self {
        Self {
            wiring: Wiring::new(document),
            database: ComponentId::new(aik_store::DEFAULT_COMPONENT_ID),
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

    /// Controls whether this becomes the registry's default `dyn QuotaGuard`.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.wiring.default = default;
        self
    }
}

#[async_trait]
impl Component for RedbQuotaComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.wiring.id.clone())
            .described("durable cumulative spend ceilings")
            .requires(self.database.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let db = ctx.service_named::<Db>(&self.database)?;
        self.wiring.init(ctx, Arc::new(RedbUsageLedger::new(db)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::QuotaRule;
    use crate::period::QuotaPeriod;

    fn document() -> QuotaDocument {
        QuotaDocument {
            limits: vec![QuotaRule::turns("*", QuotaPeriod::Day, 10)],
            ..QuotaDocument::empty()
        }
    }

    #[test]
    fn defaults_are_sensible() {
        let component = QuotaComponent::new(document());
        assert_eq!(component.wiring.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.wiring.default);
    }

    #[test]
    fn both_backends_publish_the_same_capability_under_the_same_id() {
        assert_eq!(
            QuotaComponent::new(document()).descriptor().id,
            RedbQuotaComponent::new(document()).descriptor().id
        );
    }

    #[test]
    fn the_persistent_component_depends_on_the_database() {
        let descriptor = RedbQuotaComponent::new(document()).descriptor();
        let required: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert!(required.contains(&&ComponentId::new(aik_store::DEFAULT_COMPONENT_ID)));
    }

    #[tokio::test]
    async fn a_kernel_resolves_the_guard_by_capability() {
        let kernel = Kernel::builder()
            .component(QuotaComponent::new(document()))
            .build()
            .unwrap();
        kernel.start().await.unwrap();
        assert!(kernel.context().service::<dyn QuotaGuard>().is_ok());
        kernel.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_invalid_document_stops_the_kernel_from_starting() {
        let kernel = Kernel::builder()
            .component(QuotaComponent::new(QuotaDocument {
                limits: vec![QuotaRule::turns("*", QuotaPeriod::Day, 0)],
                ..QuotaDocument::empty()
            }))
            .build()
            .unwrap();
        assert!(kernel.start().await.is_err());
    }
}
