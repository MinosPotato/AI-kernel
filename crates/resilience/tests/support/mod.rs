//! A model provider that fails exactly how a test tells it to.

// A shared test module is compiled into every integration test binary, so anything one of
// them does not use looks dead, and nothing in a test binary is reachable from outside it.
#![allow(dead_code, unreachable_pub)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelCapabilities, ModelDescriptor, ModelId, ModelProvider, Role,
};
use aik_api::resilience::TransientFailure;
use aik_core::{Error, Result};
use futures_core::stream::BoxStream;

/// What one scripted attempt does.
pub enum Attempt {
    /// Answers with `text`.
    Reply(&'static str),
    /// Fails transiently, so the layer above may try again.
    Transient,
    /// Fails transiently, asking to be left alone for a while.
    TransientAfter(Duration),
    /// Fails in a way nothing marked, so the layer above must not try again.
    Terminal,
    /// Establishes a stream that yields `text` and then ends.
    StreamReply(&'static str),
    /// Establishes a stream that yields a chunk and then fails transiently.
    StreamCutMidway,
}

/// A provider that works through a script, one entry per call.
///
/// Running off the end of the script is a panic rather than a failure: a test that asserted
/// on three attempts and got four should say so loudly.
pub struct ScriptedProvider {
    script: Mutex<std::collections::VecDeque<Attempt>>,
    calls: AtomicUsize,
    concurrent: AtomicUsize,
    peak_concurrent: AtomicUsize,
    /// How long each attempt takes, so a test can overlap them.
    latency: Duration,
}

impl std::fmt::Debug for ScriptedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedProvider")
            .field("calls", &self.calls())
            .field("peak_concurrent", &self.peak_concurrent())
            .finish_non_exhaustive()
    }
}

impl ScriptedProvider {
    /// Creates a provider that answers according to `script`.
    pub fn new(script: impl IntoIterator<Item = Attempt>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: AtomicUsize::new(0),
            concurrent: AtomicUsize::new(0),
            peak_concurrent: AtomicUsize::new(0),
            latency: Duration::ZERO,
        }
    }

    /// Makes every attempt take `latency`.
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// How many attempts have been made in total.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The most attempts that were ever in flight at the same moment.
    pub fn peak_concurrent(&self) -> usize {
        self.peak_concurrent.load(Ordering::SeqCst)
    }

    fn next(&self) -> Attempt {
        self.script
            .lock()
            .expect("script lock poisoned")
            .pop_front()
            .expect("the provider was called more times than the script allows")
    }

    async fn enter(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let in_flight = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_concurrent.fetch_max(in_flight, Ordering::SeqCst);
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
    }

    fn leave(&self) {
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
    }
}

fn reply(text: &str) -> CompletionResponse {
    CompletionResponse {
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn transient(retry_after: Option<Duration>) -> Error {
    let failure = TransientFailure::new("the upstream is overloaded");
    match retry_after {
        Some(after) => failure.after(after),
        None => failure,
    }
    .wrapped("calling the scripted provider")
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        Ok(vec![ModelDescriptor {
            id: ModelId::new("scripted"),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities(vec![ModelCapabilities::STREAMING.to_owned()]),
        }])
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        self.enter().await;
        let outcome = match self.next() {
            Attempt::Reply(text) => Ok(reply(text)),
            Attempt::Transient => Err(transient(None)),
            Attempt::TransientAfter(after) => Err(transient(Some(after))),
            Attempt::Terminal => Err(Error::InvalidArgument("max_tokens is required".into())),
            Attempt::StreamReply(_) | Attempt::StreamCutMidway => {
                Err(Error::Unsupported("this entry scripts a stream".into()))
            }
        };
        self.leave();
        outcome
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>> {
        self.enter().await;
        let outcome: Result<BoxStream<'static, Result<CompletionChunk>>> = match self.next() {
            Attempt::StreamReply(text) => Ok(Box::pin(futures::stream::iter(vec![
                Ok(CompletionChunk::Delta(ContentPart::text(text))),
                Ok(CompletionChunk::Done {
                    finish_reason: FinishReason::Stop,
                    usage: None,
                }),
            ]))),
            Attempt::StreamCutMidway => Ok(Box::pin(futures::stream::iter(vec![
                Ok(CompletionChunk::Delta(ContentPart::text("partial"))),
                Err(transient(None)),
            ]))),
            Attempt::Transient => Err(transient(None)),
            Attempt::TransientAfter(after) => Err(transient(Some(after))),
            Attempt::Terminal => Err(Error::InvalidArgument("max_tokens is required".into())),
            Attempt::Reply(_) => Err(Error::Unsupported("this entry scripts a completion".into())),
        };
        self.leave();
        outcome
    }
}

/// A request the scripted provider ignores the contents of.
pub fn request() -> CompletionRequest {
    CompletionRequest::new("scripted", vec![Message::text(Role::User, "hello")])
}
