//! What a [`ResilientProvider`] does to calls that fail, and to calls that do not.

mod support;

use std::sync::Arc;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::model::ModelProvider;
use aik_api::resilience::{
    CircuitState, ProviderCircuitChanged, ProviderRetryScheduled, transient_failure,
};
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::{Error, EventBus};
use aik_resilience::{BreakerSettings, ResilienceSettings, ResilientProvider, RetrySettings};
use futures::StreamExt;
use support::{Attempt, ScriptedProvider, request};

/// Retries with no waiting, so a test asserts on *whether* a call is repeated rather than on
/// how long the schedule made it wait — which `backoff`'s own tests already pin down.
fn prompt_settings(max_attempts: u32) -> ResilienceSettings {
    ResilienceSettings {
        retry: RetrySettings {
            max_attempts,
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retry_after_ms: 0,
        },
        breaker: BreakerSettings {
            failure_threshold: 0,
            cooldown_ms: 0,
        },
        max_concurrent: 0,
        acquire_timeout_ms: 1_000,
    }
}

fn clock() -> SharedClock {
    Arc::new(SystemClock)
}

fn wrap(
    inner: Arc<ScriptedProvider>,
    settings: ResilienceSettings,
) -> (Arc<ScriptedProvider>, ResilientProvider) {
    let provider = ResilientProvider::new(
        inner.clone() as Arc<dyn ModelProvider>,
        "model.scripted",
        settings,
        clock(),
    );
    (inner, provider)
}

fn scripted(
    script: impl IntoIterator<Item = Attempt>,
    settings: ResilienceSettings,
) -> (Arc<ScriptedProvider>, ResilientProvider) {
    wrap(Arc::new(ScriptedProvider::new(script)), settings)
}

#[tokio::test]
async fn a_call_that_succeeds_is_made_once() {
    let (inner, provider) = scripted([Attempt::Reply("hello")], prompt_settings(3));

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(inner.calls(), 1);
}

#[tokio::test]
async fn a_transient_failure_is_attempted_again() {
    let (inner, provider) = scripted(
        [Attempt::Transient, Attempt::Transient, Attempt::Reply("ok")],
        prompt_settings(3),
    );

    let response = provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(inner.calls(), 3);
    assert_eq!(response.finish_reason, aik_api::model::FinishReason::Stop);
}

