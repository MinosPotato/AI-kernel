//! Turning stored records into a bounded model payload.
//!
//! This is the whole of the token-saving mechanism, and it is deliberately *deterministic*:
//! given the same records, budget and counter, it produces the same window every time, with
//! no model call, no heuristics about meaning, and no invented text. Summarisation — the
//! other kind of compaction — needs a model and belongs one layer up; see the
//! [crate documentation](crate#what-this-deliberately-does-not-do).
//!
//! Two independent reductions happen here, in this order:
//!
//! 1. **Elision.** Any single content part costing more than
//!    [`ContextBudget::max_part_tokens`] has its bulk payload replaced by a marker naming
//!    the record it came from. This runs first because shrinking one oversized tool result
//!    is nearly always better than evicting the five turns of conversation that would
//!    otherwise have to go to make room for it.
//! 2. **Eviction.** Whatever still does not fit [`ContextBudget::max_tokens`] or
//!    [`ContextBudget::max_records`] is dropped from the oldest end, keeping a contiguous
//!    run of the most recent turns plus every pinned record.
//!
//! A third pass then repairs the one structural invariant eviction can break: a tool result
//! whose call was evicted is removed, because most providers reject a result that answers
//! nothing.

use std::collections::HashSet;

use aik_api::context::{
    ContextBudget, ContextId, ContextRecord, ContextUsage, ContextWindow, ELISION_MARKER,
    TokenCounter,
};
use aik_api::model::{ContentPart, Message};
use aik_api::tool::ToolCall;
use serde_json::{Value, json};

/// One record, prepared for selection.
struct Prepared {
    id: ContextId,
    pinned: bool,
    message: Message,
    /// The cost of the record as stored, before anything was removed.
    original_tokens: u64,
    /// The cost after per-part elision.
    tokens: u64,
    /// How many parts elision rewrote.
    elided_parts: usize,
}

/// The note appended to text that was truncated.
fn text_note(record: &ContextId, removed: usize, total: usize) -> String {
    format!(" [{ELISION_MARKER}: {removed} of {total} bytes removed; context record {record}]")
}

/// The value that replaces a payload too large to carry.
fn elided_value(record: &ContextId, bytes: usize) -> Value {
    json!({ ELISION_MARKER: { "record": record.to_string(), "bytes": bytes } })
}

/// Finds the longest prefix of `text` that fits `limit`, at a character boundary.
///
/// Binary search over prefixes, which is why [`TokenCounter`] requires counts to be
/// non-decreasing over them: without that the midpoint comparison means nothing. The
/// boundary table is materialised rather than probed lazily because tool output is already
/// size-bounded by the tools themselves, and an obviously correct search is worth more here
/// than avoiding one allocation.
fn truncate_to_tokens<'a>(counter: &dyn TokenCounter, text: &'a str, limit: u64) -> &'a str {
    if counter.count_text(text) <= limit {
        return text;
    }

    let boundaries: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    if boundaries.is_empty() {
        return text;
    }

    // `boundaries[0]` is 0, so the empty prefix always fits and `low` is always valid.
    let mut low = 0;
    let mut high = boundaries.len();
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        if counter.count_text(&text[..boundaries[middle]]) <= limit {
            low = middle;
        } else {
            high = middle;
        }
    }
    &text[..boundaries[low]]
}

