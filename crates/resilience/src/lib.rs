//! Keeping a model provider's bad day from becoming the deployment's.
//!
//! Everything a [`ModelProvider`](aik_api::model::ModelProvider) does crosses a boundary this
//! process does not control — a socket to a local server, or a request to a service on the
//! other side of the internet. Those fail in ways that have nothing to do with whether the
//! request was any good: a rate limit, an overloaded upstream, a connection cut before a
//! status arrived, a server restarting. Until now a single one of those ended a run, and a
//! run that ended took its transcript's worth of assembled context with it.
//!
//! This crate is one [`ModelProvider`](aik_api::model::ModelProvider) that wraps another and
//! applies three mechanisms to
//! every call. It is registered *on* the kernel like every other capability here, not into
//! it, and it changes nothing about the contract: whatever resolves `dyn ModelProvider` gets
//! something that behaves the same way and fails less often.
//!
//! # The three mechanisms, and the failure each one answers
//!
//! * **Retry** answers one failure. A rate limit or a 503 is usually over by the time a
//!   client has waited a second, and the request that provoked it is unchanged.
//! * **A circuit breaker** answers the hundredth. A provider that is down does not come back
//!   because more requests arrived; retrying into it converts one outage into a queue of
//!   calls that each take several seconds to fail.
//! * **A concurrency limit** answers the failure a client causes itself. A scheduler firing
//!   several agent jobs on the same minute, each retrying, is how a deployment manufactures
//!   its own rate limit.
//!
//! # What decides that a failure may be repeated
//!
//! [`aik_api::resilience::TransientFailure`], and nothing else — not a status code read here,
//! not a message matched here. Providers classify their own failures because they are the only
//! code that can; everything unclassified is terminal. That is the fail-closed direction: the
//! cost of not retrying something that would have worked is one failed run, and the cost of
//! retrying a request a service refused on its merits is that same refusal several times over,
//! paid for each time.
//!
//! # Spending, and why nothing here charges for it
//!
//! A retried call can cost an upstream real work whose result never arrived, and this crate
//! deliberately does not invent a figure for that. What it does instead is keep the count
//! bounded and the accounting honest: retrying happens strictly *below* the point where a
//! response exists, so the [`QuotaGuard`](aik_api::quota::QuotaGuard) above charges exactly
//! once per turn no matter how many attempts that turn took, and
//! [`RetrySettings::max_attempts`] is the stated bound on how many that can be.
//!
//! ```
//! use aik_core::prelude::*;
//! use aik_resilience::{ResilienceComponent, ResilienceSettings};
//!
//! # fn build(builder: KernelBuilder) -> Result<Kernel> {
//! builder
//!     .component(
//!         ResilienceComponent::new(ResilienceSettings::default()).wrapping("model.ollama"),
//!     )
//!     .build()
//! # }
//! ```

mod backoff;
mod breaker;
mod component;
mod limit;
mod provider;
mod settings;
mod stream;

pub use breaker::CircuitBreaker;
pub use component::{DEFAULT_COMPONENT_ID, ResilienceComponent};
pub use provider::ResilientProvider;
pub use settings::{BreakerSettings, ResilienceSettings, RetrySettings};
