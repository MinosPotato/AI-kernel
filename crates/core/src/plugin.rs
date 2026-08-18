//! Extensibility.
//!
//! A plugin is a unit of *registration*: given a [`PluginRegistrar`], it contributes
//! components to the kernel being built. That is all the kernel needs to know. Whether the
//! plugin was compiled in, loaded from a shared object, or synthesised from a
//! configuration file is the loader's problem, not the kernel's.
//!
//! Dynamic loading is deliberately absent — it needs platform-specific code, which does
//! not belong here. What is present is the piece that is expensive to retrofit:
//! [`PluginMetadata::abi_version`], checked against [`KERNEL_ABI_VERSION`] at registration
//! time, so a future dynamic loader has a compatibility gate from day one.
//!
//! ```
//! use aik_core::prelude::*;
//!
//! struct DemoPlugin;
//!
//! # struct Widget;
//! # #[async_trait]
//! # impl Component for Widget {
//! #     fn descriptor(&self) -> ComponentDescriptor { ComponentDescriptor::new("demo.widget") }
//! # }
//! impl Plugin for DemoPlugin {
//!     fn metadata(&self) -> PluginMetadata {
//!         PluginMetadata::new("demo", env!("CARGO_PKG_VERSION"))
//!     }
//!
//!     fn register(&self, registrar: &mut PluginRegistrar<'_>) -> Result<()> {
//!         registrar.component(Widget);
//!         Ok(())
//!     }
//! }
//! ```

use std::sync::Arc;

use crate::component::Component;
use crate::error::Result;
use crate::id::PluginId;

/// The version of the contract between the kernel and plugins.
///
/// Bumped whenever [`Plugin`], [`Component`] or the context types change in a way that
/// would break a separately compiled plugin.
pub const KERNEL_ABI_VERSION: u32 = 1;

/// What a plugin tells the kernel about itself.
#[derive(Debug, Clone)]
pub struct PluginMetadata {
    /// Unique plugin name.
    pub id: PluginId,
    /// The plugin's own version.
    pub version: String,
    /// The kernel ABI the plugin was built against.
    pub abi_version: u32,
    /// A human-readable summary.
    pub description: Option<String>,
}

impl PluginMetadata {
    /// Describes a plugin built against the current ABI.
    pub fn new(id: impl Into<PluginId>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            abi_version: KERNEL_ABI_VERSION,
            description: None,
        }
    }

    /// Adds a human-readable summary.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A bundle of components contributed to a kernel.
pub trait Plugin: Send + Sync + 'static {
    /// Describes the plugin.
    fn metadata(&self) -> PluginMetadata;

    /// Contributes components to the kernel under construction.
    ///
    /// This runs at build time, before any component is initialised, and must not block.
    /// Anything that needs to happen at runtime belongs in a component's lifecycle.
    fn register(&self, registrar: &mut PluginRegistrar<'_>) -> Result<()>;
}

/// The handle a plugin uses to contribute to the kernel being built.
#[derive(Debug)]
pub struct PluginRegistrar<'a> {
    plugin: PluginId,
    components: &'a mut Vec<Arc<dyn Component>>,
}

impl<'a> PluginRegistrar<'a> {
    pub(crate) fn new(plugin: PluginId, components: &'a mut Vec<Arc<dyn Component>>) -> Self {
        Self {
            plugin,
            components,
        }
    }

    /// The id of the plugin doing the registering.
    ///
    /// Useful for namespacing the component ids a plugin creates.
    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin
    }

    /// Contributes a component.
    pub fn component(&mut self, component: impl Component) -> &mut Self {
        self.components.push(Arc::new(component));
        self
    }

    /// Contributes an already-shared component.
    ///
    /// Use this when the plugin needs to keep a handle to the component itself.
    pub fn shared_component(&mut self, component: Arc<dyn Component>) -> &mut Self {
        self.components.push(component);
        self
    }
}

impl std::fmt::Debug for dyn Component {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Component")
            .field("id", &self.descriptor().id)
            .finish()
    }
}
