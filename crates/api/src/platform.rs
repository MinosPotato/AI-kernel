//! Platform integration contracts.
//!
//! This is the single seam between the kernel and an operating system or desktop. The
//! target is Arch Linux with Hyprland, but nothing here says so: a platform integration is
//! described by the [capabilities](PlatformCapability) it reports, not by the platform it
//! runs on.
//!
//! Capabilities are strings rather than an enum on purpose. A Hyprland backend will offer
//! things a generic Wayland backend cannot, and the kernel must not need a new release to
//! accommodate them.
//!
//! Everything here is a *contract*. There is no OS-specific code in this workspace, and
//! there should never be: backends live in their own crates and are registered as
//! components.

use aik_core::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;

aik_core::string_id! {
    /// Names a platform feature, e.g. `windows`, `workspaces`, `notifications`, `clipboard`.
    pub PlatformCapability
}

/// What the system is running on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInfo {
    /// The operating system, e.g. `linux`.
    pub os: String,
    /// The distribution or variant, if meaningful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<String>,
    /// The desktop environment or compositor, e.g. `hyprland`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
    /// The backend's own version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A request to a platform backend.
///
/// Commands are named and JSON-shaped rather than typed methods, because the set of things
/// a desktop can do is open-ended and backend-specific. A typed façade over the commands a
/// particular backend supports belongs in that backend's crate, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformCommand {
    /// What to do, e.g. `window.focus`.
    pub name: String,
    /// Command-specific arguments.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub arguments: Value,
}

/// A connection to an operating system or desktop.
#[async_trait]
pub trait PlatformIntegration: Send + Sync + 'static {
    /// Describes the platform.
    fn info(&self) -> PlatformInfo;

    /// Lists what this backend can do.
    ///
    /// Callers should check before acting, so that a missing capability is a graceful
    /// degradation rather than a runtime failure.
    fn capabilities(&self) -> Vec<PlatformCapability>;

    /// Returns whether a capability is available.
    fn supports(&self, capability: &PlatformCapability) -> bool {
        self.capabilities().contains(capability)
    }

    /// Issues a command.
    ///
    /// Backends should return [`Error::Unsupported`](aik_core::Error::Unsupported) for
    /// commands they do not implement.
    async fn execute(&self, command: PlatformCommand, cx: &ExecutionContext) -> Result<Value>;
}
