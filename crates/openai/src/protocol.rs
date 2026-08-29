//! The chat-completions wire format and its translation to and from the kernel's model
//! contract.
//!
//! Nothing here is public outside the crate. It is the seam where this dialect's JSON meets
//! [`aik_api::model`], and it is where the mismatches between the two are resolved:
//!
//! * **A tool result is its own message.** [`Role::Tool`] messages carrying several
//!   [`ContentPart::ToolResult`] parts — parallel calls answered in one turn — become
//!   several `tool` messages, each keyed to the call it answers.
//! * **Tool arguments are a string, not an object.** They are serialised on the way out and
//!   parsed on the way back, and a fragment that does not parse is an error rather than an
//!   empty argument object.
//! * **There is no error flag on a tool result.** [`is_error`](ContentPart::ToolResult) is
//!   encoded into the content instead of dropped; see [`tool_result_content`].
//! * **A text-only message may be a bare string.** The array form is used only when a
//!   message actually carries a non-text part, because plenty of servers that speak this
//!   dialect accept a string and reject a one-element array.
//!
//! What the module refuses to do is drop anything replayable. Content this provider cannot
//! express is an [`Error::Unsupported`] naming it, never a silently shorter conversation: a
//! history with a tool call quietly removed still looks well-formed, and the model would
//! answer from a conversation that never happened.

