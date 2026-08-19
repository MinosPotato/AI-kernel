//! Contracts for the subsystems built on top of the AI kernel.
//!
//! This crate contains **traits and data types only**. Nothing here is implemented, and
//! `aik-core` does not depend on it: a kernel can be built, started and run with none of
//! these present. They exist so that subsystems can be developed independently against a
//! shared shape, and so that the kernel's registry has meaningful capabilities to resolve.
//!
//! | Module | Contract |
//! |---|---|
//! | [`execution`] | The context an operation runs in: who, what for, until when |
//! | [`model`] | Inference providers and embedders |
//! | [`tool`] | Callable, schema-described capabilities, and the invocation gate |
//! | [`permission`] | Principals, policy, human approval and resource authorization |
//! | [`audit`] | Structured authorization and invocation events |
//! | [`memory`] | Persistent records and retrieval |
//! | [`scheduler`] | Time- and event-triggered jobs |
//! | [`agent`] | Long-running work with streamed progress |
//! | [`platform`] | The single seam to an OS or desktop |
//!
//! # How these are used
//!
//! An implementation is a component that publishes itself into the kernel registry under
//! the capability it implements:
//!
//! ```
//! use std::sync::Arc;
//! use aik_api::model::ModelProvider;
//! use aik_core::prelude::*;
//!
//! # struct MyProvider;
//! # #[async_trait]
//! # impl ModelProvider for MyProvider {
//! #     async fn models(&self) -> Result<Vec<aik_api::model::ModelDescriptor>> { Ok(vec![]) }
//! #     async fn complete(&self, _: aik_api::model::CompletionRequest, _: &aik_api::execution::ExecutionContext)
//! #         -> Result<aik_api::model::CompletionResponse> { Err(Error::Unsupported("demo".into())) }
//! #     async fn stream(&self, _: aik_api::model::CompletionRequest, _: &aik_api::execution::ExecutionContext)
//! #         -> Result<futures_core::stream::BoxStream<'static, Result<aik_api::model::CompletionChunk>>> {
//! #         Err(Error::Unsupported("demo".into()))
//! #     }
//! # }
//! struct MyProviderComponent;
//!
//! #[async_trait]
//! impl Component for MyProviderComponent {
//!     fn descriptor(&self) -> ComponentDescriptor {
//!         ComponentDescriptor::new("model.my-provider")
//!     }
//!
//!     async fn init(&self, ctx: &ComponentContext) -> Result<()> {
//!         ctx.provide::<dyn ModelProvider>(Arc::new(MyProvider))
//!     }
//! }
//! ```
//!
//! Consumers resolve the capability, never the implementation:
//!
//! ```no_run
//! # use aik_api::model::ModelProvider;
//! # use aik_core::KernelContext;
//! # fn demo(ctx: &KernelContext) -> aik_core::Result<()> {
//! let provider = ctx.service::<dyn ModelProvider>()?;
//! # let _ = provider;
//! # Ok(())
//! # }
//! ```
//!
//! # Stability
//!
//! These types are provisional and will evolve as the subsystems are actually built.
//! `aik-core` is the part that is meant to stay still.

pub mod agent;
pub mod audit;
pub mod execution;
pub mod memory;
pub mod model;
pub mod permission;
pub mod platform;
pub mod scheduler;
pub mod tool;

/// The contracts most implementations need.
pub mod prelude {
    pub use crate::audit::{AuthorizationDecided, AuthorizationPhase, ToolInvoked};
    pub use crate::execution::ExecutionContext;
    pub use crate::memory::{MemoryQuery, MemoryRecord, MemoryStore};
    pub use crate::model::{
        CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, Embedder, Message,
        ModelId, ModelProvider, Role,
    };
    pub use crate::permission::{
        Decision, PermissionRequest, PolicyEngine, Principal, ResourceAuthorizer,
    };
    pub use crate::platform::{PlatformCapability, PlatformIntegration};
    pub use crate::scheduler::{JobHandler, JobSpec, Scheduler, Trigger};
    pub use crate::tool::{
        ResourceClaim, Tool, ToolCatalog, ToolName, ToolOutcome, ToolRegistry, ToolSpec,
    };
}
