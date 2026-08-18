//! End-to-end tests against a mocked Ollama server.
//!
//! No test here talks to a real Ollama instance — [`wiremock`] stands in for it, so the
//! suite is deterministic and runs in CI with no external dependency. Real-server behaviour
//! is exercised by the `chat` example instead, which fails gracefully when Ollama is not
//! running.

use std::sync::Arc;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, ContentPart, FinishReason, Message, ModelId, ModelProvider,
    Role,
};
use aik_core::Error;
use aik_core::clock::{ManualClock, SharedClock, SystemClock, Timestamp};
use aik_core::prelude::*;
use aik_ollama::{OllamaComponent, OllamaProvider, OllamaSettings};
use futures::StreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider_for(server: &MockServer) -> OllamaProvider {
    provider_with_clock(server, Arc::new(SystemClock))
}

fn provider_with_clock(server: &MockServer, clock: SharedClock) -> OllamaProvider {
    OllamaProvider::new(
        OllamaSettings {
            endpoint: server.uri(),
            request_timeout_ms: 60_000,
        },
        clock,
    )
}

// ---------------------------------------------------------------------------
// models()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn models_lists_what_the_server_reports() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{ "name": "llama3.2:latest" }, { "name": "mistral:latest" }]
        })))
        .mount(&server)
        .await;

    let models = provider_for(&server).models().await.unwrap();

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, ModelId::new("llama3.2:latest"));
    assert_eq!(models[1].id, ModelId::new("mistral:latest"));
}

// ---------------------------------------------------------------------------
// complete()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_sends_a_non_streaming_request_and_parses_the_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": { "role": "assistant", "content": "hello there" },
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 4,
            "eval_count": 2
        })))
        .mount(&server)
        .await;

    let request = CompletionRequest::new("llama3.2", vec![Message::text(Role::User, "hi")]);
    let response = provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(
        response.message,
        Message::text(Role::Assistant, "hello there")
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.usage.unwrap().input_tokens, 4);
}

#[tokio::test]
async fn the_request_body_matches_ollamas_chat_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(wiremock::matchers::body_json(json!({
            "model": "llama3.2",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" }
            ],
            "stream": false,
            "options": { "temperature": 0.1 }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": { "role": "assistant", "content": "ok" },
            "done": true
        })))
        .mount(&server)
        .await;

    let mut request = CompletionRequest::new(
        "llama3.2",
        vec![
            Message::text(Role::System, "be terse"),
            Message::text(Role::User, "hi"),
        ],
    );
    request.parameters = json!({ "temperature": 0.1 });

    provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap();
    // wiremock asserts the matcher above was actually hit when the server is dropped only
    // if `.expect(..)` was set; the request succeeding at all already proves the shape
    // matched, since an unmatched request gets a 404 from wiremock's default response.
}

#[tokio::test]
async fn complete_surfaces_a_404_as_an_error_carrying_ollamas_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "model 'ghost' not found, try pulling it first"
        })))
        .mount(&server)
        .await;

    let request = CompletionRequest::new("ghost", vec![Message::text(Role::User, "hi")]);
    let error = provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("Ollama returned HTTP 404"),
        "{error}"
    );
    assert!(
        std::error::Error::source(&error)
            .unwrap()
            .to_string()
            .contains("model 'ghost' not found"),
        "{error}"
    );
}

#[tokio::test]
async fn requesting_tools_fails_before_any_request_is_sent() {
    // No mock is registered, so if a request were sent it would 404 from wiremock's
    // default handler and this test would fail for the wrong reason.
    let server = MockServer::start().await;
    let mut request = CompletionRequest::new("llama3.2", vec![Message::text(Role::User, "hi")]);
    request.tools.push(aik_api::tool::ToolName::new("fs.read"));

    let error = provider_for(&server)
        .complete(request, &ExecutionContext::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error}");
}

// ---------------------------------------------------------------------------
// stream()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_yields_deltas_in_order_then_a_done_chunk() {
    let server = MockServer::start().await;
    let body = concat!(
        r#"{"message":{"role":"assistant","content":"hel"},"done":false}"#,
        "\n",
        r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#,
        "\n",
        r#"{"done":true,"done_reason":"stop","prompt_eval_count":1,"eval_count":2}"#,
        "\n",
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson"))
        .mount(&server)
        .await;

    let request = CompletionRequest::new("llama3.2", vec![Message::text(Role::User, "hi")]);
    let mut chunks = provider_for(&server)
        .stream(request, &ExecutionContext::new())
        .await
        .unwrap();

    let first = chunks.next().await.unwrap().unwrap();
    assert_eq!(first, CompletionChunk::Delta(ContentPart::text("hel")));

    let second = chunks.next().await.unwrap().unwrap();
    assert_eq!(second, CompletionChunk::Delta(ContentPart::text("lo")));

    let third = chunks.next().await.unwrap().unwrap();
    assert_eq!(
        third,
        CompletionChunk::Done {
            finish_reason: FinishReason::Stop,
            usage: Some(aik_api::model::Usage {
                input_tokens: 1,
                output_tokens: 2
            }),
        }
    );

    assert!(chunks.next().await.is_none());
}

