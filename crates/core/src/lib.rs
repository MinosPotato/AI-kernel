//! The kernel of an AI operating layer.
//!
//! This crate is the permanent foundation the rest of the system is built on. It knows
//! nothing about models, agents, tools, memory, desktops or operating systems — it knows
//! how to *hold* such things: how they are named, configured, wired together, started,
//! stopped, discovered, and how they talk to each other.
//!
//! # The five mechanisms
//!
//! | Mechanism | Type | What it is for |
//! |---|---|---|
//! | Configuration | [`Config`] | Layered, immutable settings, format-agnostic |
//! | Wiring | [`Registry`] | Capability → implementation, resolved at runtime |
//! | Lifecycle | [`Component`], [`Kernel`] | Dependency-ordered startup and shutdown |
//! | Communication | [`EventBus`] | Typed pub/sub, with a JSON firehose for bridges |
//! | Concurrency | [`Tasks`] | Cancellation scopes and tracked background work |
//!
//! Extensibility ties them together: a [`Plugin`] contributes components, a component
//! publishes services into the registry, and everything else finds those services by
//! capability rather than by type.
//!
//! # A whole system in miniature
//!
//! ```
//! use std::sync::Arc;
//! use aik_core::prelude::*;
//! use serde::{Deserialize, Serialize};
//!
//! // A capability. Anything can implement it; nothing depends on the implementation.
//! trait Notifier: Send + Sync {
//!     fn notify(&self, message: &str);
//! }
//!
//! // An event. Serialisable, so bridges can observe it without knowing the type.
//! #[derive(Debug, Clone, Serialize, Deserialize)]
//! struct Notified {
//!     message: String,
//! }
//!
//! impl Event for Notified {
//!     const NAME: &'static str = "demo.notified";
//! }
//!
//! // A component that publishes the capability.
//! struct LogNotifier;
//!
//! impl Notifier for LogNotifier {
//!     fn notify(&self, message: &str) {
//!         println!("{message}");
//!     }
//! }
//!
//! #[async_trait]
//! impl Component for LogNotifier {
//!     fn descriptor(&self) -> ComponentDescriptor {
//!         ComponentDescriptor::new("demo.notifier")
//!     }
//!
//!     async fn init(&self, ctx: &ComponentContext) -> Result<()> {
//!         ctx.provide_default::<dyn Notifier>(Arc::new(LogNotifier))
//!     }
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let kernel = Kernel::builder().component(LogNotifier).build()?;
//! kernel.start().await?;
//!
//! let ctx = kernel.context();
//! ctx.service::<dyn Notifier>()?.notify("hello");
//! ctx.publish(Notified { message: "hello".into() });
//!
//! kernel.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # What is deliberately absent
//!
//! No UI, no compositor, no OS-specific code, no model clients, no storage, no CLI, no
//! signal handling, no logging setup. Those live downstream and reach the kernel through
//! the registry, the event bus and the component lifecycle. See `aik-api` for the contracts
//! they are expected to implement.

pub mod clock;
pub mod component;
pub mod config;
pub mod context;
pub mod error;
pub mod event;
mod graph;
pub mod id;
pub mod kernel;
pub mod plugin;
pub mod registry;
pub mod task;

pub use clock::{Clock, SharedClock, SystemClock, Timestamp};
pub use component::{Component, ComponentDescriptor, ComponentState, Health, HealthStatus};
pub use config::Config;
pub use context::{ComponentContext, KernelContext};
pub use error::{Error, ErrorKind, Result};
pub use event::{Envelope, Event, EventBus, EventMetadata};
pub use id::{ComponentId, CorrelationId, EventId, EventName, PluginId, TaskId};
pub use kernel::{Kernel, KernelBuilder, KernelState};
pub use plugin::{KERNEL_ABI_VERSION, Plugin, PluginMetadata, PluginRegistrar};
pub use registry::Registry;
pub use task::{TaskHandle, Tasks};

/// Everything needed to write a component or a plugin.
///
/// ```
/// use aik_core::prelude::*;
/// ```
pub mod prelude {
    pub use crate::component::{Component, ComponentDescriptor, ComponentState, Health};
    pub use crate::config::Config;
    pub use crate::context::{ComponentContext, KernelContext};
    pub use crate::error::{Error, Result};
    pub use crate::event::{Envelope, Event, EventBus, EventStream};
    pub use crate::id::{ComponentId, CorrelationId, PluginId};
    pub use crate::kernel::{Kernel, KernelBuilder, KernelState};
    pub use crate::plugin::{Plugin, PluginMetadata, PluginRegistrar};
    pub use crate::registry::Registry;
    pub use crate::task::Tasks;
    pub use async_trait::async_trait;
}

/// Re-exports for the identifier macros. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use serde;
    pub use uuid;
}
