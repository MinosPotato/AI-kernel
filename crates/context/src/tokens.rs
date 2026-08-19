//! [`HeuristicTokenCounter`]: the default, provider-neutral size estimate.

use aik_api::context::TokenCounter;

/// The divisor used when none is configured.
///
/// Four bytes per token is the usual rule of thumb for English text in byte-pair
/// vocabularies. It is not accurate for code, for CJK text, or for any particular model —
/// see [`HeuristicTokenCounter`] for why that is acceptable here and what to do when it is
/// not.
pub const DEFAULT_BYTES_PER_TOKEN: u64 = 4;

/// A [`TokenCounter`] that estimates from byte length alone.
///
/// This exists so that budgeting works out of the box, in every deployment, without the
/// kernel acquiring a tokenizer, a vocabulary file, or a dependency that has to be kept in
/// step with a model release. It satisfies the trait's obligations exactly: monotonic over
/// prefixes (byte length is), deterministic, and cheap.
///
/// # When this is good enough, and when it is not
///
/// Budgeting compares estimates against a limit the caller chose. A systematic bias in the
/// estimate is absorbed by choosing a correspondingly conservative limit, so an approximate
/// counter still produces correct *relative* decisions: which records are the expensive
/// ones, how much a directory listing costs next to a file read, whether a session is
/// growing. What it cannot do is tell you precisely how close you are to a specific model's
/// hard context limit.
///
/// A deployment that needs that registers its own [`TokenCounter`] for the provider it uses
/// — the capability is resolved from the kernel registry, so nothing else changes.
///
/// Byte length rather than character count is deliberate: it over-estimates multi-byte
/// text, and [`TokenCounter`] asks implementations to err high, because over-counting
/// spends budget the caller had while under-counting overruns a real context window.
#[derive(Debug, Clone, Copy)]
pub struct HeuristicTokenCounter {
    bytes_per_token: u64,
}

impl Default for HeuristicTokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl HeuristicTokenCounter {
    /// Creates a counter using [`DEFAULT_BYTES_PER_TOKEN`].
    pub const fn new() -> Self {
        Self {
            bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
        }
    }

    /// Uses a different divisor.
    ///
    /// A divisor of zero is treated as one, so that a misconfiguration over-estimates
    /// rather than dividing by zero or silently disabling the budget.
    #[must_use]
    pub const fn with_bytes_per_token(mut self, bytes_per_token: u64) -> Self {
        self.bytes_per_token = if bytes_per_token == 0 {
            1
        } else {
            bytes_per_token
        };
        self
    }

    /// The divisor in use.
    pub const fn bytes_per_token(&self) -> u64 {
        self.bytes_per_token
    }
}

impl TokenCounter for HeuristicTokenCounter {
    fn count_text(&self, text: &str) -> u64 {
        (text.len() as u64).div_ceil(self.bytes_per_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::model::{ContentPart, Message, Role};
    use aik_api::tool::{ToolCall, ToolName};
    use serde_json::json;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(HeuristicTokenCounter::new().count_text(""), 0);
    }

    #[test]
    fn partial_tokens_round_up() {
        let counter = HeuristicTokenCounter::new();
        assert_eq!(counter.count_text("a"), 1);
        assert_eq!(counter.count_text("abcd"), 1);
        assert_eq!(counter.count_text("abcde"), 2);
    }

    #[test]
    fn counts_are_monotonic_over_prefixes() {
        let counter = HeuristicTokenCounter::new();
        let text = "the quick brown fox jumps over the lazy dog";
        let mut previous = 0;
        for (index, _) in text.char_indices() {
            let current = counter.count_text(&text[..index]);
            assert!(current >= previous, "count decreased at byte {index}");
            previous = current;
        }
    }

    #[test]
    fn a_zero_divisor_is_clamped_rather_than_dividing_by_zero() {
        let counter = HeuristicTokenCounter::new().with_bytes_per_token(0);
        assert_eq!(counter.bytes_per_token(), 1);
        assert_eq!(counter.count_text("abcd"), 4);
    }

    #[test]
    fn tool_results_are_counted_through_their_json() {
        let counter = HeuristicTokenCounter::new();
        let part = ContentPart::ToolResult {
            call_id: "1".into(),
            content: json!({ "path": "a.txt" }),
            is_error: false,
        };
        assert_eq!(
            counter.count_part(&part),
            counter.count_text(r#"{"path":"a.txt"}"#)
        );
    }

    #[test]
    fn messages_include_framing_overhead() {
        let counter = HeuristicTokenCounter::new();
        let message = Message::text(Role::User, "abcd");
        assert_eq!(
            counter.count_message(&message),
            aik_api::context::MESSAGE_OVERHEAD_TOKENS + 1
        );
    }

    #[test]
    fn tool_calls_count_their_name_and_arguments() {
        let counter = HeuristicTokenCounter::new();
        let part = ContentPart::ToolCall(ToolCall {
            call_id: "1".into(),
            name: ToolName::new("filesystem.read"),
            arguments: json!({ "path": "a.txt" }),
        });
        assert_eq!(
            counter.count_part(&part),
            counter.count_text("filesystem.read") + counter.count_text(r#"{"path":"a.txt"}"#)
        );
    }
}
