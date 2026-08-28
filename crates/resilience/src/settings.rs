//! Configuration for the resilience layer.
//!
//! Every default here is chosen so that a deployment that says nothing gets the behaviour a
//! careful operator would have configured, and so that each mechanism can be turned off
//! individually without turning off the others. There is no `enabled` flag: a layer with
//! [`RetrySettings::max_attempts`] of 1, [`BreakerSettings::failure_threshold`] of 0 and
//! [`ResilienceSettings::max_concurrent`] of 0 is exactly a pass-through, and saying so in
//! three numbers is one fewer way for the configuration and the behaviour to disagree.

use serde::{Deserialize, Serialize};

const fn default_max_attempts() -> u32 {
    3
}

const fn default_base_delay_ms() -> u64 {
    500
}

const fn default_max_delay_ms() -> u64 {
    8_000
}

const fn default_max_retry_after_ms() -> u64 {
    60_000
}

const fn default_failure_threshold() -> u32 {
    5
}

const fn default_cooldown_ms() -> u64 {
    30_000
}

const fn default_max_concurrent() -> usize {
    4
}

const fn default_acquire_timeout_ms() -> u64 {
    30_000
}

/// When and how long to wait before sending the same request again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetrySettings {
    /// How many attempts a single call may make in total, including the first.
    ///
    /// `1` disables retrying. `0` is read as `1`: configuration can decline to retry, but
    /// cannot produce a provider that is never called at all.
    pub max_attempts: u32,
    /// The first attempt's backoff ceiling, doubling per attempt.
    pub base_delay_ms: u64,
    /// The largest backoff ceiling, however many attempts have failed.
    pub max_delay_ms: u64,
    /// The longest a service's own `retry-after` may park a call for.
    ///
    /// Separate from [`max_delay_ms`](Self::max_delay_ms), and larger by default, because
    /// the two bound different things: one bounds a guess this client made, the other bounds
    /// how far a client will let something upstream move its own schedule.
    pub max_retry_after_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            base_delay_ms: default_base_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
            max_retry_after_ms: default_max_retry_after_ms(),
        }
    }
}

/// When to stop calling a provider that keeps failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BreakerSettings {
    /// How many calls must fail transiently in a row before calls are refused outright.
    ///
    /// `0` disables the breaker.
    pub failure_threshold: u32,
    /// How long the breaker stays open before letting one call through to test the service.
    pub cooldown_ms: u64,
}

impl Default for BreakerSettings {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

/// Everything the resilience layer is configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResilienceSettings {
    /// When and how long to wait before repeating a transiently failed call.
    pub retry: RetrySettings,
    /// When to stop calling a provider that keeps failing.
    pub breaker: BreakerSettings,
    /// How many calls to the provider may be in flight at once.
    ///
    /// `0` means unlimited. The default is deliberately small: the failure this prevents is
    /// a scheduler firing several agent jobs at the same minute, each of which then retries,
    /// which is how a client turns one slow upstream into a rate limit of its own making.
    pub max_concurrent: usize,
    /// How long a call may wait for a slot before giving up.
    ///
    /// Bounded so that a saturated provider surfaces as a failure rather than as a request
    /// that never returns. A caller's own deadline still wins when it is shorter.
    pub acquire_timeout_ms: u64,
}

impl Default for ResilienceSettings {
    fn default() -> Self {
        Self {
            retry: RetrySettings::default(),
            breaker: BreakerSettings::default(),
            max_concurrent: default_max_concurrent(),
            acquire_timeout_ms: default_acquire_timeout_ms(),
        }
    }
}

impl ResilienceSettings {
    /// Settings that change nothing: one attempt, no breaker, no concurrency limit.
    ///
    /// The behaviour a deployment gets with no resilience layer at all, expressed as
    /// settings so a test — or an operator narrowing a problem down — can turn the layer
    /// into a pass-through without removing it from the wiring.
    pub const fn pass_through() -> Self {
        Self {
            retry: RetrySettings {
                max_attempts: 1,
                base_delay_ms: 0,
                max_delay_ms: 0,
                max_retry_after_ms: 0,
            },
            breaker: BreakerSettings {
                failure_threshold: 0,
                cooldown_ms: 0,
            },
            max_concurrent: 0,
            acquire_timeout_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_retry_a_little_and_bound_concurrency() {
        let settings = ResilienceSettings::default();
        assert_eq!(settings.retry.max_attempts, 3);
        assert_eq!(settings.retry.base_delay_ms, 500);
        assert_eq!(settings.breaker.failure_threshold, 5);
        assert_eq!(settings.max_concurrent, 4);
    }

    #[test]
    fn an_empty_section_is_the_default() {
        let settings: ResilienceSettings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(settings, ResilienceSettings::default());
    }

    #[test]
    fn one_mechanism_can_be_turned_off_without_the_others() {
        let settings: ResilienceSettings =
            serde_json::from_value(json!({ "breaker": { "failure_threshold": 0 } })).unwrap();
        assert_eq!(settings.breaker.failure_threshold, 0);
        assert_eq!(settings.retry, RetrySettings::default());
        assert_eq!(settings.max_concurrent, 4);
    }

    #[test]
    fn a_misspelled_key_is_refused_rather_than_ignored() {
        // The failure this exists to prevent: a deployment that configured a ceiling,
        // misspelled it, and was told nothing.
        let error = serde_json::from_value::<ResilienceSettings>(json!({ "max_concurent": 2 }))
            .unwrap_err();
        assert!(error.to_string().contains("max_concurent"), "{error}");
    }

    #[test]
    fn a_pass_through_is_expressible_in_configuration() {
        let settings: ResilienceSettings = serde_json::from_value(json!({
            "retry": { "max_attempts": 1 },
            "breaker": { "failure_threshold": 0 },
            "max_concurrent": 0,
        }))
        .unwrap();
        assert_eq!(settings.retry.max_attempts, 1);
        assert_eq!(settings.breaker.failure_threshold, 0);
        assert_eq!(settings.max_concurrent, 0);
    }

    #[test]
    fn the_pass_through_constant_agrees_with_what_it_documents() {
        let settings = ResilienceSettings::pass_through();
        assert_eq!(settings.retry.max_attempts, 1);
        assert_eq!(settings.breaker.failure_threshold, 0);
        assert_eq!(settings.max_concurrent, 0);
    }
}
