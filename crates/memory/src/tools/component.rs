//! Wires the memory tools to the store the kernel published.

use std::sync::Arc;

use aik_api::memory::MemoryStore;
use aik_core::prelude::*;

use super::binding::MemoryToolBinding;
use super::read::{MemoryGetTool, MemoryQueryTool};
use super::write::{MemoryDeleteTool, MemoryPutTool};

/// The component id used when none is given explicitly.
pub const DEFAULT_TOOLS_COMPONENT_ID: &str = "memory.tools";

/// Binds a set of memory tools to a [`MemoryStore`] published by another component.
///
/// # Why a component of its own
///
/// Tools are given to the tool registry's component *before* the kernel is built, and the
/// store is published *during* `init`. Nothing can hold both at once at construction time,
/// so one of three things has to give:
///
/// 1. **Build the store outside the kernel** and hand it to both. It works for
///    [`InMemoryMemoryStore`](crate::InMemoryMemoryStore) and not at all for
///    [`RedbMemoryStore`](crate::RedbMemoryStore), which needs a database another component
///    owns — so the durable backend would need different wiring from the volatile one,
///    which is exactly the asymmetry both stores exist to avoid.
/// 2. **Let a tool resolve the store itself at call time**, holding a kernel context. That
///    hands every tool an open door onto every capability in the registry, for the sake of
///    one it was supposed to be given deliberately.
/// 3. **Bind late, once, from a component that declares the dependency.** What this is.
///
/// The tools this hands out share one binding. It is filled during this component's `init`,
/// which the kernel orders after the memory component's because this one
/// [requires](ComponentDescriptor::requires) it. A tool invoked before that — which means a
/// tool whose component was never added — refuses rather than proceeding without a store.
///
/// ```no_run
/// use aik_core::prelude::*;
/// use aik_memory::{MemoryComponent, MemoryToolsComponent};
/// use aik_tools::ToolsComponent;
///
/// # fn build() -> Result<Kernel> {
/// let memory_tools = MemoryToolsComponent::new();
///
/// Kernel::builder()
///     .component(MemoryComponent::new())
///     .component(
///         ToolsComponent::new()
///             .with_tool(memory_tools.put())
///             .with_tool(memory_tools.query()),
///     )
///     .component(memory_tools)
///     .build()
/// # }
/// ```
///
/// Only the tools actually registered exist, which is the first of the two independent ways
/// to withhold a memory capability; the second is denying its permission in policy.
#[derive(Debug)]
pub struct MemoryToolsComponent {
    id: ComponentId,
    memory: ComponentId,
    binding: Arc<MemoryToolBinding>,
}

