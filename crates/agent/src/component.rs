//! Wires [`AgentLoop`] into the kernel as a normal component.

use std::sync::Arc;

use aik_api::agent::{Agent, AgentId};
use aik_api::context::{ContextCompactor, ContextStore, TokenCounter};
use aik_api::model::ModelProvider;
use aik_api::quota::QuotaGuard;
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::prelude::*;

use crate::agent::AgentLoop;
use crate::settings::AgentLoopSettings;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "agent.loop";

/// Registers an [`AgentLoop`] as a kernel component, publishing it as `dyn Agent`.
///
/// The loop's three collaborators — `dyn ModelProvider`, `dyn ToolRegistry` and
/// `dyn ContextStore` — are resolved from the kernel registry during `init`, by capability
/// rather than by type, so swapping the Ollama provider for another one or the in-memory
/// context store for a persistent one needs no change here.
///
/// # Declare what you depend on
///
/// Resolution happens during `init`, and `init` runs in dependency order, so this component
/// must be ordered *after* the components that publish those capabilities. Say so with
/// [`AgentComponent::requires`]; without it, whether the agent finds a tool registry depends
/// on the order components happened to be added, which is exactly the kind of silent wiring
/// failure the dependency graph exists to prevent.
///
/// ```
/// use aik_agent::{AgentComponent, AgentLoopSettings};
/// use aik_core::prelude::*;
/// use aik_context::ContextComponent;
/// use aik_tools::{EchoTool, ToolsComponent};
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(ToolsComponent::new().with_tool(EchoTool::new()))
///     .component(ContextComponent::new())
///     .component(
///         AgentComponent::new("assistant", AgentLoopSettings::new("llama3.2"))
///             .requires("tools.registry")
///             .requires("context.store")
///             .requires("model.stub"),
///     )
///     .build()
/// # }
/// ```
pub struct AgentComponent {
    id: ComponentId,
    agent: AgentId,
    description: Option<String>,
    default: bool,
    requires: Vec<ComponentId>,
    settings: AgentLoopSettings,
    tools: Option<Vec<ToolName>>,
}

impl std::fmt::Debug for AgentComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentComponent")
            .field("id", &self.id)
            .field("agent", &self.agent)
            .field("default", &self.default)
            .field("requires", &self.requires)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl AgentComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default `dyn Agent`, with no declared dependencies.
    pub fn new(agent: impl Into<AgentId>, settings: AgentLoopSettings) -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            agent: agent.into(),
            description: None,
            default: true,
            requires: Vec::new(),
            settings,
            tools: None,
        }
    }

    /// Registers under a different component id.
    ///
    /// Use this to run several agents — different prompts, models or tool sets — in one
    /// kernel; pair it with [`AgentComponent::as_default`].
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this agent becomes the registry's default `dyn Agent`.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Describes the agent, for a catalogue or a UI.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declares a component that must be initialised before this one.
    ///
    /// Name the components that publish the model provider, the tool registry and the
    /// context store.
    #[must_use]
    pub fn requires(mut self, component: impl Into<ComponentId>) -> Self {
        self.requires.push(component.into());
        self
    }

    /// Restricts the agent to a fixed set of tools. See [`AgentLoop::with_tools`].
    #[must_use]
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = ToolName>) -> Self {
        self.tools = Some(tools.into_iter().collect());
        self
    }
}

#[async_trait]
impl Component for AgentComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        let mut descriptor = ComponentDescriptor::new(self.id.clone())
            .described("a model/tool agent loop over the kernel's context and tool registry");
        for dependency in &self.requires {
            descriptor = descriptor.requires(dependency.clone());
        }
        descriptor
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let models = ctx.service::<dyn ModelProvider>()?;
        let tools = ctx.service::<dyn ToolRegistry>()?;
        let context = ctx.service::<dyn ContextStore>()?;

        let mut agent = AgentLoop::new(
            self.agent.clone(),
            models,
            tools,
            context,
            self.settings.clone(),
        )
        .with_clock(ctx.clock().clone())
        .with_events(ctx.events().clone(), self.id.clone());
        // The same counter the context store uses, when one is registered, so
        // `RequestMeasured` and `ContextAssembled` report consistent numbers for the same
        // window. Its absence is not fatal — measurement degrades to an internal fallback
        // heuristic rather than the agent failing to start over a capability it does not
        // otherwise need.
        if let Ok(counter) = ctx.service::<dyn TokenCounter>() {
            agent = agent.with_token_counter(counter);
        }
        // Optional in the same way and for a stronger reason: a deployment that registered
        // no compactor has chosen the deterministic eviction the context store does on its
        // own, which is what every deployment did before compaction existed. Requiring one
        // here would turn a capability into a startup dependency.
        if let Ok(compactor) = ctx.service::<dyn ContextCompactor>() {
            agent = agent.with_compactor(compactor);
        }
        // Optional in the same way, and the asymmetry is the point: a deployment that
        // registered no guard has per-run bounds only, which is what every deployment had
        // before quotas existed. What must *not* happen is a deployment that registered one
        // and silently did not get it, and that is a wiring question rather than a resolution
        // one — `requires` is what orders the guard's component before this one, so a
        // registered guard is always found here. See `aik-runtime`, which declares it.
        if let Ok(quota) = ctx.service::<dyn QuotaGuard>() {
            agent = agent.with_quota(quota);
        }
        if let Some(description) = &self.description {
            agent = agent.described(description.clone());
        }
        if let Some(tools) = &self.tools {
            agent = agent.with_tools(tools.iter().cloned());
        }

        let agent: Arc<dyn Agent> = Arc::new(agent);
        if self.default {
            ctx.provide_default::<dyn Agent>(agent)
        } else {
            ctx.provide::<dyn Agent>(agent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = AgentComponent::new("assistant", AgentLoopSettings::new("demo"));
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert!(component.requires.is_empty());
        assert!(component.tools.is_none());
    }

    #[test]
    fn declared_dependencies_reach_the_descriptor() {
        let component = AgentComponent::new("assistant", AgentLoopSettings::new("demo"))
            .with_id("agent.secondary")
            .as_default(false)
            .described("a second agent")
            .requires("tools.registry")
            .requires("context.store")
            .with_tools([ToolName::new("kernel.echo")]);

        let descriptor = component.descriptor();
        let declared: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(descriptor.id, ComponentId::new("agent.secondary"));
        assert!(declared.contains(&&ComponentId::new("tools.registry")));
        assert!(declared.contains(&&ComponentId::new("context.store")));
        assert!(descriptor.dependencies.iter().all(|d| !d.optional));
        assert!(!component.default);
    }
}
