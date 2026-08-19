//! Observational request/token measurement.
//!
//! This module adds exactly one thing to the audit surface [`crate::audit`] already
//! provides: what a request to a [`ModelProvider`](crate::model::ModelProvider) actually
//! cost, broken down by where the cost came from. It reuses the same mechanism — a
//! serialisable [`Event`] published on the kernel [`EventBus`](aik_core::EventBus) — rather
//! than inventing a second one, for the same reason [`crate::audit`] gives: any subscriber,
//! in or out of process, gets it for free.
//!
//! # Why this exists alongside [`ContextAssembled`](crate::context::ContextAssembled)
//!
//! [`ContextAssembled`](crate::context::ContextAssembled) is published by a
//! [`ContextStore`](crate::context::ContextStore) and reports exactly what that store
//! knows: how many of *its own* records made it into a window, and at what cost. It
//! structurally cannot report two things that dominate a real request:
//!
//! * **tool-definition cost.** [`ToolDefinition`](crate::model::ToolDefinition)s are
//!   attached to a [`CompletionRequest`](crate::model::CompletionRequest) by whatever
//!   assembles it — the agent loop, not the context store — and are never themselves a
//!   [`ContextRecord`](crate::context::ContextRecord). A context store has no way to know
//!   they exist.
//! * **provider-reported usage and latency.** Both come back from
//!   [`ModelProvider::complete`](crate::model::ModelProvider::complete), which the context
//!   store never calls.
//!
//! [`RequestMeasured`] is published by whatever *does* assemble the request and call the
//! provider — the agent loop — once per model turn, and is a superset of the same
//! philosophy: counts only, never conversation content, and it changes nothing about how
//! the request is built or answered. It is not a competing mechanism; it is the other half
//! of the same picture, filled in by the only code that can see it.
//!
//! # What is exact, and what is estimated
//!
//! Nothing in [`RequestEstimate`] is a provider's real tokenizer count. It is produced by
//! whichever [`TokenCounter`](crate::context::TokenCounter) the run was given — the
//! documented byte-length heuristic by default — applied to the same request payload that
//! was actually sent. [`RequestMeasured::provider_usage`] is the one field in this event
//! that *is* exact, when the provider reports it: it is
//! [`CompletionResponse::usage`](crate::model::CompletionResponse::usage), copied through
//! unchanged. See [`RequestEstimate`]'s field documentation for exactly what each estimated
//! number does and does not include.

use aik_core::Event;
use aik_core::clock::Timestamp;
use aik_core::id::CorrelationId;
use serde::{Deserialize, Serialize};

use crate::agent::SessionId;
use crate::context::ContextUsage;
use crate::model::{ModelId, Usage};

/// A locally estimated breakdown of what one request to a model provider cost, in tokens.
///
/// Every field is produced by a [`TokenCounter`](crate::context::TokenCounter) and is
/// therefore an estimate, not a provider's real count — see the
/// [module documentation](self#what-is-exact-and-what-is-estimated). Fields do not overlap:
/// summing [`RequestEstimate::system_tokens`], [`RequestEstimate::conversation_tokens`] and
/// [`RequestEstimate::tool_definition_tokens`] gives
/// [`RequestEstimate::total_tokens`] exactly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEstimate {
    /// Estimated cost of every pinned system/instruction message in the window sent this
    /// turn (typically just the system prompt).
    pub system_tokens: u64,
    /// Estimated cost of every non-system message in the window sent this turn: prior user
    /// turns, assistant replies, tool calls and tool results still inside the budget.
    ///
    /// Includes [`RequestEstimate::tool_call_tokens`] and
    /// [`RequestEstimate::tool_result_tokens`] below — they are named separately because
    /// they are usually the fastest-growing part of a long tool-using conversation, not
    /// because they are excluded from this total.
    pub conversation_tokens: u64,
    /// How much of [`RequestEstimate::conversation_tokens`] is the current turn's fresh
    /// user input, if any was appended this turn.
    ///
    /// A run appends the caller's input exactly once, during
    /// [`Run::prepare`](https://docs.rs/aik-agent) — the first model turn of a run. Later
    /// turns of the same run see no *new* user text, only tool results, so this is `None`
    /// for every turn after the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_input_tokens: Option<u64>,
    /// How much of [`RequestEstimate::conversation_tokens`] is
    /// [`ContentPart::ToolCall`](crate::model::ContentPart::ToolCall) parts: the model's
    /// own record of tools it asked for, replayed back to it on every later turn.
    pub tool_call_tokens: u64,
    /// How much of [`RequestEstimate::conversation_tokens`] is
    /// [`ContentPart::ToolResult`](crate::model::ContentPart::ToolResult) parts.
    ///
    /// This is the figure that grows fastest in a long, tool-heavy conversation: a result
    /// already seen is resent verbatim on every later turn until it is elided or evicted —
    /// see [`ContextUsage`] for how eviction accounts for it.
    pub tool_result_tokens: u64,
    /// Estimated cost of the [`ToolDefinition`](crate::model::ToolDefinition)s attached to
    /// this request, however many tools the run currently offers.
    ///
    /// Sent in full on every turn regardless of whether any tool is called — see the
    /// [module documentation](self) for why this is invisible to
    /// [`ContextAssembled`](crate::context::ContextAssembled) and has to be measured here
    /// instead.
    pub tool_definition_tokens: u64,
    /// How many tools were offered (i.e. `request.tools.len()`), for correlating a jump in
    /// [`RequestEstimate::tool_definition_tokens`] with a change in the run's tool set.
    pub tools_offered: usize,
    /// The full estimated request size: `system_tokens + conversation_tokens +
    /// tool_definition_tokens`.
    pub total_tokens: u64,
}

