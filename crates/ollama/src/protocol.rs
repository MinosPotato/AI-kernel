//! The Ollama wire protocol and its translation to and from the kernel's model contract.
//!
//! Nothing in this module is public outside the crate: it is the seam where Ollama's JSON
//! shapes meet [`aik_api::model`], and no other crate should ever need to know it exists.

use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelDescriptor, ModelId, Role, ToolDefinition, Usage,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Wraps an error message returned by the Ollama server, e.g. "model not found".
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct OllamaApiError(pub(crate) String);

#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<WireMessage>,
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub(crate) options: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireMessage {
    pub(crate) role: String,
    pub(crate) content: String,
    /// Tool calls carried by an assistant turn being replayed to the model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<WireToolCall>,
    /// Which call a `tool` message answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

impl WireMessage {
    fn new(role: &str) -> Self {
        Self {
            role: role.to_owned(),
            content: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// One tool as Ollama's `/api/chat` expects it to be declared.
#[derive(Debug, Serialize)]
pub(crate) struct WireTool {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: WireFunction,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireFunction {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

/// One tool call, in both directions: Ollama emits and accepts the same shape.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct WireToolCall {
    /// Ollama has only carried an id since a recent version, so it is optional here and
    /// synthesised when absent — see [`convert_tool_call`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) function: WireToolCallFunction,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct WireToolCallFunction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) index: Option<u32>,
    #[serde(default)]
    pub(crate) name: String,
    /// An object, as Ollama sends and expects it — not a JSON-encoded string.
    #[serde(default)]
    pub(crate) arguments: Value,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatResponseLine {
    #[serde(default)]
    pub(crate) message: Option<WireResponseMessage>,
    #[serde(default)]
    pub(crate) done: bool,
    #[serde(default)]
    pub(crate) done_reason: Option<String>,
    #[serde(default)]
    pub(crate) prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub(crate) eval_count: Option<u64>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireResponseMessage {
    #[serde(default)]
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TagsResponse {
    #[serde(default)]
    pub(crate) models: Vec<TagModel>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TagModel {
    pub(crate) name: String,
}

/// Builds an Ollama chat request from the kernel's provider-neutral request.
///
/// Rejects content this provider cannot represent up front, with a clear
/// [`Error::Unsupported`], rather than silently dropping data the caller expected to be
/// sent. That matters most for tool interactions: a conversation with the tool calls
/// quietly removed still looks well-formed, and the model would answer it confidently from
/// a history that never happened.
pub(crate) fn build_chat_request(request: &CompletionRequest, stream: bool) -> Result<ChatRequest> {
    let messages = request
        .messages
        .iter()
        .map(convert_message)
        .collect::<Result<Vec<_>>>()?;

    Ok(ChatRequest {
        model: request.model.as_str().to_owned(),
        messages,
        stream,
        tools: request.tools.iter().map(convert_tool).collect(),
        options: request.parameters.clone(),
    })
}

/// Declares one tool to the model.
///
/// Only the three model-facing fields exist on a [`ToolDefinition`], so nothing about what
/// the tool is permitted to do can reach the wire from here.
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

fn convert_message(message: &Message) -> Result<WireMessage> {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut wire = WireMessage::new(role);

    for part in &message.content {
        match part {
            ContentPart::Text { text } => wire.content.push_str(text),

            // Only an assistant turn can have asked for a tool. Refusing the same part on
            // any other role keeps a caller from constructing a history in which the user
            // or a tool appears to have issued the call the model is about to see answered.
            ContentPart::ToolCall(call) if message.role == Role::Assistant => {
                wire.tool_calls.push(WireToolCall {
                    id: Some(call.call_id.clone()),
                    function: WireToolCallFunction {
                        index: None,
                        name: call.name.as_str().to_owned(),
                        arguments: call.arguments.clone(),
                    },
                });
            }

            // Ollama takes a tool result as the message body, as text, and has no field for
            // whether the tool failed. Dropping that flag would let a model read a refusal
            // or a missing file as a successful result and carry on from it, so a failure is
            // wrapped in an `error` key instead. A successful result is passed through
            // untouched — including a JSON string, which is sent bare rather than quoted,
            // since a tool that already formatted its output for a reader should not have it
            // re-escaped.
            ContentPart::ToolResult {
                call_id,
                content,
                is_error,
            } if message.role == Role::Tool => {
                wire.content.push_str(&match (is_error, content) {
                    (true, content) => json!({ "error": content }).to_string(),
                    (false, Value::String(text)) => text.clone(),
                    (false, other) => other.to_string(),
                });
                wire.tool_call_id = Some(call_id.clone());
            }

            other => {
                return Err(Error::Unsupported(format!(
                    "the Ollama provider does not support `{other:?}` content parts \
                     on a `{role}` message"
                )));
            }
        }
    }

    Ok(wire)
}

/// Converts one tool call reported by Ollama.
///
/// The `call_id` is what the loop uses to match a result to its call. Ollama supplies an
/// `id`; older servers and some models do not, so an absent one is replaced by a
/// deterministic stand-in built from the call's position. It is unique within a message,
/// which is as far as the correlation has to hold — the result is appended immediately
/// after the call, in the same transcript.
fn convert_tool_call(call: WireToolCall, position: usize) -> ToolCall {
    let index = call.function.index.map_or(position, |index| index as usize);
    ToolCall {
        call_id: match call.id {
            Some(id) if !id.is_empty() => id,
            _ => format!("{}-{index}", call.function.name),
        },
        name: ToolName::new(call.function.name),
        arguments: call.function.arguments,
    }
}

/// Parses one line of an Ollama streaming response into the chunks it carries.
///
/// Returns an empty vector for a line with nothing in it (an empty delta), which the caller
/// should skip rather than yield. A line can carry more than one chunk: Ollama assembles
/// tool calls server-side and emits them complete, several at a time, sometimes alongside
/// text.
pub(crate) fn parse_line(line: &[u8]) -> Result<Vec<CompletionChunk>> {
    let text = std::str::from_utf8(line)
        .map_err(|error| Error::wrap("decoding an Ollama response line", error))?;
    let parsed: ChatResponseLine = serde_json::from_str(text)?;

    if let Some(message) = parsed.error {
        return Err(Error::wrap(
            "Ollama reported an error",
            OllamaApiError(message),
        ));
    }

    if parsed.done {
        return Ok(vec![CompletionChunk::Done {
            finish_reason: map_done_reason(parsed.done_reason.as_deref()),
            usage: extract_usage(&parsed),
        }]);
    }

    let Some(message) = parsed.message else {
        return Ok(Vec::new());
    };

    let mut chunks = Vec::new();
    if !message.content.is_empty() {
        chunks.push(CompletionChunk::Delta(ContentPart::text(message.content)));
    }
    for (position, call) in message.tool_calls.into_iter().enumerate() {
        chunks.push(CompletionChunk::ToolCall(convert_tool_call(call, position)));
    }
    Ok(chunks)
}

/// Converts a non-streaming (`stream: false`) response body.
pub(crate) fn convert_response(parsed: ChatResponseLine) -> Result<CompletionResponse> {
    if let Some(message) = parsed.error {
        return Err(Error::wrap(
            "Ollama reported an error",
            OllamaApiError(message),
        ));
    }

    let usage = extract_usage(&parsed);
    let finish_reason = map_done_reason(parsed.done_reason.as_deref());
    let message = parsed.message.unwrap_or_default();
    let asked_for_tools = !message.tool_calls.is_empty();

    // Ollama sends `content: ""` alongside tool calls; an empty text part next to them is
    // noise. Without them it is the whole answer, empty or not, and dropping it would turn
    // a model that said nothing into a message with no content at all.
    let mut content = Vec::new();
    if !asked_for_tools || !message.content.is_empty() {
        content.push(ContentPart::text(message.content));
    }
    for (position, call) in message.tool_calls.into_iter().enumerate() {
        content.push(ContentPart::ToolCall(convert_tool_call(call, position)));
    }

    Ok(CompletionResponse {
        message: Message {
            role: Role::Assistant,
            content,
            name: None,
        },
        // Ollama reports `done_reason: "stop"` even for a turn that consists entirely of
        // tool calls, so what the message contains is the more truthful answer.
        finish_reason: if asked_for_tools {
            FinishReason::ToolCalls
        } else {
            finish_reason
        },
        usage,
    })
}

fn extract_usage(parsed: &ChatResponseLine) -> Option<Usage> {
    match (parsed.prompt_eval_count, parsed.eval_count) {
        (None, None) => None,
        (input, output) => Some(Usage {
            input_tokens: input.unwrap_or(0),
            output_tokens: output.unwrap_or(0),
        }),
    }
}

fn map_done_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("length") => FinishReason::Length,
        _ => FinishReason::Stop,
    }
}

/// Converts an `/api/tags` response into provider-neutral model descriptors.
pub(crate) fn convert_tags(response: TagsResponse) -> Vec<ModelDescriptor> {
    response
        .models
        .into_iter()
        .map(|model| ModelDescriptor {
            id: ModelId::new(model.name),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: aik_api::model::ModelCapabilities(vec![
                aik_api::model::ModelCapabilities::STREAMING.to_owned(),
            ]),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_only_messages_convert_cleanly() {
        let request = CompletionRequest::new(
            "llama3.2",
            vec![
                Message::text(Role::System, "be terse"),
                Message::text(Role::User, "hello"),
            ],
        );
        let wire = build_chat_request(&request, true).unwrap();
        assert_eq!(wire.model, "llama3.2");
        assert!(wire.stream);
        assert_eq!(wire.messages.len(), 2);
        assert_eq!(wire.messages[0].role, "system");
        assert_eq!(wire.messages[1].content, "hello");
    }

    fn definition() -> ToolDefinition {
        ToolDefinition::new(
            "filesystem.read",
            "Reads a file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )
    }

    #[test]
    fn tool_definitions_become_ollama_function_declarations() {
        let mut request = CompletionRequest::new("llama3.2", vec![]);
        request.tools.push(definition());

        let wire = build_chat_request(&request, false).unwrap();
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "filesystem.read",
                    "description": "Reads a file",
                    "parameters": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                    },
                },
            }]),
        );
    }

    #[test]
    fn a_request_without_tools_omits_the_field_entirely() {
        let request = CompletionRequest::new("llama3.2", vec![]);
        let json = serde_json::to_value(build_chat_request(&request, false).unwrap()).unwrap();
        assert!(json.get("tools").is_none(), "{json}");
    }

    #[test]
    fn an_assistant_turns_tool_calls_are_replayed_with_their_ids() {
        let request = CompletionRequest::new(
            "llama3.2",
            vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall {
                    call_id: "call_1".into(),
                    name: ToolName::new("filesystem.read"),
                    arguments: json!({ "path": "a.txt" }),
                })],
                name: None,
            }],
        );

        let wire = build_chat_request(&request, false).unwrap();
        let json = serde_json::to_value(&wire.messages[0]).unwrap();
        assert_eq!(
            json,
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": { "name": "filesystem.read", "arguments": { "path": "a.txt" } },
                }],
            }),
        );
    }

    #[test]
    fn a_tool_result_becomes_a_tool_message_naming_the_call_it_answers() {
        let request = CompletionRequest::new(
            "llama3.2",
            vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    call_id: "call_1".into(),
                    content: json!({ "bytes": 12 }),
                    is_error: false,
                }],
                name: None,
            }],
        );

        let wire = build_chat_request(&request, false).unwrap();
        assert_eq!(wire.messages[0].role, "tool");
        assert_eq!(wire.messages[0].content, r#"{"bytes":12}"#);
        assert_eq!(wire.messages[0].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn a_failed_tool_result_says_so_on_the_wire() {
        // Ollama has no `is_error` field. Without this the model would read a refusal as a
        // successful result and carry on from it.
        let request = CompletionRequest::new(
            "llama3.2",
            vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    call_id: "call_1".into(),
                    content: json!({ "kind": "permissiondenied", "message": "not allowed" }),
                    is_error: true,
                }],
                name: None,
            }],
        );

        let wire = build_chat_request(&request, false).unwrap();
        let sent: Value = serde_json::from_str(&wire.messages[0].content).unwrap();
        assert_eq!(
            sent,
            json!({ "error": { "kind": "permissiondenied", "message": "not allowed" } }),
        );
    }

    #[test]
    fn a_string_tool_result_is_sent_unquoted() {
        let request = CompletionRequest::new(
            "llama3.2",
            vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    call_id: "call_1".into(),
                    content: json!("hello"),
                    is_error: false,
                }],
                name: None,
            }],
        );

        let wire = build_chat_request(&request, false).unwrap();
        assert_eq!(wire.messages[0].content, "hello");
    }

    #[test]
    fn a_tool_call_attributed_to_anyone_but_the_assistant_is_rejected() {
        // A history in which the *user* appears to have asked for a tool is a forged one:
        // sending it would let a caller put words in the model's mouth about what it
        // already decided to do.
        for role in [Role::User, Role::System, Role::Tool] {
            let request = CompletionRequest::new(
                "llama3.2",
                vec![Message {
                    role,
                    content: vec![ContentPart::ToolCall(ToolCall {
                        call_id: "call_1".into(),
                        name: ToolName::new("filesystem.read"),
                        arguments: json!({}),
                    })],
                    name: None,
                }],
            );
            let error = build_chat_request(&request, false).unwrap_err();
            assert!(matches!(error, Error::Unsupported(_)), "{role:?}: {error}");
        }
    }

    #[test]
    fn a_tool_result_on_anything_but_a_tool_message_is_rejected() {
        for role in [Role::User, Role::System, Role::Assistant] {
            let request = CompletionRequest::new(
                "llama3.2",
                vec![Message {
                    role,
                    content: vec![ContentPart::ToolResult {
                        call_id: "call_1".into(),
                        content: json!("done"),
                        is_error: false,
                    }],
                    name: None,
                }],
            );
            let error = build_chat_request(&request, false).unwrap_err();
            assert!(matches!(error, Error::Unsupported(_)), "{role:?}: {error}");
        }
    }

    #[test]
    fn non_text_content_is_rejected_up_front() {
        let request = CompletionRequest::new(
            "llama3.2",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::Blob {
                    mime_type: "image/png".into(),
                    data: "AA==".into(),
                }],
                name: None,
            }],
        );
        let error = build_chat_request(&request, false).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error}");
    }

    #[test]
    fn parameters_pass_through_as_options_verbatim() {
        let mut request = CompletionRequest::new("llama3.2", vec![]);
        request.parameters = serde_json::json!({ "temperature": 0.2 });
        let wire = build_chat_request(&request, false).unwrap();
        assert_eq!(wire.options, serde_json::json!({ "temperature": 0.2 }));
    }

    fn only_chunk(line: &[u8]) -> CompletionChunk {
        let mut chunks = parse_line(line).unwrap();
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        chunks.remove(0)
    }

    #[test]
    fn empty_deltas_are_skipped() {
        let line = br#"{"message":{"role":"assistant","content":""},"done":false}"#;
        assert!(parse_line(line).unwrap().is_empty());
    }

    #[test]
    fn non_empty_deltas_become_content_chunks() {
        let line = br#"{"message":{"role":"assistant","content":"hi"},"done":false}"#;
        assert_eq!(
            only_chunk(line),
            CompletionChunk::Delta(ContentPart::text("hi"))
        );
    }

    #[test]
    fn streamed_tool_calls_arrive_complete() {
        // Ollama assembles arguments server-side and emits the whole call on one line.
        let line = br#"{"message":{"role":"assistant","content":"","tool_calls":[
            {"id":"call_a","function":{"index":0,"name":"filesystem.read",
             "arguments":{"path":"a.txt"}}}]},"done":false}"#;
        assert_eq!(
            only_chunk(line),
            CompletionChunk::ToolCall(ToolCall {
                call_id: "call_a".into(),
                name: ToolName::new("filesystem.read"),
                arguments: json!({ "path": "a.txt" }),
            }),
        );
    }

    #[test]
    fn a_line_carrying_text_and_two_calls_yields_all_three_chunks() {
        let line = br#"{"message":{"role":"assistant","content":"working","tool_calls":[
            {"id":"a","function":{"index":0,"name":"one","arguments":{}}},
            {"id":"b","function":{"index":1,"name":"two","arguments":{}}}]},"done":false}"#;
        let chunks = parse_line(line).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0],
            CompletionChunk::Delta(ContentPart::text("working"))
        );
        assert!(matches!(&chunks[1], CompletionChunk::ToolCall(call) if call.call_id == "a"));
        assert!(matches!(&chunks[2], CompletionChunk::ToolCall(call) if call.call_id == "b"));
    }

    #[test]
    fn calls_without_a_server_supplied_id_get_distinct_ones() {
        // Older servers and some models omit `id`. The loop still has to be able to match
        // each result to the call it answers.
        let line = br#"{"message":{"role":"assistant","tool_calls":[
            {"function":{"name":"one","arguments":{}}},
            {"function":{"name":"one","arguments":{}}}]},"done":false}"#;
        let chunks = parse_line(line).unwrap();
        let ids: Vec<&str> = chunks
            .iter()
            .filter_map(|chunk| match chunk {
                CompletionChunk::ToolCall(call) => Some(call.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["one-0", "one-1"]);
    }

    #[test]
    fn done_lines_report_usage_and_finish_reason() {
        let line = br#"{"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":7}"#;
        assert_eq!(
            only_chunk(line),
            CompletionChunk::Done {
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 7
                }),
            }
        );
    }

    #[test]
    fn length_done_reason_maps_to_length() {
        let line = br#"{"done":true,"done_reason":"length"}"#;
        assert_eq!(
            only_chunk(line),
            CompletionChunk::Done {
                finish_reason: FinishReason::Length,
                usage: None,
            }
        );
    }

    #[test]
    fn error_lines_surface_as_errors() {
        let line = br#"{"error":"model 'ghost' not found"}"#;
        let error = parse_line(line).unwrap_err();
        assert!(
            error.to_string().contains("Ollama reported an error"),
            "{error}"
        );
    }

    fn response_line(json: &str) -> ChatResponseLine {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn non_streaming_responses_convert_to_a_completion_response() {
        let response = convert_response(response_line(
            r#"{"message":{"role":"assistant","content":"hello there"},
                "done":true,"done_reason":"stop","prompt_eval_count":5,"eval_count":2}"#,
        ))
        .unwrap();

        assert_eq!(
            response.message,
            Message::text(Role::Assistant, "hello there")
        );
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 5,
                output_tokens: 2
            })
        );
    }

    #[test]
    fn a_response_asking_for_tools_finishes_for_that_reason() {
        // Ollama says `done_reason: "stop"` even for a turn that is nothing but tool calls,
        // so the message's own content is what the finish reason has to come from.
        let response = convert_response(response_line(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[
                {"id":"call_a","function":{"index":0,"name":"filesystem.read",
                 "arguments":{"path":"a.txt"}}}]},
                "done":true,"done_reason":"stop"}"#,
        ))
        .unwrap();

        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            response.message.content,
            vec![ContentPart::ToolCall(ToolCall {
                call_id: "call_a".into(),
                name: ToolName::new("filesystem.read"),
                arguments: json!({ "path": "a.txt" }),
            })],
            "the empty text Ollama sends alongside a tool call is not content",
        );
    }

    #[test]
    fn text_alongside_tool_calls_is_kept_ahead_of_them() {
        let response = convert_response(response_line(
            r#"{"message":{"role":"assistant","content":"let me look",
                "tool_calls":[{"id":"a","function":{"name":"one","arguments":{}}}]},
                "done":true,"done_reason":"stop"}"#,
        ))
        .unwrap();

        assert_eq!(response.message.content.len(), 2);
        assert_eq!(
            response.message.content[0],
            ContentPart::text("let me look")
        );
        assert!(matches!(
            response.message.content[1],
            ContentPart::ToolCall(_)
        ));
    }

    #[test]
    fn a_model_that_said_nothing_still_produces_a_text_part() {
        let response = convert_response(response_line(
            r#"{"message":{"role":"assistant","content":""},
                "done":true,"done_reason":"stop"}"#,
        ))
        .unwrap();
        assert_eq!(response.message, Message::text(Role::Assistant, ""));
    }
}
