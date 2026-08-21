//! Wires [`Db`] into the kernel as a normal component.

use std::sync::Arc;

use aik_core::prelude::*;

use crate::db::Db;
use crate::settings::StoreSettings;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "store.db";

/// Opens the shared database and registers it as a kernel service.
///
/// Settings are read from this component's own configuration section — `components.<id>`,
/// see [`ComponentContext::settings`](aik_core::ComponentContext::settings) — and
/// deserialised as [`StoreSettings`]. With no configuration at all, the database lands in
/// the XDG data directory.
///
/// The database is opened during
/// [`init`](aik_core::Component::init), not `start`, because the whole point of `init` is
/// that every service exists before anything runs: a durable subsystem resolves `Arc<Db>`
/// in its own `init` and must find it there. It also means a database this build cannot
/// safely open fails the kernel during startup — where the failure is attributed to this
/// component and rolls everything back — rather than mid-conversation.
///
/// ```
/// use aik_core::prelude::*;
/// use aik_store::StoreComponent;
///
/// # fn build() -> Result<Kernel> {
/// Kernel::builder().component(StoreComponent::new()).build()
/// # }
/// ```
///
/// A subsystem that needs the database declares a dependency on it, so the kernel orders
/// the two and refuses to start if the store is absent:
///
/// ```
/// use aik_core::prelude::*;
/// use aik_store::DEFAULT_COMPONENT_ID;
///
/// # struct MemoryComponent;
/// # impl MemoryComponent {
/// fn descriptor(&self) -> ComponentDescriptor {
///     ComponentDescriptor::new("memory.store").requires(DEFAULT_COMPONENT_ID)
/// }
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct StoreComponent {
    id: ComponentId,
    default: bool,
}

impl Default for StoreComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl StoreComponent {
    /// Creates a component registered under [`DEFAULT_COMPONENT_ID`], as the registry's
    /// default [`Db`].
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            default: true,
        }
    }

    /// Registers under a different component id.
    ///
    /// Use this to open a second database — a separate file, with its own schema version —
    /// in the same kernel; pair it with [`StoreComponent::as_default`]. Pointing two
    /// components at the *same* file does not work and is not meant to: redb holds an
    /// exclusive lock, so the second open fails with a conflict.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Controls whether this database becomes the registry's default [`Db`].
    ///
    /// Set to `false` when registering more than one; resolve this one by name instead, via
    /// [`KernelContext::service_named`](aik_core::KernelContext::service_named).
    #[must_use]
    pub fn as_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }
}

#[async_trait]
impl Component for StoreComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("the embedded database shared by the kernel's durable subsystems")
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let settings: StoreSettings = ctx.settings().get_or_default("")?;
        let path = settings.resolve_path()?;
        let db = Arc::new(Db::open(path)?);

        if self.default {
            ctx.provide_default::<Db>(db)
        } else {
            ctx.provide::<Db>(db)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let component = StoreComponent::new();
        assert_eq!(component.id, ComponentId::new(DEFAULT_COMPONENT_ID));
        assert!(component.default);
        assert_eq!(
            component.descriptor().id,
            ComponentId::new(DEFAULT_COMPONENT_ID)
        );
    }

    #[test]
    fn id_and_default_flag_are_overridable() {
        let component = StoreComponent::new()
            .with_id("store.db.secondary")
            .as_default(false);
        assert_eq!(component.id, ComponentId::new("store.db.secondary"));
        assert!(!component.default);
    }

    #[test]
    fn the_component_declares_no_dependencies_of_its_own() {
        // The store is the bottom of the durable stack: subsystems depend on it, never the
        // reverse. A dependency here would mean something has to run before the database
        // can be opened, which would also mean it could not use the database itself.
        assert!(StoreComponent::new().descriptor().dependencies.is_empty());
    }
}
