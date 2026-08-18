//! Components: the unit of lifecycle in the kernel.
//!
//! A component is anything the kernel starts and stops: a model provider pool, a memory
//! backend, a Hyprland connection, a Telegram bridge, a scheduler. It declares an id and
//! its dependencies, and the kernel wires it into the right position in the startup order.
//!
//! Components take `&self`, never `&mut self`. They own whatever interior mutability they
//! need, which keeps them shareable across tasks and avoids a kernel-wide lock on
//! everything.
//!
//! ```
//! use aik_core::prelude::*;
//!
//! struct Heartbeat;
//!
//! #[async_trait]
//! impl Component for Heartbeat {
//!     fn descriptor(&self) -> ComponentDescriptor {
//!         ComponentDescriptor::new("demo.heartbeat")
//!             .described("emits a tick on a timer")
//!             .requires("demo.clock-source")
//!     }
//!
//!     async fn start(&self, ctx: &ComponentContext) -> Result<()> {
//!         ctx.tasks().spawn_cancellable("tick", |token| async move {
//!             token.cancelled().await;
//!         });
//!         Ok(())
//!     }
//! }
//! ```

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::context::ComponentContext;
use crate::error::Result;
use crate::event::Event;
use crate::id::ComponentId;

/// A declared dependency on another component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The component depended upon.
    pub id: ComponentId,
    /// If true, the kernel starts anyway when the dependency is absent.
    ///
    /// Optional dependencies are how a component adapts to what is actually installed —
    /// a UI bridge that enriches its output when a platform integration happens to be
    /// present, for instance.
    pub optional: bool,
}

/// What a component tells the kernel about itself.
#[derive(Debug, Clone)]
pub struct ComponentDescriptor {
    /// The component's unique name.
    pub id: ComponentId,
    /// A human-readable summary, for introspection and diagnostics.
    pub description: Option<String>,
    /// Components that must be initialised and started before this one.
    pub dependencies: Vec<Dependency>,
    /// Free-form annotations, e.g. a version or the plugin that supplied the component.
    pub metadata: BTreeMap<String, String>,
}

impl ComponentDescriptor {
    /// Describes a component with the given id and no dependencies.
    pub fn new(id: impl Into<ComponentId>) -> Self {
        Self {
            id: id.into(),
            description: None,
            dependencies: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    /// Adds a human-readable summary.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declares a required dependency.
    #[must_use]
    pub fn requires(mut self, id: impl Into<ComponentId>) -> Self {
        self.dependencies.push(Dependency {
            id: id.into(),
            optional: false,
        });
        self
    }

    /// Declares an optional dependency: ordering is respected if present, absence is fine.
    #[must_use]
    pub fn optionally_requires(mut self, id: impl Into<ComponentId>) -> Self {
        self.dependencies.push(Dependency {
            id: id.into(),
            optional: true,
        });
        self
    }

    /// Adds a metadata annotation.
    #[must_use]
    pub fn annotated(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// A lifecycle-managed unit of the system.
///
/// The three phases are separate on purpose:
///
/// * [`init`](Component::init) wires things up — publish services into the registry,
///   subscribe to events, read configuration. All components are initialised before any
///   are started, so a component may rely on its dependencies' services existing without
///   relying on them being active yet.
/// * [`start`](Component::start) begins activity — spawn background tasks, open
///   connections. Dependencies are already running.
/// * [`stop`](Component::stop) releases resources. Called in reverse dependency order.
///
/// All three default to doing nothing, so a component only implements the phases it needs.
#[async_trait]
pub trait Component: Send + Sync + 'static {
    /// Describes this component's identity and dependencies.
    ///
    /// Called by the kernel during wiring; it must be cheap and must return the same
    /// descriptor every time.
    fn descriptor(&self) -> ComponentDescriptor;

    /// Wires the component up without starting activity.
    async fn init(&self, ctx: &ComponentContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Starts the component's activity.
    async fn start(&self, ctx: &ComponentContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Stops the component and releases its resources.
    ///
    /// The component's task scope is cancelled by the kernel after this returns, so
    /// cooperative tasks do not need to be stopped here individually.
    async fn stop(&self, ctx: &ComponentContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Reports whether the component is working.
    ///
    /// The kernel never acts on this; it exists so that supervisors, UIs and diagnostics
    /// have one place to ask. Long-lived systems need this and retrofitting it later would
    /// mean touching every component.
    async fn health(&self) -> Health {
        Health::up()
    }
}

/// Whether a component is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Fully operational.
    Up,
    /// Working, but not as intended — degraded backend, retrying, partial functionality.
    Degraded,
    /// Not working.
    Down,
}

/// A health report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// The status.
    pub status: HealthStatus,
    /// An optional explanation, mainly useful when not [`HealthStatus::Up`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Health {
    /// A healthy report.
    pub fn up() -> Self {
        Self {
            status: HealthStatus::Up,
            detail: None,
        }
    }

    /// A degraded report with an explanation.
    pub fn degraded(detail: impl Into<String>) -> Self {
        Self {
            status: HealthStatus::Degraded,
            detail: Some(detail.into()),
        }
    }

    /// An unhealthy report with an explanation.
    pub fn down(detail: impl Into<String>) -> Self {
        Self {
            status: HealthStatus::Down,
            detail: Some(detail.into()),
        }
    }
}

/// Where a component is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    /// Known to the kernel, not yet touched.
    Registered,
    /// `init` succeeded.
    Initialized,
    /// `start` succeeded; the component is active.
    Running,
    /// `stop` completed.
    Stopped,
    /// A lifecycle phase returned an error.
    Failed,
}

/// Published whenever a component changes state.
///
/// Subscribing to this is how a UI shows what the system is doing while it boots, and how
/// a supervisor notices that something failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentStateChanged {
    /// The component that changed.
    pub component: ComponentId,
    /// Its new state.
    pub state: ComponentState,
}

impl Event for ComponentStateChanged {
    const NAME: &'static str = "kernel.component_state_changed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_built_declaratively() {
        let descriptor = ComponentDescriptor::new("demo.a")
            .described("a demo")
            .requires("demo.b")
            .optionally_requires("demo.c")
            .annotated("version", "1");

        assert_eq!(descriptor.id, ComponentId::new("demo.a"));
        assert_eq!(descriptor.description.as_deref(), Some("a demo"));
        assert_eq!(descriptor.dependencies.len(), 2);
        assert!(!descriptor.dependencies[0].optional);
        assert!(descriptor.dependencies[1].optional);
        assert_eq!(descriptor.metadata["version"], "1");
    }

    #[test]
    fn health_serialises_compactly() {
        let json = serde_json::to_value(Health::up()).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "up" }));

        let json = serde_json::to_value(Health::degraded("retrying")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "status": "degraded", "detail": "retrying" })
        );
    }
}
