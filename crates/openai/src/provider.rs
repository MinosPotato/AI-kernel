//! The [`ModelProvider`] and [`Embedder`] implementations themselves.

use std::sync::Arc;
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

use crate::credentials::ApiKey;
use crate::deadline::{Deadline, race};
use crate::http::{ensure_success, map_reqwest_error};
use crate::protocol::{
    ChatResponse, EmbedRequest, EmbedResponse, ModelsResponse, build_request, convert_embeddings,
    convert_models, convert_response,
};
use crate::settings::OpenAiSettings;
use crate::stream::sse_chunks;

/// Talks the OpenAI chat-completions dialect to a single endpoint.
///
/// This is the only place in the workspace that knows that wire format. Consumers depend on
/// [`ModelProvider`] and [`Embedder`], resolved through the kernel
/// [`Registry`](aik_core::Registry); they never see this type.
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    default_timeout: Duration,
    clock: SharedClock,
}

impl std::fmt::Debug for OpenAiProvider {
    /// Prints everything except the credential, which has no `Debug` worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("base_url", &self.base_url)
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Builds a provider from settings, a resolved key and the kernel's clock.
    ///
    /// The key is consumed here and never stored anywhere else: it is turned into the header
    /// map the client sends with every request, marked sensitive, and dropped. There is no
    /// accessor for it on this type, so nothing downstream can read it back out.
    ///
    /// The clock is injected rather than read from the system, so that timeout behaviour can
    /// be driven deterministically in tests — the same pattern the kernel itself uses.
    pub fn new(settings: &OpenAiSettings, key: Option<ApiKey>, clock: SharedClock) -> Result<Self> {
        // Re-checked here rather than trusted from `OpenAiSettings::read`, because this is the
        // point at which a credential is bound to a destination. A caller that built the
        // settings literally never went through `read`, and "the endpoint is https or
        // loopback" is not a claim worth taking on trust when getting it wrong sends a key in
        // cleartext.
        settings.validate()?;

        let headers = crate::http::headers(
            key.as_ref(),
            settings.organization.as_deref(),
            settings.project.as_deref(),
        )?;
        drop(key);

        let client = reqwest::Client::builder()
            .default_headers(headers)
            // Nothing in this provider follows a redirect: a 3xx from the API would send
            // the credential to whatever host the response named.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| map_reqwest_error("building the OpenAI HTTP client", error))?;

        Ok(Self {
            client,
            base_url: settings.base_url(),
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
impl ModelProvider for OpenAiProvider {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        let response = self
            .client
            .get(self.url("models"))
            // The trait gives this call no `ExecutionContext`, so there is no caller
            // deadline to honour — and it is on the startup path, where a server that
            // accepts a connection and then says nothing would otherwise hang a frontend
            // before it has printed anything. The configured timeout stands in.
            .timeout(self.default_timeout)
            .send()
            .await
            .map_err(|error| map_reqwest_error("listing OpenAI models", error))?;
        let response = ensure_success(response).await?;
        let body: ModelsResponse = response
            .json()
            .await
            .map_err(|error| map_reqwest_error("decoding the OpenAI model list", error))?;
        Ok(convert_models(body))
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        let deadline = self.deadline(cx);
        let wire = build_request(&request, false)?;
        let url = self.url("chat/completions");

        let attempt = async {
            let response = self
                .client
                .post(&url)
                .json(&wire)
                .send()
                .await
                .map_err(|error| {
                    map_reqwest_error("sending a completion request to the OpenAI API", error)
                })?;
            let response = ensure_success(response).await?;
            let body: ChatResponse = response.json().await.map_err(|error| {
                map_reqwest_error("decoding the OpenAI completion response", error)
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
        let wire = build_request(&request, true)?;
        let url = self.url("chat/completions");

        let opening = async {
            self.client
                .post(&url)
                .json(&wire)
                .send()
                .await
                .map_err(|error| map_reqwest_error("opening an OpenAI stream", error))
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

#[async_trait]
impl Embedder for OpenAiProvider {
    /// Embeds a batch in one `/embeddings` call, under the same deadline and cancellation
    /// rules as a completion.
    ///
    /// An empty batch is answered without a request: there is nothing to ask a server, and a
    /// round trip that can only return an empty list is one a caller should not pay for.
    /// Vectors come back keyed by index, and are put back in input order rather than trusted
    /// to arrive in it: a batch whose length, indices or vector widths do not line up is an
    /// error, not a guess about which input each vector belongs to.
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
        let url = self.url("embeddings");
        let model = model.as_str().to_owned();
        let inputs = inputs.to_vec();
        let expected = inputs.len();

        let attempt = async move {
            let wire = EmbedRequest {
                model: &model,
                input: &inputs,
                encoding_format: "float",
            };
            let response = client.post(url).json(&wire).send().await.map_err(|error| {
                map_reqwest_error("sending an embedding request to the OpenAI API", error)
            })?;
            let response = ensure_success(response).await?;
            let body: EmbedResponse = response.json().await.map_err(|error| {
                map_reqwest_error("decoding the OpenAI embedding response", error)
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

    fn provider(endpoint: &str) -> OpenAiProvider {
        OpenAiProvider::new(
            &OpenAiSettings {
                endpoint: endpoint.to_owned(),
                ..OpenAiSettings::default()
            },
            Some(ApiKey::new("sk-test", "TEST").unwrap()),
            Arc::new(SystemClock),
        )
        .unwrap()
    }

    #[test]
    fn a_trailing_slash_on_the_endpoint_does_not_produce_a_double_slash() {
        assert_eq!(
            provider("https://api.openai.com/v1/").url("chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            provider("https://api.openai.com/v1").url("chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn debug_output_cannot_contain_the_key() {
        let text = format!("{:?}", provider("https://api.openai.com/v1"));
        assert!(!text.contains("sk-test"), "{text}");
    }
}