// ---------------------------------------------------------------------------
// Cancellation and timeouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_while_a_request_is_in_flight_returns_promptly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)).set_body_json(
            json!({ "message": { "role": "assistant", "content": "too late" }, "done": true }),
        ))
        .mount(&server)
        .await;

    let cx = ExecutionContext::new();
    let cancellation = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
    });

    let request = CompletionRequest::new("llama3.2", vec![Message::text(Role::User, "hi")]);
    let started = tokio::time::Instant::now();
    let result = provider_for(&server).complete(request, &cx).await;

    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation should not wait for the mocked 30s delay"
    );
}

#[tokio::test]
async fn a_configured_timeout_shorter_than_the_response_fails_with_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(
                    json!({ "message": { "role": "assistant", "content": "x" }, "done": true }),
                ),
        )
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(
        OllamaSettings {
            endpoint: server.uri(),
            request_timeout_ms: 50,
        },
        Arc::new(SystemClock),
    );

    let request = CompletionRequest::new("llama3.2", vec![Message::text(Role::User, "hi")]);
    let started = tokio::time::Instant::now();
    let result = provider.complete(request, &ExecutionContext::new()).await;

    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn an_execution_context_deadline_shorter_than_the_default_still_times_out() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(
                    json!({ "message": { "role": "assistant", "content": "x" }, "done": true }),
                ),
        )
        .mount(&server)
        .await;

    // A ManualClock frozen at zero, with a deadline 50ms out: the effective wait is exactly
    // that 50ms, regardless of wall-clock time, proving the deadline (not the default
    // timeout) is what triggered it.
    let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let provider = provider_with_clock(&server, clock);

    let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(50));
    let request = CompletionRequest::new("llama3.2", vec![Message::text(Role::User, "hi")]);
    let started = tokio::time::Instant::now();
    let result = provider.complete(request, &cx).await;

    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// As a kernel component
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_component_registers_a_provider_resolvable_through_the_registry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{ "name": "llama3.2:latest" }]
        })))
        .mount(&server)
        .await;

    let kernel = Kernel::builder()
        .config(
            Config::builder()
                // `components.<id>` is a dotted config path, so a dotted component id
                // (see DEFAULT_COMPONENT_ID) needs a matching *nested* JSON object here,
                // not a single flat key containing literal dots.
                .layer(json!({
                    "components": { "model": { "ollama": { "endpoint": server.uri() } } }
                }))
                .build(),
        )
        .component(OllamaComponent::new())
        .build()
        .unwrap();

    kernel.start().await.unwrap();

    // The consumer resolves the capability, never `aik_ollama` itself.
    let provider = kernel
        .context()
        .service::<dyn ModelProvider>()
        .expect("OllamaComponent should have registered the default ModelProvider");
    let models = provider.models().await.unwrap();
    assert_eq!(models[0].id, ModelId::new("llama3.2:latest"));

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_named_provider_does_not_disturb_the_default() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;
    for server in [&primary, &secondary] {
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "models": [] })))
            .mount(server)
            .await;
    }

    let kernel = Kernel::builder()
        .config(
            Config::builder()
                .layer(json!({
                    "components": {
                        "model": {
                            "ollama": { "endpoint": primary.uri() },
                            "ollama-secondary": { "endpoint": secondary.uri() }
                        }
                    }
                }))
                .build(),
        )
        .component(OllamaComponent::new())
        .component(
            OllamaComponent::new()
                .with_id("model.ollama-secondary")
                .as_default(false),
        )
        .build()
        .unwrap();

    kernel.start().await.unwrap();
    let ctx = kernel.context();

    // The default still resolves unambiguously...
    ctx.service::<dyn ModelProvider>().unwrap();
    // ...and the second one is reachable by name.
    ctx.service_named::<dyn ModelProvider>(&ComponentId::new("model.ollama-secondary"))
        .unwrap();
    assert_eq!(ctx.registry().list::<dyn ModelProvider>().len(), 2);

    kernel.shutdown().await.unwrap();
}
