//! A [`ModelProvider`] wrapped in the three mechanisms this crate exists for.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ModelDescriptor, ModelId, ModelProvider,
};
use aik_api::resilience::{ProviderRetryScheduled, transient_failure};
use aik_core::clock::SharedClock;
use aik_core::{ComponentId, Error, EventBus, Result};
use futures_core::stream::BoxStream;

use crate::backoff::{Backoff, BackoffSchedule};
use crate::breaker::CircuitBreaker;
use crate::limit::ConcurrencyLimit;
use crate::settings::ResilienceSettings;

/// A model provider that retries, bounds its own concurrency, and stops calling an upstream
/// that has stopped answering.
///
/// # Where each mechanism sits, and why
///
/// One call passes through three gates, in this order:
///
/// 1. **The breaker**, consulted before every attempt. Checking it first means a provider
///    everybody already knows is down costs nothing — not a permit, not a connection, not a
///    backoff.
/// 2. **The concurrency limit**, held for the duration of one attempt and released before the
///    backoff. Holding a slot while sleeping would let a handful of retrying callers starve
///    every fresh one, which is the opposite of what the limit is for.
/// 3. **The attempt**, whose failure is classified by
///    [`transient_failure`](aik_api::resilience::transient_failure) and nothing else.
///
/// # What is never retried
///
/// A failure nobody marked transient. That is the fail-closed direction and it is the whole
/// classification rule: a provider that says nothing about a failure gets one attempt, which
/// is what it got before this type existed. See
/// [`aik_api::resilience`](aik_api::resilience) for why matching on message text is not an
/// alternative.
///
/// # Streaming is retried only until the first byte
///
/// [`stream`](ModelProvider::stream) retries *establishing* a stream, and never a stream that
/// has begun. A stream that fails after it has yielded a chunk cannot be restarted without
/// either duplicating what the caller already saw or silently dropping it, and a provider
/// cannot resume one. The failure of an established stream still counts towards the breaker,
/// so a service failing halfway through every response is still noticed.
///
/// # What this does not do
///
/// It does not charge anything. An attempt that failed on the way out cost the upstream real
/// work and this client no tokens it can count, and inventing a figure for it would put a
/// number nobody can check into the ledger. The bound that keeps that honest is
/// [`RetrySettings::max_attempts`](crate::RetrySettings::max_attempts): a turn that the
/// [`QuotaGuard`](aik_api::quota::QuotaGuard) charges once cost at most that many upstream
/// calls, and the layer above charges exactly once because retrying happens strictly below
/// the point where a response exists to charge for.
pub struct ResilientProvider {
    inner: Arc<dyn ModelProvider>,
    provider: ComponentId,
    schedule: BackoffSchedule,
    breaker: Arc<CircuitBreaker>,
    limit: ConcurrencyLimit,
    events: Option<EventBus>,
}

impl std::fmt::Debug for ResilientProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResilientProvider")
            .field("provider", &self.provider)
            .field("attempts", &self.schedule.max_attempts())
            .field("circuit", &self.breaker.state())
            .field("slots_available", &self.limit.available())
            .finish_non_exhaustive()
    }
}

/// Produces a distinct jitter seed per constructed provider.
///
/// Two kernels in one process — which is exactly what the test suite runs — would otherwise
/// jitter identically, which is the one thing jitter exists to prevent.
fn next_seed(clock: &SharedClock) -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    clock
        .now()
        .as_millis()
        .rotate_left(17)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ sequence
}

impl ResilientProvider {
    /// Wraps `inner`, naming it `provider` in events and messages.
    pub fn new(
        inner: Arc<dyn ModelProvider>,
        provider: impl Into<ComponentId>,
        settings: ResilienceSettings,
        clock: SharedClock,
    ) -> Self {
        let provider = provider.into();
        Self {
            inner,
            schedule: BackoffSchedule::new(settings.retry, next_seed(&clock)),
            breaker: Arc::new(CircuitBreaker::new(
                provider.clone(),
                settings.breaker,
                clock.clone(),
            )),
            limit: ConcurrencyLimit::new(
                settings.max_concurrent,
                Duration::from_millis(settings.acquire_timeout_ms),
                clock,
            ),
            events: None,
            provider,
        }
    }

    /// Publishes [`ProviderRetryScheduled`] and the breaker's own transitions.
    #[must_use]
    pub fn with_events(mut self, events: EventBus) -> Self {
        self.breaker = Arc::new(
            Arc::try_unwrap(self.breaker)
                .expect("the breaker is not shared before the provider is built")
                .with_events(events.clone()),
        );
        self.events = Some(events);
        self
    }

