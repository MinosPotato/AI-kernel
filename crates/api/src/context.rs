//! Agent context: the durable transcript, and the bounded view of it a model sees.
//!
//! Everything else in this crate is about *doing* things. This module is about what an
//! agent knows while it does them, and — just as importantly — what it has to pay to know
//! it.
//!
//! # Why this is a kernel-level contract
//!
//! A model provider takes [`CompletionRequest::messages`](crate::model::CompletionRequest),
//! a `Vec<Message>`, on every single call. Without something in between, the only way to
//! run a long conversation is for the caller to hold the entire history and re-send it,
//! whole, every turn. That is quadratic in turns, it re-sends tool output the model already
//! saw and will never look at again, and it has no answer at all for what to do when the
//! history outgrows the model's context window.
//!
//! The fix is not to compress messages. It is to stop treating the model payload as the
//! place state lives:
//!
//! ```text
//! kernel-side, full fidelity            model-side, bounded
//! ────────────────────────────          ───────────────────────────
//! ContextStore                    →     ContextWindow
//!   every record, forever                the records that fit a budget,
//!   full tool output                     with oversized payloads elided
//!   principal attribution                (no attribution, no policy state)
//!   token accounting
//! ```
//!
//! [`ContextStore`] is the durable side: append-only, ordered, per-session, attributed.
//! [`ContextStore::window`] is the derived side: a `Vec<Message>` computed *from* the
//! records under a [`ContextBudget`], ready to hand to a provider. The transcript is the
//! truth; the model payload is a projection of it that is recomputed each turn and never
//! stored.
//!
//! # This is not `ExecutionContext`
//!
//! Three unrelated things in this system are called "context", so, precisely:
//!
//! | Type | What it is |
//! |---|---|
//! | [`KernelContext`](aik_core::KernelContext) | A handle onto the running kernel |
//! | [`ExecutionContext`] | Who is asking, for what operation, until when |
//! | [`ContextStore`] | What an agent remembers, and what a model gets told |
//!
//! Only the last one is "context" in the LLM sense. It is the one an agent budgets.
//!
//! # What the model can and cannot touch
//!
//! A [`ContextStore`] is **not a [`Tool`](crate::tool::Tool)** and must never be exposed as
//! one. Nothing a model emits reaches this trait: a model produces content, trusted code
//! decides whether to record it, and only trusted code calls [`ContextStore::append`]. That
//! asymmetry is the whole security property — the model can influence what is *in* a
//! record, and can never influence the record's attribution, its session, its ordering, or
//! whether some other session's records become visible.
//!
//! Concretely, the following are set by the store from the [`ExecutionContext`] and are not
//! part of the appended payload, so they cannot be forged by anything a model wrote:
//!
//! * [`ContextRecord::principal`] — who appended it;
//! * [`ContextRecord::session`] and [`ContextRecord::sequence`] — where it sits;
//! * [`ContextRecord::created_at`] — when, by the kernel clock.
//!
//! Sessions are owned. The first append to a session records the calling principal as its
//! owner, and every later access must come from that principal or from one acting
//! [`on_behalf_of`](crate::permission::Principal::on_behalf_of) it; anything else is
//! [`Error::PermissionDenied`](aik_core::Error::PermissionDenied). Note the limit of that
//! guarantee, in the same spirit as the [TOCTOU
//! discussion](crate::tool#time-of-check-to-time-of-use): in-process code can construct an
//! `ExecutionContext` naming any principal it likes, so the owner check is a boundary
//! against *the model*, which can never construct one at all, and defence in depth against
//! a confused caller. It is not a boundary against hostile code already inside the process.
//!
//! Nothing security-relevant is *stored* here either. Policy rules, authorization decisions
//! and approvals live in [`permission`](crate::permission) and [`audit`](crate::audit) and
//! are never round-tripped through a transcript, so no amount of writing to a context
//! store can change what a principal is allowed to do.
//!
//! # Measuring
//!
//! Budgeting requires counting, and counting requires a tokenizer that the kernel
//! deliberately does not have. [`TokenCounter`] is the seam: provider-neutral, with one
//! documented heuristic implementation, and replaceable by a provider that knows its own
//! tokenizer. Every [`ContextWindow`] carries a [`ContextUsage`], and a
//! [`ContextAssembled`] event is published for each one, so context cost is observable
//! through the mechanism the kernel already has for everything else.

