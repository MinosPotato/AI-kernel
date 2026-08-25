//! Sending a request: headers, failure translation, and when to try again.

use std::time::Duration;

use aik_core::{Error, Result};
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::credentials::ApiKey;
use crate::deadline::Deadline;

/// Wraps an error message returned by the API, e.g. "credit balance is too low".
#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
pub(crate) struct AnthropicApiError {
    pub(crate) kind: String,
    pub(crate) message: String,
}

/// The error envelope the API returns for every non-2xx response.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

/// Builds the headers every request carries.
///
/// The key's header is marked sensitive, which is what keeps `hyper`'s own tracing from
/// printing it when a request is logged at debug level. That is the last of the three places
/// a key could leak on its own — configuration, this crate's own errors, and the HTTP
/// stack's logging — and it is the only one not enforced by the type in
/// [`credentials`](crate::credentials).
pub(crate) fn headers(key: &ApiKey, api_version: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();

    let mut value = HeaderValue::from_str(key.expose()).map_err(|_| {
        // Unreachable in practice: `ApiKey::new` already rejects anything a header cannot
        // carry. Answered rather than unwrapped, because "the key was malformed" must never
        // become a panic that prints a backtrace next to the value.
        Error::config("api key", "the API key cannot be sent as an HTTP header")
    })?;
    value.set_sensitive(true);
    headers.insert("x-api-key", value);

    headers.insert(
        "anthropic-version",
        HeaderValue::from_str(api_version)
            .map_err(|_| Error::config("api_version", "cannot be sent as an HTTP header"))?,
    );

    Ok(headers)
}

/// Turns a non-2xx response into an [`Error`], extracting the API's own message.
pub(crate) async fn ensure_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let error = match serde_json::from_str::<ErrorEnvelope>(&body) {
        Ok(envelope) => AnthropicApiError {
            kind: envelope.error.kind,
            message: envelope.error.message,
        },
        Err(_) => AnthropicApiError {
            kind: "http".to_owned(),
            message: body,
        },
    };

    Err(Error::wrap(
        format!("the Anthropic API returned HTTP {status}"),
        error,
    ))
}

