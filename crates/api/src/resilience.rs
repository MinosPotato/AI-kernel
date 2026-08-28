//! Telling a failure that is worth repeating from one that is not.
//!
//! Every other contract in this crate describes something the system can *do*. This one
//! describes something a failure can *say about itself*, and it exists because nothing in
//! [`Error`] could say it.
//!
//! # Why the kernel error type was not enough
//!
//! [`ErrorKind`](aik_core::ErrorKind) classifies failures well enough to decide whether to
//! report or escalate, and deliberately does not classify them well enough to decide whether
//! to *retry*. A rate limit, an overloaded upstream, a connection cut before a status
//! arrived and a malformed request are all [`ErrorKind::Other`](aik_core::ErrorKind::Other)
//! once a provider has wrapped them, because that is what
//! [`Error::wrap`](aik_core::Error::wrap) produces. A caller looking at that has two ways to
//! choose: match on the message text, or retry everything.
//!
//! Matching on message text is a coupling that no test catches when it breaks. Retrying
//! everything is worse than not retrying at all: a retry re-sends the entire transcript, so
//! repeating a request the service already refused on its merits spends the same money
//! again, several times, to be told the same thing.
//!
//! So a provider that knows a failure is about the service rather than about the request
//! says so, by carrying a [`TransientFailure`] in the error's source chain. Everything else
//! is terminal. That is the fail-closed direction: a failure nobody classified is not
//! repeated, and a provider that never learns to classify anything behaves exactly as it did
//! before this module existed.
//!
//! # What a caller may assume
//!
//! Exactly one thing: that the *request* may be worth sending again. Not that it is safe to,
//! and not that it had no effect — a response cut in transit came from a model that already
//! ran and a bill that was already incurred. That is why this marker belongs on a
//! [`ModelProvider`](crate::model::ModelProvider) call, whose only effect is the answer it
//! returns, and why nothing in this crate marks a tool invocation transient: a tool that
//! failed halfway may have written a file.

use std::time::Duration;

use aik_core::clock::Timestamp;
use aik_core::id::{ComponentId, CorrelationId};
use aik_core::{BoxError, Error, Event};
use serde::{Deserialize, Serialize};

use crate::model::ModelId;

/// A failure that is about the service rather than about the request.
///
/// Carried as the source of an [`Error::wrap`] by the implementation that recognised it, and
/// found again with [`transient_failure`]. Constructing one is a claim that repeating the
/// same request could plausibly succeed; see the [module documentation](self) for the much
/// narrower thing that entitles a caller to assume.
#[derive(Debug)]
pub struct TransientFailure {
    detail: String,
    retry_after: Option<Duration>,
    source: Option<BoxError>,
}

impl TransientFailure {
    /// Marks a failure transient, described by `detail`.
    ///
    /// `detail` is what the failure renders as, so it should read the way the unmarked
    /// failure would have: wrapping is not meant to hide the message underneath it.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            retry_after: None,
            source: None,
        }
    }

    /// Records how long the service asked to be left alone.
    ///
    /// A caller is expected to treat this as a floor on its own backoff and to cap it —
    /// nothing stops a service from asking for a week, and a client that obeyed would be a
    /// client any upstream could hang indefinitely.
    #[must_use]
    pub fn after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// Keeps the original failure underneath this one.
    #[must_use]
    pub fn caused_by(mut self, source: impl Into<BoxError>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// How long the service asked to be left alone, if it said.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Wraps this into a kernel [`Error`], describing what was being attempted.
    ///
    /// The counterpart of [`Error::wrap`], and the only way this type is meant to reach a
    /// caller: nothing returns a `TransientFailure` directly, because the kernel has one
    /// error type and this is a note attached to it.
    pub fn wrapped(self, context: impl Into<String>) -> Error {
        Error::wrap(context, self)
    }
}

