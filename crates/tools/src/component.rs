//! Wires [`InProcessToolRegistry`] into the kernel as a normal component.

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ApprovalSink, PolicyEngine};
use aik_api::tool::{Tool, ToolCatalog, ToolRegistry};
use aik_core::prelude::*;

use crate::registry::InProcessToolRegistry;
use crate::trust::TrustEnforcement;

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
    catalogs: Vec<Arc<dyn ToolCatalog>>,
    policy: Option<Arc<dyn PolicyEngine>>,
    approvals: Option<Arc<dyn ApprovalSink>>,
    enforcement: TrustEnforcement,
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
            .field("catalogs", &self.catalogs.len())
            .field("policy_configured", &self.policy.is_some())
            .field("approvals_configured", &self.approvals.is_some())
            .field("trust_enforcement", &self.enforcement)
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
            catalogs: Vec::new(),
            policy: None,
            approvals: None,
            enforcement: TrustEnforcement::default(),
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

    /// Adds every tool a catalogue offers.
    ///
    /// The tools a [`ToolCatalog`] contributes are discovered rather than written down —
    /// an external server is asked, at `init`, what it serves — so they cannot be supplied
    /// through [`ToolsComponent::with_tool`], which needs the value up front. What does not
    /// change is when registration happens: the catalogue is drained once, during `init`,
    /// before anything holds an `Arc<dyn ToolRegistry>`, so the set of tools is still frozen
    /// by the time it is reachable. There is no path that adds a tool to a registry that is
    /// already in use.
    ///
    /// A catalogue that cannot be listed is a startup failure, not an empty contribution. A
    /// deployment that configured an external tool source and got none of it would be one
    /// whose model quietly cannot do what the operator believes it can, and the kernel not
    /// starting is the smaller problem. The same goes for a name collision: a catalogue
    /// offering a tool that is already registered — under a native tool's name, or another
    /// catalogue's — is refused by [`InProcessToolRegistry::register_arc`], so nothing can
    /// shadow `fs.write`.
    #[must_use]
    pub fn with_catalog(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.catalogs.push(catalog);
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

    /// Sets how strictly the registry enforces [provenance](aik_api::provenance).
    ///
    /// Defaults to [`TrustEnforcement::Approval`]. There is deliberately no setting here for
    /// whether provenance is *tracked*: it always is.
    #[must_use]
    pub fn with_trust_enforcement(mut self, enforcement: TrustEnforcement) -> Self {
        self.enforcement = enforcement;
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
            .with_clock(ctx.clock().clone())
            .with_trust_enforcement(self.enforcement);
        if let Some(policy) = &self.policy {
            registry = registry.with_policy(policy.clone());
        }
        if let Some(approvals) = &self.approvals {
            registry = registry.with_approvals(approvals.clone());
        }
        for tool in &self.tools {
            registry.register_arc(tool.clone())?;
        }

        // Catalogues after the tools this deployment wrote itself, so that a collision is
        // always the catalogue's to lose: a native tool is never displaced by a discovered
        // one, whichever order they were configured in.
        //
        // The context is a plain root one rather than anything a caller supplied, because
        // there is no caller — this is the kernel starting up, acting as itself. Listing is
        // not authorized in any case: knowing a tool exists is not being allowed to use it.
        let cx = ExecutionContext::new();
        for catalog in &self.catalogs {
            for spec in catalog.list(&cx).await? {
                let tool = catalog
                    .get(&spec.name, &cx)
                    .await?
                    .ok_or_else(|| Error::not_found("tool", &spec.name))?;
                registry.register_arc(Arc::from(tool))?;
            }
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
    use aik_api::provenance::{Reach, Trust};

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

    /// A catalogue offering exactly the tools it was built from.
    struct FixedCatalog(Vec<&'static str>);

    #[async_trait]
    impl ToolCatalog for FixedCatalog {
        async fn list(&self, _cx: &ExecutionContext) -> Result<Vec<aik_api::tool::ToolSpec>> {
            Ok(self
                .0
                .iter()
                .map(|name| aik_api::tool::ToolSpec {
                    name: aik_api::tool::ToolName::new(*name),
                    description: "from a catalogue".into(),
                    input_schema: serde_json::json!({ "type": "object" }),
                    output_schema: None,
                    required_permissions: Vec::new(),
                    read_only: false,
                    output_trust: Trust::Untrusted,
                    reach: Reach::External,
                })
                .collect())
        }

        async fn get(
            &self,
            name: &aik_api::tool::ToolName,
            _cx: &ExecutionContext,
        ) -> Result<Option<Box<dyn Tool>>> {
            Ok(self
                .0
                .iter()
                .find(|candidate| *name == aik_api::tool::ToolName::new(**candidate))
                .map(|name| Box::new(crate::EchoTool::new().with_name(*name)) as Box<dyn Tool>))
        }
    }

    #[test]
    fn defaults_are_sensible() {
        let component = ToolsComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert!(component.tools.is_empty());
        assert!(component.catalogs.is_empty());
    }

    #[tokio::test]
    async fn a_catalogue_s_tools_are_registered_at_init() {
        let kernel = Kernel::builder()
            .component(
                ToolsComponent::new()
                    .with_catalog(Arc::new(FixedCatalog(vec!["remote.one", "remote.two"]))),
            )
            .build()
            .unwrap();
        kernel.start().await.unwrap();

        let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
        let names: Vec<String> = registry
            .list(&ExecutionContext::new())
            .await
            .unwrap()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .collect();
        assert_eq!(names, ["remote.one", "remote.two"]);

        kernel.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_catalogue_cannot_shadow_a_tool_this_deployment_wrote() {
        // The failure this rules out is an external server offering `kernel.echo` and being
        // called wherever the native one would have been.
        let error = Kernel::builder()
            .component(
                ToolsComponent::new()
                    .with_tool(crate::EchoTool::new())
                    .with_catalog(Arc::new(FixedCatalog(vec![crate::DEFAULT_NAME]))),
            )
            .build()
            .unwrap()
            .start()
            .await
            .unwrap_err();

        // The kernel wraps a component's `init` failure, so the name is in the source chain
        // rather than in the outermost message.
        let mut chain = format!("{error}");
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            chain.push_str(&format!(": {cause}"));
            source = std::error::Error::source(cause);
        }
        assert!(chain.contains(crate::DEFAULT_NAME), "{chain}");
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