use aik_core::Result;
use aik_core::clock::Timestamp;
use aik_core::event::Event;
use aik_core::id::CorrelationId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::SessionId;
use crate::execution::ExecutionContext;
use crate::model::{ContentPart, Message};
use crate::permission::PrincipalId;

aik_core::uuid_id! {
    /// Identifies one record within a session's context.
    ///
    /// Time-ordered, so records sort by creation even without their sequence number.
    pub ContextId
}

/// The marker a context implementation uses to say "something was removed here".
///
/// It appears as an object key in an elided JSON payload
/// (`{"aik.elided": {"record": …, "bytes": …}}`) and inside the bracketed note appended to
/// elided text. It is a stable string so that an agent — or a model that has been told
/// about it — can recognise an elision and ask for the full record by id, rather than
/// mistaking the marker for content.
pub const ELISION_MARKER: &str = "aik.elided";

/// What trusted code asks a [`ContextStore`] to record.
///
/// Deliberately thin: the message, and whether it is exempt from eviction. Everything else
/// on a [`ContextRecord`] is assigned by the store, precisely so that it cannot be supplied
/// by whatever produced the message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEntry {
    /// The turn to record, at full fidelity.
    pub message: Message,
    /// Whether this record survives budget eviction.
    ///
    /// System instructions and durable task framing should be pinned; conversation should
    /// not. Pinning is a decision for the trusted caller assembling the agent, never for
    /// the model: a model that could pin its own output could make itself unforgettable.
    #[serde(default)]
    pub pinned: bool,
}

impl ContextEntry {
    /// Records a message as ordinary, evictable conversation.
    pub fn new(message: Message) -> Self {
        Self {
            message,
            pinned: false,
        }
    }

    /// Marks the entry exempt from budget eviction.
    #[must_use]
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }
}

/// One stored turn, as the kernel holds it.
///
/// This is the full-fidelity form: no truncation, no elision, no budget applied. What a
/// model is shown is derived from these by [`ContextStore::window`] and is always a subset
/// of what is here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextRecord {
    /// The record's identifier.
    pub id: ContextId,
    /// The session it belongs to.
    pub session: SessionId,
    /// Its position in the session, starting at zero and never reused.
    pub sequence: u64,
    /// The turn, exactly as recorded.
    pub message: Message,
    /// Whether it survives budget eviction.
    pub pinned: bool,
    /// Who appended it, taken from the [`ExecutionContext`], never from the payload.
    pub principal: PrincipalId,
    /// When it was appended, by the kernel clock.
    pub created_at: Timestamp,
    /// The estimated cost of the full-fidelity message, per the store's [`TokenCounter`].
    pub tokens: u64,
}

/// How much context a caller is willing to spend.
///
/// Every field is optional and `None` means unbounded, so [`ContextBudget::default()`]
/// reproduces the naive "send everything" behaviour exactly. That is the point: budgeting
/// is opt-in, and turning it off is not a special case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    /// The most tokens the assembled window may cost.
    ///
    /// Pinned records are counted against this but are never dropped to satisfy it; if they
    /// alone exceed it, the window is returned over budget and says so via
    /// [`ContextUsage::over_budget`]. Silently discarding a system prompt to hit a number
    /// would be a worse failure than reporting one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// The most records the assembled window may contain, pinned ones included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_records: Option<usize>,
    /// The most tokens any single [`ContentPart`] may cost before it is elided.
    ///
    /// This is where the large, one-off payloads go: a file's contents, a directory
    /// listing, a base64 image. The full value stays in the record and stays retrievable by
    /// [`ContextStore::get`]; only the copy in the window is replaced by a marker naming
    /// the record it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_part_tokens: Option<u64>,
}

impl ContextBudget {
    /// A budget that constrains nothing.
    pub const UNLIMITED: Self = Self {
        max_tokens: None,
        max_records: None,
        max_part_tokens: None,
    };

    /// Bounds the whole window.
    pub const fn tokens(max_tokens: u64) -> Self {
        Self {
            max_tokens: Some(max_tokens),
            max_records: None,
            max_part_tokens: None,
        }
    }