impl std::fmt::Display for TransientFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for TransientFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Finds a [`TransientFailure`] anywhere in `error`'s source chain.
///
/// Returns `None` for anything unmarked, which is the answer for every error the kernel
/// produces on its own. Two failures are deliberately never transient however they are
/// wrapped, because repeating them would defeat the mechanism that produced them:
///
/// * [`Error::Cancelled`] — somebody asked for this to stop.
/// * [`Error::Timeout`] — the budget for the whole operation is gone, so there is nothing
///   left to spend on another attempt.
///
/// ```
/// use aik_api::resilience::{TransientFailure, transient_failure};
/// use aik_core::Error;
/// use std::time::Duration;
///
/// let error = TransientFailure::new("HTTP 503")
///     .after(Duration::from_secs(2))
///     .wrapped("calling the model");
/// assert_eq!(
///     transient_failure(&error).and_then(TransientFailure::retry_after),
///     Some(Duration::from_secs(2))
/// );
///
/// assert!(transient_failure(&Error::other("a bad request")).is_none());
/// assert!(transient_failure(&Error::Cancelled).is_none());
/// ```
pub fn transient_failure(error: &Error) -> Option<&TransientFailure> {
    if matches!(error, Error::Cancelled | Error::Timeout(_)) {
        return None;
    }

    let mut current = std::error::Error::source(error);
    while let Some(source) = current {
        if let Some(failure) = source.downcast_ref::<TransientFailure>() {
            return Some(failure);
        }
        current = source.source();
    }
    None
}

/// Whether an HTTP status describes the service's condition rather than the request's.
///
/// Provider crates live in their own workspaces and each speak a different wire format, but
/// they all speak HTTP, and "which status codes mean try again" is one question with one
/// answer. Two copies of the list would be two answers as soon as somebody edited one.
///
/// The list is short on purpose, and everything absent from it is terminal:
///
/// * `408 Request Timeout` and `409 Conflict` — the server gave up on, or serialised away, a
///   request it never processed.
/// * `429 Too Many Requests` — the canonical "later, not never".
/// * `500`, `502`, `503`, `504` — the server's own fault by definition.
/// * `529` — not in any RFC, and used by more than one hosted model API to mean "overloaded".
///
/// `400`, `401`, `403`, `404`, `413` and `422` are deliberately absent. Each says something
/// about the request that sending it again will not change.
///
/// ```
/// use aik_api::resilience::transient_status;
///
/// assert!(transient_status(503));
/// assert!(!transient_status(401));
/// ```
pub fn transient_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// What a circuit breaker is currently doing about a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    /// Calls pass through. The normal state.
    Closed,
    /// Calls are refused without being attempted, because enough of them failed in a row.
    Open,
    /// One call is allowed through to find out whether the service came back.
    HalfOpen,
}

impl std::fmt::Display for CircuitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half-open",
        })
    }
}

/// A call to a model provider failed transiently and will be attempted again.
///
/// Counts and timings only, like every other event in this crate: which provider, which
/// model, which attempt, and how long the next one waits. Never the failure's message, which
/// is the one part of a provider error that can quote the request back — an API that echoes a
/// malformed field, a proxy that includes the URL it could not reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetryScheduled {
    /// The operation the call belongs to.
    pub correlation: CorrelationId,
    /// When the retry was scheduled, by the kernel clock.
    pub timestamp: Timestamp,
    /// The component whose calls are being retried.
    pub provider: ComponentId,
    /// The model the call was for.
    pub model: ModelId,
    /// Which attempt just failed, starting at 1.
    pub attempt: u32,
    /// How long the next attempt waits.
    pub delay_ms: u64,
    /// Whether [`delay_ms`](Self::delay_ms) came from the service's own `retry-after`
    /// rather than from the caller's backoff.
    pub honoured_retry_after: bool,
}

impl Event for ProviderRetryScheduled {
    const NAME: &'static str = "aik.resilience.retry_scheduled";
}

