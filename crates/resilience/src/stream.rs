//! Watching an established stream without being able to restart it.
//!
//! Once a stream has been handed to a caller, this crate's only remaining job is to notice how
//! it ends. It cannot retry: a provider cannot resume a response, so a second attempt would
//! either repeat what the caller already consumed or drop it. It must still *count*, because
//! a service that accepts every request and then fails halfway through the answer is exactly
//! the kind of failure a breaker exists to notice, and a wrapper that stopped paying attention
//! at the first chunk would report that service as perfectly healthy.
//!
//! So the stream is passed through unchanged and its terminal item is observed: an error
//! records a transient failure if it is marked as one, and reaching the end records a success.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use aik_api::model::CompletionChunk;
use aik_api::resilience::transient_failure;
use aik_core::Result;
use futures_core::Stream;
use futures_core::stream::BoxStream;

use crate::breaker::CircuitBreaker;

/// Wraps `stream` so that how it ends reaches `breaker`.
pub(crate) fn watch(
    stream: BoxStream<'static, Result<CompletionChunk>>,
    breaker: Arc<CircuitBreaker>,
) -> BoxStream<'static, Result<CompletionChunk>> {
    Box::pin(WatchedStream {
        inner: stream,
        breaker,
        settled: false,
    })
}

struct WatchedStream {
    inner: BoxStream<'static, Result<CompletionChunk>>,
    breaker: Arc<CircuitBreaker>,
    /// Whether the outcome has already been recorded.
    ///
    /// A stream is allowed to be polled after it has yielded an error, and a caller that
    /// keeps polling one must not be able to drive the breaker open by itself.
    settled: bool,
}

impl std::fmt::Debug for WatchedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchedStream")
            .field("settled", &self.settled)
            .finish_non_exhaustive()
    }
}

impl Stream for WatchedStream {
    type Item = Result<CompletionChunk>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let polled = this.inner.as_mut().poll_next(cx);

        match &polled {
            Poll::Ready(Some(Err(error))) if !this.settled => {
                this.settled = true;
                if transient_failure(error).is_some() {
                    this.breaker.record_failure();
                }
            }
            Poll::Ready(None) if !this.settled => {
                this.settled = true;
                this.breaker.record_success();
            }
            _ => {}
        }

        polled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::model::ContentPart;
    use aik_api::resilience::{CircuitState, TransientFailure};
    use aik_core::ComponentId;
    use aik_core::clock::{ManualClock, SharedClock, Timestamp};
    use aik_core::{Error, Result as KernelResult};
    use futures::StreamExt;

    use crate::settings::BreakerSettings;

    fn breaker(threshold: u32) -> Arc<CircuitBreaker> {
        let clock: SharedClock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        Arc::new(CircuitBreaker::new(
            ComponentId::new("model.test"),
            BreakerSettings {
                failure_threshold: threshold,
                cooldown_ms: 1_000,
            },
            clock,
        ))
    }

    fn chunks(
        items: Vec<KernelResult<CompletionChunk>>,
    ) -> BoxStream<'static, KernelResult<CompletionChunk>> {
        Box::pin(futures::stream::iter(items))
    }

    fn delta(text: &str) -> KernelResult<CompletionChunk> {
        Ok(CompletionChunk::Delta(ContentPart::text(text)))
    }

    #[tokio::test]
    async fn a_stream_that_completes_records_a_success() {
        let breaker = breaker(1);
        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);

        let mut stream = watch(chunks(vec![delta("hi")]), breaker.clone());
        while stream.next().await.is_some() {}

        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn a_stream_that_fails_midway_still_reaches_the_breaker() {
        let breaker = breaker(2);
        let failing = chunks(vec![
            delta("partial"),
            Err(TransientFailure::new("connection reset").wrapped("streaming")),
        ]);

        let mut stream = watch(failing, breaker.clone());
        while stream.next().await.is_some() {}

        assert_eq!(breaker.state(), CircuitState::Closed, "one failure of two");

        let failing = chunks(vec![Err(
            TransientFailure::new("connection reset").wrapped("streaming")
        )]);
        let mut stream = watch(failing, breaker.clone());
        while stream.next().await.is_some() {}

        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn chunks_reach_the_caller_unchanged() {
        let breaker = breaker(2);
        let mut stream = watch(chunks(vec![delta("a"), delta("b")]), breaker);
        let mut received = Vec::new();
        while let Some(item) = stream.next().await {
            received.push(item.unwrap());
        }
        assert_eq!(
            received,
            vec![
                CompletionChunk::Delta(ContentPart::text("a")),
                CompletionChunk::Delta(ContentPart::text("b")),
            ]
        );
    }

    #[tokio::test]
    async fn a_terminal_failure_in_a_stream_is_not_evidence_about_the_service() {
        let breaker = breaker(1);
        let failing = chunks(vec![Err(Error::InvalidArgument("bad tool schema".into()))]);

        let mut stream = watch(failing, breaker.clone());
        while stream.next().await.is_some() {}

        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn one_stream_records_one_outcome_however_often_it_is_polled() {
        let breaker = breaker(2);
        let failing = chunks(vec![
            Err(TransientFailure::new("reset").wrapped("streaming")),
            Err(TransientFailure::new("reset").wrapped("streaming")),
            Err(TransientFailure::new("reset").wrapped("streaming")),
        ]);

        let mut stream = watch(failing, breaker.clone());
        while stream.next().await.is_some() {}

        assert_eq!(
            breaker.state(),
            CircuitState::Closed,
            "a caller polling past the first error must not drive the breaker on its own"
        );
    }
}
