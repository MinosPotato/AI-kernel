//! Summarising compaction: a session's oldest turns, replaced by a recap of them.
//!
//! [`aik-context`](../aik_context/index.html) answers "what does a model get told?"
//! deterministically. When a session outgrows its budget, the window drops the oldest
//! records and says how many it dropped. Nothing is lost from the store — every record is
//! still there, addressable by id — but the model stops being told about it, and from the
//! model's side that is indistinguishable from it never having happened. A long conversation
//! silently forgets its own beginning.
//!
//! This crate is the other half of that. It reads the turns that are about to fall out of
//! the window, has a model write down what they amounted to, appends that back through the
//! ordinary [`ContextStore::append`](aik_api::context::ContextStore::append) path, and only
//! then asks the store to reclaim what it covered:
//!
//! ```text
//!  window overflows                       ┌── nothing was summarised ──▶ nothing removed
//!        │                                │
//!        ▼                                │
//!  ContextStore::window (unbudgeted) ──▶ plan ──▶ ModelProvider::complete (no tools)
//!        the whole transcript              │              │
//!                                          │              ▼
//!                                          │      ContextStore::append   (the recap,
//!                                          │              │               unpinned)
//!                                          │              ▼
//!                                          └────▶ ContextStore::compact  (exactly what
//!                                                                         the recap covers)
//! ```
//!
//! # Why it is not in the store, and not in the loop
//!
//! [`ContextStore::compact`](aik_api::context::ContextStore::compact) is deterministic,
//! model-free and cannot fail for an interesting reason. Summarisation is none of those: it
//! needs a model, so it is fallible, costly, non-deterministic and — because its input is a
//! transcript full of tool output — an injection surface. Putting it behind the same trait
//! would make every context store either implement all of that or refuse it.
//!
//! [`aik-agent`](../aik_agent/index.html) is the other place it could have gone, and the
//! reason it did not is that the loop is deliberately a conduit: it holds no capability it
//! cannot name a contract for. So it names one — [`ContextCompactor`](aik_api::context::ContextCompactor) — and asks it for room
//! when its own window says it is losing records. What the loop gained is one optional
//! collaborator; what it did not gain is a prompt, a second model call and a policy about
//! what to keep.
//!
//! # Security
//!
//! The whole input to this subsystem is untrusted, and its whole output goes back into the
//! conversation that produced it. Three rules follow, and they are enforced here rather than
//! documented as advice:
//!
//! * **The summarising call offers no tools.** Text goes in, text comes out. Nothing the
//!   transcript says can turn a summarisation into an action, because the call it would have
//!   to reach does not exist — this crate holds no
//!   [`ToolRegistry`](aik_api::tool::ToolRegistry) at all.
//! * **The recap is never pinned and never a system message.** It is model output, so it
//!   goes back as ordinary, evictable conversation. A model that could pin its own words, or
//!   speak as the deployment's own instructions, would have found a way to make an injected
//!   instruction permanent.
//! * **The excerpt is bounded and its delimiter cannot be closed from inside.** Every part
//!   is truncated, binary payloads are described rather than carried, and a transcript
//!   containing the delimiter has it neutralised — so
//!   content cannot end the data section and continue as prompt.
//!
//! Two further properties come from the order of operations: the model is asked *before*
//! anything is removed, so a failure costs a call and no history; and every store call uses
//! the caller's own [`ExecutionContext`](aik_api::execution::ExecutionContext), so a
//! compactor reaches exactly the sessions its caller could and never widens a principal.
//!
//! # What it deliberately does not do
//!
//! * **No summary of a summary in place.** A recap ages like any other record, and the next
//!   round summarises it along with the turns after it. There is no separate hierarchy of
//!   summaries to keep consistent, and no pinned record that grows forever.
//! * **No decision about when to compact.** It compacts when asked. The agent loop asks when
//!   its window reports dropped records, a scheduled job could ask on idle sessions, and
//!   neither policy belongs in here.
//! * **No retrieval.** What a recap leaves out is still in the store under its record id.
//!   Fetching it back is [`memory`](../aik_memory/index.html)'s problem, or a tool's, not
//!   this crate's.

mod component;
mod excerpt;
mod plan;
mod settings;
mod summariser;

use aik_api::context::SUMMARY_MARKER;

pub use component::{DEFAULT_COMPONENT_ID, SummaryComponent};
pub use settings::{
    DEFAULT_INSTRUCTIONS, DEFAULT_KEEP_RECENT_RECORDS, DEFAULT_MAX_EXCERPT_CHARS,
    DEFAULT_MAX_PART_CHARS, DEFAULT_MAX_SUMMARY_CHARS, DEFAULT_MIN_RETAINED_RECORDS,
    DEFAULT_MIN_SUMMARISED_RECORDS, DEFAULT_RETAIN_PERCENT, SummarySettings,
};
pub use summariser::Summariser;

/// The sentence that says what the recap is doing there.
///
/// A transcript is append-only, so a recap of the *beginning* of a session is stored at its
/// *end* — it is the newest record, sitting after turns it does not cover. Without a line
/// saying so, the likeliest reading of it is that somebody just said all that, and the
/// likeliest response is an answer to it. This is written by trusted code, ahead of anything
/// the model produced, and it is the reason the marker alone is not enough.
const FRAMING: &str =
    "The earlier part of this conversation is no longer shown in full. What it covered:";

/// Labels a recap as one, frames it, and bounds what re-enters the transcript.
///
/// The label is [`SUMMARY_MARKER`], for the reason that constant exists: a model reading its
/// own history has to be able to tell a recap of what was said from something that was said,
/// and so does a person reading the transcript afterwards.
///
/// The bound is the less obvious half. A summary is written by a model, from text that may
/// have asked it to produce something enormous, and it is about to be stored. Truncating it
/// here means the worst case is a recap that stops mid-sentence rather than a session that
/// swallowed a transcript it was supposed to be shrinking.
fn mark(summary: &str, records: usize, max_chars: usize) -> String {
    format!(
        "[{SUMMARY_MARKER} of {records} earlier turns] {FRAMING}\n{}",
        excerpt::truncate(summary, max_chars)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::context::ELISION_MARKER;

    #[test]
    fn a_recap_says_what_it_is_and_how_much_it_covers() {
        let marked = mark("they discussed tea", 12, 1_000);
        assert!(marked.starts_with(&format!("[{SUMMARY_MARKER} of 12 earlier turns]")));
        assert!(marked.ends_with("they discussed tea"));
    }

    #[test]
    fn an_enormous_recap_is_cut_before_it_is_stored() {
        let marked = mark(&"x".repeat(10_000), 3, 100);
        assert!(marked.len() < 300, "{} characters", marked.len());
        assert!(marked.contains(ELISION_MARKER));
    }
}