/// A provider's circuit breaker changed state.
///
/// Published on every transition, including the ones that are good news: a subscriber that
/// only heard about failures could not tell an upstream that recovered from one that is
/// still down and simply no longer being called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCircuitChanged {
    /// When the transition happened, by the kernel clock.
    pub timestamp: Timestamp,
    /// The component the breaker guards.
    pub provider: ComponentId,
    /// What it was doing before.
    pub from: CircuitState,
    /// What it is doing now.
    pub to: CircuitState,
    /// How many calls had failed in a row at the moment of the transition.
    pub consecutive_failures: u32,
}

impl Event for ProviderCircuitChanged {
    const NAME: &'static str = "aik.resilience.circuit_changed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marked_failure_is_found_through_the_wrapping_error() {
        let error = TransientFailure::new("HTTP 529").wrapped("calling the model");
        assert_eq!(error.to_string(), "calling the model");
        let failure = transient_failure(&error).expect("marked");
        assert_eq!(failure.to_string(), "HTTP 529");
        assert!(failure.retry_after().is_none());
    }

    #[test]
    fn a_marked_failure_is_found_below_another_wrapping() {
        let inner = TransientFailure::new("connection reset").wrapped("reaching the API");
        let outer = Error::wrap("taking a turn", inner);
        assert!(transient_failure(&outer).is_some());
    }

    #[test]
    fn the_original_failure_survives_being_marked() {
        let cause = std::io::Error::other("connection reset by peer");
        let error = TransientFailure::new("HTTP 502")
            .caused_by(cause)
            .wrapped("calling the model");

        let marked = std::error::Error::source(&error).expect("a source");
        assert_eq!(marked.to_string(), "HTTP 502");
        assert_eq!(
            marked.source().expect("the original").to_string(),
            "connection reset by peer"
        );
    }

    #[test]
    fn nothing_is_transient_unless_it_says_so() {
        assert!(transient_failure(&Error::other("no")).is_none());
        assert!(transient_failure(&Error::PermissionDenied("no".into())).is_none());
        assert!(transient_failure(&Error::InvalidArgument("no".into())).is_none());
        assert!(
            transient_failure(&Error::wrap("outer", Error::other("inner"))).is_none(),
            "wrapping alone must not imply transience"
        );
    }

    #[test]
    fn cancellation_and_expiry_are_never_transient_however_they_are_wrapped() {
        // The mechanism that produced them is the one a retry would defeat, so the check
        // is on the outermost error rather than on what somebody attached underneath it.
        let cancelled = Error::Cancelled;
        assert!(transient_failure(&cancelled).is_none());

        let expired = Error::Timeout(Duration::from_secs(1));
        assert!(transient_failure(&expired).is_none());
    }

    #[test]
    fn only_service_side_statuses_are_transient() {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            assert!(transient_status(status), "{status}");
        }
        for status in [200, 201, 400, 401, 403, 404, 413, 422, 451, 501] {
            assert!(!transient_status(status), "{status}");
        }
    }

    #[test]
    fn events_carry_no_failure_text() {
        let event = ProviderRetryScheduled {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(10),
            provider: ComponentId::new("model.anthropic"),
            model: ModelId::new("claude-opus-5"),
            attempt: 2,
            delay_ms: 750,
            honoured_retry_after: true,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("message").is_none());
        assert!(json.get("detail").is_none());
        assert!(json.get("error").is_none());

        let parsed: ProviderRetryScheduled = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn circuit_transitions_round_trip() {
        let event = ProviderCircuitChanged {
            timestamp: Timestamp::from_millis(10),
            provider: ComponentId::new("model.ollama"),
            from: CircuitState::Closed,
            to: CircuitState::Open,
            consecutive_failures: 5,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["to"], serde_json::json!("open"));
        let parsed: ProviderCircuitChanged = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn states_render_for_an_operator() {
        assert_eq!(CircuitState::HalfOpen.to_string(), "half-open");
        assert_eq!(CircuitState::Closed.to_string(), "closed");
        assert_eq!(CircuitState::Open.to_string(), "open");
    }
}
