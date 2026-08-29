//! The [`ModelProvider`] implementation itself.

use std::sync::Arc;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ModelDescriptor, ModelProvider,
};
use aik_core::Result;
use aik_core::clock::SharedClock;
use async_trait::async_trait;
use futures_core::stream::BoxStream;

use crate::credentials::ApiKey;
use crate::deadline::{Deadline, race};
use crate::http::{ensure_success, map_reqwest_error};
use crate::protocol::{
    MessageResponse, ModelsResponse, build_request, convert_models, convert_response,
};
use crate::settings::AnthropicSettings;
use crate::stream::sse_chunks;

/// Talks to the Anthropic Messages API.
///
/// This is the only place in the workspace that knows the API's wire format, and — with
/// [`credentials`](crate::credentials) — the only place that holds an API key. Consumers
/// depend on [`ModelProvider`], resolved through the kernel
/// [`Registry`](aik_core::Registry); they never see this type.
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    max_output_tokens: u32,
    default_timeout: Duration,
    clock: SharedClock,
}

impl std::fmt::Debug for AnthropicProvider {
    /// Prints everything except the credential, which has no `Debug` worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Builds a provider from settings, a resolved key and the kernel's clock.
    ///
    /// The key is consumed here and never stored anywhere else: it is turned into the header
    /// map the client sends with every request, marked sensitive, and dropped. There is no
    /// accessor for it on this type, so nothing downstream can read it back out.
    ///
    /// The clock is injected rather than read from the system, so that timeout behaviour can
    /// be driven deterministically in tests — the same pattern the kernel itself uses.
    pub fn new(settings: &AnthropicSettings, key: ApiKey, clock: SharedClock) -> Result<Self> {
        let headers = crate::http::headers(&key, &settings.api_version)?;
        drop(key);

        let client = reqwest::Client::builder()
            .default_headers(headers)
            // Nothing in this provider follows a redirect: a 3xx from the API would send
            // the credential to whatever host the response named.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| map_reqwest_error("building the Anthropic HTTP client", error))?;

        Ok(Self {
            client,
            base_url: settings.base_url(),
            max_output_tokens: settings.max_output_tokens,
            default_timeout: Duration::from_millis(settings.request_timeout_ms),
            clock,
        })
    }

    /// Wraps the provider in an `Arc`, as the registry holds it.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn deadline(&self, cx: &ExecutionContext) -> Deadline {
        Deadline::compute(&self.clock, self.default_timeout, cx)
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        let response = self
            .client
            // The listing pages, and defaults to twenty; asking for the maximum keeps a
            // deployment with many models from seeing an arbitrary prefix of them.
            .get(self.url("v1/models?limit=1000"))
            // The trait gives this call no `ExecutionContext`, so there is no caller
            // deadline to honour — and it is on the startup path, where a server that
            // accepts a connection and then says nothing would otherwise hang a frontend
            // before it has printed anything. The configured timeout stands in.
            .timeout(self.default_timeout)
            .send()
            .await
            .map_err(|error| map_reqwest_error("listing Anthropic models", error))?;
        let response = ensure_success(response).await?;
        let body: ModelsResponse = response
            .json()
            .await
            .map_err(|error| map_reqwest_error("decoding the Anthropic model list", error))?;
        Ok(convert_models(body))
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        let deadline = self.deadline(cx);
        let wire = build_request(&request, self.max_output_tokens, false)?;
        let url = self.url("v1/messages");

        let attempt = async {
            let response = self
                .client
                .post(&url)
                .json(&wire)
                .send()
                .await
                .map_err(|error| {
                    map_reqwest_error("sending a completion request to the Anthropic API", error)
                })?;
            let response = ensure_success(response).await?;
            let body: MessageResponse = response.json().await.map_err(|error| {
                map_reqwest_error("decoding the Anthropic completion response", error)
            })?;
            convert_response(body)
        };

        race(attempt, &cx.cancellation, deadline).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>> {
        let deadline = self.deadline(cx);
        let wire = build_request(&request, self.max_output_tokens, true)?;
        let url = self.url("v1/messages");

        let opening = async {
            self.client
                .post(&url)
                .json(&wire)
                .send()
                .await
                .map_err(|error| map_reqwest_error("opening an Anthropic stream", error))
        };

        let response = race(opening, &cx.cancellation, deadline).await?;
        let response = ensure_success(response).await?;

        Ok(Box::pin(sse_chunks(
            response.bytes_stream(),
            cx.cancellation.clone(),
            deadline,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::SystemClock;

    fn provider(endpoint: &str) -> AnthropicProvider {
        AnthropicProvider::new(
            &AnthropicSettings {
                endpoint: endpoint.to_owned(),
                ..AnthropicSettings::default()
            },
            ApiKey::new("sk-ant-test", "TEST").unwrap(),
            Arc::new(SystemClock),
        )
        .unwrap()
    }

    #[test]
    fn a_trailing_slash_on_the_endpoint_does_not_produce_a_double_slash() {
        assert_eq!(
            provider("https://api.anthropic.com/").url("v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            provider("https://api.anthropic.com").url("v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn debug_output_cannot_contain_the_key() {
        let text = format!("{:?}", provider("https://api.anthropic.com"));
        assert!(!text.contains("sk-ant-test"), "{text}");
    }
}
