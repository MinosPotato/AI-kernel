//! Wires a [`Summariser`] into the kernel as a normal component.

use std::sync::Arc;

use aik_api::context::{ContextCompactor, ContextStore};
use aik_api::model::ModelProvider;
use aik_core::prelude::*;

use crate::settings::SummarySettings;
use crate::summariser::Summariser;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "context.compactor";

/// Registers a [`Summariser`] as a kernel component, publishing it as `dyn ContextCompactor`.
///
/// Its two collaborators — `dyn ModelProvider` and `dyn ContextStore` — are resolved from the
/// registry during `init`, by capability rather than by type, so the same component compacts
/// an in-memory transcript with a local model and a durable one with a hosted model, with no
/// change here.
///
/// # Declare what you depend on
///
/// Resolution happens during `init`, which runs in dependency order, so this component must
/// be ordered after whatever publishes the model provider and the context store. Say so with
/// [`SummaryComponent::requires`]; without it, whether compaction finds a provider depends on
/// the order components happened to be added.
///
/// Nothing depends on *this* component in turn. The agent loop resolves `dyn ContextCompactor`
/// if one is registered and works exactly as it always did if none is, so a deployment turns
/// summarisation on by adding this component and off by leaving it out.
///
/// ```
/// use aik_context::ContextComponent;
/// use aik_core::prelude::*;
/// use aik_summary::{SummaryComponent, SummarySettings};
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder()
///     .component(ContextComponent::new())
///     .component(
///         SummaryComponent::new(SummarySettings::new("llama3.2"))
///             .requires("context.store")
///             .requires("model.stub"),
///     )
///     .build()
/// # }
/// ```
pub struct SummaryComponent {
    id: ComponentId,
    default: bool,
    requires: Vec<ComponentId>,
    settings: SummarySettings,
}

impl std::fmt::Debug for SummaryComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SummaryComponent")
            .field("id", &self.id)
            .field("default", &self.default)
            .field("requires", &self.requires)
            .field("model", &self.settings.model)
            .finish_non_exhaustive()
    }
}

impl SummaryComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default `dyn ContextCompactor`, with no declared dependencies.
    pub fn new(settings: SummarySettings) -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            requires: Vec::new(),
            settings,
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this becomes the registry's default `dyn ContextCompactor`.
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Declares a component that must be initialised before this one.
    #[must_use]
    pub fn requires(mut self, component: impl Into<ComponentId>) -> Self {
        self.requires.push(component.into());
        self
    }
}

#[async_trait]
impl Component for SummaryComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        let mut descriptor = ComponentDescriptor::new(self.id.clone())
            .described("replaces a session's oldest turns with a model-written recap of them");
        for dependency in &self.requires {
            descriptor = descriptor.requires(dependency.clone());
        }
        descriptor
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let models = ctx.service::<dyn ModelProvider>()?;
        let context = ctx.service::<dyn ContextStore>()?;

        let compactor: Arc<dyn ContextCompactor> = Arc::new(
            Summariser::new(models, context, self.settings.clone())
                .with_clock(ctx.clock().clone())
                .with_events(ctx.events().clone(), self.id.clone()),
        );

        if self.default {
            ctx.provide_default::<dyn ContextCompactor>(compactor)
        } else {
            ctx.provide::<dyn ContextCompactor>(compactor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = SummaryComponent::new(SummarySettings::new("demo"));
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert!(component.requires.is_empty());
    }

    #[test]
    fn declared_dependencies_reach_the_descriptor() {
        let component = SummaryComponent::new(SummarySettings::new("demo"))
            .with_id("context.compactor.secondary")
            .as_default(false)
            .requires("context.store");

        let descriptor = component.descriptor();
        let declared: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(
            descriptor.id,
            ComponentId::new("context.compactor.secondary")
        );
        assert!(declared.contains(&&ComponentId::new("context.store")));
        assert!(!component.default);
    }
}
