//! End-to-end tests against a mocked Anthropic API.
//!
//! No test here talks to the real service — [`wiremock`] stands in for it, so the suite is
//! deterministic, needs no credential and costs nothing to run. What is being checked is the
//! whole path: the request that goes on the wire, the headers that carry the key, the
//! response that comes back, and what a kernel does when any of it is wrong.

use std::sync::Arc;
use std::time::Duration;

use aik_anthropic::{AnthropicComponent, AnthropicProvider, AnthropicSettings, ApiKey};
use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, ContentPart, FinishReason, Message, ModelId, ModelProvider,
    Role, ToolDefinition,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::Error;
use aik_core::clock::{ManualClock, SharedClock, SystemClock, Timestamp};
use aik_core::prelude::*;
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::matchers::{body_json_schema, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "sk-ant-test-key";

fn settings(server: &MockServer) -> AnthropicSettings {
    AnthropicSettings {
        endpoint: server.uri(),
        max_retries: 0,
        ..AnthropicSettings::default()
    }
}

fn provider_for(server: &MockServer) -> AnthropicProvider {
    provider_with_clock(server, Arc::new(SystemClock))
}

fn provider_with_clock(server: &MockServer, clock: SharedClock) -> AnthropicProvider {
    AnthropicProvider::new(&settings(server), ApiKey::new(KEY, "TEST").unwrap(), clock).unwrap()
}

/// Builds an SSE body out of event names and payloads.
fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
}

fn text_request() -> CompletionRequest {
    CompletionRequest::new(
        "claude-sonnet-4-5",
        vec![
            Message::text(Role::System, "be terse"),
            Message::text(Role::User, "hello"),
        ],
    )
}

// ---------------------------------------------------------------------------
// The credential on the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_request_carries_the_key_and_the_api_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{ "type": "text", "text": "hi" }],
            "stop_reason": "end_turn"
        })))
        .expect(1)
        .mount(&server)
        .await;

    provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap();

    server.verify().await;
}

#[tokio::test]
async fn a_redirect_is_not_followed_so_the_key_goes_nowhere_else() {
    // A 3xx naming another host would otherwise re-send `x-api-key` to it.
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "content": [] })))
        .expect(0)
        .mount(&elsewhere)
        .await;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("{}/v1/messages", elsewhere.uri()).as_str(),
        ))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert!(format!("{error}").contains("HTTP 307"), "{error}");
    elsewhere.verify().await;
}

// ---------------------------------------------------------------------------
// models()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn models_lists_what_the_service_reports() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("limit", "1000"))
        .and(header("x-api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "type": "model", "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5" },
                { "type": "model", "id": "claude-opus-4-1", "display_name": "Claude Opus 4.1" }
            ],
            "has_more": false
        })))
        .mount(&server)
        .await;

    let models = provider_for(&server).models().await.unwrap();

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, ModelId::new("claude-sonnet-4-5"));
    assert_eq!(models[1].id, ModelId::new("claude-opus-4-1"));
}

#[tokio::test]
async fn an_authentication_failure_says_what_the_service_said() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid x-api-key" }
        })))
        .mount(&server)
        .await;

    let error = provider_for(&server).models().await.unwrap_err();

    assert!(format!("{error}").contains("HTTP 401"), "{error}");
    let source = std::error::Error::source(&error).unwrap().to_string();
    assert!(source.contains("invalid x-api-key"), "{source}");
    // And nothing in the report quotes the key that was rejected.
    assert!(!format!("{error:?}").contains(KEY));
}