    /// The breaker guarding this provider, for reporting and for tests.
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// Runs `attempt` until it succeeds, fails terminally, or runs out of attempts.
    ///
    /// Generic over the operation rather than duplicated for `complete` and `stream`, because
    /// the two differ only in what a successful attempt produces: everything about when to
    /// try again is the same question, and two copies of it would be two answers eventually.
    async fn with_retries<T, F, Fut>(
        &self,
        model: &ModelId,
        cx: &ExecutionContext,
        attempt: F,
    ) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut number: u32 = 0;
        loop {
            number += 1;

            // Before the permit, so a provider already known to be down costs nothing. Its
            // refusal is returned as-is and never counted as another failure: the breaker
            // must not extend its own cooldown on the strength of calls it refused itself.
            self.breaker.admit()?;

            let outcome = {
                let _permit = self.limit.acquire(cx).await?;
                attempt().await
            };

            let error = match outcome {
                Ok(value) => {
                    self.breaker.record_success();
                    return Ok(value);
                }
                Err(error) => error,
            };

            let Some(failure) = transient_failure(&error) else {
                // Terminal. Not the service's fault as far as anything here can tell, so it
                // is not evidence for the breaker either.
                return Err(error);
            };
            let retry_after = failure.retry_after();
            self.breaker.record_failure();

            let Backoff::Wait {
                delay,
                honoured_retry_after,
            } = self.schedule.after(number, retry_after)
            else {
                return Err(error);
            };

            if !self.affordable(cx, delay) {
                // Returning the provider's own error rather than a timeout: "the upstream is
                // overloaded" is the truthful answer, and it is the one an operator needs.
                return Err(error);
            }

            self.announce_retry(model, cx, number, delay, honoured_retry_after);

            tokio::select! {
                biased;
                () = cx.cancellation.cancelled() => return Err(Error::Cancelled),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Whether `delay` fits inside what is left of the caller's deadline.
    ///
    /// A wait that would outlast the budget is not taken at all. Sleeping through a deadline
    /// only to report a timeout replaces a useful error with a useless one and holds the
    /// caller for the whole budget to do it.
    fn affordable(&self, cx: &ExecutionContext, delay: Duration) -> bool {
        let Some(deadline) = cx.deadline else {
            return true;
        };
        delay < deadline.saturating_since(self.breaker_clock_now())
    }

    fn breaker_clock_now(&self) -> aik_core::clock::Timestamp {
        // The breaker owns the only clock this type holds; reading it through the breaker
        // keeps one clock rather than two that could be different in a test.
        self.breaker.now()
    }

    fn announce_retry(
        &self,
        model: &ModelId,
        cx: &ExecutionContext,
        attempt: u32,
        delay: Duration,
        honoured_retry_after: bool,
    ) {
        tracing::warn!(
            provider = %self.provider,
            model = %model,
            attempt,
            delay_ms = delay.as_millis() as u64,
            honoured_retry_after,
            "a model provider call failed transiently and will be attempted again"
        );
        let Some(events) = &self.events else {
            return;
        };
        events.publish(ProviderRetryScheduled {
            correlation: cx.correlation,
            timestamp: self.breaker_clock_now(),
            provider: self.provider.clone(),
            model: model.clone(),
            attempt,
            delay_ms: delay.as_millis() as u64,
            honoured_retry_after,
        });
    }
}

#[async_trait::async_trait]
impl ModelProvider for ResilientProvider {
    /// Passed straight through.
    ///
    /// Listing models is a start-up probe, not a turn: it carries no conversation, costs
    /// nothing to repeat by hand, and a deployment whose provider cannot answer it should
    /// fail to start promptly rather than after several backoffs. Passing it through also
    /// keeps it out of the breaker, so an operator asking what a downed provider serves gets
    /// the provider's own answer instead of this crate's.
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        self.inner.models().await
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        let model = request.model.clone();
        self.with_retries(&model, cx, || {
            let request = request.clone();
            let cx = cx.clone();
            async move { self.inner.complete(request, &cx).await }
        })
        .await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>> {
        let model = request.model.clone();
        let stream = self
            .with_retries(&model, cx, || {
                let request = request.clone();
                let cx = cx.clone();
                async move { self.inner.stream(request, &cx).await }
            })
            .await?;

        Ok(crate::stream::watch(stream, self.breaker.clone()))
    }
}
