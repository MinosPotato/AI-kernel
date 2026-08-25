//! The Messages API wire format and its translation to and from the kernel's model contract.
//!
//! Nothing here is public outside the crate. It is the seam where Anthropic's JSON meets
//! [`aik_api::model`], and it is where three shape mismatches between the two are resolved:
//!
//! * **There is no `system` role.** Instructions are a top-level field, so
//!   [`Role::System`] messages are hoisted out of the conversation and joined.
//! * **There is no `tool` role.** A tool result is a `tool_result` *block* on a `user`
//!   message, so [`Role::Tool`] messages become user messages, and adjacent messages that
//!   end up with the same role are merged — which is also what puts several parallel tool
//!   results into the one turn that answers them.
//! * **`max_tokens` is required.** [`CompletionRequest`] has no such field, so it comes from
//!   settings, overridable per request through
//!   [`parameters`](CompletionRequest::parameters).
//!
//! What the module refuses to do is drop anything. Content this provider cannot express is
//! an [`Error::Unsupported`] naming it, never a silently shorter conversation: a history with
//! a tool call quietly removed still looks well-formed, and the model would answer from a
//! conversation that never happened.

use aik_api::model::{
    CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message, ModelCapabilities,
    ModelDescriptor, ModelId, Role, ToolDefinition, Usage,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::http::AnthropicApiError;

/// Fields of the request body this provider owns.
///
/// A caller's [`parameters`](CompletionRequest::parameters) may set anything the API accepts
/// — `temperature`, `top_p`, `stop_sequences`, `tool_choice`, `metadata` — except these.
/// Letting `parameters` reach them would mean a request whose model, conversation or tools
/// are not the ones the caller passed in the typed fields, which no caller could reason about
/// and no audit record would reflect.
const RESERVED_PARAMETERS: &[&str] = &["model", "messages", "system", "tools", "stream"];

/// Parameters that are refused because this provider cannot hold up its end of them.
///
/// Extended thinking returns `thinking` blocks that must be replayed verbatim on the next
/// turn to keep a tool-using conversation valid. This provider maps unknown blocks to
/// [`ContentPart::Other`] and refuses to send them back, so a conversation that enabled
/// thinking would work for exactly one turn and then fail. Refusing up front is the honest
/// version of that.
const UNSUPPORTED_PARAMETERS: &[&str] = &["thinking"];

#[derive(Debug, Serialize)]
pub(crate) struct MessagesRequest {
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<String>,
    pub(crate) messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<WireTool>,
    pub(crate) stream: bool,
    /// Whatever the caller put in `parameters`, minus the reserved keys.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct WireMessage {
    pub(crate) role: &'static str,
    pub(crate) content: Vec<Block>,
}

/// One content block, in the shape the API sends and accepts.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Block {
    Text {
        text: String,
    },
    Image {
        source: Source,
    },
    Document {
        source: Source,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Inline binary data. Only the base64 form is produced: a URL source would make the service
/// fetch something on this deployment's behalf, which is a request this provider does not
/// get to make on a caller's behalf without them saying so.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct Source {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) media_type: String,
    pub(crate) data: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct WireTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

/// A non-streaming response body.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageResponse {
    #[serde(default)]
    pub(crate) content: Vec<Value>,
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<WireUsage>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub(crate) input_tokens: u64,
    #[serde(default)]
    pub(crate) output_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }
    }
}

/// The `/v1/models` listing.
#[derive(Debug, Deserialize)]
pub(crate) struct ModelsResponse {
    #[serde(default)]
    pub(crate) data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
}