/// Produces a cheaper stand-in for one oversized part, or `None` to leave it alone.
///
/// The bulk payload goes; the part's structure does not. A tool result keeps its `call_id`
/// and its error flag, a tool call keeps its name, a blob keeps its media type — everything
/// the conversation needs to stay coherent — and only the bytes are replaced.
///
/// A [`ContentPart::ToolResult`]'s content becomes a marker rather than a truncation
/// because truncated JSON is not JSON, and deciding *which* fields of an arbitrary result
/// are worth keeping needs exactly the domain knowledge this layer does not have — the same
/// reason [`ToolRegistry`](aik_api::tool::ToolRegistry) refuses to extract resources from
/// arbitrary arguments. A tool that wants graceful degradation should emit a compact result
/// itself; what this guarantees is only that no tool can flood a context window, and that
/// nothing is lost, since the full record remains.
///
/// [`ContentPart::Other`] is never rewritten. It is provider-specific by definition, so
/// this layer cannot know what of it is safe to remove.
fn elide_part(
    part: &ContentPart,
    record: &ContextId,
    limit: u64,
    counter: &dyn TokenCounter,
) -> Option<ContentPart> {
    let replacement = match part {
        ContentPart::Text { text } => {
            let kept = truncate_to_tokens(counter, text, limit);
            let removed = text.len() - kept.len();
            ContentPart::Text {
                text: format!("{kept}{}", text_note(record, removed, text.len())),
            }
        }
        ContentPart::Blob { mime_type, data } => ContentPart::Text {
            text: format!(
                "[{ELISION_MARKER}: {mime_type} payload of {} bytes removed; context record {record}]",
                data.len()
            ),
        },
        ContentPart::ToolCall(call) => ContentPart::ToolCall(ToolCall {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            arguments: elided_value(record, call.arguments.to_string().len()),
        }),
        ContentPart::ToolResult {
            call_id,
            content,
            is_error,
        } => ContentPart::ToolResult {
            call_id: call_id.clone(),
            content: elided_value(record, content.to_string().len()),
            is_error: *is_error,
        },
        ContentPart::Other(_) => return None,
    };

    // Eliding must never cost more than not eliding. For a very small limit the marker
    // itself can be longer than what it replaced, and a "reduction" that grows the payload
    // would be worse than useless.
    if counter.count_part(&replacement) < counter.count_part(part) {
        Some(replacement)
    } else {
        None
    }
}

/// Applies [`ContextBudget::max_part_tokens`] to every part of one message.
fn elide_message(
    message: &Message,
    record: &ContextId,
    limit: u64,
    counter: &dyn TokenCounter,
) -> (Message, usize) {
    let mut elided = 0;
    let content = message
        .content
        .iter()
        .map(|part| {
            if counter.count_part(part) <= limit {
                return part.clone();
            }
            match elide_part(part, record, limit, counter) {
                Some(replacement) => {
                    elided += 1;
                    replacement
                }
                None => part.clone(),
            }
        })
        .collect();

    (
        Message {
            role: message.role,
            content,
            name: message.name.clone(),
        },
        elided,
    )
}

/// Removes tool results whose call is not present earlier in the window.
///
/// Returns how many parts were dropped. Eviction works on whole records, so cutting the
/// history at an arbitrary point can leave a `Role::Tool` message answering a call that is
/// no longer there; providers reject that, so it is repaired here rather than being handed
/// out as a malformed request.
fn drop_orphan_results(messages: &mut [Message]) -> usize {
    let mut answered: HashSet<String> = HashSet::new();
    let mut dropped = 0;

    for message in messages.iter_mut() {
        let mut kept = Vec::with_capacity(message.content.len());
        for part in message.content.drain(..) {
            match &part {
                ContentPart::ToolCall(call) => {
                    answered.insert(call.call_id.clone());
                    kept.push(part);
                }
                ContentPart::ToolResult { call_id, .. } if !answered.contains(call_id) => {
                    dropped += 1;
                }
                _ => kept.push(part),
            }
        }
        message.content = kept;
    }

    dropped
}

