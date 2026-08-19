//! Printing what happened, safely.
//!
//! # Why a terminal frontend needs a sanitiser
//!
//! Everything an agent produces is untrusted: the assistant's prose, the arguments it
//! invents for a tool call, and the bytes a tool read out of a file it was allowed to read.
//! A terminal interprets some of those bytes as commands. A model that emits `\x1b[2J` or a
//! run of `\r` can erase what is on screen, move the cursor over text already printed, or
//! repaint a line — which in a program that also prints *approval prompts* means untrusted
//! output could forge or overwrite the question a person is about to answer.
//!
//! So every untrusted string is passed through [`safe`] before it reaches the terminal, and
//! the escape character has no way through. Trusted text — the frontend's own labels, and
//! the prompt a policy engine authored — is printed as-is.

use std::fmt::Write as _;

use aik_api::agent::AgentUpdate;
use aik_api::audit::{AuthorizationDecided, AuthorizationOutcome, ToolInvoked};
use aik_api::context::ContextAssembled;
use aik_api::measurement::RequestMeasured;
use aik_api::model::{ContentPart, Usage};
use serde_json::Value;

/// How much of a tool's output or arguments is shown before it is cut short.
///
/// Display only: the full value is in the transcript either way. A tool that reads a
/// megabyte should not repaint the terminal with it.
pub const MAX_INLINE: usize = 240;

/// Rewrites a string so a terminal cannot be driven by it.
///
/// Control characters are replaced with their escaped form rather than dropped, so that
/// text which *contains* them still reads as suspicious instead of silently changing
/// meaning. Newline and tab survive, because they are the two a terminal treats as layout
/// rather than as a command.
pub fn safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' | '\t' => out.push(character),
            // C0, DEL and the C1 range, which some terminals accept as escape introducers.
            control if control.is_control() || ('\u{80}'..='\u{9f}').contains(&control) => {
                let _ = write!(out, "\\u{{{:04x}}}", control as u32);
            }
            other => out.push(other),
        }
    }
    out
}

/// Renders an untrusted JSON value on one line, sanitised and cut to [`MAX_INLINE`].
pub fn inline(value: &Value) -> String {
    let rendered = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let rendered = safe(&rendered).replace('\n', " ");
    truncate(&rendered)
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_INLINE {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_INLINE).collect();
    format!("{kept}… ({} characters total)", text.chars().count())
}

/// What one turn cost, accumulated from events as it runs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TurnStats {
    /// Tool calls the agent asked for.
    pub tool_calls: usize,
    /// Tokens the last assembled window cost.
    pub window_tokens: u64,
    /// Records the last assembled window left out.
    pub dropped_records: usize,
    /// Model turns taken, counted by windows assembled.
    pub turns: usize,
}

impl TurnStats {
    /// Folds one assembled window into the turn's running totals.
    ///
    /// One window is assembled per model turn, so counting them counts turns without the
    /// frontend having to track the loop's own progress.
    ///
    /// Note this can double-count turns against [`RequestMeasured`]: both are published
    /// once per model turn, from different subsystems, and a caller that folds in both
    /// should only take `turns` from one of them. [`Session`](crate::session::Session)
    /// takes it from here, since [`ContextAssembled`] existed first and callers may already
    /// depend on this count.
    pub fn record(&mut self, event: &ContextAssembled) {
        self.turns += 1;
        self.window_tokens = event.usage.included_tokens;
        self.dropped_records = event.usage.dropped_records;
    }
}

/// What a whole session has cost so far, accumulated across every turn of every prompt
/// answered in it.
///
/// Distinct from [`TurnStats`], which is reset at the start of every prompt: this is the
/// "cumulative run cost" a long interactive session needs to see, and it is intentionally
/// simple to fold into — one method per event type this frontend already subscribes to, so
/// accumulating it costs nothing beyond what verbose rendering was already doing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    /// Model turns taken across every prompt so far.
    pub turns: usize,
    /// Tool calls made across every prompt so far.
    pub tool_calls: usize,
    /// Provider-reported input tokens, summed across every turn that reported them.
    ///
    /// `None` until the first turn that reports usage at all — see
    /// [`RequestMeasured::provider_usage`](aik_api::measurement::RequestMeasured::provider_usage)
    /// for why a provider can simply not report this.
    pub provider_input_tokens: Option<u64>,
    /// Provider-reported output tokens, summed the same way.
    pub provider_output_tokens: Option<u64>,
    /// Locally estimated total request size, summed across every turn. Always available,
    /// since it needs no provider cooperation — see [`aik_api::measurement`] for what
    /// "estimated" means here.
    pub estimated_total_tokens: u64,
    /// Time spent waiting on the model, summed across every turn.
    pub model_latency_ms: u64,
    /// Time spent on tool execution proper (excluding authorization), summed across every
    /// completed invocation that reported it.
    pub tool_execution_latency_ms: u64,
    /// Time spent on authorization decisions, summed across every one made. Includes any
    /// approval wait — see [`AuthorizationDecided::duration_ms`].
    pub authorization_latency_ms: u64,
    /// Time spent specifically waiting for a human to answer a `require_approval`
    /// decision, summed across every such decision. A subset of
    /// [`SessionStats::authorization_latency_ms`], broken out because it is usually the
    /// dominant cost of the decisions it applies to.
    pub approval_latency_ms: u64,
}

