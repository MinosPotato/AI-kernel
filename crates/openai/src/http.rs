//! Sending a request: headers, failure translation, and saying which failures are worth
//! repeating.
//!
//! This crate retries nothing itself. That is [`aik-resilience`](https://docs.rs/aik-resilience)'s
//! job, applied to every provider rather than to this one, and two retry loops in one call
//! stack multiply the attempt counts instead of adding robustness. What stays here is the
//! half only a provider can do: recognising that a failure is the service's rather than the
//! request's, and passing on how long the service asked to be left alone. See
//! [`aik_api::resilience`] for the contract that carries both.

use std::time::Duration;

use aik_api::resilience::{TransientFailure, transient_status};
use aik_core::{Error, Result};
use reqwest::Response;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};

use serde::Deserialize;

use crate::credentials::ApiKey;

/// Wraps an error message returned by the API, e.g. "insufficient_quota".
#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
pub(crate) struct OpenAiApiError {
    pub(crate) kind: String,
    pub(crate) message: String,
}

/// The error envelope this dialect returns for a non-2xx response.
///
/// Every field is optional, and a body that is not this shape is passed through as text:
/// the servers that speak this protocol are not all OpenAI, and one of them answering a 502
/// with an HTML error page must not turn into a decode failure that hides the status.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

/// Builds the headers every request carries.
///
/// The key's header is marked sensitive, which is what keeps `hyper`'s own tracing from
/// printing it when a request is logged at debug level. That is the last of the three places
/// a key could leak on its own — configuration, this crate's own errors, and the HTTP
/// stack's logging — and it is the only one not enforced by the type in
/// [`credentials`](crate::credentials).
///
/// A `None` key produces no `authorization` header at all, which is what a loopback server
/// with no notion of an account expects; see
/// [`api_key_required`](crate::settings::OpenAiSettings::api_key_required) for what it takes
/// to reach that state.
pub(crate) fn headers(
    key: Option<&ApiKey>,
    organization: Option<&str>,
    project: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();

    if let Some(key) = key {
        let mut value =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| {
                // Unreachable in practice: `ApiKey::new` already rejects anything a header cannot
                // carry. Answered rather than unwrapped, because "the key was malformed" must
                // never become a panic that prints a backtrace next to the value.
                Error::config("api key", "the API key cannot be sent as an HTTP header")
            })?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }

    for (name, value) in [
        ("openai-organization", organization),
        ("openai-project", project),
    ] {
        if let Some(value) = value {
            headers.insert(
                name,
                HeaderValue::from_str(value)
                    .map_err(|_| Error::config(name, "cannot be sent as an HTTP header"))?,
            );
        }
    }

    Ok(headers)
}

/// Turns a non-2xx response into an [`Error`], extracting the API's own message.
pub(crate) async fn ensure_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let asked_for = retry_after(&response);
    let body = response.text().await.unwrap_or_default();
    let error = parse_error_body(&body);

    let context = format!("the OpenAI API returned HTTP {status}");
    if !transient_status(status.as_u16()) {
        return Err(Error::wrap(context, error));
    }

    let mut failure = TransientFailure::new(error.to_string()).caused_by(error);
    if let Some(after) = asked_for {
        failure = failure.after(after);
    }
    Err(failure.wrapped(context))
}

/// Extracts what an error body says, falling back to the body itself.
fn parse_error_body(body: &str) -> OpenAiApiError {
    match serde_json::from_str::<ErrorEnvelope>(body) {
        Ok(envelope) => OpenAiApiError {
            // `type` is the coarse classification and `code` the specific one; servers in
            // this family populate one, the other, or both.
            kind: envelope
                .error
                .kind
                .or(envelope.error.code)
                .unwrap_or_else(|| "error".to_owned()),
            message: envelope.error.message.unwrap_or_default(),
        },
        Err(_) => OpenAiApiError {
            kind: "http".to_owned(),
            message: body.to_owned(),
        },
    }
}

