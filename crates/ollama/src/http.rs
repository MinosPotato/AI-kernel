//! Small helpers around `reqwest` that translate its failure modes into the kernel's
//! [`Error`] type, and say which of them are worth repeating.
//!
//! Classification lives here rather than in whatever calls a provider, because this is the
//! only code that can see a status line or a `reqwest::Error`'s own predicates. See
//! [`aik_api::resilience`] for why a caller cannot work it out for itself, and
//! [`aik_resilience`](https://docs.rs/aik-resilience) for what acts on it.

use std::time::Duration;

use aik_api::resilience::{TransientFailure, transient_status};
use aik_core::{Error, Result};
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;

use crate::protocol::OllamaApiError;

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: String,
}

/// Wraps a transport-level failure with what the caller was trying to do.
///
/// A failure that never reached a status line is marked transient when it is one of the three
/// kinds that say nothing about the request: a connection that could not be made, one that
/// timed out, and a body that stopped arriving. A decode failure is deliberately not among
/// them — a server that answers with something this client cannot parse will answer the same
/// way again, and Ollama runs on the same machine often enough that a tight retry loop against
/// it is a real cost.
pub(crate) fn map_reqwest_error(context: impl Into<String>, error: reqwest::Error) -> Error {
    let context = context.into();
    if error.is_connect() || error.is_timeout() || error.is_body() {
        return TransientFailure::new(error.to_string())
            .caused_by(error)
            .wrapped(context);
    }
    Error::wrap(context, error)
}

/// Reads a `retry-after` header expressed in whole seconds.
///
/// The HTTP-date form is not honoured: reading it needs a clock this layer does not have, and
/// a caller's own backoff is a safe fallback rather than a failure.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Turns a non-2xx response into an [`Error`], extracting Ollama's `{"error": "..."}` body
/// when present so the message reaches the caller instead of a bare status code.
pub(crate) async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let asked_for = retry_after(&response);
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ErrorBody>(&body)
        .map(|parsed| parsed.error)
        .unwrap_or(body);

    let context = format!("Ollama returned HTTP {status}");
    if !transient_status(status.as_u16()) {
        return Err(Error::wrap(context, OllamaApiError(message)));
    }

    let mut failure = TransientFailure::new(message.clone()).caused_by(OllamaApiError(message));
    if let Some(after) = asked_for {
        failure = failure.after(after);
    }
    Err(failure.wrapped(context))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn error_bodies_are_extracted_from_json() {
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "model 'ghost' not found"
            })))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/fail", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(
            error.to_string().contains("Ollama returned HTTP 404"),
            "{error}"
        );
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .to_string()
                .contains("model 'ghost' not found")
        );
    }

    #[tokio::test]
    async fn non_json_error_bodies_are_passed_through_verbatim() {
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/fail", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .to_string()
                .contains("internal error")
        );
    }

    #[tokio::test]
    async fn a_server_side_status_is_marked_worth_repeating() {
        let server = MockServer::start().await;
        Mock::given(path("/busy"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "4")
                    .set_body_json(serde_json::json!({ "error": "model is loading" })),
            )
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/busy", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        let failure = aik_api::resilience::transient_failure(&error).expect("marked transient");
        assert_eq!(failure.retry_after(), Some(Duration::from_secs(4)));
        assert!(
            failure.to_string().contains("model is loading"),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn a_request_side_status_is_terminal() {
        let server = MockServer::start().await;
        Mock::given(path("/gone"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "model 'ghost' not found"
            })))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/gone", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(
            aik_api::resilience::transient_failure(&error).is_none(),
            "asking again for a model that does not exist is asking again for nothing"
        );
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_made_is_worth_repeating() {
        // Port 1 on the loopback interface: nothing listens, and nothing is expected to.
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/api/tags")
            .send()
            .await
            .expect_err("nothing is listening");
        let mapped = map_reqwest_error("listing Ollama models", error);

        assert!(aik_api::resilience::transient_failure(&mapped).is_some());
        assert_eq!(mapped.to_string(), "listing Ollama models");
    }

    #[tokio::test]
    async fn successful_responses_pass_through_unchanged() {
        let server = MockServer::start().await;
        Mock::given(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fine"))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/ok", server.uri())).await.unwrap();
        let response = ensure_success(response).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "fine");
    }
}