/// Builds the model payload for `records` under `budget`.
///
/// `records` must be in append order; the window preserves it.
pub(crate) fn assemble(
    records: &[ContextRecord],
    budget: &ContextBudget,
    counter: &dyn TokenCounter,
) -> ContextWindow {
    let prepared: Vec<Prepared> = records
        .iter()
        .map(|record| {
            let (message, elided_parts) = match budget.max_part_tokens {
                Some(limit) => elide_message(&record.message, &record.id, limit, counter),
                None => (record.message.clone(), 0),
            };
            let tokens = if elided_parts == 0 {
                record.tokens
            } else {
                counter.count_message(&message)
            };
            Prepared {
                id: record.id,
                pinned: record.pinned,
                message,
                original_tokens: record.tokens,
                tokens,
                elided_parts,
            }
        })
        .collect();

    let pinned_tokens: u64 = prepared.iter().filter(|p| p.pinned).map(|p| p.tokens).sum();
    let pinned_count = prepared.iter().filter(|p| p.pinned).count();
    let over_budget = budget
        .max_tokens
        .is_some_and(|max_tokens| pinned_tokens > max_tokens);

    // Pinned records are spent first and are never given back: what remains is what the
    // conversation gets.
    let token_allowance = budget
        .max_tokens
        .map(|max_tokens| max_tokens.saturating_sub(pinned_tokens));
    let record_allowance = budget
        .max_records
        .map(|max_records| max_records.saturating_sub(pinned_count));

    // Newest first, stopping at the first record that does not fit rather than skipping it:
    // a window with holes in it reads worse to a model than a shorter, contiguous one.
    let mut keep = vec![false; prepared.len()];
    let mut accepting = true;
    let mut used_tokens = 0u64;
    let mut used_records = 0usize;
    for index in (0..prepared.len()).rev() {
        let candidate = &prepared[index];
        if candidate.pinned {
            keep[index] = true;
            continue;
        }
        if !accepting {
            continue;
        }
        if record_allowance.is_some_and(|allowance| used_records >= allowance) {
            accepting = false;
            continue;
        }
        if token_allowance.is_some_and(|allowance| used_tokens + candidate.tokens > allowance) {
            accepting = false;
            continue;
        }
        keep[index] = true;
        used_tokens += candidate.tokens;
        used_records += 1;
    }

    let selected: Vec<usize> = (0..prepared.len()).filter(|index| keep[*index]).collect();
    let mut messages: Vec<Message> = selected
        .iter()
        .map(|index| prepared[*index].message.clone())
        .collect();
    let orphans = drop_orphan_results(&mut messages);

    // A record whose every part was an orphaned result carries nothing now; sending an
    // empty message would be noise at best and a provider error at worst.
    let mut kept_ids = Vec::with_capacity(selected.len());
    let mut kept_messages = Vec::with_capacity(selected.len());
    let mut usage = ContextUsage {
        elided_parts: orphans,
        over_budget,
        ..ContextUsage::default()
    };
    for (position, message) in messages.into_iter().enumerate() {
        let candidate = &prepared[selected[position]];
        if message.content.is_empty() {
            usage.dropped_records += 1;
            usage.dropped_tokens += candidate.original_tokens;
            continue;
        }
        let tokens = counter.count_message(&message);
        usage.included_records += 1;
        usage.included_tokens += tokens;
        usage.elided_parts += candidate.elided_parts;
        usage.elided_tokens += candidate.original_tokens.saturating_sub(tokens);
        kept_ids.push(candidate.id);
        kept_messages.push(message);
    }

    for (index, candidate) in prepared.iter().enumerate() {
        if !keep[index] {
            usage.dropped_records += 1;
            usage.dropped_tokens += candidate.original_tokens;
        }
    }

    ContextWindow {
        messages: kept_messages,
        records: kept_ids,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::HeuristicTokenCounter;
    use aik_api::agent::SessionId;
    use aik_api::context::ContextRecord;
    use aik_api::model::Role;
    use aik_api::permission::PrincipalId;
    use aik_api::tool::ToolName;
    use aik_core::clock::Timestamp;

    fn record(sequence: u64, message: Message, pinned: bool) -> ContextRecord {
        let counter = HeuristicTokenCounter::new();
        ContextRecord {
            id: ContextId::new(),
            session: SessionId::new(),
            sequence,
            tokens: counter.count_message(&message),
            message,
            pinned,
            principal: PrincipalId::new("tester"),
            created_at: Timestamp::from_millis(sequence),
        }
    }

    fn text(sequence: u64, body: &str) -> ContextRecord {
        record(sequence, Message::text(Role::User, body), false)
    }

    #[test]
    fn truncation_finds_the_longest_fitting_prefix() {
        let counter = HeuristicTokenCounter::new();
        // Four bytes per token, so a limit of two tokens fits eight bytes.
        let kept = truncate_to_tokens(&counter, "abcdefghijklmnop", 2);
        assert_eq!(kept, "abcdefgh");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let counter = HeuristicTokenCounter::new();
        // Each `é` is two bytes, so a five-byte allowance cannot land on a boundary.
        let kept = truncate_to_tokens(&counter, "ééééé", 1);
        assert!(kept.chars().all(|character| character == 'é'));
        assert!(kept.len() <= 4);
    }

    #[test]
    fn an_unlimited_budget_returns_everything_unchanged() {
        let records = vec![text(0, "one"), text(1, "two")];
        let window = assemble(
            &records,
            &ContextBudget::UNLIMITED,
            &HeuristicTokenCounter::new(),
        );
        assert_eq!(window.messages.len(), 2);
        assert_eq!(window.usage.dropped_records, 0);
        assert_eq!(window.usage.elided_tokens, 0);
    }

    #[test]
    fn eviction_keeps_the_most_recent_contiguous_run() {
        let counter = HeuristicTokenCounter::new();
        let records: Vec<ContextRecord> = (0..5).map(|index| text(index, "abcdefgh")).collect();
        let each = records[0].tokens;

        let window = assemble(&records, &ContextBudget::tokens(each * 2), &counter);
        assert_eq!(window.usage.included_records, 2);
        assert_eq!(window.usage.dropped_records, 3);
        assert_eq!(window.records, vec![records[3].id, records[4].id]);
    }

    #[test]
    fn pinned_records_survive_a_budget_that_cannot_fit_them() {
        let counter = HeuristicTokenCounter::new();
        let records = vec![
            record(0, Message::text(Role::System, "always"), true),
            text(1, "evictable"),
        ];

        let window = assemble(&records, &ContextBudget::tokens(1), &counter);
        assert_eq!(window.usage.included_records, 1);
        assert!(window.usage.over_budget);
        assert_eq!(window.records, vec![records[0].id]);
    }

    #[test]
    fn every_stored_token_is_accounted_for_exactly_once() {
        let counter = HeuristicTokenCounter::new();
        let records: Vec<ContextRecord> = (0..6)
            .map(|index| text(index, "abcdefghijklmnopqrstuvwxyz"))
            .collect();
        let stored: u64 = records.iter().map(|record| record.tokens).sum();

        let budget = ContextBudget::tokens(20).with_max_part_tokens(2);
        let window = assemble(&records, &budget, &counter);
        assert_eq!(window.usage.total_tokens(), stored);
    }

    #[test]
    fn a_record_limit_counts_pinned_records_too() {
        let counter = HeuristicTokenCounter::new();
        let records = vec![
            record(0, Message::text(Role::System, "pinned"), true),
            text(1, "a"),
            text(2, "b"),
            text(3, "c"),
        ];

        let window = assemble(
            &records,
            &ContextBudget::default().with_max_records(2),
            &counter,
        );
        assert_eq!(window.usage.included_records, 2);
        assert_eq!(window.records, vec![records[0].id, records[3].id]);
    }

    #[test]
    fn oversized_text_is_truncated_with_a_marker_naming_its_record() {
        let counter = HeuristicTokenCounter::new();
        let records = vec![text(0, &"x".repeat(400))];

        let budget = ContextBudget::default().with_max_part_tokens(4);
        let window = assemble(&records, &budget, &counter);

        let ContentPart::Text { text } = &window.messages[0].content[0] else {
            panic!("expected a text part");
        };
        assert!(text.starts_with("xxxx"));
        assert!(text.contains(ELISION_MARKER), "{text}");
        assert!(text.contains(&records[0].id.to_string()), "{text}");
        assert_eq!(window.usage.elided_parts, 1);
        assert!(window.usage.elided_tokens > 0);
    }

    #[test]
    fn an_oversized_tool_result_keeps_its_call_id_and_loses_only_its_payload() {
        let counter = HeuristicTokenCounter::new();
        let records = vec![
            record(
                0,
                Message {
                    role: Role::Assistant,
                    content: vec![call("call-1")],
                    name: None,
                },
                false,
            ),
            record(
                1,
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        call_id: "call-1".into(),
                        content: json!({ "content": "y".repeat(400) }),
                        is_error: false,
                    }],
                    name: None,
                },
                false,
            ),
        ];

        let budget = ContextBudget::default().with_max_part_tokens(4);
        let window = assemble(&records, &budget, &counter);

        let ContentPart::ToolResult {
            call_id, content, ..
        } = &window.messages[1].content[0]
        else {
            panic!("expected a tool result");
        };
        assert_eq!(call_id, "call-1");
        assert_eq!(
            content[ELISION_MARKER]["record"],
            json!(records[1].id.to_string())
        );
        assert!(!content.to_string().contains("yyyy"));
    }

    #[test]
    fn a_blob_is_replaced_by_a_note_rather_than_carried() {
        let counter = HeuristicTokenCounter::new();
        let message = Message {
            role: Role::User,
            content: vec![ContentPart::Blob {
                mime_type: "image/png".into(),
                data: "A".repeat(4_000),
            }],
            name: None,
        };
        let records = vec![record(0, message, false)];

        let budget = ContextBudget::default().with_max_part_tokens(8);
        let window = assemble(&records, &budget, &counter);

        let ContentPart::Text { text } = &window.messages[0].content[0] else {
            panic!("expected the blob to become a note");
        };
        assert!(text.contains("image/png"), "{text}");
        assert!(text.contains("4000"), "{text}");
        assert!(window.usage.included_tokens < records[0].tokens);
    }

    #[test]
    fn elision_never_makes_a_part_more_expensive() {
        let counter = HeuristicTokenCounter::new();
        // Short enough that any marker would be longer than the content it replaces.
        let records = vec![text(0, "ab")];

        let budget = ContextBudget::default().with_max_part_tokens(0);
        let window = assemble(&records, &budget, &counter);
        assert_eq!(window.usage.elided_parts, 0);
        assert_eq!(window.messages[0].content[0], ContentPart::text("ab"));
    }

    #[test]
    fn provider_specific_parts_are_never_rewritten() {
        let counter = HeuristicTokenCounter::new();
        let opaque = ContentPart::Other(json!({ "vendor": "z".repeat(400) }));
        let message = Message {
            role: Role::Assistant,
            content: vec![opaque.clone()],
            name: None,
        };
        let records = vec![record(0, message, false)];

        let budget = ContextBudget::default().with_max_part_tokens(1);
        let window = assemble(&records, &budget, &counter);
        assert_eq!(window.messages[0].content[0], opaque);
        assert_eq!(window.usage.elided_parts, 0);
    }

    fn call(call_id: &str) -> ContentPart {
        ContentPart::ToolCall(ToolCall {
            call_id: call_id.into(),
            name: ToolName::new("demo.tool"),
            arguments: json!({}),
        })
    }

    fn result(call_id: &str) -> ContentPart {
        ContentPart::ToolResult {
            call_id: call_id.into(),
            content: json!({ "ok": true }),
            is_error: false,
        }
    }

    #[test]
    fn a_tool_result_whose_call_was_evicted_is_removed() {
        let counter = HeuristicTokenCounter::new();
        let records = vec![
            record(
                0,
                Message {
                    role: Role::Assistant,
                    content: vec![call("call-1")],
                    name: None,
                },
                false,
            ),
            record(
                1,
                Message {
                    role: Role::Tool,
                    content: vec![result("call-1")],
                    name: None,
                },
                false,
            ),
            text(2, "and then"),
        ];

        // Room for the last two records only, which orphans the surviving tool result.
        let budget = ContextBudget::tokens(records[1].tokens + records[2].tokens);
        let window = assemble(&records, &budget, &counter);

        assert_eq!(window.usage.included_records, 1);
        assert_eq!(window.records, vec![records[2].id]);
        for message in &window.messages {
            for part in &message.content {
                assert!(!matches!(part, ContentPart::ToolResult { .. }));
            }
        }
    }

    #[test]
    fn a_tool_result_whose_call_survived_is_kept() {
        let counter = HeuristicTokenCounter::new();
        let records = vec![
            record(
                0,
                Message {
                    role: Role::Assistant,
                    content: vec![call("call-1")],
                    name: None,
                },
                false,
            ),
            record(
                1,
                Message {
                    role: Role::Tool,
                    content: vec![result("call-1")],
                    name: None,
                },
                false,
            ),
        ];

        let window = assemble(&records, &ContextBudget::UNLIMITED, &counter);
        assert_eq!(window.usage.included_records, 2);
        assert!(matches!(
            window.messages[1].content[0],
            ContentPart::ToolResult { .. }
        ));
    }

    #[test]
    fn an_empty_session_yields_an_empty_window() {
        let window = assemble(
            &[],
            &ContextBudget::UNLIMITED,
            &HeuristicTokenCounter::new(),
        );
        assert_eq!(window, ContextWindow::empty());
    }
}