/// Whether a failed attempt is worth repeating.
///
/// Only the failures that are about the service rather than the request: rate limiting,
/// overload, and the 5xx family. A 400 repeated is a 400, and a 401 repeated is a 401 with
/// one more chance for a wrong key to be noticed by whatever is counting.
fn is_retryable(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 409 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

/// How long to wait before attempt number `attempt` (1-based), honouring `retry-after`.
///
/// Exponential from half a second, capped at eight. No jitter: this is one client making one
/// request at a time, so there is no thundering herd to spread out, and a deterministic delay
/// is one a test can assert on.
fn backoff(attempt: u32, response: Option<&Response>) -> Duration {
    if let Some(after) = response.and_then(retry_after) {
        return after.min(Duration::from_secs(60));
    }
    let exponent = attempt.saturating_sub(1).min(4);
    Duration::from_millis(500 << exponent).min(Duration::from_secs(8))
}

/// Reads a `retry-after` header expressed in seconds.
///
/// The HTTP-date form is not honoured: it needs a clock this layer does not have, and the
/// backoff below it is a safe fallback rather than a failure.
fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Sends a request, retrying the failures that are worth retrying.
///
/// `build` is called once per attempt rather than taking a built request, because a
/// `RequestBuilder` is consumed by sending it and a retry needs an identical one.
///
/// Three rules keep this from making things worse:
///
/// * The deadline is never extended. A sleep that would outlast the remaining budget is not
///   taken; the last error is returned instead, which is the truthful one.
/// * Cancellation wins over a pending retry, immediately.
/// * Only the classes in [`is_retryable`] are repeated, and only up to `max_retries`.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> RequestBuilder,
    context: &'static str,
    max_retries: u32,
    cancellation: &CancellationToken,
    deadline: Deadline,
) -> Result<Response> {
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        let has_budget = attempt <= max_retries;

        // A response is returned whatever its status: turning a non-2xx into an error is
        // `ensure_success`'s job, and it needs the body to say what went wrong.
        let wait = match build().send().await {
            Ok(response) => {
                if !has_budget || !is_retryable(response.status()) {
                    return Ok(response);
                }
                let wait = backoff(attempt, Some(&response));
                if wait >= deadline.remaining() {
                    return Ok(response);
                }
                wait
            }
            // A transport failure has no response to inspect: no connection, a TLS refusal,
            // a stream cut before a status arrived. Retrying is right for the first two and
            // harmless for the third, since nothing reached the caller.
            Err(error) => {
                let wait = backoff(attempt, None);
                if !has_budget || wait >= deadline.remaining() {
                    return Err(map_reqwest_error(context, error));
                }
                wait
            }
        };

        tracing::debug!(
            attempt,
            wait_ms = wait.as_millis() as u64,
            "retrying an Anthropic request"
        );

        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = tokio::time::sleep_until(deadline.instant()) => {
                return Err(Error::Timeout(deadline.budget()));
            }
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Wraps a transport-level failure with what the caller was trying to do.
pub(crate) fn map_reqwest_error(context: impl Into<String>, error: reqwest::Error) -> Error {
    Error::wrap(context, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::{ManualClock, SharedClock, Timestamp};
    use std::sync::Arc;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn deadline(budget: Duration) -> Deadline {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        Deadline::compute(&clock, budget, &aik_api::execution::ExecutionContext::new())
    }

    #[test]
    fn only_service_side_failures_are_retried() {
        for status in [429, 500, 502, 503, 529] {
            assert!(
                is_retryable(StatusCode::from_u16(status).unwrap()),
                "{status}"
            );
        }
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(
                !is_retryable(StatusCode::from_u16(status).unwrap()),
                "{status}"
            );
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff(1, None), Duration::from_millis(500));
        assert_eq!(backoff(2, None), Duration::from_secs(1));
        assert_eq!(backoff(3, None), Duration::from_secs(2));
        assert_eq!(backoff(9, None), Duration::from_secs(8));
    }

    #[test]
    fn the_key_header_is_marked_sensitive() {
        let key = ApiKey::new("sk-ant-secret", "TEST").unwrap();
        let headers = headers(&key, "2023-06-01").unwrap();
        assert!(headers["x-api-key"].is_sensitive());
        assert_eq!(headers["anthropic-version"], "2023-06-01");
    }

    #[tokio::test]
    async fn error_bodies_are_extracted_from_the_api_envelope() {
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": { "type": "invalid_request_error", "message": "max_tokens: required" }
            })))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/fail", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(format!("{error}").contains("HTTP 400"), "{error}");
        let source = std::error::Error::source(&error).unwrap().to_string();
        assert!(source.contains("invalid_request_error"), "{source}");
        assert!(source.contains("max_tokens: required"), "{source}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_the_envelope_is_passed_through() {
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
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
                .contains("bad gateway")
        );
    }

    #[tokio::test]
    async fn a_retryable_status_is_tried_again_and_can_succeed() {
        let server = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(529))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let response = send_with_retry(
            || client.get(&url),
            "testing",
            2,
            &CancellationToken::new(),
            deadline(Duration::from_secs(30)),
        )
        .await
        .unwrap();

        assert_eq!(response.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn a_client_error_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "invalid x-api-key" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let response = send_with_retry(
            || client.get(&url),
            "testing",
            5,
            &CancellationToken::new(),
            deadline(Duration::from_secs(30)),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), 401);
        server.verify().await;
    }

    #[tokio::test]
    async fn retries_stop_at_the_configured_limit() {
        let server = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let response = send_with_retry(
            || client.get(&url),
            "testing",
            1,
            &CancellationToken::new(),
            deadline(Duration::from_secs(30)),
        )
        .await
        .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(format!("{error}").contains("HTTP 503"), "{error}");
        server.verify().await;
    }

    #[tokio::test]
    async fn a_retry_that_would_outlast_the_deadline_is_not_taken() {
        let server = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let response = send_with_retry(
            || client.get(&url),
            "testing",
            5,
            &CancellationToken::new(),
            // Less than the first backoff, so no attempt after the first one fits.
            deadline(Duration::from_millis(100)),
        )
        .await
        .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(format!("{error}").contains("HTTP 503"), "{error}");
        server.verify().await;
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_retry() {
        let server = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let cancellation = CancellationToken::new();
        let token = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });

        let error = send_with_retry(
            || client.get(&url),
            "testing",
            5,
            &cancellation,
            deadline(Duration::from_secs(30)),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, Error::Cancelled), "{error}");
    }
}