// ---------------------------------------------------------------------------
// complete()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_hoists_the_system_prompt_and_parses_the_reply() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_json_schema::<Value>)
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["model"], json!("claude-sonnet-4-5"));
            assert_eq!(body["system"], json!("be terse"));
            assert_eq!(body["stream"], json!(false));
            assert_eq!(body["max_tokens"], json!(4096));
            assert_eq!(
                body["messages"],
                json!([
                    { "role": "user", "content": [{ "type": "text", "text": "hello" }] }
                ])
            );
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_1",
                "content": [{ "type": "text", "text": "hi" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 5, "output_tokens": 2 }
            }))
        })
        .mount(&server)
        .await;

    let response = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(response.message.role, Role::Assistant);
    assert_eq!(response.message.content[0], ContentPart::text("hi"));
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.usage.unwrap().input_tokens, 5);
}

#[tokio::test]
async fn a_tool_conversation_survives_a_round_trip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            // The call and its result reached the wire in the shapes the API expects.
            assert_eq!(body["messages"][1]["role"], json!("assistant"));
            assert_eq!(body["messages"][1]["content"][0]["type"], json!("tool_use"));
            assert_eq!(body["messages"][2]["role"], json!("user"));
            assert_eq!(
                body["messages"][2]["content"][0],
                json!({
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "hello"
                })
            );
            assert_eq!(body["tools"][0]["name"], json!("filesystem.read"));
            ResponseTemplate::new(200).set_body_json(json!({
                "content": [{ "type": "text", "text": "it says hello" }],
                "stop_reason": "end_turn"
            }))
        })
        .mount(&server)
        .await;

    let mut request = CompletionRequest::new(
        "claude-sonnet-4-5",
        vec![
            Message::text(Role::User, "read a.txt"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall {
                    call_id: "toolu_1".to_owned(),
                    name: ToolName::new("filesystem.read"),
                    arguments: json!({ "path": "a.txt" }),
                })],
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
        ],
    );
    request.tools.push(ToolDefinition::new(
        "filesystem.read",
        "Reads a file",
        json!({ "type": "object" }),
    ));

    let response = provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(
        response.message.content[0],
        ContentPart::text("it says hello")
    );
}

#[tokio::test]
async fn a_cancelled_request_reports_cancellation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(json!({ "content": [] })),
        )
        .mount(&server)
        .await;

    let cx = ExecutionContext::new();
    let token = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });

    let error = provider_for(&server)
        .complete(text_request(), &cx)
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Cancelled), "{error}");
}

#[tokio::test]
async fn the_configured_timeout_bounds_a_slow_service() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(json!({ "content": [] })),
        )
        .mount(&server)
        .await;

    let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let provider = AnthropicProvider::new(
        &AnthropicSettings {
            endpoint: server.uri(),
            request_timeout_ms: 50,
            max_retries: 0,
            ..AnthropicSettings::default()
        },
        ApiKey::new(KEY, "TEST").unwrap(),
        clock,
    )
    .unwrap();

    let error = provider
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Timeout(_)), "{error}");
}

// ---------------------------------------------------------------------------
// stream()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_yields_deltas_then_done() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "message_start",
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 4, "output_tokens": 0 } } }),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "hel" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |request: &Request| {
            let sent: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(sent["stream"], json!(true));
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.clone())
        })
        .mount(&server)
        .await;

    let chunks: Vec<_> = provider_for(&server)
        .stream(text_request(), &ExecutionContext::new())
        .await
        .unwrap()
        .collect()
        .await;

    assert_eq!(chunks.len(), 3);
    assert_eq!(
        chunks[0].as_ref().unwrap(),
        &CompletionChunk::Delta(ContentPart::text("hel"))
    );
    match chunks[2].as_ref().unwrap() {
        CompletionChunk::Done {
            finish_reason,
            usage,
        } => {
            assert_eq!(*finish_reason, FinishReason::Stop);
            assert_eq!(usage.unwrap().output_tokens, 2);
        }
        other => panic!("expected done, got {other:?}"),
    }
}