/// One request/response cycle with a model provider, measured.
///
/// Published once per model turn — once per
/// [`ModelProvider::complete`](crate::model::ModelProvider::complete) call the agent loop
/// makes — carrying only counts, timings and identifiers, never message content. See the
/// [module documentation](self) for why this exists alongside
/// [`ContextAssembled`](crate::context::ContextAssembled) rather than replacing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestMeasured {
    /// The operation this measurement belongs to.
    pub correlation: CorrelationId,
    /// When the turn finished, by the kernel clock.
    pub timestamp: Timestamp,
    /// Which session the turn belongs to.
    pub session: SessionId,
    /// Which model answered.
    pub model: ModelId,
    /// This run's turn number, starting at 1.
    pub turn: usize,
    /// How many tool calls the run has made in total, up to and including this turn.
    pub cumulative_tool_calls: usize,
    /// The locally estimated breakdown of what was sent.
    pub estimate: RequestEstimate,
    /// What [`ContextStore::window`](crate::context::ContextStore::window) reported for
    /// the window this request was built from.
    ///
    /// Included here so a subscriber does not have to correlate a separate
    /// [`ContextAssembled`](crate::context::ContextAssembled) event by hand to see both
    /// halves of one turn's cost.
    pub context: ContextUsage,
    /// What the provider reported for *this* turn, if it reports usage at all.
    ///
    /// The one field on this event that is not a local estimate — see the
    /// [module documentation](self).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_usage: Option<Usage>,
    /// Provider-reported usage summed across every turn of this run so far, if the
    /// provider reports usage at all. `None` until the first turn that reports it, and
    /// still exact when present — it is a sum of exact figures, not an estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_provider_usage: Option<Usage>,
    /// How long [`ModelProvider::complete`](crate::model::ModelProvider::complete) took
    /// for this turn, in milliseconds. A locally measured wall-clock duration
    /// (`std::time::Instant`), not a provider-reported figure — no provider in this
    /// codebase reports its own processing time.
    pub model_latency_ms: u64,
}

impl Event for RequestMeasured {
    const NAME: &'static str = "aik.measurement.request";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn estimate() -> RequestEstimate {
        RequestEstimate {
            system_tokens: 10,
            conversation_tokens: 20,
            user_input_tokens: Some(5),
            tool_call_tokens: 0,
            tool_result_tokens: 0,
            tool_definition_tokens: 400,
            tools_offered: 2,
            total_tokens: 430,
        }
    }

    fn event() -> RequestMeasured {
        RequestMeasured {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1_000),
            session: SessionId::new(),
            model: ModelId::new("llama3.1:8b"),
            turn: 1,
            cumulative_tool_calls: 0,
            estimate: estimate(),
            context: ContextUsage::default(),
            provider_usage: Some(Usage {
                input_tokens: 423,
                output_tokens: 12,
            }),
            cumulative_provider_usage: Some(Usage {
                input_tokens: 423,
                output_tokens: 12,
            }),
            model_latency_ms: 850,
        }
    }

    #[test]
    fn a_component_estimate_sums_to_the_reported_total() {
        let estimate = estimate();
        assert_eq!(
            estimate.system_tokens + estimate.conversation_tokens + estimate.tool_definition_tokens,
            estimate.total_tokens
        );
    }

    #[test]
    fn measurement_events_round_trip_and_carry_no_content() {
        let event = event();
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("message").is_none());
        assert!(json.get("content").is_none());
        assert!(json.get("messages").is_none());
        assert_eq!(json["model_latency_ms"], json!(850));

        let parsed: RequestMeasured = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn absent_provider_usage_is_omitted_rather_than_null() {
        let event = RequestMeasured {
            provider_usage: None,
            cumulative_provider_usage: None,
            ..event()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("provider_usage").is_none());
        assert!(json.get("cumulative_provider_usage").is_none());
    }

    #[test]
    fn user_input_tokens_is_absent_after_the_first_turn() {
        let event = RequestMeasured {
            estimate: RequestEstimate {
                user_input_tokens: None,
                ..estimate()
            },
            turn: 2,
            ..event()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json["estimate"].get("user_input_tokens").is_none());
    }
}
