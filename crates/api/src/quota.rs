//! Cumulative spend limits.
//!
//! [`AgentLoopSettings`](https://docs.rs/aik-agent) already bounds one run: so many model
//! turns, so many tool calls, so large a window. Those bounds are per run, and they reset
//! with it. Nothing in them stops the same principal from starting another run, and nothing
//! at all stops a [scheduled](crate::scheduler) job from starting one every minute for a
//! week. A per-run ceiling answers "how far can this conversation go?"; it structurally
//! cannot answer "how much may this principal spend?", because it has no memory between
//! runs.
//!
//! [`QuotaGuard`] is that memory. It is consulted before a model turn is taken and told what
//! the turn cost afterwards, and it accumulates across runs, across sessions and — for a
//! durable implementation — across restarts.
//!
//! # What this is not
//!
//! It is not authorization. [`PolicyEngine`](crate::permission::PolicyEngine) answers
//! whether an action is permitted at all; a quota answers whether there is any budget left
//! for one that already is. The two are independent and both apply: a principal with no
//! policy rule allowing a tool cannot use it however much budget it has, and a principal
//! with budget exhausted cannot take a turn however permissive the policy is.
//!
//! It is also not [measurement](crate::measurement).
//! [`RequestMeasured`](crate::measurement::RequestMeasured) reports what a turn cost to
//! whoever is listening and changes nothing; a guard is asked, and can refuse.
//!
//! # The overshoot this design accepts
//!
//! What a turn costs is only knowable once it has been taken, so a guard is checked before
//! and told after. A principal can therefore end a period at most *one turn* over its
//! ceiling — the turn that crossed it. Reserving an estimate up front instead would replace
//! a bounded, explicable overshoot with a systematic over- or under-charge, since the
//! estimate is a heuristic ([`TokenCounter`](crate::context::TokenCounter)) and the real
//! figure comes back with the response. The bound is documented rather than hidden: set the
//! ceiling where one turn of slack is acceptable.
//!
//! # Attribution
//!
//! A charge is attributed from the [`ExecutionContext`], never from anything a model
//! produced — the same rule the rest of the kernel follows. An implementation is expected to
//! charge both the acting principal and, when the action is delegated, whoever it acts for:
//! a ceiling written for a person should hold however many agents do that person's work, and
//! a ceiling written for an autonomous identity should hold whoever it is acting for.

use aik_core::Result;
use aik_core::clock::Timestamp;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::execution::ExecutionContext;
use crate::model::ModelId;
use crate::permission::PrincipalId;

/// What one completed model turn cost.
///
/// Reported by whatever took the turn, once it is over. The token figures are the
/// provider's own when it reports usage and a local estimate when it does not — which one
/// they are is stated by [`UsageCharge::estimated`], because a ledger that could not tell
/// the difference would report a number nobody could check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCharge {
    /// Which model answered. A guard prices the tokens with it; it is never used to decide
    /// who is charged.
    pub model: ModelId,
    /// How many model turns this charge covers. Always exact.
    pub turns: u64,
    /// What went to the model.
    pub input_tokens: u64,
    /// What came back.
    pub output_tokens: u64,
    /// Whether the token figures are a local estimate rather than the provider's own count.
    ///
    /// A provider that reports no usage would otherwise charge zero tokens for every turn,
    /// which would make a token or cost ceiling silently unreachable — so the caller
    /// substitutes its own estimate and says so here.
    pub estimated: bool,
}

impl UsageCharge {
    /// One turn, with figures the provider reported itself.
    pub fn turn(model: impl Into<ModelId>, input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            model: model.into(),
            turns: 1,
            input_tokens,
            output_tokens,
            estimated: false,
        }
    }

    /// Marks the token figures as a local estimate. See [`UsageCharge::estimated`].
    #[must_use]
    pub fn as_estimate(mut self) -> Self {
        self.estimated = true;
        self
    }

    /// Input plus output, saturating.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// One thing a quota can be set on.
///
/// Fixed rather than open-ended, because each variant is a field of [`UsageCharge`] or a sum
/// of them: a dimension nothing reports could never be enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaDimension {
    /// Model turns taken.
    Turns,
    /// Tokens sent to a model.
    InputTokens,
    /// Tokens received from a model.
    OutputTokens,
    /// Both together.
    TotalTokens,
    /// Millionths of a currency unit, priced from the tokens by the implementation.
    ///
    /// No currency is named here: an operator prices models in whatever unit it bills in,
    /// and the kernel neither knows nor converts.
    CostMicros,
}

