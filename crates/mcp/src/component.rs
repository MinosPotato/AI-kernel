//! Wiring the servers' lifetime into the kernel's.
//!
//! The catalogue itself is not a component. It is built at wiring time and handed to
//! whatever assembles the tool registry, for the same reason the policy engine and the
//! approval sink are: which component happens to initialise first would otherwise silently
//! decide whether a deployment has any MCP tools at all.
//!
//! What *is* a component is the thing that stops the servers. A tool server is a process
//! this kernel started; a kernel that shut down and left it running would be a kernel whose
//! shutdown is not one. So [`McpComponent`] holds the same catalogue and kills every server
//! on `stop`.

use std::sync::Arc;

use aik_core::prelude::*;

use crate::catalog::McpCatalog;

/// The component id used when none is given explicitly.
pub const DEFAULT_COMPONENT_ID: &str = "mcp.servers";

/// Stops every MCP server when the kernel does.
pub struct McpComponent {
    id: ComponentId,
    catalog: Arc<McpCatalog>,
}

impl std::fmt::Debug for McpComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpComponent")
            .field("id", &self.id)
            .field("catalog", &self.catalog)
            .finish()
    }
}

impl McpComponent {
    /// Registers under [`DEFAULT_COMPONENT_ID`], owning the lifetime of `catalog`'s servers.
    pub fn new(catalog: Arc<McpCatalog>) -> Self {
        Self {
            id: ComponentId::new(DEFAULT_COMPONENT_ID),
            catalog,
        }
    }

    /// Registers under a different component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = id.into();
        self
    }
}

#[async_trait]
impl Component for McpComponent {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(self.id.clone())
            .described("external MCP tool servers, started on demand and stopped with the kernel")
    }

    async fn stop(&self, _ctx: &ComponentContext) -> Result<()> {
        // Never an error. A server that already exited, or one whose process could not be
        // reaped, must not stop the rest of the kernel from shutting down cleanly — the
        // failure is logged where it happens.
        self.catalog.shutdown().await;
        Ok(())
    }
}