/// Builds a request body from the provider-neutral request.
pub(crate) fn build_request(
    request: &CompletionRequest,
    default_max_tokens: u32,
    stream: bool,
) -> Result<MessagesRequest> {
    let extra = extra_parameters(&request.parameters)?;
    let max_tokens = match extra.get("max_tokens") {
        Some(value) => value
            .as_u64()
            .and_then(|tokens| u32::try_from(tokens).ok())
            .filter(|tokens| *tokens > 0)
            .ok_or_else(|| {
                Error::InvalidArgument(
                    "`max_tokens` in the request parameters must be a positive integer".to_owned(),
                )
            })?,
        None => default_max_tokens,
    };
    let mut extra = extra;
    extra.remove("max_tokens");

    let (system, messages) = convert_messages(&request.messages)?;

    Ok(MessagesRequest {
        model: request.model.as_str().to_owned(),
        max_tokens,
        system,
        messages,
        tools: request.tools.iter().map(convert_tool).collect(),
        stream,
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
                "the Anthropic provider expects request parameters to be a JSON object".to_owned(),
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
                "the Anthropic provider does not support `{key}`, because it cannot replay the \
                 blocks it produces on a later turn"
            )));
        }
    }

    Ok(map)
}

fn convert_tool(definition: &ToolDefinition) -> WireTool {
    WireTool {
        name: definition.name.as_str().to_owned(),
        description: definition.description.clone(),
        input_schema: definition.input_schema.clone(),
    }
}

/// Splits a conversation into the hoisted system prompt and the wire messages.
fn convert_messages(messages: &[Message]) -> Result<(Option<String>, Vec<WireMessage>)> {
    let mut system: Vec<String> = Vec::new();
    let mut wire: Vec<WireMessage> = Vec::new();

    for message in messages {
        if message.role == Role::System {
            system.push(system_text(message)?);
            continue;
        }

        let role = match message.role {
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
            Role::System => unreachable!("handled above"),
        };
        let blocks = convert_blocks(message)?;
        if blocks.is_empty() {
            // Nothing to say. The API rejects an empty content array, and a turn that
            // carried only an empty text part said nothing to begin with.
            continue;
        }

        match wire.last_mut() {
            // Adjacent turns with the same wire role are one turn: several tool results
            // answering parallel calls, or a caller that appended two user messages.
            Some(previous) if previous.role == role => previous.content.extend(blocks),
            _ => wire.push(WireMessage {
                role,
                content: blocks,
            }),
        }
    }

    let system = match system.is_empty() {
        true => None,
        false => Some(system.join("\n\n")),
    };
    Ok((system, wire))
}

/// A system message must be text: there is nowhere for anything else to go.
fn system_text(message: &Message) -> Result<String> {
    let mut text = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
            other => {
                return Err(Error::Unsupported(format!(
                    "a system message may only contain text; got `{other:?}`"
                )));
            }
        }
    }
    Ok(text)
}

fn convert_blocks(message: &Message) -> Result<Vec<Block>> {
    let mut blocks = Vec::new();

    for part in &message.content {
        match part {
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => blocks.push(Block::Text { text: text.clone() }),

            ContentPart::Blob { mime_type, data } => {
                blocks.push(blob_block(mime_type, data)?);
            }

            // Only an assistant turn can have asked for a tool. Refusing the same part on
            // any other role stops a caller constructing a history in which the user or a
            // tool appears to have issued the call the model is about to see answered.
            ContentPart::ToolCall(call) if message.role == Role::Assistant => {
                blocks.push(Block::ToolUse {
                    id: call.call_id.clone(),
                    name: call.name.as_str().to_owned(),
                    input: call.arguments.clone(),
                });
            }

            ContentPart::ToolResult {
                call_id,
                content,
                is_error,
            } if message.role == Role::Tool => blocks.push(Block::ToolResult {
                tool_use_id: call_id.clone(),
                // A `tool_result` takes a string or a list of blocks, not arbitrary JSON.
                // A string is sent as it stands — a tool that formatted its output for a
                // reader should not have it re-escaped — and anything else is serialised.
                content: match content {
                    Value::String(text) => Value::String(text.clone()),
                    other => Value::String(other.to_string()),
                },
                is_error: *is_error,
            }),

            other => {
                return Err(Error::Unsupported(format!(
                    "the Anthropic provider does not support `{other:?}` content parts on a \
                     `{:?}` message",
                    message.role
                )));
            }
        }
    }

    Ok(blocks)
}