#[tokio::test]
async fn a_failure_nobody_marked_is_not_attempted_again() {
    // The fail-closed direction, and the reason for it: repeating a request the service
    // refused on its merits spends the same money to be told the same thing.
    let (inner, provider) = scripted([Attempt::Terminal], prompt_settings(5));

    let error = provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert_eq!(inner.calls(), 1);
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn attempts_stop_at_the_configured_limit_and_the_last_failure_is_reported() {
    let (inner, provider) = scripted(
        [Attempt::Transient, Attempt::Transient, Attempt::Transient],
        prompt_settings(3),
    );

    let error = provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert_eq!(inner.calls(), 3);
    assert!(
        transient_failure(&error).is_some(),
        "the caller should still be told the upstream was at fault: {error}"
    );
}

#[tokio::test]
async fn one_attempt_means_no_retrying_at_all() {
    let (inner, provider) = scripted([Attempt::Transient], prompt_settings(1));

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert_eq!(inner.calls(), 1);
}

#[tokio::test]
async fn a_stated_retry_after_is_waited_out() {
    let settings = ResilienceSettings {
        retry: RetrySettings {
            max_attempts: 2,
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retry_after_ms: 60_000,
        },
        ..prompt_settings(2)
    };
    let (_inner, provider) = scripted(
        [
            Attempt::TransientAfter(Duration::from_millis(120)),
            Attempt::Reply("ok"),
        ],
        settings,
    );

    let started = std::time::Instant::now();
    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(110),
        "a service that asked to be left alone was not: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_wait_that_would_outlast_the_deadline_is_not_taken() {
    let settings = ResilienceSettings {
        retry: RetrySettings {
            max_attempts: 5,
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retry_after_ms: 60_000,
        },
        ..prompt_settings(5)
    };
    let (inner, provider) = scripted([Attempt::TransientAfter(Duration::from_secs(30))], settings);

    let cx = ExecutionContext::new()
        .with_deadline(Timestamp::now().saturating_add(Duration::from_millis(200)));

    let started = std::time::Instant::now();
    let error = provider.complete(request(), &cx).await.unwrap_err();

    assert_eq!(inner.calls(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call slept through a deadline it could not have met"
    );
    assert!(
        transient_failure(&error).is_some(),
        "the upstream's own error is the truthful one to report: {error}"
    );
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_retry() {
    let settings = ResilienceSettings {
        retry: RetrySettings {
            max_attempts: 5,
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retry_after_ms: 60_000,
        },
        ..prompt_settings(5)
    };
    let (_inner, provider) = scripted([Attempt::TransientAfter(Duration::from_secs(30))], settings);

    let cx = ExecutionContext::new();
    let token = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        token.cancel();
    });

    let error = provider.complete(request(), &cx).await.unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
}

#[tokio::test]
async fn enough_failures_open_the_circuit_and_later_calls_cost_nothing() {
    let settings = ResilienceSettings {
        breaker: BreakerSettings {
            failure_threshold: 2,
            cooldown_ms: 60_000,
        },
        ..prompt_settings(2)
    };
    // Two attempts, both failing, is enough to reach a threshold of two.
    let (inner, provider) = scripted([Attempt::Transient, Attempt::Transient], settings);

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();
    assert_eq!(inner.calls(), 2);
    assert_eq!(provider.breaker().state(), CircuitState::Open);

    // The script has nothing left; reaching the provider at all would panic.
    let error = provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();
    assert_eq!(inner.calls(), 2, "an open circuit must not reach upstream");
    assert!(error.to_string().contains("model.scripted"), "{error}");
}

#[tokio::test]
async fn an_open_circuit_is_not_retried_around() {
    // The refusal is transient, so a loop that treated it as evidence would retry into its
    // own breaker for as many attempts as it had.
    let settings = ResilienceSettings {
        breaker: BreakerSettings {
            failure_threshold: 1,
            cooldown_ms: 60_000,
        },
        ..prompt_settings(5)
    };
    let (inner, provider) = scripted([Attempt::Transient], settings);

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert_eq!(
        inner.calls(),
        1,
        "the first failure opened the circuit, which ended the call"
    );
    assert_eq!(provider.breaker().state(), CircuitState::Open);
}

#[tokio::test]
async fn a_terminal_failure_is_not_evidence_for_the_breaker() {
    let settings = ResilienceSettings {
        breaker: BreakerSettings {
            failure_threshold: 2,
            cooldown_ms: 60_000,
        },
        ..prompt_settings(1)
    };
    let (_inner, provider) = scripted([Attempt::Terminal, Attempt::Terminal], settings);

    for _ in 0..2 {
        provider
            .complete(request(), &ExecutionContext::new())
            .await
            .unwrap_err();
    }

    assert_eq!(
        provider.breaker().state(),
        CircuitState::Closed,
        "one session asking for something impossible must not take the deployment down"
    );
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let settings = ResilienceSettings {
        max_concurrent: 2,
        acquire_timeout_ms: 10_000,
        ..prompt_settings(1)
    };
    let inner = Arc::new(
        ScriptedProvider::new((0..8).map(|_| Attempt::Reply("ok")))
            .with_latency(Duration::from_millis(40)),
    );
    let (inner, provider) = wrap(inner, settings);
    let provider = Arc::new(provider);

    let calls: Vec<_> = (0..8)
        .map(|_| {
            let provider = provider.clone();
            tokio::spawn(
                async move { provider.complete(request(), &ExecutionContext::new()).await },
            )
        })
        .collect();

    for call in calls {
        call.await.unwrap().unwrap();
    }

    assert_eq!(inner.calls(), 8);
    assert!(
        inner.peak_concurrent() <= 2,
        "the limit let {} calls through at once",
        inner.peak_concurrent()
    );
}

#[tokio::test]
async fn establishing_a_stream_is_retried() {
    let (inner, provider) = scripted(
        [Attempt::Transient, Attempt::StreamReply("hello")],
        prompt_settings(3),
    );

    let mut stream = provider
        .stream(request(), &ExecutionContext::new())
        .await
        .unwrap();
    let mut chunks = 0;
    while let Some(item) = stream.next().await {
        item.unwrap();
        chunks += 1;
    }

    assert_eq!(inner.calls(), 2);
    assert_eq!(chunks, 2);
}

#[tokio::test]
async fn a_stream_that_has_begun_is_never_restarted() {
    // The property: a caller that already saw "partial" must not be sent it again, and must
    // not have it silently replaced by a second attempt's output.
    let (inner, provider) = scripted(
        [Attempt::StreamCutMidway, Attempt::StreamReply("second try")],
        prompt_settings(3),
    );

    let mut stream = provider
        .stream(request(), &ExecutionContext::new())
        .await
        .unwrap();

    let mut seen = Vec::new();
    let mut failed = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => seen.push(chunk),
            Err(_) => failed = true,
        }
    }

    assert_eq!(inner.calls(), 1, "the stream was re-established");
    assert_eq!(seen.len(), 1);
    assert!(failed, "the caller must be told the stream broke");
}