impl std::fmt::Display for QuotaDimension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Turns => "model turns",
            Self::InputTokens => "input tokens",
            Self::OutputTokens => "output tokens",
            Self::TotalTokens => "tokens",
            Self::CostMicros => "cost (micros)",
        })
    }
}

/// One ceiling that applies to a principal, and how much of it is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaStatus {
    /// The identity the counter belongs to — the actor, or whoever it acts for.
    pub subject: PrincipalId,
    /// The accounting window this ceiling is measured over, as the implementation names it,
    /// e.g. `day:2026-08-28`.
    pub window: String,
    /// What is being counted.
    pub dimension: QuotaDimension,
    /// How much has been used in this window.
    pub used: u64,
    /// The ceiling.
    pub limit: u64,
    /// When the window closes and the counter starts again, if it ever does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
}

impl QuotaStatus {
    /// How much of the ceiling is left.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// Whether this ceiling has been reached.
    pub fn exhausted(&self) -> bool {
        self.used >= self.limit
    }
}

/// A cumulative ceiling on what a principal may spend on models.
///
/// # The contract an implementation owes
///
/// * [`check`](QuotaGuard::check) **fails closed**. A ledger that cannot be read, a model
///   that cannot be priced when a cost ceiling applies, a window that cannot be computed:
///   each is a refusal, never a pass. A guard whose failure mode is "allow" is not a limit.
/// * [`check`](QuotaGuard::check) refuses with an [`Error::PermissionDenied`](aik_core::Error::PermissionDenied),
///   so a caller can tell an exhausted budget from a broken one.
/// * [`record`](QuotaGuard::record) is **not** best-effort. A caller that cannot record what
///   it spent must stop, because the alternative is unbounded spend with no account of it.
/// * Neither method reads anything a model produced. The principal comes from the
///   [`ExecutionContext`], the figures from the provider or the caller's own counter.
#[async_trait]
pub trait QuotaGuard: Send + Sync + 'static {
    /// Refuses if any ceiling that applies to `cx`'s principal is already reached.
    ///
    /// `model` is the model the caller is about to use. It is needed before the fact
    /// because a cost ceiling cannot be enforced against a model the deployment has not
    /// priced, and discovering that after the request would be discovering it too late.
    async fn check(&self, model: &ModelId, cx: &ExecutionContext) -> Result<()>;

    /// Adds `charge` to every counter that applies to `cx`'s principal.
    async fn record(&self, charge: &UsageCharge, cx: &ExecutionContext) -> Result<()>;

    /// Every ceiling that applies to `cx`'s principal, with what it has used.
    ///
    /// For an operator, and for a frontend that wants to say why it stopped. Reporting is
    /// not enforcement: nothing here refuses anything.
    async fn status(&self, cx: &ExecutionContext) -> Result<Vec<QuotaStatus>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_is_exact_until_it_is_marked_otherwise() {
        let charge = UsageCharge::turn("llama3.1:8b", 100, 20);
        assert!(!charge.estimated);
        assert_eq!(charge.turns, 1);
        assert_eq!(charge.total_tokens(), 120);
        assert!(charge.as_estimate().estimated);
    }

    #[test]
    fn totals_saturate_rather_than_wrapping() {
        let charge = UsageCharge::turn("m", u64::MAX, 10);
        assert_eq!(charge.total_tokens(), u64::MAX);
    }

    #[test]
    fn a_status_reports_what_is_left() {
        let status = QuotaStatus {
            subject: PrincipalId::new("alice"),
            window: "day:2026-08-28".into(),
            dimension: QuotaDimension::Turns,
            used: 3,
            limit: 10,
            resets_at: Some(Timestamp::from_millis(1_000)),
        };
        assert_eq!(status.remaining(), 7);
        assert!(!status.exhausted());

        let spent = QuotaStatus { used: 11, ..status };
        assert_eq!(spent.remaining(), 0);
        assert!(spent.exhausted());
    }

    #[test]
    fn charges_round_trip_through_json() {
        let charge = UsageCharge::turn("m", 1, 2).as_estimate();
        let json = serde_json::to_value(&charge).unwrap();
        let parsed: UsageCharge = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, charge);
    }
}