/// Maps binary content onto the two block types that carry it.
fn blob_block(mime_type: &str, data: &str) -> Result<Block> {
    let source = Source {
        kind: "base64",
        media_type: mime_type.to_owned(),
        data: data.to_owned(),
    };
    match mime_type {
        "application/pdf" => Ok(Block::Document { source }),
        mime if mime.starts_with("image/") => Ok(Block::Image { source }),
        other => Err(Error::Unsupported(format!(
            "the Anthropic provider cannot send `{other}` content"
        ))),
    }
}

/// Converts a completed response body.
pub(crate) fn convert_response(body: MessageResponse) -> Result<CompletionResponse> {
    let content = body
        .content
        .into_iter()
        .map(convert_block)
        .collect::<Result<Vec<_>>>()?;
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
            false => map_stop_reason(body.stop_reason.as_deref()),
        },
        usage: body.usage.map(Usage::from),
    })
}

/// Converts one response block.
///
/// Blocks this crate does not model — `thinking`, a server-side tool's own blocks, whatever
/// is added next — become [`ContentPart::Other`] rather than an error or a silent omission.
/// The caller can see exactly what arrived; what it cannot do is send it back, which
/// [`convert_blocks`] refuses.
fn convert_block(block: Value) -> Result<ContentPart> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ContentPart::text(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )),
        Some("tool_use") => {
            let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                Error::other(
                    "a tool_use block arrived without an id, so its result could not \
                              be matched to it",
                )
            })?;
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::other("a tool_use block arrived without a name"))?;
            Ok(ContentPart::ToolCall(ToolCall {
                call_id: id.to_owned(),
                name: ToolName::new(name),
                arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
            }))
        }
        _ => Ok(ContentPart::Other(block)),
    }
}

/// Maps the API's `stop_reason`.
///
/// `pause_turn` — a long-running server-side tool asking to be continued — is reported as a
/// stop, because that is what it is from here: this provider does not continue a turn on the
/// caller's behalf.
pub(crate) fn map_stop_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("refusal") => FinishReason::Filtered,
        _ => FinishReason::Stop,
    }
}

/// Turns an error envelope arriving inside a stream into an [`Error`].
pub(crate) fn stream_error(value: &Value) -> Error {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_owned();
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the Anthropic API reported an error")
        .to_owned();
    Error::wrap(
        "the Anthropic API reported an error mid-stream",
        AnthropicApiError { kind, message },
    )
}

