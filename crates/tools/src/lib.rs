//! An authorization-gated tool registry for the AI kernel.
//!
//! This crate is the reference implementation of `aik_api::tool::ToolRegistry`: the
//! enforcement point described in that trait's documentation, where a tool's declared
//! permissions are always resolved before it runs. It proves the contract can be
//! implemented cleanly on top of the existing kernel — nothing here required a change to
//! `aik-core`, and `aik-api`'s existing `Tool`/`ToolSpec`/permission types needed only one
//! addition (the `ToolRegistry` trait itself).
//!
//! No real tools live here. [`EchoTool`] is the one concrete tool in this crate, and it
//! does nothing but echo its input back — its purpose is to exercise every part of the
//! foundation (registration, discovery, schema, authorization, execution, cancellation,
//! structured errors, audit events) without being able to do anything harmful if that
//! exercise goes wrong.
//!
//! # Authorization in three phases
//!
//! [`InProcessToolRegistry`] resolves, for every invocation:
//!
//! 1. **Capability level** — each of the tool's
//!    [`required_permissions`](aik_api::tool::ToolSpec::required_permissions): *may this
//!    principal use `filesystem.write` at all?*
//! 2. **Resource level** — each [`ResourceClaim`](aik_api::tool::ResourceClaim) the tool
//!    derives from its arguments: *…on `/home/user/project/file.rs`?* Both of these are
//!    settled before the tool runs, so a refusal means nothing executed.
//! 3. **Discovered resources** — asked by the tool itself, through the
//!    [`ResourceAuthorizer`](aik_api::permission::ResourceAuthorizer) it is handed, for
//!    anything it only learns about while running.
//!
//! All three go through the same policy engine and emit the same
//! [audit events](aik_api::audit). See the [`aik_api::tool`] module docs for why the split
//! exists and what it does and does not guarantee.
//!
//! # And a fourth, about the conversation rather than the caller
//!
//! The three phases above all ask what a *principal* may do. A system that reads the outside
//! world has a second question to answer — what the conversation has already been *told* —
//! because a model cannot tell a fetched page's instructions from its operator's. So the
//! registry keeps a [`TrustLedger`](aik_api::provenance::TrustLedger) of what each
//! conversation has read, and a call that would let untrusted content act reaches a human, or
//! nobody, before it acts. See [`aik_api::provenance`] for the shape of that and
//! [`TrustEnforcement`] for the one dial on it.
//!
//! ```
//! use std::sync::Arc;
//! use aik_api::execution::ExecutionContext;
//! use aik_api::permission::{Decision, PermissionRequest, PolicyEngine};
//! use aik_api::tool::ToolRegistry;
//! use aik_core::prelude::*;
//! use aik_tools::{EchoTool, ToolsComponent};
//!
//! # struct AllowEverything;
//! # #[async_trait]
//! # impl PolicyEngine for AllowEverything {
//! #     async fn evaluate(&self, _: &PermissionRequest, _: &ExecutionContext) -> Result<Decision> {
//! #         Ok(Decision::Allow)
//! #     }
//! # }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let kernel = Kernel::builder()
//!     .component(
//!         ToolsComponent::new()
//!             .with_tool(EchoTool::new())
//!             .with_policy(Arc::new(AllowEverything)),
//!     )
//!     .build()?;
//! kernel.start().await?;
//!
//! // An agent would hold exactly this: a `dyn ToolRegistry`, never a `dyn Tool`.
//! let tools = kernel.context().service::<dyn ToolRegistry>()?;
//! let outcome = tools
//!     .invoke(
//!         &"kernel.echo".into(),
//!         serde_json::json!({ "text": "hello" }),
//!         &ExecutionContext::new(),
//!     )
//!     .await?;
//! assert_eq!(outcome.output["text"], serde_json::json!("hello"));
//!
//! kernel.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod component;
mod echo;
mod registry;
mod trust;

pub use component::{DEFAULT_COMPONENT_ID, ToolsComponent};
pub use echo::{DEFAULT_NAME, DEFAULT_PERMISSION, EchoTool};
pub use registry::{InProcessToolRegistry, system_principal_id};
pub use trust::{
    DEFAULT_CAPACITY, InMemoryTrustLedger, TrustEnforcement, UNTRUSTED_CONTENT_ACTION,
};