impl Default for MemoryToolsComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryToolsComponent {
    /// Creates a component registered under [`DEFAULT_TOOLS_COMPONENT_ID`], binding to the
    /// memory store published under [`crate::DEFAULT_COMPONENT_ID`].
    ///
    /// That is the id both [`MemoryComponent`](crate::MemoryComponent) and
    /// [`RedbMemoryComponent`](crate::RedbMemoryComponent) use by default, so the same
    /// wiring works for either backend.
    pub fn new() -> Self {
        Self {
            id: ComponentId::new(DEFAULT_TOOLS_COMPONENT_ID),
            memory: ComponentId::new(crate::DEFAULT_COMPONENT_ID),
            binding: Arc::new(MemoryToolBinding::new()),
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }

    /// Binds to the memory store published by a differently named component.
    ///
    /// Naming it rather than resolving the registry's default `dyn MemoryStore` is what lets
    /// the dependency be *declared*, so the kernel can order the two components; a default
    /// resolved during `init` would depend on the order they happened to be added in.
    #[must_use]
    pub fn with_memory(mut self, memory: impl Into<ComponentId>) -> Self {
        self.memory = memory.into();
        self
    }

    /// A tool that stores a memory. See [`MemoryPutTool`].
    pub fn put(&self) -> MemoryPutTool {
        MemoryPutTool::new(self.binding.clone())
    }

    /// A tool that fetches one memory by id. See [`MemoryGetTool`].
    pub fn get(&self) -> MemoryGetTool {
        MemoryGetTool::new(self.binding.clone())
    }

    /// A tool that searches memories. See [`MemoryQueryTool`].
    pub fn query(&self) -> MemoryQueryTool {
        MemoryQueryTool::new(self.binding.clone())
    }

    /// A tool that forgets one memory. See [`MemoryDeleteTool`].
    pub fn delete(&self) -> MemoryDeleteTool {
        MemoryDeleteTool::new(self.binding.clone())
    }
}

#[async_trait]
impl Component for MemoryToolsComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("binds the memory tools to a memory store")
            .requires(self.memory.clone())
    }

    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let store = ctx.service_named::<dyn MemoryStore>(&self.memory)?;
        // The kernel clock, not `Timestamp::now`: a memory's `created_at` is kernel-assigned
        // metadata, and a test that controls time has to be able to control this too.
        self.binding.bind(store, ctx.clock().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::tool::Tool;

    use crate::MemoryComponent;

    #[test]
    fn it_depends_on_the_memory_component_it_binds_to() {
        let component = MemoryToolsComponent::new().with_memory("memory.secondary");
        let descriptor = component.descriptor();
        assert_eq!(descriptor.id, ComponentId::new(DEFAULT_TOOLS_COMPONENT_ID));
        assert_eq!(
            descriptor.dependencies[0].id,
            ComponentId::new("memory.secondary")
        );
        assert!(!descriptor.dependencies[0].optional);
    }

    #[test]
    fn the_tools_it_hands_out_have_the_expected_names_and_permissions() {
        let component = MemoryToolsComponent::new();
        let specs = [
            component.put().spec(),
            component.get().spec(),
            component.query().spec(),
            component.delete().spec(),
        ];
        let names: Vec<String> = specs.iter().map(|spec| spec.name.to_string()).collect();
        assert_eq!(
            names,
            vec!["memory.put", "memory.get", "memory.query", "memory.delete"]
        );
        for spec in &specs {
            assert_eq!(
                spec.required_permissions,
                vec![aik_api::permission::ActionId::new(spec.name.as_str())],
                "`{}` should require exactly its own permission",
                spec.name
            );
        }
        assert!(!specs[0].read_only);
        assert!(specs[1].read_only);
        assert!(specs[2].read_only);
        assert!(!specs[3].read_only);
    }

    #[tokio::test]
    async fn init_binds_the_tools_to_the_published_store() {
        let component = MemoryToolsComponent::new();
        let tool = component.put();
        let kernel = Kernel::builder()
            .component(MemoryComponent::new())
            .component(component)
            .build()
            .expect("a valid kernel");
        kernel.start().await.expect("the kernel starts");

        let outcome = crate::tools::testing::invoke(
            &tool,
            serde_json::json!({ "kind": "fact", "content": "bound" }),
            &crate::tools::testing::cx("alice"),
        )
        .await
        .expect("the tool is bound");
        assert!(!outcome.is_error);

        kernel.shutdown().await.expect("the kernel stops");
    }

    #[tokio::test]
    async fn the_declared_dependency_orders_init_whatever_order_components_were_added_in() {
        // The tools component is added *first*, so it only finds a store because it declared
        // the dependency the kernel sorts on. Without that declaration this wiring would
        // depend on the order somebody happened to write the builder in.
        let component = MemoryToolsComponent::new();
        let tool = component.put();
        let kernel = Kernel::builder()
            .component(component)
            .component(MemoryComponent::new())
            .build()
            .expect("a valid kernel");
        kernel.start().await.expect("the kernel starts");

        let outcome = crate::tools::testing::invoke(
            &tool,
            serde_json::json!({ "kind": "fact", "content": "bound" }),
            &crate::tools::testing::cx("alice"),
        )
        .await
        .expect("the tool is bound");
        assert!(!outcome.is_error);

        kernel.shutdown().await.expect("the kernel stops");
    }

    #[tokio::test]
    async fn a_missing_memory_component_fails_the_kernel_rather_than_the_first_call() {
        let error = Kernel::builder()
            .component(MemoryToolsComponent::new())
            .build()
            .expect_err("the declared dependency is not registered");
        assert_eq!(error.kind(), aik_core::ErrorKind::Wiring);
    }

    #[tokio::test]
    async fn a_tool_whose_component_was_never_added_refuses() {
        let component = MemoryToolsComponent::new();
        let tool = component.put();
        drop(component);

        let error = crate::tools::testing::invoke(
            &tool,
            serde_json::json!({ "kind": "fact", "content": "unbound" }),
            &crate::tools::testing::cx("alice"),
        )
        .await
        .expect_err("nothing bound the tool");
        assert_eq!(error.kind(), aik_core::ErrorKind::Lifecycle);
    }
}