/// Converts a `/v1/models` listing.
///
/// The capabilities reported are the endpoint's, not introspection: every model served by the
/// Messages API streams and accepts tools. Nothing is claimed about vision or context window,
/// because the listing does not say and a guess here would be a guess a router acts on.
pub(crate) fn convert_models(response: ModelsResponse) -> Vec<ModelDescriptor> {
    response
        .data
        .into_iter()
        .map(|entry| ModelDescriptor {
            id: ModelId::new(entry.id),
            display_name: entry.display_name,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities(vec![
                ModelCapabilities::STREAMING.to_owned(),
                ModelCapabilities::TOOLS.to_owned(),
            ]),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest::new("claude-sonnet-4-5", messages)
    }

    #[test]
    fn system_messages_are_hoisted_and_joined() {
        let body = build_request(
            &request(vec![
                Message::text(Role::System, "be terse"),
                Message::text(Role::User, "hello"),
                Message::text(Role::System, "and precise"),
            ]),
            1024,
            false,
        )
        .unwrap();

        assert_eq!(body.system.as_deref(), Some("be terse\n\nand precise"));
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
    }

    #[test]
    fn max_tokens_comes_from_settings_unless_the_request_sets_it() {
        let body = build_request(&request(vec![]), 1024, false).unwrap();
        assert_eq!(body.max_tokens, 1024);

        let mut overridden = request(vec![]);
        overridden.parameters = json!({ "max_tokens": 64, "temperature": 0.2 });
        let body = build_request(&overridden, 1024, false).unwrap();
        assert_eq!(body.max_tokens, 64);
        // And it is not also sent twice, through the flattened extras.
        let encoded = serde_json::to_value(&body).unwrap();
        assert_eq!(encoded["max_tokens"], json!(64));
        assert_eq!(encoded["temperature"], json!(0.2));
    }

    #[test]
    fn a_nonsense_max_tokens_is_refused() {
        let mut bad = request(vec![]);
        bad.parameters = json!({ "max_tokens": 0 });
        assert_eq!(
            build_request(&bad, 1024, false).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );

        bad.parameters = json!({ "max_tokens": "lots" });
        assert!(build_request(&bad, 1024, false).is_err());
    }

    #[test]
    fn parameters_cannot_overwrite_what_the_request_decides() {
        for key in RESERVED_PARAMETERS {
            let mut bad = request(vec![Message::text(Role::User, "hi")]);
            let mut parameters = Map::new();
            parameters.insert((*key).to_owned(), json!("hijacked"));
            bad.parameters = Value::Object(parameters);
            let error = build_request(&bad, 1024, false).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{key}");
        }
    }

    #[test]
    fn extended_thinking_is_refused_rather_than_half_supported() {
        let mut bad = request(vec![]);
        bad.parameters = json!({ "thinking": { "type": "enabled", "budget_tokens": 1024 } });
        assert_eq!(
            build_request(&bad, 1024, false).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn parameters_that_are_not_an_object_are_refused() {
        let mut bad = request(vec![]);
        bad.parameters = json!("temperature=0");
        assert!(build_request(&bad, 1024, false).is_err());
    }

    #[test]
    fn tool_calls_and_their_results_replay_onto_the_wire() {
        let call = ToolCall {
            call_id: "toolu_1".to_owned(),
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
                        call_id: "toolu_1".to_owned(),
                        content: json!("hello"),
                        is_error: false,
                    }],
                    name: None,
                },
            ]),
            1024,
            false,
        )
        .unwrap();

        assert_eq!(body.messages.len(), 3);
        assert_eq!(body.messages[1].role, "assistant");
        assert_eq!(
            body.messages[1].content[0],
            Block::ToolUse {
                id: "toolu_1".to_owned(),
                name: "filesystem.read".to_owned(),
                input: json!({ "path": "a.txt" }),
            }
        );
        // The result became a user turn, which is where the API expects it.
        assert_eq!(body.messages[2].role, "user");
        assert_eq!(
            body.messages[2].content[0],
            Block::ToolResult {
                tool_use_id: "toolu_1".to_owned(),
                content: json!("hello"),
                is_error: false,
            }
        );
    }

    #[test]
    fn parallel_tool_results_become_one_turn() {
        let result = |id: &str| Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                call_id: id.to_owned(),
                content: json!("ok"),
                is_error: false,
            }],
            name: None,
        };
        let body = build_request(&request(vec![result("a"), result("b")]), 1024, false).unwrap();

        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].content.len(), 2);
    }

    #[test]
    fn a_failed_tool_keeps_its_error_flag() {
        let body = build_request(
            &request(vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    call_id: "toolu_1".to_owned(),
                    content: json!({ "denied": "policy" }),
                    is_error: true,
                }],
                name: None,
            }]),
            1024,
            false,
        )
        .unwrap();

        let encoded = serde_json::to_value(&body.messages[0].content[0]).unwrap();
        assert_eq!(encoded["is_error"], json!(true));
        assert_eq!(encoded["content"], json!("{\"denied\":\"policy\"}"));
    }

    #[test]
    fn a_tool_call_attributed_to_anyone_but_the_assistant_is_refused() {
        let call = ContentPart::ToolCall(ToolCall {
            call_id: "toolu_1".to_owned(),
            name: ToolName::new("filesystem.write"),
            arguments: json!({}),
        });
        for role in [Role::User, Role::Tool] {
            let error = build_request(
                &request(vec![Message {
                    role,
                    content: vec![call.clone()],
                    name: None,
                }]),
                1024,
                false,
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{role:?}");
        }
    }

    #[test]
    fn images_and_documents_are_sent_inline_and_anything_else_is_refused() {
        let blob = |mime: &str| {
            build_request(
                &request(vec![Message {
                    role: Role::User,
                    content: vec![ContentPart::Blob {
                        mime_type: mime.to_owned(),
                        data: "AAAA".to_owned(),
                    }],
                    name: None,
                }]),
                1024,
                false,
            )
        };

        assert!(matches!(
            blob("image/png").unwrap().messages[0].content[0],
            Block::Image { .. }
        ));
        assert!(matches!(
            blob("application/pdf").unwrap().messages[0].content[0],
            Block::Document { .. }
        ));
        assert_eq!(
            blob("audio/wav").unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn a_block_this_crate_does_not_model_cannot_be_replayed() {
        let error = build_request(
            &request(vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Other(json!({ "type": "thinking" }))],
                name: None,
            }]),
            1024,
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn responses_carry_text_tool_calls_and_usage() {
        let body: MessageResponse = serde_json::from_value(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "reading it" },
                { "type": "tool_use", "id": "toolu_1", "name": "filesystem.read",
                  "input": { "path": "a.txt" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 12, "output_tokens": 7 }
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
                assert_eq!(call.call_id, "toolu_1");
                assert_eq!(call.name.as_str(), "filesystem.read");
                assert_eq!(call.arguments, json!({ "path": "a.txt" }));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_block_is_preserved_rather_than_dropped() {
        let body: MessageResponse = serde_json::from_value(json!({
            "content": [{ "type": "thinking", "thinking": "hmm", "signature": "s" }],
            "stop_reason": "end_turn"
        }))
        .unwrap();

        let response = convert_response(body).unwrap();
        match &response.message.content[0] {
            ContentPart::Other(value) => assert_eq!(value["type"], json!("thinking")),
            other => panic!("expected the raw block, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_use_block_without_an_id_is_an_error() {
        let body: MessageResponse = serde_json::from_value(json!({
            "content": [{ "type": "tool_use", "name": "x", "input": {} }]
        }))
        .unwrap();
        assert!(convert_response(body).is_err());
    }

    #[test]
    fn stop_reasons_map_onto_the_contract() {
        assert_eq!(map_stop_reason(Some("end_turn")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("stop_sequence")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("pause_turn")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("max_tokens")), FinishReason::Length);
        assert_eq!(map_stop_reason(Some("tool_use")), FinishReason::ToolCalls);
        assert_eq!(map_stop_reason(Some("refusal")), FinishReason::Filtered);
        assert_eq!(map_stop_reason(None), FinishReason::Stop);
    }

    #[test]
    fn the_model_listing_reports_what_the_endpoint_guarantees() {
        let response: ModelsResponse = serde_json::from_value(json!({
            "data": [{ "type": "model", "id": "claude-x", "display_name": "Claude X" }],
            "has_more": false
        }))
        .unwrap();

        let models = convert_models(response);
        assert_eq!(models[0].id.as_str(), "claude-x");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude X"));
        assert!(models[0].capabilities.has(ModelCapabilities::TOOLS));
        assert!(models[0].capabilities.has(ModelCapabilities::STREAMING));
        assert_eq!(models[0].context_window, None);
    }

    #[test]
    fn tools_are_declared_with_only_their_model_facing_fields() {
        let mut with_tools = request(vec![]);
        with_tools.tools.push(ToolDefinition::new(
            "filesystem.read",
            "Reads a file",
            json!({ "type": "object" }),
        ));

        let body = build_request(&with_tools, 1024, false).unwrap();
        assert_eq!(
            serde_json::to_value(&body.tools).unwrap(),
            json!([{
                "name": "filesystem.read",
                "description": "Reads a file",
                "input_schema": { "type": "object" }
            }])
        );
    }
}