impl SessionStats {
    /// Folds in one measured model turn.
    pub fn record_measurement(&mut self, event: &RequestMeasured) {
        self.turns += 1;
        self.estimated_total_tokens += event.estimate.total_tokens;
        self.model_latency_ms += event.model_latency_ms;
        if let Some(usage) = event.provider_usage {
            *self.provider_input_tokens.get_or_insert(0) += usage.input_tokens;
            *self.provider_output_tokens.get_or_insert(0) += usage.output_tokens;
        }
    }

    /// Folds in one completed tool invocation.
    pub fn record_invocation(&mut self, event: &ToolInvoked) {
        self.tool_calls += 1;
        self.tool_execution_latency_ms += event.execution_duration_ms.unwrap_or(0);
    }

    /// Folds in one authorization decision.
    pub fn record_authorization(&mut self, event: &AuthorizationDecided) {
        self.authorization_latency_ms += event.duration_ms;
        if matches!(
            event.outcome,
            AuthorizationOutcome::ApprovalGranted
                | AuthorizationOutcome::ApprovalRefused
                | AuthorizationOutcome::ApprovalUnavailable
        ) {
            self.approval_latency_ms += event.duration_ms;
        }
    }
}

/// Prints an agent update, returning the assistant text it carried.
pub fn update(update: &AgentUpdate, stats: &mut TurnStats) {
    match update {
        AgentUpdate::Content(part) => content(part),
        AgentUpdate::Status { message } => println!("  · {}", safe(message)),
        AgentUpdate::ToolCall(call) => {
            stats.tool_calls += 1;
            println!(
                "  → {} {}",
                safe(call.name.as_str()),
                inline(&call.arguments),
            );
        }
        AgentUpdate::ToolResult { outcome, .. } => {
            let marker = if outcome.is_error { "✗" } else { "←" };
            println!("  {marker} {}", inline(&outcome.output));
        }
        AgentUpdate::Finished(response) => {
            if let Some(usage) = response.usage {
                println!("{}", summary(&usage, stats));
            }
        }
    }
}

fn content(part: &ContentPart) {
    match part {
        ContentPart::Text { text } => println!("{}", safe(text)),
        ContentPart::Blob { mime_type, data } => {
            println!(
                "  [{} blob, {} base64 characters]",
                safe(mime_type),
                data.len()
            );
        }
        other => println!(
            "  {}",
            inline(&serde_json::to_value(other).unwrap_or_default())
        ),
    }
}

/// One line describing what a turn cost.
pub fn summary(usage: &Usage, stats: &TurnStats) -> String {
    format!(
        "\n  [{} turns, {} tool calls, {} in / {} out tokens, window {} tokens]",
        stats.turns.max(1),
        stats.tool_calls,
        usage.input_tokens,
        usage.output_tokens,
        stats.window_tokens,
    )
}

/// Prints one authorization decision, for `--verbose`.
///
/// Every field here is produced by the kernel or by a policy author, never by a model, so
/// this is the one view of a run that untrusted output cannot shape. It is still sanitised:
/// a resource id is a path, and a path can contain anything a filesystem allows.
pub fn authorization(event: &AuthorizationDecided) {
    let resource = event
        .resource
        .as_ref()
        .map(|resource| format!(" on {}", safe(resource.as_str())))
        .unwrap_or_default();
    println!(
        "  [auth] {:?} {}{} → {:?} ({}ms)",
        event.phase,
        safe(event.action.as_str()),
        resource,
        event.outcome,
        event.duration_ms,
    );
}

/// Prints one completed invocation, for `--verbose`.
pub fn invocation(event: &ToolInvoked) {
    let timing = match (event.authorization_duration_ms, event.execution_duration_ms) {
        (Some(auth), Some(exec)) => format!(" ({exec}ms exec, {auth}ms auth)"),
        (Some(auth), None) => format!(" ({auth}ms auth)"),
        _ => String::new(),
    };
    println!(
        "  [tool] {} → {:?}{timing}",
        safe(event.tool.as_str()),
        event.outcome,
    );
}

