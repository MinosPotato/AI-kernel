//! What a run is allowed to spend, and what it is told before it starts.

use aik_api::context::ContextBudget;
use aik_api::model::ModelId;
use serde_json::Value;

/// How many model turns one run may take before it is stopped.
///
/// A turn is one [`ModelProvider::complete`](aik_api::model::ModelProvider::complete) call.
/// The loop terminates on its own as soon as a model answers without asking for a tool, so
/// this bound exists for the case where it never does: two tools that keep pointing at each
/// other, a model that re-issues the same call because it dislikes the result, a prompt that
/// makes "call a tool" the likeliest continuation forever. Without a ceiling that is an
/// unbounded spend of money, tokens and — since every tool call is authorized and possibly
/// approved — a person's attention.
pub const DEFAULT_MAX_TURNS: usize = 16;

/// How many tools one run may invoke in total.
///
/// Separate from [`DEFAULT_MAX_TURNS`] because a single turn can ask for many tools at once:
/// bounding turns alone bounds the number of model calls, not the amount of work the loop
/// performs between them.
pub const DEFAULT_MAX_TOOL_CALLS: usize = 64;

/// The most tokens a run's model window may cost, by default.
///
/// Deliberately conservative. A window that overruns the model's real context window fails
/// the request outright, and the loop cannot know that window — it is provider- and
/// model-specific, and [`ModelDescriptor::context_window`](aik_api::model::ModelDescriptor)
/// is optional. A caller that knows better raises it.
pub const DEFAULT_MAX_WINDOW_TOKENS: u64 = 8_192;

/// The most tokens any single content part may cost before the window elides it, by default.
///
/// This is what keeps one large tool result — a file, a directory listing — from evicting
/// the conversation around it. The full value stays in the
/// [`ContextStore`](aik_api::context::ContextStore) and stays retrievable by record id.
pub const DEFAULT_MAX_PART_TOKENS: u64 = 1_024;

/// Everything about a run that is decided before the conversation starts.
///
/// Every field here is *trusted execution metadata*: it comes from whoever assembled the
/// agent, is fixed for the whole run, and is never derived from a model's output. That is
/// the point — the model influences what is said, never how much may be spent saying it,
/// which model answers, or when to stop.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentLoopSettings {
    /// The model each turn is sent to.
    pub model: ModelId,
    /// The budget applied to the window assembled for every turn.
    pub budget: ContextBudget,
    /// How many model turns the run may take. See [`DEFAULT_MAX_TURNS`].
    pub max_turns: usize,
    /// How many tools the run may invoke in total. See [`DEFAULT_MAX_TOOL_CALLS`].
    pub max_tool_calls: usize,
    /// Instructions appended, pinned, as the first record of a session.
    ///
    /// Appended once per session rather than once per run, so continuing a conversation does
    /// not accumulate copies of it. Pinning is set here, by trusted code, and never from
    /// anything a model produced — see
    /// [`ContextEntry::pinned`](aik_api::context::ContextEntry::pinned).
    pub system_prompt: Option<String>,
    /// Provider-specific completion settings, passed through unchanged.
    pub parameters: Value,
}

impl AgentLoopSettings {
    /// Settings for `model`, with the default bounds and no system prompt.
    pub fn new(model: impl Into<ModelId>) -> Self {
        Self {
            model: model.into(),
            budget: ContextBudget::tokens(DEFAULT_MAX_WINDOW_TOKENS)
                .with_max_part_tokens(DEFAULT_MAX_PART_TOKENS),
            max_turns: DEFAULT_MAX_TURNS,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            system_prompt: None,
            parameters: Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bound_both_turns_and_tool_calls() {
        let settings = AgentLoopSettings::new("demo");
        assert_eq!(settings.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(settings.max_tool_calls, DEFAULT_MAX_TOOL_CALLS);
        assert!(settings.max_turns > 0 && settings.max_tool_calls > 0);
    }

    #[test]
    fn the_default_budget_bounds_both_the_window_and_any_single_part() {
        let settings = AgentLoopSettings::new("demo");
        assert_eq!(settings.budget.max_tokens, Some(DEFAULT_MAX_WINDOW_TOKENS));
        assert_eq!(
            settings.budget.max_part_tokens,
            Some(DEFAULT_MAX_PART_TOKENS)
        );
    }

    #[test]
    fn nothing_is_pinned_unless_a_system_prompt_is_configured() {
        assert!(AgentLoopSettings::new("demo").system_prompt.is_none());
    }
}
