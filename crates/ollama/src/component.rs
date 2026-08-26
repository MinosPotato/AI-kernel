//! Wires [`OllamaProvider`] into the kernel as a normal component.

use std::sync::Arc;

use aik_api::model::{Embedder, ModelProvider};
use aik_core::prelude::*;

use crate::provider::OllamaProvider;
use crate::settings::OllamaSettings;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "model.ollama";

/// Registers an [`OllamaProvider`] as a kernel component, under both `dyn ModelProvider`
/// and `dyn Embedder`.
///
/// Settings are read from this component's own configuration section — `components.<id>`,
/// see [`ComponentContext::settings`] — and deserialised as [`OllamaSettings`]. With no
/// configuration at all, it targets a local Ollama install on the default port.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_ollama::OllamaComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(OllamaComponent::new()).build()
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct OllamaComponent {
    id: ComponentId,
    default: bool,
}

impl Default for OllamaComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default `dyn ModelProvider`.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
        }
    }

    /// Registers under a different component id.
    ///
    /// Use this to run more than one Ollama provider — pointed at different servers — in
    /// the same kernel; pair it with [`OllamaComponent::as_default`].
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this provider becomes the registry's default `dyn ModelProvider`.
    ///
    /// Set to `false` when registering more than one provider; resolve this one by name
    /// instead, via [`KernelContext::service_named`](aik_core::KernelContext::service_named).
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }
}

#[async_trait]
impl Component for OllamaComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone()).described("Ollama model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let settings: OllamaSettings = ctx.settings().get_or_default("")?;
        let provider = Arc::new(OllamaProvider::new(settings, ctx.clock().clone()));

        // The same server answers both, so one instance is published under both
        // capabilities rather than two instances holding two connection pools onto it.
        // Whoever wants embeddings resolves `dyn Embedder`; nothing forces a consumer of
        // one capability to know about the other.
        let embedder: Arc<dyn Embedder> = provider.clone();
        if self.default {
            ctx.provide_default::<dyn Embedder>(embedder)?;
            ctx.provide_default::<dyn ModelProvider>(provider)
        } else {
            ctx.provide::<dyn Embedder>(embedder)?;
            ctx.provide::<dyn ModelProvider>(provider)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = OllamaComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert_eq!(
            component.descriptor().id,
            ComponentId::new(DEFAULT_COMPONENT_ID)
        );
    }

    #[test]
    fn id_and_default_flag_are_overridable() {
        let component = OllamaComponent::new()
            .with_id("model.ollama.secondary")
            .as_default(false);
        assert_eq!(component.id, ComponentId::new("model.ollama.secondary"));
        assert!(!component.default);
    }
}
