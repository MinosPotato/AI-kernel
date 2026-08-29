//! End-to-end tests against a mocked OpenAI-compatible API.
//!
//! No test here talks to a real service — [`wiremock`] stands in for one, so the suite is
//! deterministic, needs no credential and costs nothing to run. What is being checked is the
//! whole path: the request that goes on the wire, the headers that carry the key, the
//! response that comes back, and what a kernel does when any of it is wrong.

use std::sync::Arc;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, ContentPart, Embedder, FinishReason, Message, ModelId,
    ModelProvider, Role, ToolDefinition,
};
use aik_api::tool::{ToolCall, ToolName};
use aik_core::Error;
use aik_core::clock::{ManualClock, SharedClock, SystemClock, Timestamp};
use aik_core::prelude::*;
use aik_openai::{ApiKey, OpenAiComponent, OpenAiProvider, OpenAiSettings};
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "sk-test-key";

fn settings(server: &MockServer) -> OpenAiSettings {
    OpenAiSettings {
        endpoint: server.uri(),
        ..OpenAiSettings::default()
    }
}

fn provider_for(server: &MockServer) -> OpenAiProvider {
    provider_with_clock(server, Arc::new(SystemClock))
}

fn provider_with_clock(server: &MockServer, clock: SharedClock) -> OpenAiProvider {
    OpenAiProvider::new(
        &settings(server),
        Some(ApiKey::new(KEY, "TEST").unwrap()),
        clock,
    )
    .unwrap()
}

/// Builds an SSE body out of data frames.
fn sse(frames: &[Value]) -> String {
    let mut body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_request() -> CompletionRequest {
    CompletionRequest::new(
        "gpt-4.1-mini",
        vec![
            Message::text(Role::System, "be terse"),
            Message::text(Role::User, "hello"),
        ],
    )
}

fn text_response() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hi" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 9, "completion_tokens": 1, "total_tokens": 10 }
    })
}

async fn mount_completion(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_completion_carries_the_credential_and_the_conversation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .mount(&server)
        .await;

    let response = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(response.message.content[0], ContentPart::text("hi"));
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.usage.unwrap().input_tokens, 9);

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("a JSON body");
    assert_eq!(sent["model"], json!("gpt-4.1-mini"));
    assert_eq!(
        sent["messages"],
        json!([
            { "role": "system", "content": "be terse" },
            { "role": "user", "content": "hello" },
        ])
    );
    assert_eq!(sent["stream"], json!(false));
}

#[tokio::test]
async fn the_key_never_appears_in_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "type": "invalid_request_error", "message": "Incorrect API key" }
        })))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    let text = format!("{error:?} {error}");
    assert!(!text.contains(KEY), "{text}");
    assert!(text.contains("401"), "{text}");
}

#[tokio::test]
async fn a_local_endpoint_may_be_reached_with_no_credential_at_all() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .mount(&server)
        .await;

    // `wiremock` binds loopback, which is the only place this is allowed.
    let settings = OpenAiSettings {
        endpoint: server.uri(),
        api_key_required: false,
        ..OpenAiSettings::default()
    };
    let provider = OpenAiProvider::new(&settings, None, Arc::new(SystemClock)).unwrap();

    provider
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap();

    let sent = &server.received_requests().await.unwrap()[0];
    assert!(
        sent.headers.get("authorization").is_none(),
        "no credential should be sent when none was resolved"
    );
}

#[tokio::test]
async fn a_tool_call_round_trips_through_the_wire_format() {
    let server = MockServer::start().await;
    mount_completion(
        &server,
        json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "filesystem.read",
                            "arguments": "{\"path\": \"a.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
    )
    .await;

    let mut request = text_request();
    request.tools.push(ToolDefinition::new(
        "filesystem.read",
        "Reads a file",
        json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
    ));

    let response = provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    match &response.message.content[0] {
        ContentPart::ToolCall(call) => {
            assert_eq!(call.call_id, "call_1");
            assert_eq!(call.name.as_str(), "filesystem.read");
            assert_eq!(call.arguments, json!({ "path": "a.txt" }));
        }
        other => panic!("expected a tool call, got {other:?}"),
    }

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("a JSON body");
    assert_eq!(sent["tools"][0]["type"], json!("function"));
    assert_eq!(
        sent["tools"][0]["function"]["name"],
        json!("filesystem.read")
    );
    // The authorization metadata a `ToolSpec` carries must never reach the service.
    assert!(!sent.to_string().contains("required_permissions"), "{sent}");
}

#[tokio::test]
async fn a_tool_result_is_replayed_as_its_own_message() {
    let server = MockServer::start().await;
    mount_completion(&server, text_response()).await;

    let request = CompletionRequest::new(
        "gpt-4.1-mini",
        vec![
            Message::text(Role::User, "read a.txt"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall {
                    call_id: "call_1".to_owned(),
                    name: ToolName::new("filesystem.read"),
                    arguments: json!({ "path": "a.txt" }),
                })],
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
        ],
    );

    provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap();

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("a JSON body");
    assert_eq!(
        sent["messages"],
        json!([
            { "role": "user", "content": "read a.txt" },
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "filesystem.read", "arguments": "{\"path\":\"a.txt\"}" }
                }]
            },
            { "role": "tool", "content": "hello", "tool_call_id": "call_1" },
        ])
    );
}