    /// Bounds the number of records.
    #[must_use]
    pub const fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = Some(max_records);
        self
    }

    /// Bounds any single content part, eliding what exceeds it.
    #[must_use]
    pub const fn with_max_part_tokens(mut self, max_part_tokens: u64) -> Self {
        self.max_part_tokens = Some(max_part_tokens);
        self
    }
}

/// What one [`ContextWindow`] cost, and what it left out.
///
/// Every stored token is accounted for exactly once:
/// `included + elided + dropped` equals the session's full-fidelity total for the records
/// considered. That invariant is what makes this usable for deciding *when* to compact,
/// rather than merely reporting that compaction happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Records present in the window.
    pub included_records: usize,
    /// Estimated tokens the window costs, after elision.
    pub included_tokens: u64,
    /// Records left out entirely because the budget could not fit them.
    pub dropped_records: usize,
    /// Estimated tokens those records would have cost.
    pub dropped_tokens: u64,
    /// Content parts rewritten inside included records.
    ///
    /// Covers both parts elided for exceeding
    /// [`ContextBudget::max_part_tokens`] and tool results removed because the call they
    /// answer did not survive.
    pub elided_parts: usize,
    /// Estimated tokens saved by those rewrites.
    pub elided_tokens: u64,
    /// Whether pinned records alone exceeded [`ContextBudget::max_tokens`].
    pub over_budget: bool,
}

impl ContextUsage {
    /// The full-fidelity cost of everything considered, elided and dropped included.
    pub fn total_tokens(&self) -> u64 {
        self.included_tokens + self.elided_tokens + self.dropped_tokens
    }
}

/// A session's context, reduced to what a model should be sent.
///
/// [`ContextWindow::messages`] goes straight into
/// [`CompletionRequest::messages`](crate::model::CompletionRequest::messages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextWindow {
    /// The messages to send, oldest first.
    pub messages: Vec<Message>,
    /// The record each message came from, index-aligned with
    /// [`ContextWindow::messages`].
    ///
    /// This is how a caller gets from something in the window back to the full-fidelity
    /// record behind it — which is the only way an elided payload can be recovered.
    pub records: Vec<ContextId>,
    /// What it cost.
    pub usage: ContextUsage,
}

impl ContextWindow {
    /// An empty window, as returned for a session with no records.
    pub fn empty() -> Self {
        Self {
            messages: Vec::new(),
            records: Vec::new(),
            usage: ContextUsage::default(),
        }
    }
}

/// A session's full-fidelity totals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextStats {
    /// The session described.
    pub session: SessionId,
    /// The principal that created it and may access it.
    pub owner: PrincipalId,
    /// How many records it holds.
    pub records: usize,
    /// Their total estimated cost, with nothing elided or dropped.
    pub tokens: u64,
    /// When the session's first record was appended.
    pub created_at: Timestamp,
    /// When its most recent record was appended.
    pub updated_at: Timestamp,
}

