//! Wires an [`ApprovalBroker`] into the kernel as a normal component.

use std::sync::Arc;

use aik_core::prelude::*;

use crate::broker::ApprovalBroker;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "approval.broker";

/// Publishes an [`ApprovalBroker`] as a kernel service, and closes it on shutdown.
///
/// The broker is constructed by the host rather than by this component, because the same
/// value has to be handed to whatever consumes it as an
/// [`ApprovalSink`](aik_api::permission::ApprovalSink) — typically
/// `ToolsComponent::with_approvals` — before the kernel is built. What the component adds
/// is the two things that need a running kernel:
///
/// * the broker is resolvable, so a frontend component can attach its
///   [`ApprovalGate`](crate::ApprovalGate) during its own `init` without the host having to
///   thread a handle through;
/// * [`ApprovalBroker::close`] runs on `stop`, so shutdown refuses whatever was still
///   waiting instead of leaving a tool call parked on an answer that will never come.
///
/// Registering the broker in the kernel registry deliberately publishes the *asking* side.
/// Answering needs [`ApprovalBroker::gate`], and everything holding a
/// [`KernelContext`](aik_core::KernelContext) is trusted infrastructure — components, not
/// agents, which only ever hold a [`ToolRegistry`](aik_api::tool::ToolRegistry). See
/// [`ApprovalGate`](crate::ApprovalGate) for why that distinction has to hold.
///
/// ```
/// use std::sync::Arc;
/// use aik_approval::{ApprovalBroker, ApprovalComponent};
/// use aik_core::prelude::*;
///
/// # fn build() -> Result<Kernel> {
/// let broker = Arc::new(ApprovalBroker::new());
/// Kernel::builder()
///     .component(ApprovalComponent::new(broker.clone()))
///     .build()
/// # }
/// ```
#[derive(Debug)]
pub struct ApprovalComponent {
    id: ComponentId,
    broker: Arc<ApprovalBroker>,
    default: bool,
}

impl ApprovalComponent {
    /// Registers `broker` under [`DEFAULT_COMPONENT_ID`], as the default `ApprovalBroker`.
    pub fn new(broker: Arc<ApprovalBroker>) -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            broker,
            default: true,
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this broker becomes the registry's default `ApprovalBroker`.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }
}

#[async_trait]
impl Component for ApprovalComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("human-in-the-loop approval broker for authorization decisions")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        if self.default {
            ctx.provide_default::<ApprovalBroker>(self.broker.clone())
        } else {
            ctx.provide::<ApprovalBroker>(self.broker.clone())
        }
    }

    async fn stop(&self, _ctx: &ComponentContext) -> Result<()> {
        self.broker.close();
        Ok(())
    }

    /// Reports degraded while nobody can answer.
    ///
    /// Not down: a deployment may legitimately run with no frontend attached, and every
    /// request in that state is refused rather than mishandled. It is worth surfacing,
    /// because from an agent's side it looks identical to a policy that denies everything.
    async fn health(&self) -> Health {
        if self.broker.is_closed() {
            Health::down("the approval broker is closed")
        } else if self.broker.gate_count() == 0 {
            Health::degraded("no approval responder is attached; approvals are refused")
        } else {
            Health::up()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::component::HealthStatus;

    #[tokio::test]
    async fn the_broker_is_resolvable_and_closed_on_shutdown() {
        let broker = Arc::new(ApprovalBroker::new());
        let kernel = Kernel::builder()
            .component(ApprovalComponent::new(broker.clone()))
            .build()
            .unwrap();
        kernel.start().await.unwrap();

        let resolved = kernel.context().service::<ApprovalBroker>().unwrap();
        let gate = resolved.gate();
        assert_eq!(
            broker.gate_count(),
            1,
            "the resolved broker is the same one"
        );
        drop(gate);

        kernel.shutdown().await.unwrap();
        assert!(broker.is_closed());
    }

    #[tokio::test]
    async fn health_reflects_whether_anyone_can_answer() {
        let broker = Arc::new(ApprovalBroker::new());
        let component = ApprovalComponent::new(broker.clone());

        assert_eq!(component.health().await.status, HealthStatus::Degraded);
        let gate = broker.gate();
        assert_eq!(component.health().await.status, HealthStatus::Up);
        drop(gate);

        broker.close();
        assert_eq!(component.health().await.status, HealthStatus::Down);
    }
}
