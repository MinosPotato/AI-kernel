//! Wires [`InProcessToolRegistry`] into the kernel as a normal component.

use std::sync::Arc;

use aik_api::permission::{ApprovalSink, PolicyEngine};
use aik_api::tool::{Tool, ToolRegistry};
use aik_core::prelude::*;

use crate::registry::InProcessToolRegistry;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "tools.registry";

/// Registers an [`InProcessToolRegistry`] as a kernel component.
///
/// Tools, the policy engine, and the approval sink are all supplied explicitly, at build
/// time, before the kernel starts — see [`InProcessToolRegistry`] for why this component
/// does not instead resolve a `dyn PolicyEngine` from the kernel registry during `init`.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_tools::{EchoTool, ToolsComponent};
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(ToolsComponent::new().with_tool(EchoTool::new()))
///     .build()
/// # }
/// ```
pub struct ToolsComponent {
    id: ComponentId,
    default: bool,
    tools: Vec<Arc<dyn Tool>>,
    policy: Option<Arc<dyn PolicyEngine>>,
    approvals: Option<Arc<dyn ApprovalSink>>,
}

impl std::fmt::Debug for ToolsComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<String> = self
            .tools
            .iter()
            .map(|tool| tool.spec().name.to_string())
            .collect();
        names.sort();
        f.debug_struct("ToolsComponent")
            .field("id", &self.id)
            .field("default", &self.default)
            .field("tools", &names)
            .field("policy_configured", &self.policy.is_some())
            .field("approvals_configured", &self.approvals.is_some())
            .finish()
    }
}

impl Default for ToolsComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolsComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default `dyn ToolRegistry`, with no tools, no policy, and no approval sink.
    ///
    /// With no policy configured, any tool requiring a permission is denied
    /// unconditionally — see [`InProcessToolRegistry`].
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            tools: Vec::new(),
            policy: None,
            approvals: None,
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this registry becomes the registry's default `dyn ToolRegistry`.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Adds a tool.
    #[must_use]
    pub fn with_tool(mut self, tool: impl Tool) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Configures the policy engine consulted for every permission a tool requires.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn PolicyEngine>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Configures where a permission that requires human approval is resolved.
    #[must_use]
    pub fn with_approvals(mut self, approvals: Arc<dyn ApprovalSink>) -> Self {
        self.approvals = Some(approvals);
        self
    }
}

#[async_trait]
impl Component for ToolsComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("in-process, authorization-gated tool registry")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        // Audit events go to the kernel's own bus, attributed to this component, so an
        // audit sink is an ordinary subscriber and needs no wiring of its own.
        let mut registry = InProcessToolRegistry::new()
            .with_audit(ctx.events().clone(), self.id.clone())
            .with_clock(ctx.clock().clone());
        if let Some(policy) = &self.policy {
            registry = registry.with_policy(policy.clone());
        }
        if let Some(approvals) = &self.approvals {
            registry = registry.with_approvals(approvals.clone());
        }
        for tool in &self.tools {
            registry.register_arc(tool.clone())?;
        }

        let registry: Arc<dyn ToolRegistry> = Arc::new(registry);
        if self.default {
            ctx.provide_default::<dyn ToolRegistry>(registry)
        } else {
            ctx.provide::<dyn ToolRegistry>(registry)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::execution::ExecutionContext;
    use aik_api::permission::{Decision, PermissionRequest};

    struct AlwaysAllow;

    #[async_trait]
    impl PolicyEngine for AlwaysAllow {
        async fn evaluate(
            &self,
            _request: &PermissionRequest,
            _cx: &ExecutionContext,
        ) -> Result<Decision> {
            Ok(Decision::Allow)
        }
    }

    #[test]
    fn defaults_are_sensible() {
        let component = ToolsComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert!(component.tools.is_empty());
    }

    #[test]
    fn builders_accumulate() {
        let component = ToolsComponent::new()
            .with_id("tools.secondary")
            .as_default(false)
            .with_tool(crate::EchoTool::new())
            .with_policy(Arc::new(AlwaysAllow));
        assert_eq!(component.id, ComponentId::new("tools.secondary"));
        assert!(!component.default);
        assert_eq!(component.tools.len(), 1);
        assert!(component.policy.is_some());
    }
}