#[tokio::test]
async fn a_stream_that_breaks_midway_still_counts_towards_the_breaker() {
    let settings = ResilienceSettings {
        breaker: BreakerSettings {
            failure_threshold: 1,
            cooldown_ms: 60_000,
        },
        ..prompt_settings(1)
    };
    let (_inner, provider) = scripted([Attempt::StreamCutMidway], settings);

    let mut stream = provider
        .stream(request(), &ExecutionContext::new())
        .await
        .unwrap();
    while stream.next().await.is_some() {}

    assert_eq!(
        provider.breaker().state(),
        CircuitState::Open,
        "a service that fails halfway through every answer is still failing"
    );
}

#[tokio::test]
async fn listing_models_is_passed_straight_through() {
    let settings = ResilienceSettings {
        breaker: BreakerSettings {
            failure_threshold: 1,
            cooldown_ms: 60_000,
        },
        ..prompt_settings(1)
    };
    let (_inner, provider) = scripted([Attempt::Transient], settings);

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();
    assert_eq!(provider.breaker().state(), CircuitState::Open);

    // An operator asking what a downed provider serves gets the provider's own answer.
    let models = provider.models().await.unwrap();
    assert_eq!(models.len(), 1);
}

#[tokio::test]
async fn retries_and_transitions_are_published() {
    let events = EventBus::new(64, clock());
    let mut retries = events.subscribe::<ProviderRetryScheduled>();
    let mut transitions = events.subscribe::<ProviderCircuitChanged>();

    let settings = ResilienceSettings {
        breaker: BreakerSettings {
            failure_threshold: 2,
            cooldown_ms: 60_000,
        },
        ..prompt_settings(2)
    };
    let inner: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new([
        Attempt::Transient,
        Attempt::Transient,
    ]));
    let provider = ResilientProvider::new(inner, "model.scripted", settings, clock())
        .with_events(events.clone());

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    let retry = retries
        .try_recv()
        .expect("a retry was scheduled")
        .expect("no lag")
        .payload;
    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.provider, aik_core::ComponentId::new("model.scripted"));
    assert_eq!(retry.model, aik_api::model::ModelId::new("scripted"));

    let transition = transitions
        .try_recv()
        .expect("the circuit opened")
        .expect("no lag")
        .payload;
    assert_eq!(transition.to, CircuitState::Open);
}

#[tokio::test]
async fn pass_through_settings_change_nothing() {
    let (inner, provider) = scripted([Attempt::Transient], ResilienceSettings::pass_through());

    provider
        .complete(request(), &ExecutionContext::new())
        .await
        .unwrap_err();

    assert_eq!(inner.calls(), 1);
    assert_eq!(provider.breaker().state(), CircuitState::Closed);
}
