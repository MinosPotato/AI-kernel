//! The [`ModelProvider`] implementation itself.

use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, Embedder, ModelDescriptor, ModelId,
    ModelProvider,
};
use aik_core::Result;
use aik_core::clock::SharedClock;
use async_trait::async_trait;
use futures_core::stream::BoxStream;

use crate::deadline::{Deadline, race};
use crate::http::{ensure_success, map_reqwest_error};
use crate::protocol::{
    ChatResponseLine, EmbedRequest, EmbedResponse, TagsResponse, build_chat_request,
    convert_embeddings, convert_response, convert_tags,
};
use crate::settings::OllamaSettings;
use crate::stream::ndjson_chunks;

/// Talks to a single Ollama server over HTTP.
///
/// This is the only place in the crate — indeed, in the whole workspace outside this
/// crate's tests — that knows Ollama's wire protocol. Consumers depend on
/// [`ModelProvider`], resolved through the kernel [`Registry`](aik_core::Registry); they
/// never see this type.
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    default_timeout: Duration,
    clock: SharedClock,
}

impl std::fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("base_url", &self.base_url)
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl OllamaProvider {
    /// Creates a provider from settings and the kernel's clock.
    ///
    /// The clock is injected rather than read from the system directly, so that timeout
    /// behaviour can be driven deterministically in tests — the same pattern the kernel
    /// itself uses.
    pub fn new(settings: OllamaSettings, clock: SharedClock) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: settings.endpoint.trim_end_matches('/').to_owned(),
            default_timeout: Duration::from_millis(settings.request_timeout_ms),
            clock,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn deadline(&self, cx: &ExecutionContext) -> Deadline {
        Deadline::compute(&self.clock, self.default_timeout, cx)
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        let response = self
            .client
            .get(self.url("api/tags"))
            .send()
            .await
            .map_err(|error| map_reqwest_error("listing Ollama models", error))?;
        let response = ensure_success(response).await?;
        let body: TagsResponse = response
            .json()
            .await
            .map_err(|error| map_reqwest_error("decoding the Ollama model list", error))?;
        Ok(convert_tags(body))
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        let deadline = self.deadline(cx);
        let wire = build_chat_request(&request, false)?;
        let client = self.client.clone();
        let url = self.url("api/chat");

        let attempt = async move {
            let response = client.post(url).json(&wire).send().await.map_err(|error| {
                map_reqwest_error("sending a completion request to Ollama", error)
            })?;
            let response = ensure_success(response).await?;
            let body: ChatResponseLine = response.json().await.map_err(|error| {
                map_reqwest_error("decoding the Ollama completion response", error)
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
        let wire = build_chat_request(&request, true)?;
        let client = self.client.clone();
        let url = self.url("api/chat");

        let opening = async move {
            client
                .post(url)
                .json(&wire)
                .send()
                .await
                .map_err(|error| map_reqwest_error("opening an Ollama stream", error))
        };

        let response = race(opening, &cx.cancellation, deadline).await?;
        let response = ensure_success(response).await?;

        Ok(Box::pin(ndjson_chunks(
            response.bytes_stream(),
            cx.cancellation.clone(),
            deadline,
        )))
    }
}

#[async_trait]
impl Embedder for OllamaProvider {
    /// Embeds a batch in one `/api/embed` call, under the same deadline and cancellation
    /// rules as a completion.
    ///
    /// An empty batch is answered without a request: there is nothing to ask a server, and a
    /// round trip that can only return an empty list is one a caller should not pay for.
    /// Vectors come back in input order, which this verifies against the request rather than
    /// assumes: a batch whose length or vector widths do not line up is an error, not a
    /// guess about which input each vector belongs to.
    async fn embed(
        &self,
        model: &ModelId,
        inputs: &[String],
        cx: &ExecutionContext,
    ) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let deadline = self.deadline(cx);
        let client = self.client.clone();
        let url = self.url("api/embed");
        let model = model.as_str().to_owned();
        let inputs = inputs.to_vec();
        let expected = inputs.len();

        let attempt = async move {
            let wire = EmbedRequest {
                model: &model,
                input: &inputs,
            };
            let response = client.post(url).json(&wire).send().await.map_err(|error| {
                map_reqwest_error("sending an embedding request to Ollama", error)
            })?;
            let response = ensure_success(response).await?;
            let body: EmbedResponse = response.json().await.map_err(|error| {
                map_reqwest_error("decoding the Ollama embedding response", error)
            })?;
            convert_embeddings(body, expected)
        };

        race(attempt, &cx.cancellation, deadline).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::SystemClock;
    use std::sync::Arc;

    fn provider(endpoint: &str) -> OllamaProvider {
        OllamaProvider::new(
            OllamaSettings {
                endpoint: endpoint.to_owned(),
                ..OllamaSettings::default()
            },
            Arc::new(SystemClock),
        )
    }

    #[test]
    fn a_trailing_slash_on_the_endpoint_does_not_produce_a_double_slash() {
        assert_eq!(
            provider("http://localhost:11434/").url("api/chat"),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(
            provider("http://localhost:11434").url("api/chat"),
            "http://localhost:11434/api/chat"
        );
    }
}
