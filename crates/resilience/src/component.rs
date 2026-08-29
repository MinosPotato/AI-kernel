//! Wiring a [`ResilientProvider`] in front of whichever provider the deployment chose.

use std::sync::Arc;

use aik_api::model::ModelProvider;
use aik_core::prelude::*;

use crate::provider::ResilientProvider;
use crate::settings::ResilienceSettings;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "model.resilient";

/// Registers a [`ResilientProvider`] wrapping another provider, as the default
/// `dyn ModelProvider`.
///
/// # How the substitution works, and why it is safe
///
/// The wrapped provider stays registered under its own component id and remains reachable by
/// name; this component registers itself under a *different* id and becomes the registry's
/// default. Every consumer that resolves `dyn ModelProvider` by capability — the agent loop,
/// the compactor — therefore gets the wrapped one, and nothing had to be told about it.
///
/// That only holds if this component initialises after the provider it wraps and before
/// anything that resolves one, which is why [`wrapping`](ResilienceComponent::wrapping) both
/// names the inner provider and declares a dependency on it: registration order is the whole
/// mechanism, and leaving it to the order components happened to be added would make "is this
/// deployment retrying anything?" depend on something nobody looks at.
///
/// # Reading the inner provider by name rather than as the default
///
/// Deliberately. Resolving the default would work today and would silently become
/// self-wrapping the moment a second resilience component existed, which is a stack overflow
/// on the first model call rather than a configuration error at start-up.
pub struct ResilienceComponent {
    id: ComponentId,
    inner: Option<ComponentId>,
    settings: Option<ResilienceSettings>,
}

impl std::fmt::Debug for ResilienceComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResilienceComponent")
            .field("id", &self.id)
            .field("inner", &self.inner)
            .field("settings", &self.settings)
            .finish()
    }
}

impl ResilienceComponent {
    /// Creates a component with fixed settings, registered under [`DEFAULT_COMPONENT_ID`].
    ///
    /// Settings given here win over the component's configuration section, which is what a
    /// frontend wants: a value it resolved from a flag and a file should not be quietly
    /// overridden by a second copy of the file.
    pub fn new(settings: ResilienceSettings) -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            inner: None,
            settings: Some(settings),
        }
    }

    /// Creates a component that reads its settings from its own configuration section.
    ///
    /// A section that is absent means the defaults; a section that is malformed fails at
    /// start-up rather than at the first failed call.
    pub fn from_config() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            inner: None,
            settings: None,
        }
    }

    /// Names the provider to wrap, and declares it as a dependency.
    #[must_use]
    pub fn wrapping(mut self, provider: impl Into<ComponentId>) -> Self {
        self.inner = Some(provider.into());
        self
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// The settings this component will use, resolving the configuration section if needed.
    fn settings(&self, ctx: &ComponentContext) -> Result<ResilienceSettings> {
        if let Some(settings) = self.settings {
            return Ok(settings);
        }
        ctx.settings().get_or_default("").map_err(|error| {
            Error::config(
                format!("components.{}", self.id),
                format!("the resilience settings are malformed: {error}"),
            )
        })
    }
}

#[async_trait]
impl Component for ResilienceComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        let descriptor = ComponentDescriptor::new(self.id.clone())
            .described("retries, bounds and circuit-breaks calls to a model provider");
        match &self.inner {
            Some(inner) => descriptor.requires(inner.clone()),
            None => descriptor,
        }
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let Some(inner_id) = &self.inner else {
            // A component that wraps nothing would register itself as the default provider
            // and then have nothing to call. Refused at start-up, where it is one line of
            // wiring to fix, rather than at the first turn.
            return Err(Error::config(
                format!("components.{}", self.id),
                "no model provider to wrap; call `wrapping` with a provider's component id",
            ));
        };
        if inner_id == &self.id {
            return Err(Error::config(
                format!("components.{}", self.id),
                "a resilience component cannot wrap itself",
            ));
        }

        let settings = self.settings(ctx)?;
        let inner = ctx.service_named::<dyn ModelProvider>(inner_id)?;

        let provider: Arc<dyn ModelProvider> = Arc::new(
            ResilientProvider::new(inner, inner_id.clone(), settings, ctx.clock().clone())
                .with_events(ctx.events().clone()),
        );

        ctx.provide_default::<dyn ModelProvider>(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrapped_provider_is_declared_as_a_dependency() {
        let component =
            ResilienceComponent::new(ResilienceSettings::default()).wrapping("model.ollama");
        let descriptor = component.descriptor();
        let declared: Vec<&ComponentId> = descriptor
            .dependencies
            .iter()
            .map(|dependency| &dependency.id)
            .collect();
        assert_eq!(declared, vec![&ComponentId::new("model.ollama")]);
    }

    #[test]
    fn the_default_id_is_not_a_providers_own() {
        let component = ResilienceComponent::from_config();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.settings.is_none());
    }

    #[test]
    fn a_component_with_nothing_to_wrap_declares_no_dependency() {
        let component = ResilienceComponent::from_config();
        assert!(component.descriptor().dependencies.is_empty());
    }
}
