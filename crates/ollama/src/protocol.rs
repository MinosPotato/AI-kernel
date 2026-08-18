//! The Ollama wire protocol and its translation to and from the kernel's model contract.
//!
//! Nothing in this module is public outside the crate: it is the seam where Ollama's JSON
//! shapes meet [`aik_api::model`], and no other crate should ever need to know it exists.

use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelDescriptor, ModelId, Role, Usage,
};
use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wraps an error message returned by the Ollama server, e.g. "model not found".
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct OllamaApiError(pub(crate) String);

#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<WireMessage>,
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub(crate) options: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireMessage {
    pub(crate) role: String,
    pub(crate) content: String,
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
/// Rejects tool calls and non-text content up front, with a clear
/// [`Error::Unsupported`], rather than silently dropping data the caller expected to be
/// sent.
pub(crate) fn build_chat_request(request: &CompletionRequest, stream: bool) -> Result<ChatRequest> {
    if !request.tools.is_empty() {
        return Err(Error::Unsupported(
            "the Ollama provider does not support tool calling yet".to_owned(),
        ));
    }

    let messages = request
        .messages
        .iter()
        .map(convert_message)
        .collect::<Result<Vec<_>>>()?;

    Ok(ChatRequest {
        model: request.model.as_str().to_owned(),
        messages,
        stream,
        options: request.parameters.clone(),
    })
}

fn convert_message(message: &Message) -> Result<WireMessage> {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    let mut content = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => content.push_str(text),
            other => {
                return Err(Error::Unsupported(format!(
                    "the Ollama provider does not support `{other:?}` content parts yet"
                )));
            }
        }
    }

    Ok(WireMessage {
        role: role.to_owned(),
        content,
    })
}

/// Parses one line of an Ollama streaming response.
///
/// Returns `Ok(None)` for a line that carries no user-visible content (an empty delta),
/// which the caller should skip rather than yield.
pub(crate) fn parse_line(line: &[u8]) -> Result<Option<CompletionChunk>> {
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
        return Ok(Some(CompletionChunk::Done {
            finish_reason: map_done_reason(parsed.done_reason.as_deref()),
            usage: extract_usage(&parsed),
        }));
    }

    let content = parsed
        .message
        .map(|message| message.content)
        .unwrap_or_default();
    if content.is_empty() {
        return Ok(None);
    }
    Ok(Some(CompletionChunk::Delta(ContentPart::text(content))))
}

/// Converts a non-streaming (`stream: false`) response body.
pub(crate) fn convert_response(parsed: ChatResponseLine) -> Result<CompletionResponse> {
    if let Some(message) = parsed.error {
        return Err(Error::wrap(
            "Ollama reported an error",
            OllamaApiError(message),
        ));
    }

    let content = parsed
        .message
        .as_ref()
        .map(|message| message.content.clone())
        .unwrap_or_default();

    Ok(CompletionResponse {
        message: Message::text(Role::Assistant, content),
        finish_reason: map_done_reason(parsed.done_reason.as_deref()),
        usage: extract_usage(&parsed),
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
    use aik_api::tool::ToolName;

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

    #[test]
    fn requested_tools_are_rejected_up_front() {
        let mut request = CompletionRequest::new("llama3.2", vec![]);
        request.tools.push(ToolName::new("fs.read"));
        let error = build_chat_request(&request, false).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error}");
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

    #[test]
    fn empty_deltas_are_skipped() {
        let line = br#"{"message":{"role":"assistant","content":""},"done":false}"#;
        assert!(parse_line(line).unwrap().is_none());
    }

    #[test]
    fn non_empty_deltas_become_content_chunks() {
        let line = br#"{"message":{"role":"assistant","content":"hi"},"done":false}"#;
        let chunk = parse_line(line).unwrap().unwrap();
        assert_eq!(chunk, CompletionChunk::Delta(ContentPart::text("hi")));
    }

    #[test]
    fn done_lines_report_usage_and_finish_reason() {
        let line = br#"{"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":7}"#;
        let chunk = parse_line(line).unwrap().unwrap();
        assert_eq!(
            chunk,
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
        let chunk = parse_line(line).unwrap().unwrap();
        assert_eq!(
            chunk,
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

    #[test]
    fn non_streaming_responses_convert_to_a_completion_response() {
        let parsed = ChatResponseLine {
            message: Some(WireResponseMessage {
                content: "hello there".into(),
            }),
            done: true,
            done_reason: Some("stop".into()),
            prompt_eval_count: Some(5),
            eval_count: Some(2),
            error: None,
        };
        let response = convert_response(parsed).unwrap();
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
}