/// Reads a `retry-after` header expressed in whole seconds.
///
/// The HTTP-date form is not honoured: it needs a clock this layer does not have, and the
/// caller's own backoff is a safe fallback rather than a failure.
fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Wraps a transport-level failure with what the caller was trying to do.
///
/// A failure that never reached a status line is marked transient when it is one of the three
/// kinds that say nothing about the request: a connection that could not be made, one that
/// timed out, and a body that stopped arriving. A decode failure is not among them — an API
/// answering with something this client cannot parse will answer the same way again.
pub(crate) fn map_reqwest_error(context: impl Into<String>, error: reqwest::Error) -> Error {
    let context = context.into();
    if error.is_connect() || error.is_timeout() || error.is_body() {
        return TransientFailure::new(error.to_string())
            .caused_by(error)
            .wrapped(context);
    }
    Error::wrap(context, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::resilience::transient_failure;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn the_key_header_is_a_sensitive_bearer_token() {
        let key = ApiKey::new("sk-secret", "TEST").unwrap();
        let headers = headers(Some(&key), None, None).unwrap();
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert_eq!(headers[AUTHORIZATION], "Bearer sk-secret");
        assert!(!headers.contains_key("openai-organization"));
    }

    #[test]
    fn no_key_means_no_authorization_header() {
        let headers = headers(None, None, None).unwrap();
        assert!(!headers.contains_key(AUTHORIZATION));
    }

    #[test]
    fn the_account_headers_are_sent_when_configured() {
        let headers = headers(None, Some("org-a"), Some("proj-b")).unwrap();
        assert_eq!(headers["openai-organization"], "org-a");
        assert_eq!(headers["openai-project"], "proj-b");
    }

    #[tokio::test]
    async fn error_bodies_are_extracted_from_the_api_envelope() {
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": "Unsupported parameter: 'max_tokens'",
                    "type": "invalid_request_error",
                    "code": "unsupported_parameter"
                }
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
        assert!(source.contains("Unsupported parameter"), "{source}");
    }

    #[test]
    fn an_envelope_with_only_a_code_still_names_the_failure() {
        let error = parse_error_body(r#"{"error":{"code":"model_not_found","message":"nope"}}"#);
        assert_eq!(error.kind, "model_not_found");
        assert_eq!(error.message, "nope");
    }

    #[tokio::test]
    async fn a_body_that_is_not_the_envelope_is_passed_through() {
        // A gateway answering a 502 with an HTML page must not become a decode failure.
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
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
    async fn a_service_side_status_is_marked_worth_repeating() {
        let server = MockServer::start().await;
        Mock::given(path("/busy"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_json(serde_json::json!({
                        "error": { "type": "rate_limit_exceeded", "message": "slow down" }
                    })),
            )
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/busy", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        let failure = transient_failure(&error).expect("marked transient");
        assert_eq!(failure.retry_after(), Some(Duration::from_secs(7)));
        assert!(failure.to_string().contains("rate_limit_exceeded"));
    }

    #[tokio::test]
    async fn a_refused_credential_is_never_marked_worth_repeating() {
        // Asking again with the same wrong key spends nothing useful and gives whatever is
        // counting failed authentications one more count.
        let server = MockServer::start().await;
        Mock::given(path("/fail"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": { "type": "invalid_request_error", "message": "Incorrect API key" }
            })))
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/fail", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        assert!(transient_failure(&error).is_none(), "{error}");
    }

    #[tokio::test]
    async fn a_retry_after_that_is_not_a_number_of_seconds_is_ignored_rather_than_fatal() {
        let server = MockServer::start().await;
        Mock::given(path("/busy"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"),
            )
            .mount(&server)
            .await;

        let response = reqwest::get(format!("{}/busy", server.uri()))
            .await
            .unwrap();
        let error = ensure_success(response).await.unwrap_err();

        let failure = transient_failure(&error).expect("still transient");
        assert_eq!(failure.retry_after(), None);
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_made_is_worth_repeating() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/v1/chat/completions")
            .send()
            .await
            .expect_err("nothing is listening");
        let mapped = map_reqwest_error("sending a completion request", error);

        assert!(transient_failure(&mapped).is_some());
        assert_eq!(mapped.to_string(), "sending a completion request");
    }
}
