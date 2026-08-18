//! Small helpers around `reqwest` that translate its failure modes into the kernel's
//! [`Error`] type.

use aik_core::{Error, Result};
use serde::Deserialize;

use crate::protocol::OllamaApiError;

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: String,
}

/// Wraps a transport-level failure with what the caller was trying to do.
pub(crate) fn map_reqwest_error(context: impl Into<String>, error: reqwest::Error) -> Error {
    Error::wrap(context, error)
}

/// Turns a non-2xx response into an [`Error`], extracting Ollama's `{"error": "..."}` body
/// when present so the message reaches the caller instead of a bare status code.
pub(crate) async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ErrorBody>(&body)
        .map(|parsed| parsed.error)
        .unwrap_or(body);

    Err(Error::wrap(
        format!("Ollama returned HTTP {status}"),
        OllamaApiError(message),
    ))
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