#[tokio::test]
async fn a_failed_tool_result_still_reads_as_a_failure_on_the_wire() {
    let server = MockServer::start().await;
    mount_completion(&server, text_response()).await;

    let request = CompletionRequest::new(
        "gpt-4.1-mini",
        vec![Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                call_id: "call_1".to_owned(),
                content: json!({ "denied": "policy" }),
                is_error: true,
            }],
            name: None,
        }],
    );

    provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap();

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("a JSON body");
    let content = sent["messages"][0]["content"].as_str().expect("a string");
    assert!(content.contains("is_error"), "{content}");
    assert!(content.contains("policy"), "{content}");
}

#[tokio::test]
async fn a_stream_yields_text_then_a_tool_call_then_done() {
    let server = MockServer::start().await;
    let body = sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "look" } }] }),
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "filesystem.read", "arguments": "{\"path\":" }
        }] } }] }),
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0, "function": { "arguments": "\"a.txt\"}" }
        }] } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        json!({ "choices": [], "usage": { "prompt_tokens": 20, "completion_tokens": 5 } }),
    ]);

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
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

    assert_eq!(chunks.len(), 3);
    assert_eq!(
        chunks[0].as_ref().unwrap(),
        &CompletionChunk::Delta(ContentPart::text("look"))
    );
    match chunks[1].as_ref().unwrap() {
        CompletionChunk::ToolCall(call) => {
            assert_eq!(call.arguments, json!({ "path": "a.txt" }));
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
    match chunks[2].as_ref().unwrap() {
        CompletionChunk::Done {
            finish_reason,
            usage,
        } => {
            assert_eq!(*finish_reason, FinishReason::ToolCalls);
            assert_eq!(usage.unwrap().output_tokens, 5);
        }
        other => panic!("expected the end, got {other:?}"),
    }

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("a JSON body");
    assert_eq!(sent["stream"], json!(true));
    assert_eq!(sent["stream_options"], json!({ "include_usage": true }));
}

#[tokio::test]
async fn a_stream_that_fails_before_it_opens_never_becomes_a_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": { "type": "rate_limit_exceeded", "message": "slow down" }
        })))
        .mount(&server)
        .await;

    let Err(error) = provider_for(&server)
        .stream(text_request(), &ExecutionContext::new())
        .await
    else {
        panic!("the status is reported before any chunk");
    };
    assert!(format!("{error}").contains("429"), "{error}");
    assert!(
        aik_api::resilience::transient_failure(&error).is_some(),
        "a rate limit is worth repeating"
    );
}

#[tokio::test]
async fn models_are_listed_without_capabilities_being_invented() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                { "id": "gpt-4.1-mini", "object": "model" },
                { "id": "text-embedding-3-small", "object": "model" }
            ]
        })))
        .mount(&server)
        .await;

    let models = provider_for(&server).models().await.unwrap();

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id.as_str(), "gpt-4.1-mini");
    // The listing says nothing about what a model can do, and this provider says nothing
    // either.
    assert!(models[0].capabilities.0.is_empty());
}

#[tokio::test]
async fn embeddings_come_back_in_input_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 1, "embedding": [0.3, 0.4] },
                { "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }
            ],
            "model": "text-embedding-3-small"
        })))
        .mount(&server)
        .await;

    let vectors = provider_for(&server)
        .embed(
            &ModelId::new("text-embedding-3-small"),
            &["first".to_owned(), "second".to_owned()],
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert_eq!(vectors, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);

    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body)
        .expect("a JSON body");
    assert_eq!(sent["input"], json!(["first", "second"]));
    assert_eq!(sent["encoding_format"], json!("float"));
}

#[tokio::test]
async fn an_empty_batch_never_reaches_the_server() {
    let server = MockServer::start().await;

    let vectors = provider_for(&server)
        .embed(
            &ModelId::new("text-embedding-3-small"),
            &[],
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert!(vectors.is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_short_embedding_batch_is_refused_rather_than_misaligned() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "index": 0, "embedding": [0.1, 0.2] }]
        })))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .embed(
            &ModelId::new("text-embedding-3-small"),
            &["one".to_owned(), "two".to_owned()],
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();

    assert!(format!("{error}").contains("OpenAI"), "{error}");
}

#[tokio::test]
async fn a_cancelled_request_is_abandoned() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(text_response()),
        )
        .mount(&server)
        .await;

    let cx = ExecutionContext::new();
    let cancellation = cx.cancellation.clone();
    let handle = tokio::spawn({
        let provider = provider_for(&server);
        async move { provider.complete(text_request(), &cx).await }
    });
    cancellation.cancel();

    let error = handle.await.unwrap().unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error:?}");
}