use aik_api::model::{
    CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message, ModelCapabilities,
    ModelDescriptor, ModelId, Role, ToolDefinition, Usage,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::http::OpenAiApiError;

/// Fields of the request body this provider owns.
///
/// A caller's [`parameters`](CompletionRequest::parameters) may set anything the server
/// accepts — `temperature`, `top_p`, `stop`, `tool_choice`, `max_completion_tokens`,
/// `response_format` — except these. Letting `parameters` reach them would mean a request
/// whose model, conversation or tools are not the ones the caller passed in the typed
/// fields, which no caller could reason about and no audit record would reflect.
const RESERVED_PARAMETERS: &[&str] = &["model", "messages", "tools", "stream", "stream_options"];

/// Parameters that are refused because this provider cannot hold up its end of them.
///
/// `n` asks the server to generate several independent answers. This provider reads exactly
/// one, because [`CompletionResponse`] holds one message and there is nothing in the
/// contract that could carry the rest — so a request for several would be billed for answers
/// nobody ever sees.
const UNSUPPORTED_PARAMETERS: &[&str] = &["n"];

#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<WireTool>,
    pub(crate) stream: bool,
    /// Asks a streamed response to end with a usage report.
    ///
    /// Without it this dialect streams no token counts at all, and a turn whose cost is
    /// unknown is a turn [`aik-quota`](https://docs.rs/aik-quota) has to charge an estimate
    /// for. Only sent when streaming: a server that does not know the field would otherwise
    /// reject every non-streaming request too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<StreamOptions>,
    /// Whatever the caller put in `parameters`, minus the reserved keys.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct StreamOptions {
    pub(crate) include_usage: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct WireMessage {
    pub(crate) role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

impl WireMessage {
    fn new(role: &'static str) -> Self {
        Self {
            role,
            content: None,
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// A message body, in whichever of the two shapes this dialect accepts.
#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum Content {
    /// The bare string form, used whenever a message is only text.
    Text(String),
    /// The multi-part form, used when a message carries anything else.
    Parts(Vec<Part>),
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Part {
    Text {
        text: String,
    },
    /// An image, always as an inline `data:` URL.
    ///
    /// Only the inline form is produced: a remote URL would make the service fetch something
    /// on this deployment's behalf, which is a request this provider does not get to make on
    /// a caller's behalf without them saying so.
    ImageUrl {
        image_url: ImageUrl,
    },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ImageUrl {
    pub(crate) url: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WireToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: WireFunctionCall,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WireFunctionCall {
    pub(crate) name: String,
    /// The arguments, as a JSON *string*. That is what this dialect sends and expects back.
    pub(crate) arguments: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct WireTool {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: WireFunction,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct WireFunction {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

/// A non-streaming response body.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatResponse {
    #[serde(default)]
    pub(crate) choices: Vec<Choice>,
    #[serde(default)]
    pub(crate) usage: Option<WireUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Choice {
    #[serde(default)]
    pub(crate) message: ResponseMessage,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResponseMessage {
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<ResponseToolCall>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResponseToolCall {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) function: ResponseFunction,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResponseFunction {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) arguments: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub(crate) prompt_tokens: u64,
    #[serde(default)]
    pub(crate) completion_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        }
    }
}

/// The `/models` listing.
#[derive(Debug, Deserialize)]
pub(crate) struct ModelsResponse {
    #[serde(default)]
    pub(crate) data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelEntry {
    pub(crate) id: String,
}

/// The `/embeddings` request.
#[derive(Debug, Serialize)]
pub(crate) struct EmbedRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) input: &'a [String],
    /// Always `float`, never left to the server's default.
    ///
    /// This dialect also has a `base64` form, and a server that chose it would hand back
    /// strings where this client expects numbers. Naming the format is one field on the wire
    /// and removes a whole class of "the vectors came back empty".
    pub(crate) encoding_format: &'static str,
}

/// The `/embeddings` response.
#[derive(Debug, Deserialize)]
pub(crate) struct EmbedResponse {
    #[serde(default)]
    pub(crate) data: Vec<EmbedEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EmbedEntry {
    #[serde(default)]
    pub(crate) index: usize,
    #[serde(default)]
    pub(crate) embedding: Vec<f32>,
}

/// Builds a request body from the provider-neutral request.
pub(crate) fn build_request(request: &CompletionRequest, stream: bool) -> Result<ChatRequest> {
    let extra = extra_parameters(&request.parameters)?;

    Ok(ChatRequest {
        model: request.model.as_str().to_owned(),
        messages: convert_messages(&request.messages)?,
        tools: request.tools.iter().map(convert_tool).collect(),
        stream,
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
        extra,
    })
}

/// Validates and unwraps the caller's opaque parameters.
fn extra_parameters(parameters: &Value) -> Result<Map<String, Value>> {
    let map = match parameters {
        Value::Null => return Ok(Map::new()),
        Value::Object(map) => map.clone(),
        _ => {
            return Err(Error::InvalidArgument(
                "the OpenAI provider expects request parameters to be a JSON object".to_owned(),
            ));
        }
    };

    for key in RESERVED_PARAMETERS {
        if map.contains_key(*key) {
            return Err(Error::InvalidArgument(format!(
                "`{key}` is set from the request itself and cannot be overridden through \
                 parameters"
            )));
        }
    }
    for key in UNSUPPORTED_PARAMETERS {
        if map.contains_key(*key) {
            return Err(Error::Unsupported(format!(
                "the OpenAI provider does not support `{key}`: it reads one choice per \
                 response, so the others would be paid for and discarded"
            )));
        }
    }

    Ok(map)
}

fn convert_tool(definition: &ToolDefinition) -> WireTool {
    WireTool {
        kind: "function",
        function: WireFunction {
            name: definition.name.as_str().to_owned(),
            description: definition.description.clone(),
            parameters: definition.input_schema.clone(),
        },
    }
}

/// Converts a conversation into wire messages.
///
/// One neutral message becomes one wire message, except a [`Role::Tool`] turn answering
/// several calls at once, which becomes one `tool` message per result. Nothing is merged:
/// this dialect has a role for every role the contract has, so adjacent turns stay adjacent
/// turns.
fn convert_messages(messages: &[Message]) -> Result<Vec<WireMessage>> {
    let mut wire = Vec::new();

    for message in messages {
        match message.role {
            Role::Tool => wire.extend(tool_messages(message)?),
            Role::System => wire.extend(plain_message(message, "system")?),
            Role::User => wire.extend(plain_message(message, "user")?),
            Role::Assistant => wire.extend(assistant_message(message)?),
        }
    }

    Ok(wire)
}

/// Converts a system or user turn, which may carry only content.
fn plain_message(message: &Message, role: &'static str) -> Result<Option<WireMessage>> {
    let mut parts = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => parts.push(Part::Text { text: text.clone() }),
            ContentPart::Blob { mime_type, data } if role == "user" => {
                parts.push(image_part(mime_type, data)?);
            }
            other => {
                return Err(Error::Unsupported(format!(
                    "the OpenAI provider does not support `{other:?}` content parts on a \
                     `{:?}` message",
                    message.role
                )));
            }
        }
    }

    // Nothing to say. A turn that carried only an empty text part said nothing to begin
    // with, and some servers reject a message with no content at all.
    if parts.is_empty() {
        return Ok(None);
    }

    Ok(Some(WireMessage {
        content: Some(condense(parts)),
        name: message.name.clone(),
        ..WireMessage::new(role)
    }))
}

/// Converts an assistant turn, which may carry content, tool calls, or both.
fn assistant_message(message: &Message) -> Result<Option<WireMessage>> {
    let mut parts = Vec::new();
    let mut calls = Vec::new();

    for part in &message.content {
        match part {
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => parts.push(Part::Text { text: text.clone() }),
            ContentPart::ToolCall(call) => calls.push(WireToolCall {
                id: call.call_id.clone(),
                kind: "function",
                function: WireFunctionCall {
                    name: call.name.as_str().to_owned(),
                    // The arguments travel as a string. `to_string` on a `Value` is exact,
                    // so what the model asked for is what is replayed.
                    arguments: call.arguments.to_string(),
                },
            }),
            other => {
                return Err(Error::Unsupported(format!(
                    "the OpenAI provider does not support `{other:?}` content parts on an \
                     assistant message"
                )));
            }
        }
    }

    if parts.is_empty() && calls.is_empty() {
        return Ok(None);
    }

    Ok(Some(WireMessage {
        // Omitted rather than sent empty when the turn was only tool calls: this dialect
        // treats an absent content as "the assistant said nothing", which is what happened.
        content: (!parts.is_empty()).then(|| condense(parts)),
        name: message.name.clone(),
        tool_calls: calls,
        ..WireMessage::new("assistant")
    }))
}

/// Converts a tool turn into one `tool` message per result it carries.
fn tool_messages(message: &Message) -> Result<Vec<WireMessage>> {
    let mut wire = Vec::new();

    for part in &message.content {
        match part {
            ContentPart::ToolResult {
                call_id,
                content,
                is_error,
            } => wire.push(WireMessage {
                content: Some(Content::Text(tool_result_content(content, *is_error))),
                tool_call_id: Some(call_id.clone()),
                ..WireMessage::new("tool")
            }),
            // A `tool` message in this dialect is one answer to one call and has nowhere to
            // put anything else. Refusing is better than attaching the stray part to a
            // neighbouring result, which would attribute it to a call it did not come from.
            other => {
                return Err(Error::Unsupported(format!(
                    "the OpenAI provider does not support `{other:?}` content parts on a tool \
                     message; a tool message carries exactly one result"
                )));
            }
        }
    }

    Ok(wire)
}

/// Renders a tool result, keeping the fact that it failed.
///
/// This dialect has no `is_error` field, so a failure that were sent as its bare content
/// would reach the model as an ordinary answer — a refused `filesystem.write` reading like a
/// successful one. The flag is therefore encoded: a failed result is wrapped in an object
/// that says so, and a successful one is sent exactly as it stands, a string unquoted so a
/// tool that formatted its output for a reader does not have it re-escaped.
fn tool_result_content(content: &Value, is_error: bool) -> String {
    match (is_error, content) {
        (false, Value::String(text)) => text.clone(),
        (false, other) => other.to_string(),
        (true, other) => json!({ "is_error": true, "result": other }).to_string(),
    }
}

/// Uses the bare string form when a message is only text.
fn condense(mut parts: Vec<Part>) -> Content {
    let all_text = parts.iter().all(|part| matches!(part, Part::Text { .. }));
    if !all_text {
        return Content::Parts(parts);
    }
    if parts.len() == 1 {
        if let Part::Text { text } = parts.remove(0) {
            return Content::Text(text);
        }
        unreachable!("checked above");
    }
    let joined = parts
        .into_iter()
        .map(|part| match part {
            Part::Text { text } => text,
            _ => unreachable!("checked above"),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Content::Text(joined)
}

/// Maps binary content onto the one block type this provider produces.
///
/// Images become inline `data:` URLs. Everything else — a PDF above all — is refused:
/// this dialect's file part is keyed by a filename, and
/// [`ContentPart::Blob`](aik_api::model::ContentPart::Blob) has no such field, so the
/// provider would have to invent one and the model would reason about a document whose name
/// nothing chose.
fn image_part(mime_type: &str, data: &str) -> Result<Part> {
    if !mime_type.starts_with("image/") {
        return Err(Error::Unsupported(format!(
            "the OpenAI provider cannot send `{mime_type}` content; only images have an \
             inline form here"
        )));
    }
    // The data is already base64 per the contract, so this is a concatenation rather than an
    // encoding step. A media type with a `;` in it would break the URL's own grammar.
    if mime_type.contains([';', ',']) {
        return Err(Error::Unsupported(format!(
            "`{mime_type}` cannot be expressed as a data URL media type"
        )));
    }
    Ok(Part::ImageUrl {
        image_url: ImageUrl {
            url: format!("data:{mime_type};base64,{data}"),
        },
    })
}

/// Converts a completed response body.
pub(crate) fn convert_response(body: ChatResponse) -> Result<CompletionResponse> {
    let usage = body.usage.map(Usage::from);
    let choice = body.choices.into_iter().next().ok_or_else(|| {
        Error::wrap(
            "decoding an OpenAI completion response",
            OpenAiApiError {
                kind: "http".to_owned(),
                message: "the response carried no choices".to_owned(),
            },
        )
    })?;

    let mut content = Vec::new();
    if let Some(text) = choice.message.content.filter(|text| !text.is_empty()) {
        content.push(ContentPart::text(text));
    }
    for call in choice.message.tool_calls {
        content.push(ContentPart::ToolCall(convert_tool_call(call)?));
    }

    let asked_for_tools = content
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall(_)));

    Ok(CompletionResponse {
        message: Message {
            role: Role::Assistant,
            content,
            name: None,
        },
        finish_reason: match asked_for_tools {
            true => FinishReason::ToolCalls,
            false => map_finish_reason(choice.finish_reason.as_deref()),
        },
        usage,
    })
}

/// Turns one reported call into a [`ToolCall`], parsing its arguments.
pub(crate) fn convert_tool_call(call: ResponseToolCall) -> Result<ToolCall> {
    let id = call.id.filter(|id| !id.is_empty()).ok_or_else(|| {
        Error::other("a tool call arrived without an id, so its result could not be matched to it")
    })?;
    let name = call
        .function
        .name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::other("a tool call arrived without a name"))?;

    Ok(ToolCall {
        call_id: id,
        name: ToolName::new(&name),
        arguments: parse_arguments(
            call.function.arguments.as_deref().unwrap_or_default(),
            &name,
        )?,
    })
}

/// Parses a tool call's arguments out of the string this dialect carries them in.
///
/// A tool taking no arguments is reported with an empty string or `{}` depending on the
/// server, and both mean the same thing. Anything else that does not parse is an error: a
/// tool invoked with `{}` because its arguments were truncated is a tool invoked with the
/// wrong arguments.
pub(crate) fn parse_arguments(raw: &str, name: &str) -> Result<Value> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let parsed: Value = serde_json::from_str(raw).map_err(|error| {
        Error::wrap(
            format!("the OpenAI API sent arguments for `{name}` that are not valid JSON"),
            error,
        )
    })?;
    match parsed {
        // The schema handed to the model describes an object, and
        // `ToolRegistry::invoke` is given one. A bare string or array here would be a
        // well-formed value of the wrong shape, which fails further away with a worse
        // message.
        Value::Object(_) => Ok(parsed),
        other => Err(Error::other(format!(
            "the OpenAI API sent arguments for `{name}` that are {} rather than an object",
            kind_of(&other)
        ))),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Maps this dialect's `finish_reason`.
pub(crate) fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("length") => FinishReason::Length,
        // `function_call` is the superseded spelling, still sent by some servers.
        Some("tool_calls" | "function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::Filtered,
        _ => FinishReason::Stop,
    }
}

/// Turns an error envelope arriving inside a stream into an [`Error`].
pub(crate) fn stream_error(value: &Value) -> Error {
    let kind = value
        .get("type")
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_owned();
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the OpenAI API reported an error")
        .to_owned();
    Error::wrap(
        "the OpenAI API reported an error mid-stream",
        OpenAiApiError { kind, message },
    )
}

/// Converts a `/models` listing.
///
/// No capabilities are reported, and that is not an omission. This dialect's listing
/// describes identity and nothing else, and the same listing carries models that cannot hold
/// a conversation at all — embedding models, transcription models, image models. Claiming
/// `tools` or `streaming` for every row would be a guess, and a guess here is a guess a
/// router acts on.
pub(crate) fn convert_models(response: ModelsResponse) -> Vec<ModelDescriptor> {
    response
        .data
        .into_iter()
        .map(|entry| ModelDescriptor {
            id: ModelId::new(entry.id),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        })
        .collect()
}

/// Converts an embedding response, putting the vectors back into input order.
///
/// The entries carry their own `index`, and this sorts by it rather than trusting the order
/// they arrived in: a batch whose vectors were silently transposed would embed every record
/// under the wrong text, and nothing downstream could ever detect it. A batch whose length,
/// indices or vector widths do not line up is an error, not a guess.
pub(crate) fn convert_embeddings(response: EmbedResponse, inputs: usize) -> Result<Vec<Vec<f32>>> {
    let fail = |message: String| {
        Error::wrap(
            "decoding an OpenAI embedding response",
            OpenAiApiError {
                kind: "embeddings".to_owned(),
                message,
            },
        )
    };

    if response.data.len() != inputs {
        return Err(fail(format!(
            "asked for {inputs} embeddings and got {}",
            response.data.len()
        )));
    }

    let mut slots: Vec<Option<Vec<f32>>> = vec![None; inputs];
    for entry in response.data {
        let slot = slots
            .get_mut(entry.index)
            .ok_or_else(|| fail(format!("an embedding claimed index {}", entry.index)))?;
        if slot.is_some() {
            return Err(fail(format!(
                "two embeddings claimed index {}",
                entry.index
            )));
        }
        *slot = Some(entry.embedding);
    }

    let embeddings: Vec<Vec<f32>> = slots
        .into_iter()
        .map(|slot| slot.ok_or_else(|| fail("the server left a gap in the batch".to_owned())))
        .collect::<Result<_>>()?;

    if let Some(first) = embeddings.first() {
        if first.is_empty() {
            return Err(fail("the server returned empty vectors".to_owned()));
        }
        if let Some(other) = embeddings
            .iter()
            .find(|embedding| embedding.len() != first.len())
        {
            return Err(fail(format!(
                "the server returned vectors of differing widths: {} and {}",
                first.len(),
                other.len()
            )));
        }
    }

    Ok(embeddings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest::new("gpt-4.1-mini", messages)
    }

    fn encode(body: &ChatRequest) -> Value {
        serde_json::to_value(body).unwrap()
    }

    #[test]
    fn a_text_only_conversation_keeps_every_role_in_place() {
        let body = build_request(
            &request(vec![
                Message::text(Role::System, "be terse"),
                Message::text(Role::User, "hello"),
            ]),
            false,
        )
        .unwrap();

        assert_eq!(
            encode(&body)["messages"],
            json!([
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hello" },
            ])
        );
        assert_eq!(encode(&body)["stream"], json!(false));
        assert!(encode(&body).get("stream_options").is_none());
    }

    #[test]
    fn streaming_asks_for_a_usage_report() {
        let body = build_request(&request(vec![Message::text(Role::User, "hi")]), true).unwrap();
        assert_eq!(
            encode(&body)["stream_options"],
            json!({ "include_usage": true })
        );
    }

    #[test]
    fn a_speaker_name_is_carried_through() {
        let body = build_request(
            &request(vec![Message {
                role: Role::User,
                content: vec![ContentPart::text("hi")],
                name: Some("ada".to_owned()),
            }]),
            false,
        )
        .unwrap();
        assert_eq!(encode(&body)["messages"][0]["name"], json!("ada"));
    }

    #[test]
    fn several_text_parts_become_one_string_rather_than_an_array() {
        // The array form is correct JSON but rejected by several servers in this family.
        let body = build_request(
            &request(vec![Message {
                role: Role::User,
                content: vec![ContentPart::text("one"), ContentPart::text("two")],
                name: None,
            }]),
            false,
        )
        .unwrap();
        assert_eq!(encode(&body)["messages"][0]["content"], json!("one\n\ntwo"));
    }

    #[test]
    fn a_message_that_says_nothing_is_left_out() {
        let body = build_request(
            &request(vec![
                Message::text(Role::User, ""),
                Message::text(Role::User, "hi"),
            ]),
            false,
        )
        .unwrap();
        assert_eq!(body.messages.len(), 1);
    }

    #[test]
    fn parameters_cannot_overwrite_what_the_request_decides() {
        for key in RESERVED_PARAMETERS {
            let mut bad = request(vec![Message::text(Role::User, "hi")]);
            let mut parameters = Map::new();
            parameters.insert((*key).to_owned(), json!("hijacked"));
            bad.parameters = Value::Object(parameters);
            let error = build_request(&bad, false).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{key}");
        }
    }

    #[test]
    fn several_choices_are_refused_rather_than_silently_discarded() {
        let mut bad = request(vec![]);
        bad.parameters = json!({ "n": 3 });
        assert_eq!(
            build_request(&bad, false).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn other_parameters_are_passed_through() {
        let mut tuned = request(vec![Message::text(Role::User, "hi")]);
        tuned.parameters = json!({ "temperature": 0.2, "max_completion_tokens": 64 });
        let encoded = encode(&build_request(&tuned, false).unwrap());
        assert_eq!(encoded["temperature"], json!(0.2));
        assert_eq!(encoded["max_completion_tokens"], json!(64));
    }

    #[test]
    fn parameters_that_are_not_an_object_are_refused() {
        let mut bad = request(vec![]);
        bad.parameters = json!("temperature=0");
        assert!(build_request(&bad, false).is_err());
    }

    #[test]
    fn tools_are_declared_with_only_their_model_facing_fields() {
        let mut with_tools = request(vec![]);
        with_tools.tools.push(ToolDefinition::new(
            "filesystem.read",
            "Reads a file",
            json!({ "type": "object" }),
        ));

        let body = build_request(&with_tools, false).unwrap();
        assert_eq!(
            serde_json::to_value(&body.tools).unwrap(),
            json!([{
                "type": "function",
                "function": {
                    "name": "filesystem.read",
                    "description": "Reads a file",
                    "parameters": { "type": "object" }
                }
            }])
        );
    }

    #[test]
    fn tool_calls_and_their_results_replay_onto_the_wire() {
        let call = ToolCall {
            call_id: "call_1".to_owned(),
            name: ToolName::new("filesystem.read"),
            arguments: json!({ "path": "a.txt" }),
        };
        let body = build_request(
            &request(vec![
                Message::text(Role::User, "read a.txt"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall(call)],
                    name: None,
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        call_id: "call_1".to_owned(),
                        content: json!("hello"),
                        is_error: false,
                    }],
                    name: None,
                },
            ]),
            false,
        )
        .unwrap();

        assert_eq!(
            encode(&body)["messages"],
            json!([
                { "role": "user", "content": "read a.txt" },
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "filesystem.read",
                            "arguments": "{\"path\":\"a.txt\"}"
                        }
                    }]
                },
                { "role": "tool", "content": "hello", "tool_call_id": "call_1" },
            ])
        );
    }

    #[test]
    fn parallel_tool_results_become_one_message_each() {
        let result = |id: &str| ContentPart::ToolResult {
            call_id: id.to_owned(),
            content: json!("ok"),
            is_error: false,
        };
        let body = build_request(
            &request(vec![Message {
                role: Role::Tool,
                content: vec![result("a"), result("b")],
                name: None,
            }]),
            false,
        )
        .unwrap();

        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0].tool_call_id.as_deref(), Some("a"));
        assert_eq!(body.messages[1].tool_call_id.as_deref(), Some("b"));
    }

    #[test]
    fn a_failed_tool_result_says_so_in_its_content() {
        // The dialect has no flag for it, so losing it would make a refusal read like an
        // answer.
        assert_eq!(
            tool_result_content(&json!({ "denied": "policy" }), true),
            r#"{"is_error":true,"result":{"denied":"policy"}}"#
        );
        assert_eq!(tool_result_content(&json!("hello"), false), "hello");
        assert_eq!(tool_result_content(&json!({ "n": 1 }), false), r#"{"n":1}"#);
    }

    #[test]
    fn a_tool_call_attributed_to_anyone_but_the_assistant_is_refused() {
        let call = ContentPart::ToolCall(ToolCall {
            call_id: "call_1".to_owned(),
            name: ToolName::new("filesystem.write"),
            arguments: json!({}),
        });
        for role in [Role::User, Role::System, Role::Tool] {
            let error = build_request(
                &request(vec![Message {
                    role,
                    content: vec![call.clone()],
                    name: None,
                }]),
                false,
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{role:?}");
        }
    }

    #[test]
    fn a_stray_part_on_a_tool_message_is_refused() {
        let error = build_request(
            &request(vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::text("by the way")],
                name: None,
            }]),
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn images_are_sent_inline_and_anything_else_is_refused() {
        let blob = |mime: &str| {
            build_request(
                &request(vec![Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::text("what is this"),
                        ContentPart::Blob {
                            mime_type: mime.to_owned(),
                            data: "AAAA".to_owned(),
                        },
                    ],
                    name: None,
                }]),
                false,
            )
        };

        let body = blob("image/png").unwrap();
        assert_eq!(
            encode(&body)["messages"][0]["content"],
            json!([
                { "type": "text", "text": "what is this" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
            ])
        );

        // A PDF has a form in this dialect, but one keyed by a filename the contract has no
        // field for.
        assert_eq!(
            blob("application/pdf").unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        assert_eq!(
            blob("audio/wav").unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn a_media_type_that_would_break_the_data_url_is_refused() {
        assert!(image_part("image/png;charset=binary", "AAAA").is_err());
    }

    #[test]
    fn a_blob_on_a_system_message_is_refused() {
        let error = build_request(
            &request(vec![Message {
                role: Role::System,
                content: vec![ContentPart::Blob {
                    mime_type: "image/png".to_owned(),
                    data: "AAAA".to_owned(),
                }],
                name: None,
            }]),
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn a_block_this_crate_does_not_model_cannot_be_replayed() {
        let error = build_request(
            &request(vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Other(json!({ "type": "reasoning" }))],
                name: None,
            }]),
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn responses_carry_text_tool_calls_and_usage() {
        let body: ChatResponse = serde_json::from_value(json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "reading it",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "filesystem.read",
                            "arguments": "{\"path\":\"a.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19 }
        }))
        .unwrap();

        let response = convert_response(body).unwrap();

        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 12,
                output_tokens: 7
            })
        );
        assert_eq!(response.message.content[0], ContentPart::text("reading it"));
        match &response.message.content[1] {
            ContentPart::ToolCall(call) => {
                assert_eq!(call.call_id, "call_1");
                assert_eq!(call.name.as_str(), "filesystem.read");
                assert_eq!(call.arguments, json!({ "path": "a.txt" }));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn a_null_content_is_not_an_empty_text_part() {
        let body: ChatResponse = serde_json::from_value(json!({
            "choices": [{ "message": { "role": "assistant", "content": null },
                          "finish_reason": "stop" }]
        }))
        .unwrap();
        let response = convert_response(body).unwrap();
        assert!(response.message.content.is_empty());
    }

    #[test]
    fn a_response_with_no_choices_is_an_error() {
        let body: ChatResponse = serde_json::from_value(json!({ "choices": [] })).unwrap();
        assert!(convert_response(body).is_err());
    }

    #[test]
    fn a_tool_call_without_an_id_or_a_name_is_an_error() {
        for call in [
            json!({ "function": { "name": "x", "arguments": "{}" } }),
            json!({ "id": "call_1", "function": { "arguments": "{}" } }),
        ] {
            let body: ChatResponse = serde_json::from_value(json!({
                "choices": [{ "message": { "tool_calls": [call] } }]
            }))
            .unwrap();
            assert!(convert_response(body).is_err());
        }
    }

    #[test]
    fn truncated_arguments_are_an_error_not_an_empty_call() {
        assert!(parse_arguments("{\"path\":\"/et", "rm").is_err());
    }

    #[test]
    fn absent_arguments_mean_a_tool_that_takes_none() {
        assert_eq!(parse_arguments("", "now").unwrap(), json!({}));
        assert_eq!(parse_arguments("  ", "now").unwrap(), json!({}));
        assert_eq!(parse_arguments("{}", "now").unwrap(), json!({}));
    }

    #[test]
    fn arguments_that_are_not_an_object_are_refused() {
        let error = parse_arguments("\"rm -rf /\"", "shell").unwrap_err();
        assert!(format!("{error}").contains("a string"), "{error}");
        assert!(parse_arguments("[1,2]", "shell").is_err());
    }

    #[test]
    fn finish_reasons_map_onto_the_contract() {
        assert_eq!(map_finish_reason(Some("stop")), FinishReason::Stop);
        assert_eq!(map_finish_reason(Some("length")), FinishReason::Length);
        assert_eq!(
            map_finish_reason(Some("tool_calls")),
            FinishReason::ToolCalls
        );
        assert_eq!(
            map_finish_reason(Some("function_call")),
            FinishReason::ToolCalls
        );
        assert_eq!(
            map_finish_reason(Some("content_filter")),
            FinishReason::Filtered
        );
        assert_eq!(map_finish_reason(None), FinishReason::Stop);
    }

    #[test]
    fn the_model_listing_claims_no_capabilities_it_cannot_know() {
        let response: ModelsResponse = serde_json::from_value(json!({
            "object": "list",
            "data": [
                { "id": "gpt-4.1-mini", "object": "model" },
                { "id": "text-embedding-3-small", "object": "model" }
            ]
        }))
        .unwrap();

        let models = convert_models(response);
        assert_eq!(models[0].id.as_str(), "gpt-4.1-mini");
        assert_eq!(models[1].id.as_str(), "text-embedding-3-small");
        assert!(!models[0].capabilities.has(ModelCapabilities::TOOLS));
    }

    fn embed_response(entries: Value) -> EmbedResponse {
        serde_json::from_value(json!({ "data": entries })).unwrap()
    }

    #[test]
    fn embeddings_are_returned_in_input_order_whatever_order_they_arrive_in() {
        let response = embed_response(json!([
            { "index": 1, "embedding": [0.3, 0.4] },
            { "index": 0, "embedding": [0.1, 0.2] },
        ]));
        assert_eq!(
            convert_embeddings(response, 2).unwrap(),
            vec![vec![0.1, 0.2], vec![0.3, 0.4]]
        );
    }

    #[test]
    fn a_short_batch_is_refused() {
        let response = embed_response(json!([{ "index": 0, "embedding": [0.1] }]));
        let error = convert_embeddings(response, 2).unwrap_err();
        assert!(format!("{error}").contains("OpenAI"), "{error}");
    }

    #[test]
    fn a_duplicated_or_out_of_range_index_is_refused() {
        let duplicated = embed_response(json!([
            { "index": 0, "embedding": [0.1] },
            { "index": 0, "embedding": [0.2] },
        ]));
        assert!(convert_embeddings(duplicated, 2).is_err());

        let out_of_range = embed_response(json!([
            { "index": 0, "embedding": [0.1] },
            { "index": 9, "embedding": [0.2] },
        ]));
        assert!(convert_embeddings(out_of_range, 2).is_err());
    }

    #[test]
    fn empty_or_ragged_vectors_are_refused() {
        let empty = embed_response(json!([{ "index": 0, "embedding": [] }]));
        assert!(convert_embeddings(empty, 1).is_err());

        let ragged = embed_response(json!([
            { "index": 0, "embedding": [0.1, 0.2] },
            { "index": 1, "embedding": [0.3] },
        ]));
        assert!(convert_embeddings(ragged, 2).is_err());
    }
}
