//! Refusing to call a provider that has stopped answering.
//!
//! Retrying is the right response to one failure and the wrong response to a hundred. A
//! provider that is down does not become available because a client asked more times; it
//! becomes a queue of requests, each waiting out its own backoff, each holding a
//! conversation's worth of context, each ending in the same failure several seconds later
//! than it would have. A breaker turns the hundredth failure into an immediate refusal.
//!
//! # What counts as a failure
//!
//! Only a [`TransientFailure`](aik_api::resilience::TransientFailure). A malformed request,
//! a refused credential and a model that does not exist are the *caller's* problem, and a
//! breaker that opened on them would take a whole deployment down because one session asked
//! for something impossible. Cancellation and expiry do not count either: they are this
//! process's own decisions, not evidence about the service.
//!
//! # Why an open breaker is itself a transient failure
//!
//! Because it is: the request may well be worth making again, just not now. Marking the
//! refusal transient is also what stops the two mechanisms from fighting — a retry loop above
//! a breaker sees "open" as a reason to back off rather than as a terminal error, and the
//! [`ResilientProvider`](crate::ResilientProvider) consults the breaker on each attempt so a
//! circuit that opens mid-retry ends the call instead of being retried around.

use std::sync::Mutex;

use aik_api::resilience::{CircuitState, ProviderCircuitChanged, TransientFailure};
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::{ComponentId, EventBus, Result};

use crate::settings::BreakerSettings;

#[derive(Debug)]
struct State {
    circuit: CircuitState,
    consecutive_failures: u32,
    /// When an open circuit may next let a probe through.
    opened_until: Timestamp,
}

/// A per-provider circuit breaker.
///
/// Cheap to consult and safe to share: the whole state is three fields behind one mutex, held
/// for the length of a comparison and never across an await.
#[derive(Debug)]
pub struct CircuitBreaker {
    provider: ComponentId,
    settings: BreakerSettings,
    clock: SharedClock,
    events: Option<EventBus>,
    state: Mutex<State>,
}

impl CircuitBreaker {
    /// Creates a closed breaker.
    pub fn new(provider: ComponentId, settings: BreakerSettings, clock: SharedClock) -> Self {
        Self {
            provider,
            settings,
            clock,
            events: None,
            state: Mutex::new(State {
                circuit: CircuitState::Closed,
                consecutive_failures: 0,
                opened_until: Timestamp::EPOCH,
            }),
        }
    }

    /// Publishes [`ProviderCircuitChanged`] on every transition.
    #[must_use]
    pub fn with_events(mut self, events: EventBus) -> Self {
        self.events = Some(events);
        self
    }

    /// Whether this breaker does anything at all.
    fn enabled(&self) -> bool {
        self.settings.failure_threshold > 0
    }

    /// The breaker's current state, for tests and for reporting.
    pub fn state(&self) -> CircuitState {
        self.state.lock().expect("breaker lock poisoned").circuit
    }