#[tokio::test]
async fn a_streamed_tool_call_arrives_whole() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_9", "name": "filesystem.read", "input": {} } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\"" } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": ":\"a.txt\"}" } }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let chunks: Vec<_> = provider_for(&server)
        .stream(text_request(), &ExecutionContext::new())
        .await
        .unwrap()
        .collect()
        .await;

    match chunks[0].as_ref().unwrap() {
        CompletionChunk::ToolCall(call) => {
            assert_eq!(call.call_id, "toolu_9");
            assert_eq!(call.arguments, json!({ "path": "a.txt" }));
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
    assert!(matches!(
        chunks[1].as_ref().unwrap(),
        CompletionChunk::Done {
            finish_reason: FinishReason::ToolCalls,
            ..
        }
    ));
}

#[tokio::test]
async fn a_stream_that_fails_to_open_reports_the_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": "model: unknown" }
        })))
        .mount(&server)
        .await;

    let outcome = provider_for(&server)
        .stream(text_request(), &ExecutionContext::new())
        .await;
    let error = match outcome {
        Ok(_) => panic!("a 400 should not open a stream"),
        Err(error) => error,
    };

    let source = std::error::Error::source(&error).unwrap().to_string();
    assert!(source.contains("model: unknown"), "{source}");
}

// ---------------------------------------------------------------------------
// As a kernel component
// ---------------------------------------------------------------------------

/// A key file only its owner can read, as the component requires.
fn key_file(directory: &std::path::Path) -> std::path::PathBuf {
    let path = directory.join("key");
    std::fs::write(&path, KEY).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

/// The component's own section of the configuration tree.
///
/// Nested rather than written as one dotted key, because that is how the kernel reads
/// `components.<id>` when the id itself contains dots.
fn config(server: &MockServer, extra: Value) -> Config {
    let mut section = json!({ "endpoint": server.uri() });
    for (key, value) in extra.as_object().unwrap() {
        section[key] = value.clone();
    }
    Config::from_value(json!({ "components": { "model": { "anthropic": section } } }))
}

#[tokio::test]
async fn a_kernel_resolves_the_provider_and_answers_a_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{ "type": "text", "text": "answered" }],
            "stop_reason": "end_turn"
        })))
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().unwrap();
    let kernel = Kernel::builder()
        .config(config(
            &server,
            json!({ "api_key_file": key_file(directory.path()) }),
        ))
        .component(AnthropicComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let provider = kernel.context().service::<dyn ModelProvider>().unwrap();
    let response = provider
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(response.message.content[0], ContentPart::text("answered"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deployment_with_no_key_does_not_start() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();

    let kernel = Kernel::builder()
        .config(config(
            &server,
            json!({ "api_key_file": directory.path().join("absent") }),
        ))
        .component(AnthropicComponent::new())
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();
    assert!(format!("{error}").contains("model.anthropic"), "{error}");
}

#[tokio::test]
async fn a_key_written_into_the_configuration_stops_the_kernel() {
    let server = MockServer::start().await;
    let kernel = Kernel::builder()
        .config(config(&server, json!({ "api_key": KEY })))
        .component(AnthropicComponent::new())
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();

    // The failure names the component, and the cause explains where a key belongs instead.
    assert!(format!("{error}").contains("model.anthropic"), "{error}");
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&error);
    let mut explained = false;
    while let Some(error) = cause {
        explained |= error.to_string().contains("api_key_file");
        assert!(!error.to_string().contains(KEY), "{error}");
        cause = std::error::Error::source(error);
    }
    assert!(
        explained,
        "the cause should say where a key belongs: {error}"
    );
}

#[tokio::test]
async fn a_plain_http_endpoint_that_is_not_loopback_stops_the_kernel() {
    let directory = tempfile::tempdir().unwrap();
    let kernel = Kernel::builder()
        .config(Config::from_value(json!({
            "components": {
                "model": {
                    "anthropic": {
                        "endpoint": "http://api.example.invalid",
                        "api_key_file": key_file(directory.path()),
                    }
                }
            }
        })))
        .component(AnthropicComponent::new())
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();
    assert!(format!("{error}").contains("model.anthropic"), "{error}");
}