#[tokio::test(start_paused = true)]
async fn a_request_that_outlives_its_deadline_is_a_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(600))
                .set_body_json(text_response()),
        )
        .mount(&server)
        .await;

    let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let provider = provider_with_clock(&server, clock);
    let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(50));

    let error = provider
        .complete(text_request(), &cx)
        .await
        .expect_err("the deadline passes first");
    assert!(matches!(error, Error::Timeout(_)), "{error:?}");
}

#[tokio::test]
async fn a_redirect_never_carries_the_credential_onward() {
    // A 3xx would otherwise send the key to whatever host the response named.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", "https://example.invalid/v1"),
        )
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert!(format!("{error}").contains("307"), "{error}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_component_without_a_key_refuses_to_start() {
    // The failure belongs at startup, not on the first turn a person types.
    let kernel = Kernel::builder()
        .config(Config::from_value(json!({
            "components": {
                "model": { "openai": { "api_key_env": "AIK_TEST_ABSENT_KEY_VARIABLE" } }
            }
        })))
        .component(OpenAiComponent::new())
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();
    assert!(format!("{error}").contains("model.openai"), "{error}");
    assert!(
        chain_mentions(&error, "AIK_TEST_ABSENT_KEY_VARIABLE"),
        "the cause should name the variable it consulted: {error}"
    );
}

#[tokio::test]
async fn a_component_registers_both_a_provider_and_an_embedder() {
    let kernel = Kernel::builder()
        .config(Config::from_value(json!({
            "components": { "model": { "openai": { "endpoint": "https://api.openai.com/v1" } } }
        })))
        .component(OpenAiComponent::new().with_api_key(ApiKey::new(KEY, "TEST").unwrap()))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    assert!(kernel.context().service::<dyn ModelProvider>().is_ok());
    assert!(kernel.context().service::<dyn Embedder>().is_ok());

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_configuration_carrying_a_key_stops_the_deployment() {
    let kernel = Kernel::builder()
        .config(Config::from_value(json!({
            "components": { "model": { "openai": { "api_key": "sk-inline-secret" } } }
        })))
        .component(OpenAiComponent::new())
        .build()
        .unwrap();

    let error = kernel.start().await.unwrap_err();

    // The failure names the component, and the cause explains where a key belongs instead.
    assert!(format!("{error}").contains("model.openai"), "{error}");
    assert!(
        chain_mentions(&error, "api_key_file"),
        "the cause should say where a key belongs: {error}"
    );
    assert!(
        !chain_mentions(&error, "sk-inline-secret"),
        "the key must never appear in a message: {error}"
    );
}

#[tokio::test]
async fn a_body_the_provider_cannot_parse_is_an_error_not_a_silent_empty_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>gateway</html>"))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap_err();
    assert!(format!("{error}").contains("decoding"), "{error}");
}

#[tokio::test]
async fn a_response_that_reports_no_choices_is_an_error() {
    let server = MockServer::start().await;
    mount_completion(&server, json!({ "choices": [] })).await;

    let error = provider_for(&server)
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap_err();
    assert!(chain_mentions(&error, "no choices"), "{error}");
}

/// Whether an error, or anything it was caused by, mentions `needle`.
fn chain_mentions(error: &Error, needle: &str) -> bool {
    if error.to_string().contains(needle) {
        return true;
    }
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(error) = cause {
        if error.to_string().contains(needle) {
            return true;
        }
        cause = std::error::Error::source(error);
    }
    false
}

/// Reads the one request a mock server received.
fn only_request(requests: &[Request]) -> &Request {
    assert_eq!(requests.len(), 1, "expected exactly one request");
    &requests[0]
}

#[tokio::test]
async fn the_account_headers_are_sent_when_configured() {
    let server = MockServer::start().await;
    mount_completion(&server, text_response()).await;

    let settings = OpenAiSettings {
        endpoint: server.uri(),
        organization: Some("org-a".to_owned()),
        project: Some("proj-b".to_owned()),
        ..OpenAiSettings::default()
    };
    let provider = OpenAiProvider::new(
        &settings,
        Some(ApiKey::new(KEY, "TEST").unwrap()),
        Arc::new(SystemClock),
    )
    .unwrap();

    provider
        .complete(text_request(), &ExecutionContext::new())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let sent = only_request(&requests);
    assert_eq!(sent.headers["openai-organization"], "org-a");
    assert_eq!(sent.headers["openai-project"], "proj-b");
}

#[tokio::test]
async fn a_provider_built_around_settings_that_never_saw_read_still_refuses_cleartext() {
    // `OpenAiSettings::read` is not the only way here. Binding a credential to a destination
    // is the moment the transport check has to hold, however the settings were made.
    let error = OpenAiProvider::new(
        &OpenAiSettings {
            endpoint: "http://example.invalid/v1".to_owned(),
            ..OpenAiSettings::default()
        },
        Some(ApiKey::new(KEY, "TEST").unwrap()),
        Arc::new(SystemClock),
    )
    .expect_err("plain HTTP off this machine carries the key in the clear");

    assert!(format!("{error}").contains("loopback"), "{error}");
}
