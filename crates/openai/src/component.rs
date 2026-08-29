//! Wires [`OpenAiProvider`] into the kernel as a normal component.

use aik_api::model::{Embedder, ModelProvider};
use aik_core::prelude::*;

use crate::credentials::{ApiKey, resolve};
use crate::provider::OpenAiProvider;
use crate::settings::OpenAiSettings;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "model.openai";

/// Registers an [`OpenAiProvider`] as a kernel component.
///
/// Settings are read from this component's own configuration section — `components.<id>`,
/// see [`ComponentContext::settings`] — and deserialised as [`OpenAiSettings`].
///
/// # Two capabilities, one component
///
/// This registers the provider under both `dyn ModelProvider` and `dyn Embedder`, because
/// one endpoint serves both and a deployment that configured one endpoint should not have to
/// configure it twice. That is what lets semantic memory work on a hosted service: see
/// [`aik_memory`](https://docs.rs/aik-memory) for what an embedder buys, and note that the
/// embedding model is named separately from the chat model — this dialect's `/models`
/// listing carries both kinds and says nothing about which is which.
///
/// # The key
///
/// The configuration says *where* the key is; this component reads it during
/// [`init`](Component::init) and fails to start if it is missing, malformed, or in a file
/// other users can read. Failing at startup is deliberate: a deployment whose credential is
/// wrong should not come up, serve a session and fail on the first turn a person types. A
/// loopback endpoint that has no notion of an account sets
/// [`api_key_required`](OpenAiSettings::api_key_required) to `false` and needs none.
///
/// A caller that already holds a key — a test, or a frontend that obtained one some other
/// way — passes it with [`with_api_key`](OpenAiComponent::with_api_key), and the environment
/// is not consulted at all.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_openai::OpenAiComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(OpenAiComponent::new()).build()
/// # }
/// ```
#[derive(Debug)]
pub struct OpenAiComponent {
    id: ComponentId,
    default: bool,
    key: Option<ApiKey>,
}

impl Default for OpenAiComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default `dyn ModelProvider` and `dyn Embedder`.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            key: None,
        }
    }

    /// Registers under a different component id.
    ///
    /// Use this to run more than one endpoint — a hosted service and a local server,
    /// different keys, different limits — in the same kernel; pair it with
    /// [`OpenAiComponent::as_default`].
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

    /// Supplies the API key directly, instead of resolving it from the environment.
    #[must_use]
    pub fn with_api_key(mut self, key: ApiKey) -> Self {
        self.key = Some(key);
        self
    }
}

#[async_trait]
impl Component for OpenAiComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone()).described("OpenAI-compatible model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let settings = OpenAiSettings::read(&ctx.settings())?;

        // Resolved per start rather than cached on the component, so a key rotated in the
        // environment or on disk is picked up by a restart of the kernel rather than only by
        // a restart of the process.
        let key = match &self.key {
            Some(key) => Some(ApiKey::new(key.expose(), "the supplied API key")?),
            None => resolve(
                settings.api_key_file.as_deref(),
                &settings.api_key_env,
                settings.api_key_required,
                |name| std::env::var(name).ok(),
            )?,
        };

        let provider = OpenAiProvider::new(&settings, key, ctx.clock().clone())?.shared();

        if self.default {
            ctx.provide_default::<dyn ModelProvider>(provider.clone())?;
            ctx.provide_default::<dyn Embedder>(provider)
        } else {
            ctx.provide::<dyn ModelProvider>(provider.clone())?;
            ctx.provide::<dyn Embedder>(provider)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = OpenAiComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert_eq!(
            component.descriptor().id,
            ComponentId::new(DEFAULT_COMPONENT_ID)
        );
    }

    #[test]
    fn id_and_default_flag_are_overridable() {
        let component = OpenAiComponent::new()
            .with_id("model.openai.local")
            .as_default(false);
        assert_eq!(component.id, ComponentId::new("model.openai.local"));
        assert!(!component.default);
    }

    #[test]
    fn a_supplied_key_never_reaches_the_debug_output() {
        let component =
            OpenAiComponent::new().with_api_key(ApiKey::new("sk-secret", "TEST").unwrap());
        assert!(!format!("{component:?}").contains("sk-secret"));
    }
}