    /// The kernel clock this breaker reads.
    ///
    /// Exposed so that everything in one [`ResilientProvider`](crate::ResilientProvider) —
    /// the cooldown, the deadline arithmetic, the timestamps on its events — reads the same
    /// clock. Two clocks that agree in production and not in a test is a class of bug worth
    /// designing out rather than remembering.
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Refuses if the circuit is open, and otherwise admits the call.
    ///
    /// An open circuit whose cooldown has elapsed becomes half-open here and admits exactly
    /// one call. The transition happens on the *admitting* call rather than on a timer, so
    /// there is no task to own, nothing to shut down, and no way for a probe to be admitted
    /// while nobody is calling.
    pub fn admit(&self) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }

        let now = self.clock.now();
        let mut state = self.state.lock().expect("breaker lock poisoned");
        match state.circuit {
            CircuitState::Closed | CircuitState::HalfOpen => Ok(()),
            CircuitState::Open if now >= state.opened_until => {
                let failures = state.consecutive_failures;
                state.circuit = CircuitState::HalfOpen;
                drop(state);
                self.announce(now, CircuitState::Open, CircuitState::HalfOpen, failures);
                Ok(())
            }
            CircuitState::Open => {
                let remaining = state.opened_until.saturating_since(now);
                drop(state);
                Err(TransientFailure::new(format!(
                    "the circuit for provider `{}` is open for another {}ms",
                    self.provider,
                    remaining.as_millis()
                ))
                .after(remaining)
                .wrapped(format!("calling model provider `{}`", self.provider)))
            }
        }
    }

    /// Records a call that succeeded, closing the circuit.
    pub fn record_success(&self) {
        if !self.enabled() {
            return;
        }

        let now = self.clock.now();
        let mut state = self.state.lock().expect("breaker lock poisoned");
        let was = state.circuit;
        state.consecutive_failures = 0;
        state.circuit = CircuitState::Closed;
        drop(state);

        if was != CircuitState::Closed {
            self.announce(now, was, CircuitState::Closed, 0);
        }
    }

    /// Records a call that failed transiently, opening the circuit once enough have.
    ///
    /// A failure in the half-open state re-opens immediately, whatever the count says: the
    /// probe existed to answer one question, and it answered it.
    pub fn record_failure(&self) {
        if !self.enabled() {
            return;
        }

        let now = self.clock.now();
        let cooldown = std::time::Duration::from_millis(self.settings.cooldown_ms);
        let mut state = self.state.lock().expect("breaker lock poisoned");
        let was = state.circuit;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);

        let should_open = was == CircuitState::HalfOpen
            || state.consecutive_failures >= self.settings.failure_threshold;
        if !should_open {
            return;
        }

        state.circuit = CircuitState::Open;
        state.opened_until = now.saturating_add(cooldown);
        let failures = state.consecutive_failures;
        drop(state);

        self.announce(now, was, CircuitState::Open, failures);
    }

    fn announce(&self, at: Timestamp, from: CircuitState, to: CircuitState, failures: u32) {
        tracing::warn!(
            provider = %self.provider,
            %from,
            %to,
            consecutive_failures = failures,
            "a model provider's circuit changed state"
        );
        let Some(events) = &self.events else {
            return;
        };
        events.publish(ProviderCircuitChanged {
            timestamp: at,
            provider: self.provider.clone(),
            from,
            to,
            consecutive_failures: failures,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::resilience::transient_failure;
    use aik_core::clock::ManualClock;
    use std::sync::Arc;
    use std::time::Duration;

    fn breaker(settings: BreakerSettings) -> (CircuitBreaker, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let breaker = CircuitBreaker::new(
            ComponentId::new("model.test"),
            settings,
            clock.clone() as SharedClock,
        );
        (breaker, clock)
    }

    fn settings() -> BreakerSettings {
        BreakerSettings {
            failure_threshold: 3,
            cooldown_ms: 10_000,
        }
    }

    #[test]
    fn a_closed_breaker_admits_everything() {
        let (breaker, _clock) = breaker(settings());
        for _ in 0..10 {
            breaker.admit().unwrap();
            breaker.record_success();
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn enough_consecutive_failures_open_the_circuit() {
        let (breaker, _clock) = breaker(settings());
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Closed);
        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);

        let error = breaker.admit().unwrap_err();
        assert!(error.to_string().contains("model.test"), "{error}");
    }

    #[test]
    fn a_success_resets_the_run_of_failures() {
        let (breaker, _clock) = breaker(settings());
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_success();
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn an_open_circuit_refuses_transiently_so_a_caller_can_tell_it_apart_from_a_bad_request() {
        let (breaker, _clock) = breaker(settings());
        for _ in 0..3 {
            breaker.record_failure();
        }
        let error = breaker.admit().unwrap_err();
        let failure = transient_failure(&error).expect("an open circuit is transient");
        assert_eq!(failure.retry_after(), Some(Duration::from_millis(10_000)));
    }

    #[test]
    fn the_cooldown_ends_and_one_probe_is_admitted() {
        let (breaker, clock) = breaker(settings());
        for _ in 0..3 {
            breaker.record_failure();
        }
        assert!(breaker.admit().is_err());

        clock.advance(Duration::from_millis(10_000));
        breaker.admit().expect("the probe is admitted");
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        breaker.record_success();
        assert_eq!(breaker.state(), CircuitState::Closed);
        breaker.admit().unwrap();
    }

    #[test]
    fn a_failed_probe_reopens_immediately_rather_than_after_another_run() {
        let (breaker, clock) = breaker(settings());
        for _ in 0..3 {
            breaker.record_failure();
        }
        clock.advance(Duration::from_millis(10_000));
        breaker.admit().unwrap();
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);
        // And the cooldown restarts from now, not from the original opening.
        assert!(breaker.admit().is_err());
        clock.advance(Duration::from_millis(9_999));
        assert!(breaker.admit().is_err());
        clock.advance(Duration::from_millis(1));
        assert!(breaker.admit().is_ok());
    }

    #[test]
    fn a_threshold_of_zero_disables_the_breaker_entirely() {
        let (breaker, _clock) = breaker(BreakerSettings {
            failure_threshold: 0,
            cooldown_ms: 10_000,
        });
        for _ in 0..100 {
            breaker.record_failure();
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
        breaker.admit().unwrap();
    }

    #[test]
    fn transitions_are_published() {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let events = EventBus::new(16, clock.clone() as SharedClock);
        let breaker = CircuitBreaker::new(
            ComponentId::new("model.test"),
            settings(),
            clock.clone() as SharedClock,
        )
        .with_events(events.clone());
        let mut stream = events.subscribe::<ProviderCircuitChanged>();

        for _ in 0..3 {
            breaker.record_failure();
        }

        let event = stream
            .try_recv()
            .expect("a transition")
            .expect("no lag")
            .payload;
        assert_eq!(event.from, CircuitState::Closed);
        assert_eq!(event.to, CircuitState::Open);
        assert_eq!(event.consecutive_failures, 3);
        assert_eq!(event.provider, ComponentId::new("model.test"));
    }

    #[test]
    fn recovery_is_published_too() {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let events = EventBus::new(16, clock.clone() as SharedClock);
        let breaker = CircuitBreaker::new(
            ComponentId::new("model.test"),
            settings(),
            clock.clone() as SharedClock,
        )
        .with_events(events.clone());
        let mut stream = events.subscribe::<ProviderCircuitChanged>();

        for _ in 0..3 {
            breaker.record_failure();
        }
        clock.advance(Duration::from_millis(10_000));
        breaker.admit().unwrap();
        breaker.record_success();

        let transitions: Vec<(CircuitState, CircuitState)> = std::iter::from_fn(|| {
            stream.try_recv().map(|event| {
                let payload = event.expect("no lag").payload;
                (payload.from, payload.to)
            })
        })
        .collect();

        assert_eq!(
            transitions,
            vec![
                (CircuitState::Closed, CircuitState::Open),
                (CircuitState::Open, CircuitState::HalfOpen),
                (CircuitState::HalfOpen, CircuitState::Closed),
            ]
        );
    }

    #[test]
    fn a_success_while_already_closed_publishes_nothing() {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
        let events = EventBus::new(16, clock.clone() as SharedClock);
        let breaker = CircuitBreaker::new(
            ComponentId::new("model.test"),
            settings(),
            clock as SharedClock,
        )
        .with_events(events.clone());
        let mut stream = events.subscribe::<ProviderCircuitChanged>();

        breaker.record_success();
        assert!(stream.try_recv().is_none());
    }
}
