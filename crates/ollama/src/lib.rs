//! An Ollama model provider for the AI kernel.
//!
//! This crate is the first real implementation of an [`aik_api::model::ModelProvider`], and
//! the only implementation of [`aik_api::model::Embedder`]. It exists to prove that the
//! kernel architecture can host one cleanly — nothing about Ollama, HTTP, or its JSON wire
//! format leaks outside this crate.
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
//! | [`provider`] | [`OllamaProvider`] — the `ModelProvider` and `Embedder` implementation |
//! | [`component`] | [`OllamaComponent`] — registers the provider into a kernel, under both |
//! | `protocol` (private) | Ollama's JSON wire format and its translation to `aik_api::model` |
//! | `deadline` (private) | Combines the configured timeout and a request's own deadline |
//! | `stream` (private) | Parses Ollama's newline-delimited JSON stream |
//! | `http` (private) | Maps `reqwest` failures onto [`aik_core::Error`] |
//!
//! Only [`settings`], [`provider`] and [`component`] are public; everything else is an
//! implementation detail that may change without notice.
//!
//! # Tool calling
//!
//! [`CompletionRequest::tools`](aik_api::model::CompletionRequest::tools) is translated into
//! Ollama's `tools` array, and `tool_calls` in a response come back as
//! [`ContentPart::ToolCall`](aik_api::model::ContentPart::ToolCall) parts. An assistant turn
//! carrying calls and the [`Role::Tool`](aik_api::model::Role::Tool) messages answering them
//! both replay onto the wire, which is what lets a conversation continue past a tool.
//! `cargo run -p aik-ollama --example tools` is the whole exchange against a real server.
//!
//! Three things are worth knowing:
//!
//! * **Only the model-facing subset of a tool is sent.** A
//!   [`ToolDefinition`](aik_api::model::ToolDefinition) is a name, a description and an input
//!   schema. A tool's `required_permissions` are not part of that type, so nothing about what
//!   a tool is allowed to do can reach a model or the server hosting it — and a call coming
//!   back is a *request*, not a decision: it still goes through
//!   [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke) like any other.
//! * **Content this provider cannot represent is rejected, never dropped.** A tool call
//!   attributed to anyone but the assistant, a tool result on anything but a `tool` message,
//!   or a blob is an [`Error::Unsupported`](aik_core::Error::Unsupported). Silently omitting
//!   any of them would leave a well-formed conversation that never happened, and the model
//!   would answer from it.
//! * **Not every model can call tools.** Ollama reports a `tools` capability per model, and
//!   one without it answers in prose instead. That is a model's choice rather than a provider
//!   failure, so nothing here treats it as an error.
//!
//! # Embeddings
//!
//! [`OllamaProvider`] also implements [`Embedder`](aik_api::model::Embedder), over
//! `/api/embed`, which is what makes semantic memory possible in this workspace:
//! [`aik_memory`](https://docs.rs/aik-memory) embeds a record when it stores it and a search
//! when it runs one, through whichever `dyn Embedder` the kernel published.
//! [`OllamaComponent`] publishes one instance under both capabilities, since one server
//! answers both.
//!
//! An embedding model is a *different model* from a chat model — usually a much smaller one,
//! `nomic-embed-text` or similar — and is named per call, so one provider serves both without
//! any of that being configuration. Two properties are enforced here rather than assumed of
//! the server: a batch comes back with one vector per input, in input order, and every vector
//! in it is the same width. A response that breaks either is an error, because a misaligned
//! batch would put one memory's vector on another memory's record and stay wrong for as long
//! as the record lives.
//!
//! Ollama assembles tool calls server-side and emits them complete, so
//! [`ModelProvider::stream`](aik_api::model::ModelProvider::stream) yields whole
//! [`CompletionChunk::ToolCall`](aik_api::model::CompletionChunk::ToolCall)s with no partial
//! arguments to reassemble. Call ids come from the server where it supplies them and are
//! synthesised per message where it does not, which older versions and some models do not.

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
