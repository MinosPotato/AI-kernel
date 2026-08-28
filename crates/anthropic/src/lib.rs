//! An Anthropic model provider for the AI kernel.
//!
//! The second implementation of [`aik_api::model::ModelProvider`], and the first that talks
//! to a service outside the machine. That difference is the reason this crate exists as much
//! as the models are: a provider reached over the internet needs a credential, and this is
//! where the workspace decides how a credential is held.
//!
//! ```no_run
//! use aik_anthropic::AnthropicComponent;
//! use aik_api::execution::ExecutionContext;
//! use aik_api::model::{CompletionRequest, Message, ModelProvider, Role};
//! use aik_core::prelude::*;
//! use futures::StreamExt;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! // Reads the key from ANTHROPIC_API_KEY, and refuses to start without one.
//! let kernel = Kernel::builder().component(AnthropicComponent::new()).build()?;
//! kernel.start().await?;
//!
//! // Consumers depend on the capability, never on this crate.
//! let provider = kernel.context().service::<dyn ModelProvider>()?;
//!
//! let request = CompletionRequest::new(
//!     "claude-sonnet-4-5",
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
//! | [`settings`] | [`AnthropicSettings`] — endpoint, limits, and where the key lives |
//! | [`credentials`] | [`ApiKey`] — how a secret is held, found and refused |
//! | [`provider`] | [`AnthropicProvider`] — the `ModelProvider` implementation |
//! | [`component`] | [`AnthropicComponent`] — registers the provider into a kernel |
//! | `protocol` (private) | The Messages API's JSON and its translation to `aik_api::model` |
//! | `stream` (private) | Parses the server-sent event stream, reassembling tool calls |
//! | `http` (private) | Headers, error translation, and which failures are worth repeating |
//! | `deadline` (private) | Combines the configured timeout and a request's own deadline |
//!
//! # The credential
//!
//! Three rules, each enforced by code rather than documented as a practice:
//!
//! * **Configuration names where the key is, never what it is.** `api_key_env` or
//!   `api_key_file`; a section carrying `api_key` (or four other spellings) fails at startup
//!   with a message saying why. The kernel's [`Config`](aik_core::Config) is cloned, merged
//!   and `Debug`-printed throughout the process, so anything in it is effectively public to
//!   the process.
//! * **The key cannot be printed.** [`ApiKey`] has no `Display` and no `Serialize`, its
//!   `Debug` is `ApiKey(<redacted>)`, and the only reader is crate-private. Its header is
//!   marked sensitive so the HTTP stack's own logging will not print it either.
//! * **The transport is checked before the key is sent.** A non-`https` endpoint is refused
//!   unless it is loopback, redirects are not followed, and a key file that other users can
//!   read is a startup failure rather than a warning.
//!
//! # Tool calling
//!
//! [`CompletionRequest::tools`](aik_api::model::CompletionRequest::tools) becomes the API's
//! `tools` array, and `tool_use` blocks come back as
//! [`ContentPart::ToolCall`](aik_api::model::ContentPart::ToolCall) parts. As with any
//! provider, a call reported here is a *request*: it still goes through
//! [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke) and is still refused if
//! policy says so. Only the model-facing subset of a tool is sent — a
//! [`ToolDefinition`](aik_api::model::ToolDefinition) has no `required_permissions` — so
//! nothing about what a tool may do can reach the service.
//!
//! Unlike Ollama, this API streams a tool call in pieces: the block opens with an id and a
//! name, its arguments arrive as JSON fragments, and only the concatenation is valid. They
//! are reassembled here, and a
//! [`CompletionChunk::ToolCall`](aik_api::model::CompletionChunk::ToolCall) is emitted only
//! once the block closes and its arguments parse. Fragments that do not parse are an error,
//! never a call with empty arguments.
//!
//! # What this provider does not do
//!
//! * **Extended thinking.** Its blocks would have to be replayed verbatim on the next turn,
//!   and this crate refuses to send back a block it does not model, so a `thinking` parameter
//!   is rejected up front rather than working for exactly one turn.
//! * **Batches, files, or server-side tools.** Nothing in the kernel's contract describes
//!   them yet.
//! * **`max_tokens` by inference.** The API requires the field and the contract has no
//!   equivalent, so it comes from
//!   [`AnthropicSettings::max_output_tokens`](settings::AnthropicSettings::max_output_tokens),
//!   overridable per request through
//!   [`parameters`](aik_api::model::CompletionRequest::parameters).

pub mod component;
pub mod credentials;
mod deadline;
mod http;
mod protocol;
pub mod provider;
pub mod settings;
mod stream;

pub use component::{AnthropicComponent, DEFAULT_COMPONENT_ID};
pub use credentials::ApiKey;
pub use provider::AnthropicProvider;
pub use settings::AnthropicSettings;
