//! Model provider contracts.
//!
//! A [`ModelProvider`] is a source of inference. The message and content types are
//! deliberately the small intersection that every provider supports — roles, ordered
//! content parts, tool calls — with everything provider-specific pushed into
//! [`CompletionRequest::parameters`] as opaque JSON. That way adding a provider never
//! requires changing these types, and adding a field here never breaks a provider.
//!
//! Embeddings are a separate trait: plenty of providers do one and not the other, and a
//! single trait would force stubs.

use aik_core::Result;
use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::ExecutionContext;
use crate::tool::{ToolCall, ToolName};

aik_core::string_id! {
    /// Names a model, as the provider knows it.
    pub ModelId
}

/// Who a message is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Instructions that frame the conversation.
    System,
    /// Input from the user or a calling system.
    User,
    /// Output from the model.
    Assistant,
    /// The result of a tool the model asked for.
    Tool,
}

/// One piece of a message.
///
/// Messages are a sequence of parts rather than a string, because a single turn can mix
/// text, images and tool interactions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// Binary data with a MIME type: an image, audio, a document.
    Blob {
        /// The MIME type, e.g. `image/png`.
        mime_type: String,
        /// Base64-encoded bytes, so the part stays serialisable across processes.
        data: String,
    },
    /// The model asking for a tool to be run.
    ToolCall(ToolCall),
    /// The outcome of a tool call being returned to the model.
    ToolResult {
        /// Which call this answers.
        call_id: String,
        /// What the tool produced.
        content: Value,
        /// Whether the tool failed.
        #[serde(default)]
        is_error: bool,
    },
    /// Anything a provider supports that this enum does not model.
    Other(Value),
}

impl ContentPart {
    /// Creates a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// One turn in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who it is from.
    pub role: Role,
    /// What it contains.
    pub content: Vec<ContentPart>,
    /// An optional speaker name, for multi-party conversations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    /// Creates a single-part text message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::text(text)],
            name: None,
        }
    }
}

/// What a model can do.
///
/// A set of names rather than a struct of booleans: providers keep inventing capabilities,
/// and a router should be able to ask about one that did not exist when this was written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelCapabilities(pub Vec<String>);

impl ModelCapabilities {
    /// Streaming token output.
    pub const STREAMING: &'static str = "streaming";
    /// Tool / function calling.
    pub const TOOLS: &'static str = "tools";
    /// Image input.
    pub const VISION: &'static str = "vision";
    /// Text embeddings.
    pub const EMBEDDINGS: &'static str = "embeddings";

    /// Returns whether a capability is present.
    pub fn has(&self, capability: &str) -> bool {
        self.0.iter().any(|name| name == capability)
    }
}

/// What a provider says about one of its models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// The model's identifier.
    pub id: ModelId,
    /// A human-readable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Maximum input tokens, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Maximum output tokens, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// What the model supports.
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

/// What a model is told about a tool it may call.
///
/// The model-facing subset of a [`ToolSpec`](crate::tool::ToolSpec): the name to call, what
/// the tool is for, and the shape of its arguments. A provider needs all three — no
/// provider's wire format accepts a bare name — and needs nothing else.
///
/// # Why this is not a `ToolSpec`
///
/// A [`ToolSpec`](crate::tool::ToolSpec) also carries
/// [`required_permissions`](crate::tool::ToolSpec::required_permissions), which is
/// authorization metadata: it tells the [`ToolRegistry`](crate::tool::ToolRegistry) what to
/// ask the policy engine before the tool runs. Handing that to a model would tell it which
/// capability names are worth asking for and which of its options are cheap, and it would
/// travel to whatever remote service the provider talks to. Neither is any of the model's
/// business, and the decision is not the model's to influence in any case.
///
/// Passing a distinct, smaller type is what makes that structural rather than a rule a
/// provider has to remember: a provider is handed only these three fields and cannot reach
/// the rest, whatever it does with them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// The name the model uses to call it, matching
    /// [`ToolCall::name`](crate::tool::ToolCall::name).
    pub name: ToolName,
    /// What it does, written for a model to read.
    pub description: String,
    /// JSON Schema describing the input object.
    pub input_schema: Value,
}

