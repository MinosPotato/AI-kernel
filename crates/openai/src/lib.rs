//! An OpenAI-compatible model provider for the AI kernel.
//!
//! The third implementation of [`aik_api::model::ModelProvider`], and the first that is not
//! a description of one service. The chat-completions dialect is spoken by OpenAI, and also
//! by OpenRouter, Groq, Together, vLLM, llama.cpp, LM Studio and Ollama's own compatibility
//! endpoint — so this one crate is how a deployment reaches any of them, by naming a
//! different [`endpoint`](OpenAiSettings::endpoint) and nothing else.
//!
//! That breadth is the reason it is careful about what it claims. A server speaking this
//! dialect is *not* OpenAI: it may omit the `[DONE]` sentinel, answer a 502 with an HTML
//! page, reject the multi-part content form, or have no notion of an account at all. Every
//! one of those is handled here rather than assumed away.
//!
//! ```no_run
//! use aik_api::execution::ExecutionContext;
//! use aik_api::model::{CompletionRequest, Message, ModelProvider, Role};
//! use aik_core::prelude::*;
//! use aik_openai::OpenAiComponent;
//! use futures::StreamExt;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<()> {
//! // Reads the key from OPENAI_API_KEY, and refuses to start without one.
//! let kernel = Kernel::builder().component(OpenAiComponent::new()).build()?;
//! kernel.start().await?;
//!
//! // Consumers depend on the capability, never on this crate.
//! let provider = kernel.context().service::<dyn ModelProvider>()?;
//!
//! let request = CompletionRequest::new("gpt-4.1-mini", vec![Message::text(Role::User, "hi")]);
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
//! | [`settings`] | [`OpenAiSettings`] — endpoint, limits, and where the key lives |
//! | [`credentials`] | [`ApiKey`] — how a secret is held, found and refused |
//! | [`provider`] | [`OpenAiProvider`] — the `ModelProvider` and `Embedder` implementation |
//! | [`component`] | [`OpenAiComponent`] — registers the provider into a kernel |
//! | `protocol` (private) | The chat-completions JSON and its translation to `aik_api::model` |
//! | `stream` (private) | Parses the server-sent event stream, reassembling tool calls |
//! | `http` (private) | Headers, error translation, and which failures are worth repeating |
//! | `deadline` (private) | Combines the configured timeout and a request's own deadline |
//!
//! # The credential
//!
//! The same three rules [`aik_anthropic`](https://docs.rs/aik-anthropic) established, each
//! enforced by code rather than documented as a practice:
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
//! There is one relaxation the Messages API never needed. A local inference server has no
//! notion of an account, so [`api_key_required`](OpenAiSettings::api_key_required) can turn
//! the requirement off — and only for a loopback endpoint, because an unauthenticated
//! request carrying a whole conversation off this machine is a configuration mistake far
//! more often than it is a private gateway.
//!
//! # Tool calling
//!
//! [`CompletionRequest::tools`](aik_api::model::CompletionRequest::tools) becomes the API's
//! `tools` array, and reported calls come back as
//! [`ContentPart::ToolCall`](aik_api::model::ContentPart::ToolCall) parts. As with any
//! provider, a call reported here is a *request*: it still goes through
//! [`ToolRegistry::invoke`](aik_api::tool::ToolRegistry::invoke) and is still refused if
//! policy says so. Only the model-facing subset of a tool is sent — a
//! [`ToolDefinition`](aik_api::model::ToolDefinition) has no `required_permissions` — so
//! nothing about what a tool may do can reach the service.
//!
//! Arguments travel as a JSON *string* in this dialect, in both directions, and a streamed
//! call arrives as fragments of one that are only valid concatenated. They are reassembled
//! per index and emitted once the choice reports it has finished; a fragment sequence that
//! does not parse, or one that parses to something other than an object, is an error rather
//! than a call with empty arguments.
//!
//! # Embeddings
//!
//! Unlike [`aik_anthropic`](https://docs.rs/aik-anthropic), this provider implements
//! [`Embedder`](aik_api::model::Embedder) as well, over the same endpoint and the same
//! credential — so a deployment on a hosted service can rank memory by meaning rather than
//! only by recency. The batch is checked rather than trusted: vectors are placed by the
//! index each entry carries, and a response with a missing index, a duplicate one, a
//! different length or ragged widths is an error rather than a guess about which input each
//! vector belongs to.
//!
//! # What this provider does not do
//!
//! * **Reasoning content.** Servers in this family that produce it — DeepSeek's
//!   `reasoning_content`, and the several spellings that followed — document that it must
//!   *not* be sent back on the next turn. Modelling it would therefore build a transcript
//!   this provider would have to refuse to replay, breaking every conversation at its second
//!   turn. It is display-only text with nowhere in the contract to live, so it is not
//!   surfaced.
//! * **More than one choice.** `n` is refused rather than silently discarded: a
//!   [`CompletionResponse`](aik_api::model::CompletionResponse) holds one message, so the
//!   others would be billed for and never read.
//! * **Documents.** This dialect's file part is keyed by a filename, and
//!   [`ContentPart::Blob`](aik_api::model::ContentPart::Blob) has no such field. Images are
//!   sent inline as `data:` URLs; anything else is refused rather than given an invented
//!   name.
//! * **The responses API.** A different endpoint with a different shape, and nothing in the
//!   kernel's contract needs it.
//! * **`max_tokens` by inference.** This API does not require the field, so none is sent
//!   unless a request sets one through
//!   [`parameters`](aik_api::model::CompletionRequest::parameters) — where the spelling the
//!   target server wants (`max_tokens` or `max_completion_tokens`) is the caller's to
//!   choose, because it differs between servers and between models on the same server.

pub mod component;
pub mod credentials;
mod deadline;
mod http;
mod protocol;
pub mod provider;
pub mod settings;
mod stream;

pub use component::{DEFAULT_COMPONENT_ID, OpenAiComponent};
pub use credentials::ApiKey;
pub use provider::OpenAiProvider;
pub use settings::OpenAiSettings;