/// Estimates what text will cost a model.
///
/// The kernel has no tokenizer and will not grow one: tokenization is provider-specific,
/// versioned with the model, and wrong to guess at. What the kernel needs is far weaker —
/// a *monotonic* size estimate good enough to decide what to keep — so this trait is
/// deliberately the smallest thing that supports budgeting, and a provider that knows its
/// own tokenizer can register a better implementation under the same capability without
/// anything else changing.
///
/// # Obligations
///
/// * **Monotonic over prefixes.** For any `a` that is a prefix of `b`,
///   `count_text(a) <= count_text(b)`. Truncation to a budget is found by binary search
///   over prefixes, so an implementation that violates this produces wrong truncation
///   points, not merely imprecise ones.
/// * **Deterministic.** The same input must always yield the same count, so that a stored
///   [`ContextRecord::tokens`] stays consistent with what assembly later computes.
/// * **Cheap.** It is called several times per record per turn.
///
/// Estimates should err high. Over-counting spends budget the caller had; under-counting
/// overruns a real context window, which fails the request outright.
pub trait TokenCounter: Send + Sync + 'static {
    /// Estimates the cost of a string.
    fn count_text(&self, text: &str) -> u64;

    /// Estimates the cost of a JSON value as a provider would serialise it.
    ///
    /// Strings are counted as their contents, since the surrounding quotes are an artefact
    /// of this representation rather than something a provider necessarily sends.
    fn count_json(&self, value: &Value) -> u64 {
        match value {
            Value::String(text) => self.count_text(text),
            other => self.count_text(&other.to_string()),
        }
    }

    /// Estimates the cost of one content part.
    ///
    /// The default counts a [`ContentPart::Blob`] by the length of its base64 payload,
    /// which is a large over-estimate for an image. That is the deliberately safe direction
    /// — it makes a blob the first thing a budget elides — but it is exactly the kind of
    /// thing a provider-specific counter should override.
    fn count_part(&self, part: &ContentPart) -> u64 {
        match part {
            ContentPart::Text { text } => self.count_text(text),
            ContentPart::Blob { mime_type, data } => {
                self.count_text(mime_type) + self.count_text(data)
            }
            ContentPart::ToolCall(call) => {
                self.count_text(call.name.as_str()) + self.count_json(&call.arguments)
            }
            ContentPart::ToolResult { content, .. } => self.count_json(content),
            ContentPart::Other(value) => self.count_json(value),
        }
    }

    /// Estimates the cost of a whole message, including the per-message framing every
    /// provider adds for the role.
    fn count_message(&self, message: &Message) -> u64 {
        MESSAGE_OVERHEAD_TOKENS
            + message
                .name
                .as_deref()
                .map_or(0, |name| self.count_text(name))
            + message
                .content
                .iter()
                .map(|part| self.count_part(part))
                .sum::<u64>()
    }
}

/// The fixed cost every provider adds per message for role and delimiters.
///
/// A small constant rather than a per-provider table: the exact value differs between
/// providers by one or two tokens, and being approximately right here matters far less than
/// being consistent, since it is the same on both sides of every budget comparison.
pub const MESSAGE_OVERHEAD_TOKENS: u64 = 4;

/// Where an agent's conversation lives between turns.
///
/// The store is the transcript; a model payload is derived from it and thrown away. Nothing
/// here is reachable from a model — see the [module documentation](self#what-the-model-can-and-cannot-touch).
///
/// # Session ownership
///
/// A session belongs to the principal whose [`ExecutionContext`] first appended to it.
/// Every method must reject a caller that is neither that principal nor one acting
/// [`on_behalf_of`](crate::permission::Principal::on_behalf_of) it, with
/// [`Error::PermissionDenied`](aik_core::Error::PermissionDenied). A context with no
/// principal is the system acting for itself and gets its own identity, exactly as it does
/// in [`ToolRegistry`](crate::tool::ToolRegistry) — not a wildcard.
#[async_trait]
pub trait ContextStore: Send + Sync + 'static {
    /// Appends a turn, creating the session if it does not exist.
    ///
    /// Returns the stored record, including the attribution and sequence the store
    /// assigned. Appending is the only way to add context; there is deliberately no update
    /// and no insert-at, so the transcript is append-only and its ordering cannot be
    /// rewritten after the fact.
    async fn append(
        &self,
        session: &SessionId,
        entry: ContextEntry,
        cx: &ExecutionContext,
    ) -> Result<ContextRecord>;

    /// Fetches one full-fidelity record from a session.
    ///
    /// Returns `Ok(None)` if the session holds no such record — including when the id names
    /// a real record belonging to a *different* session, which is how retrieval stays
    /// session-scoped rather than merely id-scoped.
    async fn get(
        &self,
        session: &SessionId,
        id: &ContextId,
        cx: &ExecutionContext,
    ) -> Result<Option<ContextRecord>>;

    /// Builds the model payload for a session under a budget.
    ///
    /// Implementations must:
    ///
    /// 1. keep every pinned record, whatever the budget says;
    /// 2. otherwise prefer the most recent records, keeping a contiguous run of them rather
    ///    than cherry-picking whichever happen to fit — a conversation with holes in it is
    ///    worse than a shorter one;
    /// 3. never emit a [`ContentPart::ToolResult`] whose corresponding
    ///    [`ContentPart::ToolCall`] is not also in the window, since most providers reject
    ///    that outright;
    /// 4. emit messages in the order they were appended;
    /// 5. report what was left out in [`ContextWindow::usage`].
    ///
    /// A session with no records yields [`ContextWindow::empty`], not an error: the first
    /// turn of a conversation is not a failure.
    async fn window(
        &self,
        session: &SessionId,
        budget: &ContextBudget,
        cx: &ExecutionContext,
    ) -> Result<ContextWindow>;

    /// Reports a session's full-fidelity totals, or `None` if it does not exist.
    async fn stats(
        &self,
        session: &SessionId,
        cx: &ExecutionContext,
    ) -> Result<Option<ContextStats>>;

    /// Discards a session and everything in it, returning how many records were removed.
    ///
    /// Clearing an unknown session is not an error; it removes nothing and returns zero.
    async fn clear(&self, session: &SessionId, cx: &ExecutionContext) -> Result<usize>;
}