impl ToolDefinition {
    /// Describes a tool to a model.
    pub fn new(
        name: impl Into<ToolName>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

impl From<&crate::tool::ToolSpec> for ToolDefinition {
    fn from(spec: &crate::tool::ToolSpec) -> Self {
        Self {
            name: spec.name.clone(),
            description: spec.description.clone(),
            input_schema: spec.input_schema.clone(),
        }
    }
}

/// A request for a completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Which model to use.
    pub model: ModelId,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    ///
    /// Offering a tool here is not authorization to run it, and a provider that reports a
    /// call is not a decision that the call may happen: what a model asks for still goes
    /// through [`ToolRegistry::invoke`](crate::tool::ToolRegistry::invoke) and is still
    /// refused if policy says so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// Provider-specific settings: temperature, sampling, safety, anything.
    ///
    /// Opaque on purpose. The alternative — a struct with every provider's knobs — would
    /// have to change every time a provider does.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub parameters: Value,
}

impl CompletionRequest {
    /// Creates a request with no tools and default parameters.
    pub fn new(model: impl Into<ModelId>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            parameters: Value::Null,
        }
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished its turn.
    Stop,
    /// The output limit was reached.
    Length,
    /// The model is waiting for tool results.
    ToolCalls,
    /// The provider refused.
    Filtered,
    /// The caller cancelled.
    Cancelled,
}

/// How much a request cost, in tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the input.
    pub input_tokens: u64,
    /// Tokens produced.
    pub output_tokens: u64,
}

/// A completed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// The model's message.
    pub message: Message,
    /// Why it stopped.
    pub finish_reason: FinishReason,
    /// Token usage, if the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// One piece of a streamed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompletionChunk {
    /// Incremental content.
    Delta(ContentPart),
    /// A complete tool call, once its arguments have fully arrived.
    ToolCall(ToolCall),
    /// The end of the response.
    Done {
        /// Why it stopped.
        finish_reason: FinishReason,
        /// Token usage, if reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

/// A source of inference.
///
/// Implementations are registered in the kernel registry under `dyn ModelProvider` and one
/// [`ComponentId`](aik_core::ComponentId) per provider, so a router can enumerate them and
/// configuration can pick a default.
#[async_trait]
pub trait ModelProvider: Send + Sync + 'static {
    /// The models this provider can serve.
    ///
    /// Async because it may need to be fetched; implementations should cache.
    async fn models(&self) -> Result<Vec<ModelDescriptor>>;

    /// Generates a complete response.
    async fn complete(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<CompletionResponse>;

    /// Generates a response incrementally.
    ///
    /// Providers that cannot stream should return
    /// [`Error::Unsupported`](aik_core::Error::Unsupported) rather than emulate it, so the
    /// caller can decide whether to fall back to [`complete`](ModelProvider::complete).
    async fn stream(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>>;
}

/// A source of text embeddings.
#[async_trait]
pub trait Embedder: Send + Sync + 'static {
    /// Embeds a batch of inputs, returning one vector per input, in order.
    async fn embed(
        &self,
        model: &ModelId,
        inputs: &[String],
        cx: &ExecutionContext,
    ) -> Result<Vec<Vec<f32>>>;

    /// The dimensionality of the vectors a model produces, if it is fixed and known.
    fn dimensions(&self, model: &ModelId) -> Option<usize> {
        let _ = model;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::ActionId;
    use crate::tool::ToolSpec;
    use serde_json::json;

    fn spec() -> ToolSpec {
        ToolSpec {
            name: ToolName::new("filesystem.read"),
            description: "Reads a file".to_owned(),
            input_schema: json!({ "type": "object" }),
            output_schema: Some(json!({ "type": "string" })),
            required_permissions: vec![ActionId::new("filesystem.read")],
            read_only: true,
        }
    }

    #[test]
    fn a_definition_carries_what_a_model_needs_to_call_a_tool() {
        let definition = ToolDefinition::from(&spec());
        assert_eq!(definition.name, ToolName::new("filesystem.read"));
        assert_eq!(definition.description, "Reads a file");
        assert_eq!(definition.input_schema, json!({ "type": "object" }));
    }

    #[test]
    fn a_definition_cannot_carry_authorization_metadata() {
        // The security property the type exists for: whatever a provider serialises, a
        // model never learns which permissions a tool needs.
        let json = serde_json::to_value(ToolDefinition::from(&spec())).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["description", "input_schema", "name"]);
    }

    #[test]
    fn a_request_with_no_tools_serialises_without_the_field() {
        let request = CompletionRequest::new("m", vec![Message::text(Role::User, "hi")]);
        assert!(request.tools.is_empty());
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("tools").is_none(), "{json}");
    }
}
