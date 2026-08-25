//! Wires [`AnthropicProvider`] into the kernel as a normal component.

use aik_api::model::ModelProvider;
use aik_core::prelude::*;

use crate::credentials::{ApiKey, resolve};
use crate::provider::AnthropicProvider;
use crate::settings::AnthropicSettings;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "model.anthropic";

/// Registers an [`AnthropicProvider`] as a kernel component.
///
/// Settings are read from this component's own configuration section — `components.<id>`,
/// see [`ComponentContext::settings`] — and deserialised as [`AnthropicSettings`].
///
/// # The key
///
/// The configuration says *where* the key is; this component reads it during
/// [`init`](Component::init) and fails to start if it is missing, malformed, or in a file
/// other users can read. Failing at startup is deliberate: a deployment whose credential is
/// wrong should not come up, serve a session and fail on the first turn a person types.
///
/// A caller that already holds a key — a test, or a frontend that obtained one some other way
/// — passes it with [`with_api_key`](AnthropicComponent::with_api_key), and the environment
/// is not consulted at all.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_anthropic::AnthropicComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(AnthropicComponent::new()).build()
/// # }
/// ```
#[derive(Debug)]
pub struct AnthropicComponent {
    id: ComponentId,
    default: bool,
    key: Option<ApiKey>,
}

impl Default for AnthropicComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default `dyn ModelProvider`.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
            key: None,
        }
    }

    /// Registers under a different component id.
    ///
    /// Use this to run more than one Anthropic provider — different keys, different limits —
    /// in the same kernel; pair it with [`AnthropicComponent::as_default`].
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
impl Component for AnthropicComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone()).described("Anthropic model provider")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let settings = AnthropicSettings::read(&ctx.settings())?;

        // Resolved per start rather than cached on the component, so a key rotated in the
        // environment or on disk is picked up by a restart of the kernel rather than only by
        // a restart of the process.
        let key = match &self.key {
            Some(key) => ApiKey::new(key.expose(), "the supplied API key")?,
            None => resolve(
                settings.api_key_file.as_deref(),
                &settings.api_key_env,
                |name| std::env::var(name).ok(),
            )?,
        };

        let provider = AnthropicProvider::new(&settings, key, ctx.clock().clone())?.shared();

        if self.default {
            ctx.provide_default::<dyn ModelProvider>(provider)
        } else {
            ctx.provide::<dyn ModelProvider>(provider)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = AnthropicComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert_eq!(
            component.descriptor().id,
            ComponentId::new(DEFAULT_COMPONENT_ID)
        );
    }

    #[test]
    fn id_and_default_flag_are_overridable() {
        let component = AnthropicComponent::new()
            .with_id("model.anthropic.secondary")
            .as_default(false);
        assert_eq!(component.id, ComponentId::new("model.anthropic.secondary"));
        assert!(!component.default);
    }

    #[test]
    fn a_supplied_key_never_reaches_the_debug_output() {
        let component =
            AnthropicComponent::new().with_api_key(ApiKey::new("sk-ant-secret", "TEST").unwrap());
        assert!(!format!("{component:?}").contains("sk-ant-secret"));
    }
}
