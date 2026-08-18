//! An Ollama model provider for the AI kernel.
//!
//! This crate is the first real implementation of an [`aik_api::model::ModelProvider`]. It
//! exists to prove that the kernel architecture can host one cleanly — nothing about
//! Ollama, HTTP, or its JSON wire format leaks outside this crate.
//!
//! ```no_run
//! use std::sync::Arc;
//! use aik_api::execution::ExecutionContext;
//! use aik_api::model::{CompletionRequest, Message, ModelProvider, Role};
//! use aik_core::prelude::*;
//! use aik_ollama::OllamaComponent;
//! use futures::StreamExt;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! let kernel = Kernel::builder().component(OllamaComponent::new()).build()?;
//! kernel.start().await?;
//!
//! // Consumers depend on the capability, never on this crate.
//! let provider = kernel.context().service::<dyn ModelProvider>()?;
//!
//! let request = CompletionRequest::new(
//!     "llama3.2",
//!     vec![Message::text(Role::User, "hi")],
//! );
//! let mut chunks = provider.stream(request, &ExecutionContext::new()).await?;
//! while let Some(chunk) = chunks.next().await {
//!     let _ = chunk?;
//! }
//!
//! kernel.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Structure
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`settings`] | [`OllamaSettings`] — endpoint and timeout, read from kernel configuration |
//! | [`provider`] | [`OllamaProvider`] — the `ModelProvider` implementation |
//! | [`component`] | [`OllamaComponent`] — registers the provider into a kernel |
//! | `protocol` (private) | Ollama's JSON wire format and its translation to `aik_api::model` |
//! | `deadline` (private) | Combines the configured timeout and a request's own deadline |
//! | `stream` (private) | Parses Ollama's newline-delimited JSON stream |
//! | `http` (private) | Maps `reqwest` failures onto [`aik_core::Error`] |
//!
//! Only [`settings`], [`provider`] and [`component`] are public; everything else is an
//! implementation detail that may change without notice.

pub mod component;
mod deadline;
mod http;
mod protocol;
pub mod provider;
pub mod settings;
mod stream;

pub use component::{DEFAULT_COMPONENT_ID, OllamaComponent};
pub use provider::OllamaProvider;
pub use settings::OllamaSettings;