/// Prints what one model turn's request was estimated to cost, for `--verbose`.
///
/// This is the measurement breakdown the [README](../README.md) and
/// `docs/MEASUREMENTS.md` describe: what the context store's own accounting cannot see
/// (tool-definition cost) alongside what it can, plus provider-reported usage and model
/// latency, all for the exact request this one turn sent.
pub fn measurement(event: &RequestMeasured) {
    let estimate = &event.estimate;
    println!(
        "  [req]  turn {} — system {}, tools {} ({} offered), conversation {}, total {} (estimated)",
        event.turn,
        estimate.system_tokens,
        estimate.tool_definition_tokens,
        estimate.tools_offered,
        estimate.conversation_tokens,
        estimate.total_tokens,
    );
    match event.provider_usage {
        Some(usage) => println!(
            "  [req]  provider usage: {} in / {} out (exact, as reported)",
            usage.input_tokens, usage.output_tokens,
        ),
        None => println!("  [req]  provider usage: not reported by this provider"),
    }
    println!("  [req]  model latency: {}ms", event.model_latency_ms);
}

/// Prints a session's cumulative cost so far, for `--verbose`.
pub fn session_totals(stats: &SessionStats) {
    let provider = match (stats.provider_input_tokens, stats.provider_output_tokens) {
        (Some(input), Some(output)) => format!("{input} in / {output} out (exact)"),
        _ => "not reported".to_owned(),
    };
    println!(
        "  [session] {} turns, {} tool calls, {} estimated tokens total, provider {provider}",
        stats.turns, stats.tool_calls, stats.estimated_total_tokens,
    );
    println!(
        "  [session] latency — model {}ms, tools {}ms, authorization {}ms (approval {}ms)",
        stats.model_latency_ms,
        stats.tool_execution_latency_ms,
        stats.authorization_latency_ms,
        stats.approval_latency_ms,
    );
}

/// Prints what a window cost, for `--verbose`.
pub fn assembled(event: &ContextAssembled) {
    let usage = &event.usage;
    println!(
        "  [ctx]  stored {} — included {} ({} records), elided {} ({} parts), evicted {} ({} records){over_budget}",
        usage.total_tokens(),
        usage.included_tokens,
        usage.included_records,
        usage.elided_tokens,
        usage.elided_parts,
        usage.dropped_tokens,
        usage.dropped_records,
        over_budget = if usage.over_budget {
            " [over budget: pinned records alone exceed it]"
        } else {
            ""
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_escape_sequence_cannot_reach_the_terminal() {
        let hostile = "safe\x1b[2Jcleared";
        let rendered = safe(hostile);
        assert!(!rendered.contains('\x1b'), "{rendered}");
        assert_eq!(rendered, "safe\\u{001b}[2Jcleared");
    }

    #[test]
    fn a_carriage_return_cannot_repaint_a_line() {
        // `\r` is how output overwrites what was already printed — including a prompt.
        let rendered = safe("question: allow?\rquestion: deny?");
        assert!(!rendered.contains('\r'), "{rendered}");
    }

    #[test]
    fn c1_introducers_are_escaped_too() {
        let rendered = safe("before\u{9b}2Kafter");
        assert!(!rendered.contains('\u{9b}'), "{rendered}");
    }

    #[test]
    fn newlines_and_tabs_survive_because_they_are_layout() {
        assert_eq!(safe("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(safe("héllo, wörld — ok"), "héllo, wörld — ok");
    }

    #[test]
    fn inline_values_are_one_line_and_bounded() {
        let long = json!({ "text": "x".repeat(MAX_INLINE * 2) });
        let rendered = inline(&long);
        assert!(!rendered.contains('\n'));
        assert!(rendered.contains("characters total"), "{rendered}");
    }

    #[test]
    fn a_multiline_tool_result_is_flattened_before_printing() {
        let rendered = inline(&json!("line one\nline two"));
        assert_eq!(rendered, "line one line two");
    }

    #[test]
    fn inline_strings_are_shown_unquoted() {
        assert_eq!(inline(&json!("plain")), "plain");
    }

    #[test]
    fn truncation_counts_characters_rather_than_bytes() {
        // Cutting a multi-byte character in half would panic on a byte slice.
        let rendered = inline(&json!("é".repeat(MAX_INLINE + 10)));
        assert!(rendered.starts_with('é'));
    }
}