/// One context window was assembled.
///
/// Published on the kernel [`EventBus`](aik_core::EventBus) so context cost is observable
/// the same way authorization is — see [`crate::audit`] — and so a future compaction
/// component can react to a session that keeps overflowing without being wired into the
/// agent loop.
///
/// It carries counts only. The same rule that governs audit events governs this one: an
/// event that shipped conversation content to a log aggregator would be a far larger
/// disclosure than anything it could tell an operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAssembled {
    /// The operation the window was built for.
    pub correlation: CorrelationId,
    /// When it was built, by the kernel clock.
    pub timestamp: Timestamp,
    /// Which session it came from.
    pub session: SessionId,
    /// What it cost and what was left out.
    pub usage: ContextUsage,
}

impl Event for ContextAssembled {
    const NAME: &'static str = "aik.context.assembled";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;
    use crate::tool::{ToolCall, ToolName};
    use serde_json::json;

    struct Chars;

    impl TokenCounter for Chars {
        fn count_text(&self, text: &str) -> u64 {
            text.len() as u64
        }
    }

    #[test]
    fn message_cost_is_the_sum_of_its_parts_plus_framing() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("abcd"),
                ContentPart::ToolCall(ToolCall {
                    call_id: "1".into(),
                    name: ToolName::new("fs.read"),
                    arguments: json!({}),
                }),
            ],
            name: None,
        };

        // "abcd" = 4, "fs.read" = 7, "{}" = 2, framing = 4.
        assert_eq!(
            Chars.count_message(&message),
            4 + 7 + 2 + MESSAGE_OVERHEAD_TOKENS
        );
    }

    #[test]
    fn json_strings_are_counted_as_their_contents() {
        assert_eq!(Chars.count_json(&json!("abcd")), 4);
    }

    #[test]
    fn an_unlimited_budget_constrains_nothing() {
        assert_eq!(ContextBudget::default(), ContextBudget::UNLIMITED);
        assert!(ContextBudget::UNLIMITED.max_tokens.is_none());
    }

    #[test]
    fn budgets_accumulate() {
        let budget = ContextBudget::tokens(100)
            .with_max_records(10)
            .with_max_part_tokens(20);
        assert_eq!(budget.max_tokens, Some(100));
        assert_eq!(budget.max_records, Some(10));
        assert_eq!(budget.max_part_tokens, Some(20));
    }

    #[test]
    fn usage_accounts_for_every_stored_token() {
        let usage = ContextUsage {
            included_records: 2,
            included_tokens: 30,
            dropped_records: 1,
            dropped_tokens: 50,
            elided_parts: 1,
            elided_tokens: 20,
            over_budget: false,
        };
        assert_eq!(usage.total_tokens(), 100);
    }

    #[test]
    fn assembly_events_round_trip_and_carry_no_content() {
        let event = ContextAssembled {
            correlation: CorrelationId::new(),
            timestamp: Timestamp::from_millis(1_000),
            session: SessionId::new(),
            usage: ContextUsage::default(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("messages").is_none());
        assert!(json.get("content").is_none());

        let parsed: ContextAssembled = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn entries_are_evictable_unless_pinned() {
        let entry = ContextEntry::new(Message::text(Role::User, "hi"));
        assert!(!entry.pinned);
        assert!(entry.pinned().pinned);
    }
}
